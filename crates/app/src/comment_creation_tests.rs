use super::*;
use crate::comments::{Comment, CommentKey, Entry};

fn pending_review() -> github::PendingReview {
    github::PendingReview {
        id: 91,
        node_id: "pending".into(),
        body: String::new(),
        comments: vec![],
    }
}

fn indexed_keys(data: &ItemData) -> Vec<CommentKey> {
    let mut keys: Vec<_> = data
        .comments
        .as_ref()
        .unwrap()
        .threads
        .values()
        .flat_map(|anchors| anchors.values().flatten())
        .flat_map(|t| std::iter::once(&t.root).chain(&t.replies))
        .map(|c| c.id)
        .collect();
    keys.sort();
    keys
}

#[gpui::test]
fn creation_projects_all_surfaces_and_closes_before_the_response_in_both_modes(
    cx: &mut gpui::TestAppContext,
) {
    for mode in [ViewMode::Unified, ViewMode::Split] {
        for conversation in [false, true] {
            for pending in [false, true] {
                for reply in [false, true] {
                    if conversation && !reply {
                        continue;
                    } // Inline roots start in Changes; main comments/AddSummary are separate.
                    let fake = FakePr::with_threads(vec![rc(
                        11,
                        "a.rs",
                        Some("RIGHT"),
                        Some(1),
                        "original",
                        "alice",
                        "2026-01-01T00:00:00Z",
                        None,
                    )]);
                    if pending && reply {
                        *fake.pending.lock().unwrap() = Some(pending_review());
                    }
                    let (window, app) = queue_window(cx, fake.clone());
                    install_item(&app, commenting_item(&fake), cx);
                    let mut vcx = gpui::VisualTestContext::from_window(window, cx);
                    let (token, before_rows) = vcx.update(|window, cx| {
                        let (token, _) = app.update(cx, |app, cx| {
                        if pending && reply { app.sessions.entry(0).or_default().state = ReviewSession { pending: Some(pending_review()), verified: true }; }
                        app.sessions.entry(0).or_default().comments_expanded = true;
                        app.items[0].pane = if conversation { ItemPane::Conversation } else { ItemPane::Changes };
                        let data = app.active_data_mut().unwrap(); data.mode = mode; data.diff = crate::test_util::sample_diff(); data.rebuild_rows_anchored();
                        let thread_id = data.discussion.as_ref().unwrap().threads[0].id.clone();
                        let confirmed = serde_json::to_value(&data.discussion).unwrap();
                        let placement = if conversation { ComposerPlacement::ConversationThread(thread_id) } else { ComposerPlacement::Diff };
                        app.open_comment_composer(placement, reply.then_some(11), "a.rs".into(), CommentSide::Right, if reply { 1 } else { 2 }, 1, window, cx);
                        let composer = app.composer_mut().unwrap(); composer.pending = pending;
                        // Include a ranged root; the same submitted anchor is used before and after receipt.
                        if !reply { composer.start_line = Some(1); }
                        composer.input.update(cx, |input, cx| input.set_value("new inline", window, cx));
                        let token = composer.id;
                        app.submit_composer(window, cx);
                        assert!(app.composer().is_none(), "split={}, pending={pending}, reply={reply}, conversation={conversation}", mode == ViewMode::Split);
                        assert!(app.sessions[&0].busy);
                        let data = app.active_data().unwrap();
                        assert_eq!(serde_json::to_value(&data.discussion).unwrap(), confirmed);
                        assert_eq!(indexed_keys(data), [CommentKey::Inline(11), CommentKey::Post(token)]);
                        let projection = data.comment_projection(app.sessions[&0].state.pending.as_ref());
                        assert_eq!(projection.pending_count(), usize::from(pending));
                        assert_eq!(projection.diff_index().counts["a.rs"], (2, 0));
                        if pending && !reply {
                            assert!(app.sessions[&0].state.pending.is_none(), "no speculative server review");
                            assert!(!app.sessions[&0].state.verified);
                        }
                        if reply {
                            assert!(projection.conversation().iter().any(|entry| matches!(entry, Entry::Thread(t) if t.comments.iter().any(|c| matches!(c, Comment::Posting(p) if p.token == token)))));
                        } else if !pending {
                            assert!(projection.conversation().iter().any(|entry| matches!(entry, Entry::CreatedThread(p, _) if p.token == token)));
                        }
                        let row_count = data.rows.len();
                        app.submit_composer(window, cx); // Repeated keyboard activation cannot queue another write.
                        (token, row_count)
                        });
                        let _ = window.draw(cx);
                        window.refresh();
                        let _ = window.draw(cx);
                        (token, app.read(cx).active_data().unwrap().rows.len())
                    });
                    // No executor progress has occurred: assertions above are before the backend response.
                    assert!(fake.posts().is_empty());
                    vcx.run_until_parked();
                    vcx.update(|window, cx| {
                        app.update(cx, |app, cx| {
                            assert!(!app.sessions[&0].busy);
                            assert!(app.composer().is_none());
                            let data = app.active_data().unwrap();
                            assert!(data.posts.pending.is_empty(), "pending={pending}, reply={reply}, conversation={conversation}, error={:?}, status={:?}", app.sessions[&0].error, data.posts.pending.first().map(|p| p.status));
                            assert_eq!(
                                indexed_keys(data),
                                [CommentKey::Inline(11), CommentKey::Post(token)]
                            );
                            assert_eq!(
                                data.rows.len(),
                                before_rows,
                                "metadata-only acknowledgement keeps row reservations"
                            );
                            let projection =
                                data.comment_projection(app.sessions[&0].state.pending.as_ref());
                            assert_eq!(projection.pending_count(), usize::from(pending));
                            assert!(projection.remote_id(CommentKey::Post(token)).is_some());
                            assert_eq!(
                                app.store
                                    .review_session(&loop_loc())
                                    .pending
                                    .as_ref()
                                    .map_or(0, |r| r.comments.len()),
                                usize::from(pending)
                            );
                            app.items[0].pane = ItemPane::Conversation;
                            cx.notify();
                        });
                        let _ = window.draw(cx);
                    });
                    if !pending {
                        assert_eq!(fake.posts().len(), 1);
                    } else {
                        assert!(
                            fake.posts().is_empty(),
                            "pending comments never hit the immediate endpoint"
                        );
                    }
                }
            }
        }
    }
}

