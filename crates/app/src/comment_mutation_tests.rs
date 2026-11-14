use super::*;
use crate::comments::Entry;
use std::sync::atomic::Ordering::SeqCst;

fn fake_mutations(pending: bool) -> Arc<FakePr> {
    let fake = FakePr::with_threads(if pending {
        vec![]
    } else {
        vec![
            rc(
                11,
                "a.rs",
                Some("RIGHT"),
                Some(1),
                "original root",
                "alice",
                "2026-01-01T00:00:00Z",
                None,
            ),
            rc(
                12,
                "a.rs",
                Some("RIGHT"),
                Some(1),
                "original reply",
                "bob",
                "2026-01-01T00:01:00Z",
                Some(11),
            ),
        ]
    });
    for id in [11, 12] {
        fake.comment_permissions
            .lock()
            .unwrap()
            .insert(id, permitted_comment(&format!("node-{id}"), true, true));
    }
    if pending {
        *fake.pending.lock().unwrap() = Some(github::PendingReview {
            id: 91,
            node_id: "pending".into(),
            commit_id: Some("dead10cc".into()),
            body: String::new(),
            comments: [11, 12]
                .into_iter()
                .map(|id| github::PendingComment {
                    id,
                    node_id: format!("node-{id}"),
                    path: "a.rs".into(),
                    side: CommentSide::Right,
                    line: Some(1),
                    start_line: None,
                    body: if id == 11 {
                        "original root"
                    } else {
                        "original reply"
                    }
                    .into(),
                    in_reply_to_id: (id == 12).then_some(11),
                })
                .collect(),
        });
    }
    fake
}
fn mutation_item(fake: &FakePr) -> ReviewItem {
    let mut item = commenting_item(fake);
    if let ItemState::Ready(data) = &mut item.state {
        data.discussion = Some(github::GitHubClient::pr_discussion(fake, &loop_loc()).unwrap());
        data.diff = crate::test_util::sample_diff();
        data.project_comments(None);
        data.rebuild_rows_anchored();
    }
    item
}
fn seed_pending(app: &mut ReviewApp, fake: &FakePr) {
    app.sessions.entry(0).or_default().state = ReviewSession {
        pending: fake.pending.lock().unwrap().clone(),
        verified: true,
    };
    app.sessions.get_mut(&0).unwrap().comments_expanded = true;
    app.sync_pending_comments(0);
}
fn target(pending: bool, reply: bool) -> CommentTarget {
    let id = if reply { 12 } else { 11 };
    if pending {
        CommentTarget::Pending(id)
    } else {
        CommentTarget::Published(github::CommentTarget::ReviewComment(format!("node-{id}")))
    }
}
fn bodies(app: &ReviewApp) -> Vec<String> {
    app.active_data()
        .unwrap()
        .comments
        .as_ref()
        .unwrap()
        .threads
        .values()
        .flat_map(|anchors| anchors.values().flatten())
        .flat_map(|t| std::iter::once(&t.root).chain(&t.replies))
        .map(|c| c.body.clone())
        .collect()
}
fn start(
    app: &mut ReviewApp,
    pending: bool,
    reply: bool,
    deleting: bool,
    placement: ComposerPlacement,
    window: &mut Window,
    cx: &mut Context<ReviewApp>,
) -> u64 {
    let target = target(pending, reply);
    if deleting {
        app.delete_comment(target, window, cx);
    } else {
        app.edit_comment(target, placement, window, cx);
        app.composer()
            .expect("editable")
            .input
            .update(cx, |input, cx| {
                input.set_value("submitted edit", window, cx)
            });
        app.submit_composer(window, cx);
        assert!(app.composer().is_none());
    }
    app.active_data().unwrap().posts.changes.active[0].token
}

