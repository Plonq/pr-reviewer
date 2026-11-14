use super::*;

#[gpui::test]
fn close_reopen_cannot_replace_a_delayed_mutation_lock(cx: &mut gpui::TestAppContext) {
    for pending in [false, true] {
        for fail in [false, true] {
            let fake = Arc::new(FakePr::default());
            fake.fail.store(fail, Ordering::SeqCst);
            fake.fail_pending_create.store(fail, Ordering::SeqCst);
            let (window, app) = queue_window(cx, fake.clone());
            install_item(&app, commenting_item(&fake), cx);
            let mut vcx = gpui::VisualTestContext::from_window(window, cx);
            vcx.update(|window, cx| {
                app.update(cx, |app, cx| {
                    app.open_composer(None, "a.rs".into(), CommentSide::Right, 1, 0, window, cx);
                    app.composer_mut().unwrap().pending = pending;
                    app.composer().unwrap().input.update(cx, |input, cx| {
                        input.set_value("keep this write", window, cx)
                    });
                    app.submit_composer(window, cx);
                    assert!(app.sessions[&0].busy);
                    assert!(app.items[0].comment_sync.is_writing());
                    // GPUI's deterministic executor holds the fake write until
                    // run_until_parked, so close/reopen races need no sleeps.
                    assert!(fake.posts().is_empty());
                    assert_eq!(fake.pending_create_calls.load(Ordering::SeqCst), 0);

                    app.open_composer(None, "a.rs".into(), CommentSide::Right, 1, 0, window, cx);
                    let editor = app.composer().unwrap().id;
                    app.composer()
                        .unwrap()
                        .input
                        .update(cx, |input, cx| input.set_value("newer editor", window, cx));
                    app.close_item(0, cx);
                    assert_eq!(app.items.len(), 1);
                    assert_eq!(app.composer().unwrap().id, editor);
                    assert!(app.sessions[&0]
                        .error
                        .as_ref()
                        .unwrap()
                        .contains("before closing"));
                    app.open_item(Source::Pr(loop_loc()), cx);
                    assert_eq!(app.items.len(), 1);
                    assert_eq!(app.items[0].id, 0, "reopen must keep the original lock");
                    assert_eq!(app.next_id, 1);
                    app.run_session(
                        0,
                        SessionCommand::Conversation {
                            body: "second write".into(),
                        },
                        SessionCompletion::None,
                        window,
                        cx,
                    );

                    // Only the writing PR is held open.
                    let mut other = loop_pr_item(vec![]);
                    other.id = 1;
                    if let Source::Pr(loc) = &mut other.source {
                        loc.number += 1;
                    }
                    app.items.push(other);
                    app.close_item(1, cx);
                    assert_eq!(app.items.len(), 1);
                })
            });
            cx.run_until_parked();
            assert_eq!(fake.posts().len(), usize::from(!pending));
            assert_eq!(
                fake.pending_create_calls.load(Ordering::SeqCst),
                usize::from(pending)
            );
            vcx.update(|window, cx| {
                app.update(cx, |app, cx| {
                    assert!(!app.sessions[&0].busy);
                    assert!(!app.items[0].comment_sync.is_writing());
                    assert_eq!(app.sessions[&0].error.is_some(), fail);
                    assert_eq!(
                        app.composer().unwrap().input.read(cx).value(),
                        "newer editor"
                    );
                    if fail {
                        let recovery = &app.active_data().unwrap().posts.failed[0];
                        assert_eq!(recovery.body, "keep this write");
                        if pending {
                            assert_eq!(app.sessions[&0].state.pending.as_ref().unwrap().id, 91);
                            assert_eq!(app.store.review_session(&loop_loc()).count(), 0);
                        }
                        // The rejected close must not lose the failed write's
                        // recovery action or overwrite a newer editor.
                        let token = recovery.token;
                        app.close_composer(window, cx);
                        app.restore_comment_draft(0, token, window, cx);
                        assert_eq!(
                            app.composer().unwrap().input.read(cx).value(),
                            "keep this write"
                        );
                    }
                    app.close_item(0, cx);
                    assert!(
                        app.items.is_empty(),
                        "success and failure both release the close guard"
                    );
                    assert!(app.composers.is_empty());
                    assert!(app.show_queue);
                })
            });
        }
    }
}