#[gpui::test]
fn a_server_rerouted_reply_keeps_its_key_and_becomes_honestly_pending(
    cx: &mut gpui::TestAppContext,
) {
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
    *fake.pending.lock().unwrap() = Some(pending_review()); // Unknown to the app until the write reconciles.
    let (window, app) = queue_window(cx, fake.clone());
    install_item(&app, commenting_item(&fake), cx);
    let mut vcx = gpui::VisualTestContext::from_window(window, cx);
    let token = vcx.update(|window, cx| {
        app.update(cx, |app, cx| {
            let id = app
                .active_data()
                .unwrap()
                .discussion
                .as_ref()
                .unwrap()
                .threads[0]
                .id
                .clone();
            app.open_comment_composer(
                ComposerPlacement::ConversationThread(id),
                Some(11),
                "a.rs".into(),
                CommentSide::Right,
                1,
                0,
                window,
                cx,
            );
            app.composer()
                .unwrap()
                .input
                .update(cx, |input, cx| input.set_value("rerouted", window, cx));
            let token = app.composer().unwrap().id;
            app.submit_composer(window, cx);
            assert!(app.composer().is_none());
            assert_eq!(app.active_comment_projection().unwrap().pending_count(), 0);
            token
        })
    });
    vcx.run_until_parked();
    cx.update(|cx| {
        let app = app.read(cx);
        let projection = app.active_comment_projection().unwrap();
        assert_eq!(projection.pending_count(), 1);
        assert_eq!(app.sessions[&0].state.pending.as_ref().unwrap().id, 91);
        assert_eq!(
            projection.key(projection.pending_entries()[0], true),
            CommentKey::Post(token)
        );
        assert_eq!(
            indexed_keys(app.active_data().unwrap()),
            [CommentKey::Inline(11), CommentKey::Post(token)]
        );
        assert!(app.active_data().unwrap().posts.pending.is_empty());
    });
    assert!(
        fake.posts().is_empty(),
        "rerouting must not publish or submit the review"
    );
}