#[gpui::test]
fn edit_delete_projects_before_response_in_both_modes_and_all_comment_surfaces(
    cx: &mut gpui::TestAppContext,
) {
    for pending in [false, true] {
        for mode in [ViewMode::Unified, ViewMode::Split] {
            for surface in 0..if pending { 3 } else { 2 } {
                for reply in [false, true] {
                    for deleting in [false, true] {
                        let fake = fake_mutations(pending);
                        let (window, app) = queue_window(cx, fake.clone());
                        install_item(&app, mutation_item(&fake), cx);
                        let mut vcx = gpui::VisualTestContext::from_window(window, cx);
                        vcx.update(|window, cx| {
                            app.update(cx, |app, cx| {
                                seed_pending(app, &fake);
                                app.items[0].pane = if surface == 1 {
                                    ItemPane::Conversation
                                } else {
                                    ItemPane::Changes
                                };
                                app.active_data_mut().unwrap().mode = mode;
                                let placement = match surface {
                                    1 => ComposerPlacement::ConversationComment(
                                        target(pending, reply).key(),
                                    ),
                                    2 => {
                                        ComposerPlacement::PendingTray(if reply { 12 } else { 11 })
                                    }
                                    _ => ComposerPlacement::Diff,
                                };
                                let before =
                                    serde_json::to_value(&app.active_data().unwrap().discussion)
                                        .unwrap();
                                let before_pending = app.sessions[&0].state.clone();
                                start(app, pending, reply, deleting, placement, window, cx);
                                assert!(app.sessions[&0].busy);
                                assert_eq!(
                                    fake.mutation_calls.load(SeqCst),
                                    0,
                                    "before backend response"
                                );
                                assert_eq!(
                                    serde_json::to_value(&app.active_data().unwrap().discussion)
                                        .unwrap(),
                                    before
                                );
                                assert_eq!(
                                    app.sessions[&0]
                                        .state
                                        .pending
                                        .as_ref()
                                        .map(|r| r.comments.len()),
                                    before_pending.pending.as_ref().map(|r| r.comments.len())
                                );
                                let projected = bodies(app);
                                if deleting {
                                    assert_eq!(
                                        projected,
                                        [if reply {
                                            "original root"
                                        } else {
                                            "original reply"
                                        }]
                                    );
                                } else {
                                    assert_eq!(projected[usize::from(reply)], "submitted edit");
                                }
                                let view = app.active_comment_projection().unwrap();
                                assert_eq!(
                                    view.pending_count(),
                                    if pending {
                                        2 - usize::from(deleting)
                                    } else {
                                        0
                                    }
                                );
                                if !pending {
                                    let Entry::Thread(t) = &view.conversation()[0] else {
                                        panic!()
                                    };
                                    assert_eq!(t.key.0, "11");
                                    assert_eq!(t.comments.len(), 2 - usize::from(deleting));
                                }
                                app.submit_composer(window, cx);
                                app.delete_comment(target(pending, reply), window, cx);
                                // no queued duplicate
                            });
                            let _ = window.draw(cx);
                        });
                        vcx.run_until_parked();
                        cx.update(|cx| {
                            let app = app.read(cx);
                            assert_eq!(fake.mutation_calls.load(SeqCst), 1);
                            assert!(!app.sessions[&0].busy);
                            assert!(app.composer().is_none());
                            assert!(app.active_data().unwrap().posts.changes.active.is_empty());
                            if deleting {
                                assert_eq!(
                                    bodies(app),
                                    [if reply {
                                        "original root"
                                    } else {
                                        "original reply"
                                    }]
                                );
                            } else {
                                assert_eq!(bodies(app)[usize::from(reply)], "submitted edit");
                            }
                            if pending {
                                let saved = app.store.review_session(&loop_loc());
                                assert_eq!(saved.count(), 2 - usize::from(deleting));
                                if !deleting {
                                    assert_eq!(
                                        saved.pending.unwrap().comments[usize::from(reply)].body,
                                        "submitted edit"
                                    );
                                }
                            }
                        });
                    }
                }
            }
        }
    }
}

#[gpui::test]
fn rejected_edits_and_deletes_restore_only_confirmed_content_and_preserve_new_editors(
    cx: &mut gpui::TestAppContext,
) {
    for pending in [false, true] {
        for deleting in [false, true] {
            let fake = fake_mutations(pending);
            fake.fail.store(true, SeqCst);
            let (window, app) = queue_window(cx, fake.clone());
            install_item(&app, mutation_item(&fake), cx);
            let mut vcx = gpui::VisualTestContext::from_window(window, cx);
            let (token, newer) = vcx.update(|window, cx| {
                app.update(cx, |app, cx| {
                    seed_pending(app, &fake);
                    let token = start(
                        app,
                        pending,
                        false,
                        deleting,
                        ComposerPlacement::ConversationComment(target(pending, false).key()),
                        window,
                        cx,
                    );
                    app.open_comment_composer(
                        ComposerPlacement::ConversationMain,
                        None,
                        String::new(),
                        CommentSide::Right,
                        0,
                        0,
                        window,
                        cx,
                    );
                    app.composer()
                        .unwrap()
                        .input
                        .update(cx, |input, cx| input.set_value("new editor", window, cx));
                    (token, app.composer().unwrap().id)
                })
            });
            vcx.run_until_parked();
            vcx.update(|window, cx| {
                app.update(cx, |app, cx| {
                    assert_eq!(bodies(app), ["original root", "original reply"]);
                    assert_eq!(app.composer().unwrap().id, newer);
                    assert_eq!(
                        app.composer().unwrap().input.read(cx).value().as_str(),
                        "new editor"
                    );
                    assert_eq!(app.active_data().unwrap().posts.changes.failed.len(), 1);
                    app.restore_comment_change(0, token, window, cx);
                    assert_eq!(app.composer().unwrap().id, newer);
                    app.close_composer(window, cx);
                    app.restore_comment_change(0, token, window, cx);
                    if deleting {
                        assert!(
                            app.composer().is_none(),
                            "a failed delete must never repost a body"
                        );
                    } else {
                        assert_eq!(
                            app.composer().unwrap().input.read(cx).value().as_str(),
                            "submitted edit"
                        );
                        assert_eq!(
                            app.composer().unwrap().edit_target,
                            Some(target(pending, false))
                        );
                    }
                    assert_eq!(
                        fake.mutation_calls.load(SeqCst),
                        1,
                        "restore is explicit editing, not a retry"
                    );
                })
            });
        }
    }
}