#[gpui::test]
fn all_session_mutations_hold_the_tab_until_completion(cx: &mut gpui::TestAppContext) {
    let anchor = github::CommentAnchor {
        commit_id: "dead10cc".into(),
        path: "a.rs".into(),
        side: CommentSide::Right,
        line: 1,
        start_line: None,
        position: None,
    };
    let target = github::CommentTarget::ReviewComment("node-11".into());
    let commands = [
        (
            false,
            SessionCommand::Start {
                commit_id: "dead10cc".into(),
            },
        ),
        (
            false,
            SessionCommand::Add {
                anchor: anchor.clone(),
                body: "draft".into(),
            },
        ),
        (
            false,
            SessionCommand::Reply {
                comment_id: 11,
                body: "reply".into(),
            },
        ),
        (
            true,
            SessionCommand::Reply {
                comment_id: 11,
                body: "draft reply".into(),
            },
        ),
        (
            false,
            SessionCommand::ConversationReply {
                comment_id: 11,
                commit_id: "dead10cc".into(),
                body: "reply".into(),
                pending: false,
            },
        ),
        (
            false,
            SessionCommand::ConversationReply {
                comment_id: 11,
                commit_id: "dead10cc".into(),
                body: "draft reply".into(),
                pending: true,
            },
        ),
        (
            false,
            SessionCommand::Conversation {
                body: "comment".into(),
            },
        ),
        (
            true,
            SessionCommand::Edit {
                id: 13,
                body: "edited draft".into(),
            },
        ),
        (true, SessionCommand::Delete { id: 13 }),
        (
            false,
            SessionCommand::EditPublished {
                target: target.clone(),
                body: "edited".into(),
            },
        ),
        (
            false,
            SessionCommand::DeletePublished {
                target: target.clone(),
            },
        ),
        (
            false,
            SessionCommand::SetReaction {
                target,
                content: "HEART".into(),
                reacted: true,
            },
        ),
        (
            false,
            SessionCommand::SetThreadResolved {
                thread_id: "11".into(),
                resolved: true,
            },
        ),
        (
            false,
            SessionCommand::Submit {
                commit_id: "dead10cc".into(),
                verdict: github::ReviewVerdict::Approve,
                body: String::new(),
            },
        ),
        (
            true,
            SessionCommand::Submit {
                commit_id: "dead10cc".into(),
                verdict: github::ReviewVerdict::Approve,
                body: String::new(),
            },
        ),
        (
            false,
            SessionCommand::Immediate {
                anchor,
                body: "inline".into(),
            },
        ),
    ];
    for (pending, command) in commands {
        let fake = FakePr::with_threads(vec![rc(
            11,
            "a.rs",
            Some("RIGHT"),
            Some(1),
            "root",
            "alice",
            "",
            None,
        )]);
        fake.comment_permissions
            .lock()
            .unwrap()
            .insert(11, permitted_comment("node-11", true, true));
        if pending {
            *fake.pending.lock().unwrap() = Some(github::PendingReview {
                id: 91,
                node_id: "pending".into(),
                commit_id: Some("dead10cc".into()),
                body: String::new(),
                comments: vec![github::PendingComment {
                    id: 13,
                    node_id: "draft".into(),
                    path: "a.rs".into(),
                    side: CommentSide::Right,
                    line: Some(1),
                    start_line: None,
                    body: "draft".into(),
                    in_reply_to_id: None,
                }],
            });
        }
        let (window, app) = queue_window(cx, fake.clone());
        install_item(&app, commenting_item(&fake), cx);
        let mut vcx = gpui::VisualTestContext::from_window(window, cx);
        vcx.update(|window, cx| {
            app.update(cx, |app, cx| {
                app.run_session(0, command.clone(), SessionCompletion::None, window, cx);
                app.close_item(0, cx);
                assert_eq!(app.items.len(), 1, "{command:?}");
                assert!(app.items[0].comment_sync.is_writing(), "{command:?}");
                assert!(
                    app.session_job(
                        0,
                        SessionCommand::Conversation {
                            body: "second write".into()
                        },
                        cx
                    )
                    .is_none(),
                    "{command:?}"
                );
            })
        });
        cx.run_until_parked();
        cx.update(|cx| {
            app.update(cx, |app, cx| {
                assert!(!app.sessions[&0].busy, "{command:?}");
                assert!(
                    app.sessions[&0].error.is_none(),
                    "{command:?}: {:?}",
                    app.sessions[&0].error
                );
                assert!(!app.items[0].comment_sync.is_writing(), "{command:?}");
                app.close_item(0, cx);
                assert!(app.items.is_empty(), "{command:?}");
            })
        });
    }
}

#[gpui::test]
fn read_only_resume_can_close_and_cleans_up_its_old_session(cx: &mut gpui::TestAppContext) {
    let fake = Arc::new(FakePr::default());
    let (_, app) = queue_window(cx, fake.clone());
    install_item(&app, commenting_item(&fake), cx);
    cx.update(|cx| {
        app.update(cx, |app, cx| {
            app.resume_session(0, cx);
            assert!(app.sessions[&0].busy);
            assert!(!app.items[0].comment_sync.is_writing());
            app.close_item(0, cx);
            assert!(app.items.is_empty());
            app.open_item(Source::Pr(loop_loc()), cx);
            assert_eq!(app.items[0].id, 1);
        })
    });
    cx.run_until_parked();
    cx.update(|cx| {
        let app = app.read(cx);
        assert!(
            !app.sessions.contains_key(&0),
            "a late read removes only its old session"
        );
        assert!(!app.sessions[&1].busy);
        assert!(!app.items[0].comment_sync.is_writing());
    });
    assert!(fake.posts().is_empty());
}

#[gpui::test]
fn immediate_inline_projects_and_closes_editor_before_response(cx: &mut gpui::TestAppContext) {
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
            assert!(app.composer().is_none(), "creation closes immediately");
            let data = app.active_data().unwrap();
            assert_eq!(data.posts.pending.len(), 1);
            assert_eq!(data.comments.as_ref().unwrap().counts["a.rs"], (1, 0));
            assert!(matches!(
                data.comments.as_ref().unwrap().threads["a.rs"][&(CommentSide::Right, 1)][0]
                    .root
                    .id,
                comments::CommentKey::Post(_)
            ));
            assert!(
                data.discussion
                    .as_ref()
                    .is_none_or(|d| d.threads.is_empty()),
                "confirmed data is untouched"
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
