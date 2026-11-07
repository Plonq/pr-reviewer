use super::*;

#[gpui::test]
fn immediate_inline_uses_shared_completion_and_keeps_editor_until_response(
    cx: &mut gpui::TestAppContext,
) {
    let fake = Arc::new(FakePr::default());
    let (window, app) = queue_window(cx, fake.clone());
    install_item(&app, commenting_item(&fake), cx);
    let mut vcx = gpui::VisualTestContext::from_window(window, cx);
    vcx.update(|window, cx| {
        app.update(cx, |app, cx| {
            app.open_composer(None, "a.rs".into(), CommentSide::Right, 1, 0, window, cx);
            app.composer()
                .unwrap()
                .input
                .update(cx, |input, cx| input.set_value("immediate", window, cx));
            app.submit_composer(window, cx);
            assert!(
                app.sessions[&0].busy,
                "immediate writes obey the same per-PR single flight"
            );
            assert!(
                app.composer().unwrap().in_flight,
                "step 2 does not close the editor early"
            );
            assert!(
                app.active_data().unwrap().posts.pending.is_empty(),
                "no diff optimism in step 2"
            );
            app.submit_composer(window, cx); // Repeated keyboard submission is ignored.
            app.close_composer(window, cx);
            app.open_composer(None, "a.rs".into(), CommentSide::Right, 1, 0, window, cx);
            app.composer()
                .unwrap()
                .input
                .update(cx, |input, cx| input.set_value("newer editor", window, cx));
        })
    });
    cx.run_until_parked();
    cx.update(|cx| {
        let app = app.read(cx);
        assert!(!app.sessions[&0].busy);
        assert_eq!(
            app.composer().unwrap().input.read(cx).value().as_str(),
            "newer editor"
        );
        assert_eq!(
            fake.posts().len(),
            1,
            "completion never repeats a successful write"
        );
        assert_eq!(
            app.active_data().unwrap().comments.as_ref().unwrap().counts["a.rs"],
            (1, 0)
        );
    });
}

#[gpui::test]
fn stale_comments_and_full_refreshes_cannot_undo_accepted_edits_or_deletes(
    cx: &mut gpui::TestAppContext,
) {
    for delete in [false, true] {
        let original = rc(
            11,
            "a.rs",
            Some("RIGHT"),
            Some(1),
            "confirmed old",
            "tester",
            "2026-01-01T00:00:00Z",
            None,
        );
        let fake = FakePr::with_threads(vec![original.clone()]);
        fake.comment_permissions.lock().unwrap().insert(
            11,
            github::CommentPermissions {
                node_id: "PRRC_11".into(),
                can_edit: true,
                can_delete: true,
                can_react: true,
            },
        );
        let (window, app) = queue_window(cx, fake.clone());
        let mut item = commenting_item(&fake);
        if let ItemState::Ready(data) = &mut item.state {
            data.discussion =
                Some(github::GitHubClient::pr_discussion(fake.as_ref(), &loop_loc()).unwrap());
        }
        install_item(&app, item, cx);
        let mut vcx = gpui::VisualTestContext::from_window(window, cx);
        fake.fail_discussion
            .store(true, std::sync::atomic::Ordering::SeqCst);
        vcx.update(|window, cx| {
            app.update(cx, |app, cx| {
                let data = app.active_data().unwrap();
                app.store.save_snapshot(
                    &review_target::discussion_snapshot(
                        &loop_loc(),
                        data.pr_meta.as_ref().unwrap(),
                        data.discussion.as_ref().unwrap(),
                        None,
                    )
                    .unwrap(),
                );
                let target = github::CommentTarget::ReviewComment("PRRC_11".into());
                let command = if delete {
                    SessionCommand::DeletePublished { target }
                } else {
                    SessionCommand::EditPublished {
                        target,
                        body: "accepted new".into(),
                    }
                };
                app.run_session(0, command, SessionCompletion::None, window, cx);
                assert_eq!(
                    app.active_data()
                        .unwrap()
                        .discussion
                        .as_ref()
                        .unwrap()
                        .threads[0]
                        .comments[0]
                        .body,
                    "confirmed old",
                    "no pre-response edit/delete optimism"
                );
            })
        });
        cx.run_until_parked(); // Write succeeded; its read failed.
        let assert_retained = |app: &ReviewApp| {
            let data = app.active_data().unwrap();
            let discussion = data.discussion.as_ref().unwrap();
            if delete {
                assert!(discussion.threads.is_empty());
                assert!(!data.comments.as_ref().unwrap().counts.contains_key("a.rs"));
            } else {
                assert_eq!(discussion.threads[0].comments[0].body, "accepted new");
                assert_eq!(
                    data.comments.as_ref().unwrap().threads["a.rs"][&(CommentSide::Right, 1)][0]
                        .root
                        .body,
                    "accepted new"
                );
            }
            assert_eq!(
                app.store
                    .snapshot("acme/widgets", 7)
                    .unwrap()
                    .discussion
                    .unwrap()["threads"][0]["comments"][0]["body"],
                "confirmed old",
                "unacknowledged reads cannot replace the confirmed cache"
            );
            assert_eq!(fake.posts().len(), 1);
        };
        cx.update(|cx| assert_retained(app.read(cx)));
        // A later request sequence alone is not proof of read freshness.
        *fake.comments.lock().unwrap() = vec![original];
        fake.fail_discussion
            .store(false, std::sync::atomic::Ordering::SeqCst);
        cx.update(|cx| app.update(cx, |app, cx| app.refetch_comments(0, loop_loc(), cx)));
        cx.run_until_parked();
        cx.update(|cx| assert_retained(app.read(cx)));
        cx.update(|cx| {
            app.update(cx, |app, cx| {
                app.spawn_fetch(0, Source::Pr(loop_loc()), ViewMode::Unified, cx)
            })
        });
        cx.run_until_parked();
        cx.update(|cx| {
            assert_retained(app.read(cx));
            assert!(
                app.read(cx).active_data().unwrap().diff_loaded,
                "stale comments do not prevent the new diff from installing"
            );
        });
        if delete {
            fake.comments.lock().unwrap().clear();
        } else {
            fake.comments.lock().unwrap()[0].body = "accepted new".into();
        }
        cx.update(|cx| app.update(cx, |app, cx| app.refetch_comments(0, loop_loc(), cx)));
        cx.run_until_parked();
        cx.update(|cx| {
            let app = app.read(cx);
            let snapshot = app
                .store
                .snapshot("acme/widgets", 7)
                .unwrap()
                .discussion
                .unwrap();
            if delete {
                assert!(snapshot["threads"].as_array().unwrap().is_empty());
            } else {
                assert_eq!(
                    snapshot["threads"][0]["comments"][0]["body"],
                    "accepted new"
                );
            }
            assert_eq!(fake.posts().len(), 1);
        });
    }
}