#[gpui::test]
fn partial_review_start_failure_keeps_only_confirmed_session_and_explicit_recovery(
    cx: &mut gpui::TestAppContext,
) {
    let fake = Arc::new(FakePr::default());
    fake.fail_pending_create
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let (window, app) = queue_window(cx, fake);
    install_item(&app, commenting_item(&FakePr::default()), cx);
    let mut vcx = gpui::VisualTestContext::from_window(window, cx);
    vcx.update(|window, cx| {
        app.update(cx, |app, cx| {
            app.open_composer(None, "a.rs".into(), CommentSide::Right, 1, 0, window, cx);
            app.composer_mut().unwrap().pending = true;
            app.composer().unwrap().input.update(cx, |input, cx| {
                input.set_value("preserve draft", window, cx)
            });
            app.submit_composer(window, cx);
            assert!(app.composer().is_none());
            assert!(app.sessions[&0].state.pending.is_none());
        })
    });
    vcx.run_until_parked();
    vcx.update(|window, cx| {
        app.update(cx, |app, cx| {
            let ui = &app.sessions[&0];
            assert!(ui.state.verified);
            assert_eq!(ui.state.pending.as_ref().unwrap().id, 91);
            assert!(ui.state.pending.as_ref().unwrap().comments.is_empty());
            assert!(app.active_data().unwrap().posts.pending.is_empty());
            assert_eq!(app.active_comment_projection().unwrap().pending_count(), 0);
            let recovery = &app.active_data().unwrap().posts.failed[0];
            assert_eq!(recovery.body, "preserve draft");
            assert_eq!(
                app.store
                    .review_session(&loop_loc())
                    .pending
                    .unwrap()
                    .comments
                    .len(),
                0
            );
            let token = recovery.token;
            app.open_composer(None, "a.rs".into(), CommentSide::Right, 1, 0, window, cx);
            app.composer()
                .unwrap()
                .input
                .update(cx, |input, cx| input.set_value("newer", window, cx));
            app.restore_comment_draft(0, token, window, cx);
            assert_eq!(
                app.composer().unwrap().input.read(cx).value(),
                "newer",
                "recovery cannot overwrite another editor"
            );
        })
    });
}

#[gpui::test]
fn unknown_create_outcomes_never_repost_or_guess_identity_and_block_review_submission(
    cx: &mut gpui::TestAppContext,
) {
    let fake = Arc::new(FakePr::default());
    fake.unknown_create
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let (window, app) = queue_window(cx, fake.clone());
    install_item(&app, commenting_item(&fake), cx);
    let mut vcx = gpui::VisualTestContext::from_window(window, cx);
    vcx.update(|window, cx| {
        app.update(cx, |app, cx| {
            app.open_composer(None, "a.rs".into(), CommentSide::Right, 1, 0, window, cx);
            app.composer()
                .unwrap()
                .input
                .update(cx, |input, cx| input.set_value("unknown", window, cx));
            app.submit_composer(window, cx);
            assert!(app.composer().is_none());
        })
    });
    vcx.run_until_parked();
    vcx.update(|window, cx| {
        app.update(cx, |app, cx| {
            let data = app.active_data().unwrap();
            assert!(data.posts.has_uncertain());
            let token = data.posts.pending[0].token;
            assert_eq!(data.posts.pending[0].body, "unknown");
            assert_eq!(
                app.active_comment_projection()
                    .unwrap()
                    .remote_id(CommentKey::Post(token)),
                None
            );
            assert!(
                data.discussion
                    .as_ref()
                    .unwrap()
                    .threads
                    .iter()
                    .any(|t| t.comments.iter().any(|c| c.body == "unknown")),
                "verification read can show the real comment but cannot fuzzy-bind it"
            );
            assert!(app.comment_write_blocked(0));
            app.open_review(window, cx);
            app.submit_review(window, cx);
            assert!(!app.review.as_ref().unwrap().in_flight);
            app.close_review(window, cx);
            app.open_composer(None, "a.rs".into(), CommentSide::Right, 1, 0, window, cx);
            app.composer().unwrap().input.update(cx, |input, cx| {
                input.set_value("do not post again", window, cx)
            });
            app.submit_composer(window, cx);
            assert!(app.composer().is_some());
            assert!(!app.composer().unwrap().in_flight);
        })
    });
    vcx.run_until_parked();
    assert_eq!(
        fake.posts().len(),
        1,
        "unknown outcome never triggers another mutation"
    );
}