#[gpui::test]
fn uncertain_edits_deletes_reconcile_by_target_or_remain_blocked_without_retries(
    cx: &mut gpui::TestAppContext,
) {
    for pending in [false, true] {
        for deleting in [false, true] {
            for committed in [false, true] {
                let fake = fake_mutations(pending);
                fake.fail.store(!committed, SeqCst);
                fake.unknown_mutation.store(true, SeqCst);
                let (window, app) = queue_window(cx, fake.clone());
                install_item(&app, mutation_item(&fake), cx);
                let mut vcx = gpui::VisualTestContext::from_window(window, cx);
                let token = vcx.update(|window, cx| {
                    app.update(cx, |app, cx| {
                        seed_pending(app, &fake);
                        start(
                            app,
                            pending,
                            false,
                            deleting,
                            ComposerPlacement::Diff,
                            window,
                            cx,
                        )
                    })
                });
                vcx.run_until_parked();
                vcx.update(|window, cx| {
                    app.update(cx, |app, cx| {
                        assert_eq!(fake.mutation_calls.load(SeqCst), 1);
                        assert_eq!(app.comment_write_blocked(0), !committed);
                        if !committed {
                            if deleting {
                                assert_eq!(bodies(app), ["original reply"]);
                            } else {
                                assert_eq!(bodies(app)[0], "submitted edit");
                            }
                            // Confirmed mirrors/cache cannot contain the unverified body/deletion.
                            if pending {
                                let saved = app.store.review_session(&loop_loc());
                                assert_eq!(saved.count(), 2);
                                assert_eq!(
                                    saved.pending.unwrap().comments[0].body,
                                    "original root"
                                );
                            }
                            app.delete_comment(target(pending, true), window, cx);
                            app.run_session(
                                0,
                                SessionCommand::Submit {
                                    commit_id: "dead10cc".into(),
                                    verdict: github::ReviewVerdict::Comment,
                                    body: "review".into(),
                                },
                                SessionCompletion::None,
                                window,
                                cx,
                            );
                            assert!(!app.sessions[&0].busy);
                            app.dismiss_comment_change(0, token, cx);
                            assert!(!app.comment_write_blocked(0));
                            assert_eq!(bodies(app), ["original root", "original reply"]);
                            app.restore_comment_change(0, token, window, cx);
                            assert!(
                                app.composer().is_none(),
                                "unknown outcomes cannot silently retry"
                            );
                        } else {
                            assert!(app.active_data().unwrap().posts.changes.active.is_empty());
                            if deleting {
                                assert_eq!(bodies(app), ["original reply"]);
                            } else {
                                assert_eq!(bodies(app)[0], "submitted edit");
                            }
                        }
                        assert_eq!(fake.mutation_calls.load(SeqCst), 1);
                    })
                });
            }
        }
    }
}