#[test]
fn full_fetch_started_before_a_write_cannot_replace_the_completed_write() {
    let fake = FakePr::with_threads(vec![rc(
        11,
        "a.rs",
        Some("RIGHT"),
        Some(1),
        "old",
        "tester",
        "2026-01-01T00:00:00Z",
        None,
    )]);
    let mut item = commenting_item(&fake);
    let full_ticket = item.comment_sync.begin_read();
    item.comment_sync.begin_write();
    let target =
        github::MutationTarget::Published(github::CommentTarget::ReviewComment("node".into()));
    item.comment_sync
        .finish_write(Some(&github::MutationReceipt::Edited {
            target,
            body: "accepted".into(),
        }));
    if let ItemState::Ready(data) = &mut item.state {
        data.discussion.as_mut().unwrap().threads[0].comments[0]
            .permissions
            .node_id = "node".into();
        data.discussion.as_mut().unwrap().threads[0].comments[0].body = "accepted".into();
    }
    // Even a coincidentally matching pre-write response cannot acknowledge
    // the operation and retire its barrier.
    fake.comments.lock().unwrap()[0].body = "accepted".into();
    fake.comment_permissions.lock().unwrap().insert(
        11,
        github::CommentPermissions {
            node_id: "node".into(),
            ..Default::default()
        },
    );
    let loaded = fetch_item(fake, &item.source, ViewMode::Unified, 0).unwrap();
    assert!(
        item.install_fetched(loaded, full_ticket).is_none(),
        "rejected full snapshots are never persisted"
    );
    let ItemState::Ready(data) = &item.state else {
        panic!("ready")
    };
    assert_eq!(
        data.discussion.as_ref().unwrap().threads[0].comments[0].body,
        "accepted"
    );
    assert_eq!(
        data.comments.as_ref().unwrap().threads["a.rs"][&(CommentSide::Right, 1)][0]
            .root
            .body,
        "accepted"
    );
    let mut stale = data.discussion.as_ref().unwrap().clone();
    stale.threads[0].comments[0].body = "old".into();
    let fresh_ticket = item.comment_sync.begin_read();
    assert!(
        !item.comment_sync.accept(fresh_ticket, &stale),
        "pre-write equality did not retire the successful write barrier"
    );
}