#[gpui::test]
fn successful_creation_survives_failed_stale_and_full_reads_without_caching_the_overlay(
    cx: &mut gpui::TestAppContext,
) {
    let fake = FakePr::with_threads(vec![rc(
        11,
        "a.rs",
        Some("RIGHT"),
        Some(1),
        "original",
        "alice",
        "",
        None,
    )]);
    let (window, app) = queue_window(cx, fake.clone());
    install_item(&app, commenting_item(&fake), cx);
    let mut vcx = gpui::VisualTestContext::from_window(window, cx);
    fake.fail_discussion
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let (token, cached) = vcx.update(|window, cx| {
        app.update(cx, |app, cx| {
            let data = app.active_data().unwrap();
            let snapshot = review_target::discussion_snapshot(
                &loop_loc(),
                data.pr_meta.as_ref().unwrap(),
                data.discussion.as_ref().unwrap(),
                None,
            )
            .unwrap();
            app.store.save_snapshot(&snapshot);
            let cached = serde_json::to_value(&snapshot).unwrap();
            app.open_composer(None, "a.rs".into(), CommentSide::Right, 1, 0, window, cx);
            app.composer().unwrap().input.update(cx, |input, cx| {
                input.set_value("keep optimistic", window, cx)
            });
            let token = app.composer().unwrap().id;
            app.submit_composer(window, cx);
            (token, cached)
        })
    });
    vcx.run_until_parked();
    vcx.update(|_, cx| {
        app.update(cx, |app, cx| {
            let data = app.active_data().unwrap();
            assert_eq!(
                data.posts.pending[0].status,
                comments::posting::Status::Syncing
            );
            assert_eq!(
                indexed_keys(data),
                [CommentKey::Inline(11), CommentKey::Post(token)]
            );
            assert!(app.composer().is_none());
            assert!(
                app.items[0].refresh_error.is_some(),
                "read failure is not write failure"
            );
            assert_eq!(
                serde_json::to_value(
                    app.store
                        .snapshot(&loop_loc().repo_slug(), loop_loc().number)
                        .unwrap()
                )
                .unwrap(),
                cached
            );
            fake.fail_discussion
                .store(false, std::sync::atomic::Ordering::SeqCst);
            fake.stale_discussion
                .store(true, std::sync::atomic::Ordering::SeqCst);
            app.refetch_meta(0, loop_loc(), cx);
        })
    });
    vcx.run_until_parked();
    vcx.update(|_, cx| {
        app.update(cx, |app, cx| {
            let data = app.active_data().unwrap();
            assert_eq!(
                indexed_keys(data),
                [CommentKey::Inline(11), CommentKey::Post(token)]
            );
            assert_eq!(
                data.posts.pending.len(),
                1,
                "stale full refresh cannot remove the overlay"
            );
            assert_eq!(
                serde_json::to_value(
                    app.store
                        .snapshot(&loop_loc().repo_slug(), loop_loc().number)
                        .unwrap()
                )
                .unwrap(),
                cached
            );
            fake.stale_discussion
                .store(false, std::sync::atomic::Ordering::SeqCst);
            app.refetch_comments(0, loop_loc(), cx);
        })
    });
    vcx.run_until_parked();
    cx.update(|cx| {
        let app = app.read(cx);
        assert!(app.active_data().unwrap().posts.pending.is_empty());
        assert_eq!(
            indexed_keys(app.active_data().unwrap()),
            [CommentKey::Inline(11), CommentKey::Post(token)]
        );
        let stored = app
            .store
            .snapshot(&loop_loc().repo_slug(), loop_loc().number)
            .unwrap();
        let discussion: github::PrDiscussion =
            serde_json::from_value(stored.discussion.clone().unwrap()).unwrap();
        assert_eq!(
            discussion
                .threads
                .iter()
                .flat_map(|t| &t.comments)
                .filter(|c| c.body == "keep optimistic")
                .count(),
            1
        );
        assert!(
            !stored
                .discussion
                .unwrap()
                .to_string()
                .contains(&format!("post-{token}")),
            "only actual server data is cached"
        );
    });
    assert_eq!(
        fake.posts().len(),
        1,
        "no refresh may repeat a successful POST"
    );
}