#[gpui::test]
fn pending_edit_delete_receipts_protect_stale_refreshes_and_confirmed_session_mirrors(
    cx: &mut gpui::TestAppContext,
) {
    for deleting in [false, true] {
        let fake = fake_mutations(true);
        let stale = fake.pending.lock().unwrap().clone();
        let (window, app) = queue_window(cx, fake.clone());
        install_item(&app, mutation_item(&fake), cx);
        let mut vcx = gpui::VisualTestContext::from_window(window, cx);
        vcx.update(|window, cx| {
            app.update(cx, |app, cx| {
                seed_pending(app, &fake);
                // A successful receipt can install without waiting for its follow-up read.
                let command = if deleting {
                    SessionCommand::Delete { id: 11 }
                } else {
                    SessionCommand::Edit {
                        id: 11,
                        body: "accepted".into(),
                    }
                };
                let op = app
                    .comment_change(0, &command, 99, ComposerPlacement::Diff)
                    .unwrap();
                let receipt = if deleting {
                    github::MutationReceipt::Deleted {
                        target: op.target.clone(),
                    }
                } else {
                    github::MutationReceipt::Edited {
                        target: op.target.clone(),
                        body: "accepted".into(),
                    }
                };
                let data = app.active_data_mut().unwrap();
                data.posts.changes.begin(op, None, stale.as_ref());
                data.posts.changes.finish(99, &Ok(receipt));
                let mut state = ReviewSession {
                    pending: stale.clone(),
                    verified: true,
                };
                if deleting {
                    state.pending.as_mut().unwrap().comments.remove(0);
                } else {
                    state.pending.as_mut().unwrap().comments[0].body = "accepted".into();
                }
                app.finish_session(0, &loop_loc(), state, &Ok(()), cx);
                app.resume_session(0, cx); // Fake still returns the old server snapshot.
                let _ = window;
            })
        });
        vcx.run_until_parked();
        cx.update(|cx| {
            let app = app.read(cx);
            let saved = app.store.review_session(&loop_loc());
            assert_eq!(saved.count(), 2 - usize::from(deleting));
            if deleting {
                assert_eq!(bodies(app), ["original reply"]);
            } else {
                assert_eq!(saved.pending.unwrap().comments[0].body, "accepted");
                assert_eq!(bodies(app)[0], "accepted");
            }
            assert_eq!(
                fake.mutation_calls.load(SeqCst),
                0,
                "verification never replays a write"
            );
        });
    }
}

#[test]
fn unverified_edit_requires_an_eligible_full_snapshot_even_when_the_old_read_matches() {
    let fake = fake_mutations(false);
    let mut item = mutation_item(&fake);
    let ticket = item.comment_sync.begin_read();
    item.comment_sync.begin_write();
    let target =
        github::MutationTarget::Published(github::CommentTarget::ReviewComment("node-11".into()));
    let ItemState::Ready(data) = &mut item.state else {
        panic!()
    };
    data.posts.changes.begin(
        crate::comments::mutations::Change {
            token: 1,
            key: crate::comments::CommentKey::Inline(11),
            target,
            body: Some("accepted".into()),
            recovery_body: "accepted".into(),
            placement: ComposerPlacement::Diff,
            uncertain: false,
            error: None,
        },
        data.discussion.as_ref(),
        None,
    );
    data.posts.changes.finish(
        1,
        &Err(github::ClientError::Failed("transport lost".into())),
    );
    item.comment_sync.finish_write(None);
    fake.comments.lock().unwrap()[0].body = "accepted".into();
    let loaded = fetch_item(fake.clone(), &item.source, ViewMode::Unified, 0).unwrap();
    assert!(item.install_fetched(loaded, ticket).is_none());
    let ItemState::Ready(data) = &item.state else {
        panic!()
    };
    assert!(data.posts.has_uncertain());
    assert_eq!(
        data.discussion.as_ref().unwrap().threads[0].comments[0].body,
        "original root"
    );
    let ticket = item.comment_sync.begin_read();
    let loaded = fetch_item(fake, &item.source, ViewMode::Unified, 0).unwrap();
    assert!(item.install_fetched(loaded, ticket).is_some());
    let ItemState::Ready(data) = &item.state else {
        panic!()
    };
    assert!(!data.posts.has_uncertain());
    assert_eq!(
        data.discussion.as_ref().unwrap().threads[0].comments[0].body,
        "accepted"
    );
}

#[gpui::test]
fn edit_completion_after_item_close_still_saves_confirmed_pending_state(
    cx: &mut gpui::TestAppContext,
) {
    let fake = fake_mutations(true);
    let (window, app) = queue_window(cx, fake.clone());
    install_item(&app, mutation_item(&fake), cx);
    let mut vcx = gpui::VisualTestContext::from_window(window, cx);
    vcx.update(|window, cx| {
        app.update(cx, |app, cx| {
            seed_pending(app, &fake);
            start(app, true, false, false, ComposerPlacement::Diff, window, cx);
            app.items.clear();
            app.show_queue = true;
        })
    });
    vcx.run_until_parked();
    cx.update(|cx| {
        let app = app.read(cx);
        assert!(app.sessions.is_empty());
        assert_eq!(
            app.store
                .review_session(&loop_loc())
                .pending
                .unwrap()
                .comments[0]
                .body,
            "submitted edit"
        );
        assert_eq!(fake.mutation_calls.load(SeqCst), 1);
    });
}