#[gpui::test]
fn accepted_pending_creation_survives_stale_resume_until_exact_positive_acknowledgement(
    cx: &mut gpui::TestAppContext,
) {
    for reply in [false, true] {
        let fake = FakePr::with_threads(if reply {
            vec![rc(
                11,
                "a.rs",
                Some("RIGHT"),
                Some(1),
                "existing root",
                "alice",
                "2026-01-01T00:00:00Z",
                None,
            )]
        } else {
            vec![]
        });
        if reply {
            *fake.pending.lock().unwrap() = Some(pending_review());
        }
        let (window, app) = queue_window(cx, fake.clone());
        install_item(&app, commenting_item(&fake), cx);
        let mut vcx = gpui::VisualTestContext::from_window(window, cx);
        vcx.update(|window, cx| {
            app.update(cx, |app, cx| {
                if reply {
                    app.sessions.entry(0).or_default().state = ReviewSession {
                        pending: Some(pending_review()),
                        verified: true,
                    };
                }
                app.open_composer(
                    reply.then_some(11),
                    "a.rs".into(),
                    CommentSide::Right,
                    1,
                    0,
                    window,
                    cx,
                );
                app.composer_mut().unwrap().pending = true;
                app.composer().unwrap().input.update(cx, |input, cx| {
                    input.set_value("confirmed pending creation", window, cx)
                });
                app.submit_composer(window, cx);
                assert!(app.composer().is_none());
            })
        });
        vcx.run_until_parked();
        let confirmed = fake.pending.lock().unwrap().clone().unwrap();
        assert_eq!(confirmed.comments.len(), 1);
        fake.pending
            .lock()
            .unwrap()
            .as_mut()
            .unwrap()
            .comments
            .clear();
        cx.update(|cx| app.update(cx, |app, cx| app.resume_session(0, cx)));
        vcx.run_until_parked();
        cx.update(|cx| {
            let app = app.read(cx);
            assert_eq!(
                app.active_comment_projection().unwrap().pending_count(),
                1,
                "a stale session read must not undo a receipt"
            );
            assert_eq!(
                indexed_keys(app.active_data().unwrap()),
                if reply {
                    vec![CommentKey::Inline(11), CommentKey::Post(1)]
                } else {
                    vec![CommentKey::Post(1)]
                }
            );
            assert_eq!(
                app.store
                    .review_session(&loop_loc())
                    .pending
                    .unwrap()
                    .comments,
                confirmed.comments
            );
            assert!(app.composer().is_none());
        });
        *fake.pending.lock().unwrap() = Some(confirmed);
        cx.update(|cx| app.update(cx, |app, cx| app.resume_session(0, cx)));
        vcx.run_until_parked();
        // Once the same owner/node/body is positively observed, a later actual
        // deletion must be allowed through; protection is not a permanent overlay.
        fake.pending
            .lock()
            .unwrap()
            .as_mut()
            .unwrap()
            .comments
            .clear();
        cx.update(|cx| app.update(cx, |app, cx| app.resume_session(0, cx)));
        vcx.run_until_parked();
        cx.update(|cx| {
            assert_eq!(
                app.read(cx)
                    .active_comment_projection()
                    .unwrap()
                    .pending_count(),
                0
            )
        });
        assert_eq!(
            fake.pending_create_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            1
        );
    }
}

#[gpui::test]
fn invalid_creation_anchor_keeps_the_editor_and_never_registers_an_operation(
    cx: &mut gpui::TestAppContext,
) {
    let fake = Arc::new(FakePr::default());
    let (window, app) = queue_window(cx, fake.clone());
    install_item(&app, commenting_item(&fake), cx);
    let mut vcx = gpui::VisualTestContext::from_window(window, cx);
    vcx.update(|window, cx| {
        app.update(cx, |app, cx| {
            app.open_composer(None, "a.rs".into(), CommentSide::Right, 1, 0, window, cx);
            app.composer_mut().unwrap().start_line = Some(1); // Invalid equal endpoints.
            app.composer().unwrap().input.update(cx, |input, cx| {
                input.set_value("keep invalid draft", window, cx)
            });
            app.submit_composer(window, cx);
            let composer = app.composer().unwrap();
            assert!(!composer.in_flight);
            assert!(composer.error.is_some());
            assert!(app.active_data().unwrap().posts.pending.is_empty());
        })
    });
    vcx.run_until_parked();
    assert!(fake.posts().is_empty());
}
