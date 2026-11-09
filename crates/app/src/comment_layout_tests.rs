// Part 5: assert painted geometry, not just projection keys or row estimates.
use super::*;
use crate::comments::CommentKey;
use std::sync::atomic::Ordering::SeqCst;

const BODY: &str =
    "A **formatted** comment.\n\n- first item\n- second item\n\n```rust\nlet value = 42;\n```";

fn paint(vcx: &mut gpui::VisualTestContext) {
    // Deliberately do not drive the executor: the write must not run while we
    // paint and inspect its optimistic card.
    for _ in 0..3 {
        vcx.update(|window, cx| {
            window.refresh();
            let _ = window.draw(cx);
        });
    }
}

fn layout_item(fake: &FakePr, mode: ViewMode, conversation: bool) -> ReviewItem {
    let mut item = commenting_item(fake);
    item.pane = if conversation {
        ItemPane::Conversation
    } else {
        ItemPane::Changes
    };
    let ItemState::Ready(data) = &mut item.state else {
        unreachable!()
    };
    let mut patch =
        "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1,80 +1,80 @@\n-old\n+new\n"
            .to_owned();
    for n in 2..=80 {
        patch.push_str(&format!(" context {n}\n"));
    }
    data.diff = diff_core::parse_patch(&patch);
    data.mode = mode;
    data.discussion = Some(github::GitHubClient::pr_discussion(fake, &loop_loc()).unwrap());
    data.project_comments(None);
    data.rebuild_rows_anchored();
    data.rebuild_tree();
    item
}

fn pending_review() -> github::PendingReview {
    github::PendingReview {
        id: 91,
        node_id: "pending".into(),
        body: String::new(),
        comments: vec![],
    }
}

fn layout_fake() -> Arc<FakePr> {
    let fake = FakePr::with_threads(vec![
        rc(
            11,
            "a.rs",
            Some("RIGHT"),
            Some(1),
            "Original root",
            "alice",
            "2026-01-01T00:00:00Z",
            None,
        ),
        rc(
            22,
            "a.rs",
            Some("RIGHT"),
            Some(2),
            "Following card",
            "bob",
            "2026-01-02T00:00:00Z",
            None,
        ),
    ]);
    fake.review_ids
        .lock()
        .unwrap()
        .insert(11, "original-review".into());
    fake.conversation
        .lock()
        .unwrap()
        .push(github::DiscussionComment {
            id: 42,
            permissions: Default::default(),
            author: "alice".into(),
            body: String::new(),
            created_at: "2026-01-01T00:00:00Z".into(),
            reactions: vec![],
            review_state: Some("COMMENTED".into()),
            review_id: Some("original-review".into()),
        });
    fake
}

#[gpui::test]
fn edits_share_composer_content_keep_authorship_and_cannot_be_replaced(
    cx: &mut gpui::TestAppContext,
) {
    for mode in [ViewMode::Unified, ViewMode::Split] {
        for conversation in [false, true] {
            for reply in [false, true] {
                let fake = layout_fake();
                fake.comments.lock().unwrap().push(rc(
                    12,
                    "a.rs",
                    Some("RIGHT"),
                    Some(1),
                    "A reply",
                    "bob",
                    "2026-01-01T00:01:00Z",
                    Some(11),
                ));
                for id in [11, 12] {
                    fake.comment_permissions
                        .lock()
                        .unwrap()
                        .insert(id, permitted_comment(&format!("node-{id}"), true, true));
                }
                let (window, app) = queue_window(cx, fake.clone());
                install_item(&app, layout_item(&fake, mode, conversation), cx);
                let mut vcx = gpui::VisualTestContext::from_window(window, cx);
                let id = if reply { 12 } else { 11 };
                let target = CommentTarget::Published(github::CommentTarget::ReviewComment(
                    format!("node-{id}"),
                ));
                let token = vcx.update(|window, cx| {
                    app.update(cx, |app, cx| {
                        let placement = if conversation {
                            ComposerPlacement::ConversationComment(target.key())
                        } else {
                            ComposerPlacement::Diff
                        };
                        app.edit_comment(target, placement, window, cx);
                        let composer = app.composer().unwrap();
                        let input = composer.input.read(cx);
                        let end = input.text().len();
                        assert_eq!(
                            input.selected_range(),
                            end..end,
                            "editing starts ready to append"
                        );
                        composer
                            .input
                            .update(cx, |input, cx| input.set_value("unsaved edit", window, cx));
                        composer.id
                    })
                });
                paint(&mut vcx);
                assert!(vcx.debug_bounds("composer-card").is_some());
                assert!(vcx.debug_bounds("comment-editing").is_some());
                assert!(
                    vcx.debug_bounds("composer-shell").is_none(),
                    "no nested standalone panel"
                );
                assert!(vcx
                    .debug_bounds(if reply {
                        "comment-edit-review-comment-node-12"
                    } else {
                        "comment-edit-review-comment-node-11"
                    })
                    .is_none());
                let avatar = match (conversation, reply) {
                    (true, true) => "discussion-avatar-0-review-false-12",
                    (true, false) => "discussion-avatar-0-review-false-11",
                    (false, true) => "inline-avatar-12",
                    (false, false) => "inline-avatar-11",
                };
                assert!(
                    vcx.debug_bounds(avatar).is_some(),
                    "editing retains the author"
                );
                if !conversation && reply {
                    assert!(
                        vcx.debug_bounds("inline-reply-11").is_none(),
                        "editor replaces the footer"
                    );
                }
                // The other thread's Reply control must not cancel this edit.
                if !conversation {
                    if let Some(bounds) = vcx.debug_bounds("inline-reply-22") {
                        vcx.simulate_click(bounds.center(), gpui::Modifiers::default());
                    }
                }
                vcx.update(|window, cx| {
                    app.update(cx, |app, cx| {
                        app.open_composer(
                            Some(22),
                            "a.rs".into(),
                            CommentSide::Right,
                            2,
                            999,
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
                        app.open_composer_on_selection(window, cx);
                        assert_eq!(app.composer().unwrap().id, token);
                        assert_eq!(
                            app.composer().unwrap().input.read(cx).value(),
                            "unsaved edit"
                        );
                    })
                });
                vcx.simulate_keystrokes("escape");
                paint(&mut vcx);
                cx.update(|cx| {
                    let app = app.read(cx);
                    assert!(app.composer().is_none());
                    assert!(app.active_data().unwrap().editing_comments.is_empty());
                    assert!(app.active_data().unwrap().replying_comments.is_empty());
                });
                assert!(vcx.debug_bounds("composer-card").is_none());
            }
        }
    }
}

#[gpui::test]
fn diff_reply_is_attached_to_the_current_thread_after_row_rebuilds(cx: &mut gpui::TestAppContext) {
    for mode in [ViewMode::Unified, ViewMode::Split] {
        let fake = layout_fake();
        let (window, app) = queue_window(cx, fake.clone());
        install_item(&app, layout_item(&fake, mode, false), cx);
        let mut vcx = gpui::VisualTestContext::from_window(window, cx);
        vcx.update(|window, cx| {
            app.update(cx, |app, cx| {
                // A stale display index is deliberately not the thread's footer.
                app.open_composer(
                    Some(11),
                    "a.rs".into(),
                    CommentSide::Right,
                    1,
                    999,
                    window,
                    cx,
                );
            })
        });
        paint(&mut vcx);
        let old_anchor = cx.update(|cx| {
            app.read(cx)
                .active_data()
                .unwrap()
                .rows
                .iter()
                .position(|row| {
                    matches!(
                        row,
                        Row::CommentActions {
                            root_id: CommentKey::Inline(11),
                            ..
                        }
                    )
                })
                .unwrap()
        });
        vcx.update(|_, cx| {
            app.update(cx, |app, cx| {
                let data = app.active_data_mut().unwrap();
                data.discussion
                    .as_mut()
                    .unwrap()
                    .threads
                    .iter_mut()
                    .find(|t| t.id == "11")
                    .unwrap()
                    .comments[0]
                    .body = "More context\n".repeat(12);
                data.comment_text_layouts.clear();
                data.project_comments(None);
                data.rebuild_rows_anchored();
                cx.notify();
            })
        });
        paint(&mut vcx);
        let content = vcx
            .debug_bounds("composer-card")
            .expect("the attached composer is visible");
        let following = vcx
            .debug_bounds("inline-avatar-22")
            .expect("the next thread stays visible");
        assert!(
            content.bottom() < following.top(),
            "reply reserves space instead of covering the next card"
        );
        assert!(vcx.debug_bounds("composer-shell").is_none());
        vcx.update(|window, cx| {
            app.update(cx, |app, cx| {
                let data = app.active_data().unwrap();
                let anchor = data
                    .rows
                    .iter()
                    .position(|row| {
                        matches!(
                            row,
                            Row::CommentActions {
                                root_id: CommentKey::Inline(11),
                                ..
                            }
                        )
                    })
                    .unwrap();
                assert_ne!(anchor, old_anchor);
                let state = data.scroll.0.borrow();
                let expected_top = state.base_handle.bounds().top()
                    + state.base_handle.offset().y
                    + px((anchor + 1) as f32 * row_height());
                assert!((f32::from(content.top() - expected_top) - 12.).abs() < 2.);
                drop(state);
                assert!(matches!(data.rows[anchor + 1], Row::CommentEditor { .. }));
                app.close_composer(window, cx);
                assert!(app.active_data().unwrap().replying_comments.is_empty());
                assert!(!app
                    .active_data()
                    .unwrap()
                    .rows
                    .iter()
                    .any(|r| matches!(r, Row::CommentEditor { .. })));
            })
        });
    }
}

#[gpui::test]
fn metadata_acknowledgement_preserves_painted_cards_followers_and_viewports(
    cx: &mut gpui::TestAppContext,
) {
    for mode in [ViewMode::Unified, ViewMode::Split] {
        for conversation in [false, true] {
            for pending in [false, true] {
                for reply in [false, true] {
                    // Pending standalone roots intentionally live in Changes
                    // and the tray, not in the public Conversation timeline.
                    if conversation && pending && !reply {
                        continue;
                    }
                    for width in [850., 1400.] {
                        let fake = layout_fake();
                        if pending && reply {
                            *fake.pending.lock().unwrap() = Some(pending_review());
                        }
                        fake.stale_discussion.store(true, SeqCst);
                        let (window, app) = queue_window(cx, fake.clone());
                        install_item(&app, layout_item(&fake, mode, conversation), cx);
                        let mut vcx = gpui::VisualTestContext::from_window(window, cx);
                        vcx.simulate_resize(size(px(width), px(1200.)));
                        let token = vcx.update(|window, cx| {
                            app.update(cx, |app, cx| {
                                app.account = AccountStatus::SignedIn(github::Viewer {
                                    login: "a-very-long-signed-in-reviewer-login".into(),
                                    avatar_url: "https://example.invalid/avatar".into(),
                                });
                                app.sessions.entry(0).or_default().comments_expanded = true;
                                if pending && reply {
                                    app.sessions.entry(0).or_default().state = ReviewSession {
                                        pending: Some(pending_review()),
                                        verified: true,
                                    };
                                }
                                let placement = if conversation && reply {
                                    ComposerPlacement::ConversationThread("11".into())
                                } else {
                                    ComposerPlacement::Diff
                                };
                                app.open_comment_composer(
                                    placement,
                                    reply.then_some(11),
                                    "a.rs".into(),
                                    CommentSide::Right,
                                    1,
                                    0,
                                    window,
                                    cx,
                                );
                                app.composer_mut().unwrap().pending = pending;
                                app.composer()
                                    .unwrap()
                                    .input
                                    .update(cx, |input, cx| input.set_value(BODY, window, cx));
                                let token = app.composer().unwrap().id;
                                app.submit_composer(window, cx);
                                assert!(app.composer().is_none());
                                assert!(app.sessions[&0].busy);
                                token
                            })
                        });
                        paint(&mut vcx);
                        assert!(
                            fake.posts().is_empty(),
                            "paint must precede the backend response"
                        );
                        assert_eq!(token, 1);
                        let card_selector = if conversation {
                            "discussion-0-post-1"
                        } else {
                            "inline-text-0-post-1-0-card"
                        };
                        let follower_selector = if conversation {
                            if reply {
                                "thread-header-0-22"
                            } else {
                                "conversation-compose-0-0"
                            }
                        } else {
                            "inline-text-0-22-0-card"
                        };
                        let before_card = vcx.debug_bounds(card_selector).unwrap_or_else(|| panic!("optimistic card missing: split={}, conversation={conversation}, pending={pending}, reply={reply}, width={width}", mode == ViewMode::Split));
                        let before_follower = vcx
                            .debug_bounds(follower_selector)
                            .expect("following card painted");
                        if !conversation {
                            let time = vcx.debug_bounds("inline-comment-time-post-1").unwrap();
                            assert!(
                                time.size.height <= px(row_height()),
                                "sync text must fit a single uniform row"
                            );
                        }
                        let before_state = cx.update(|cx| {
                            let data = app.read(cx).active_data().unwrap();
                            let mut keys: Vec<_> =
                                data.comment_text_layouts.keys().copied().collect();
                            keys.sort();
                            let markdown = data
                                .conv_markdown
                                .borrow()
                                .get(&format!("discussion-0-post-{token}"))
                                .map(|state| state.entity_id());
                            (
                                data.rows.len(),
                                data.scroll.0.borrow().base_handle.offset(),
                                data.conv_scroll.offset(),
                                keys,
                                markdown,
                            )
                        });
                        vcx.run_until_parked();
                        paint(&mut vcx);
                        assert_eq!(
                            vcx.debug_bounds(card_selector).unwrap(),
                            before_card,
                            "receipt must not resize the card"
                        );
                        assert_eq!(
                            vcx.debug_bounds(follower_selector).unwrap(),
                            before_follower,
                            "receipt must not move the following card"
                        );
                        // Pending receipts bind directly; published writes remain
                        // visible while the intentionally stale GET is rejected.
                        cx.update(|cx| {
                            let app = app.read(cx);
                            assert!(!app.sessions[&0].busy);
                            assert!(app.composer().is_none());
                            assert!(app
                                .active_comment_projection()
                                .unwrap()
                                .remote_id(CommentKey::Post(token))
                                .is_some());
                        });
                        // Supply normal server chrome and owning-review metadata
                        // in the acknowledgement, not in the first frame.
                        if !pending {
                            let id = cx.update(|cx| {
                                app.read(cx)
                                    .active_comment_projection()
                                    .unwrap()
                                    .remote_id(CommentKey::Post(token))
                                    .unwrap()
                            });
                            fake.comment_permissions.lock().unwrap().insert(
                                id,
                                github::CommentPermissions {
                                    node_id: format!("fake-created-{id}"),
                                    can_edit: true,
                                    can_delete: true,
                                    can_react: true,
                                },
                            );
                            fake.reactions.lock().unwrap().insert(
                                format!("fake-created-{id}"),
                                vec![github::Reaction {
                                    content: "THUMBS_UP".into(),
                                    count: 1,
                                    viewer_has_reacted: false,
                                }],
                            );
                            fake.stale_discussion.store(false, SeqCst);
                            cx.update(|cx| {
                                app.update(cx, |app, cx| app.refetch_comments(0, loop_loc(), cx))
                            });
                            vcx.run_until_parked();
                            vcx.update(|_, cx| {
                                app.update(cx, |app, cx| {
                                    let data = app.active_data_mut().unwrap();
                                    let discussion = data.discussion.as_mut().unwrap();
                                    let thread = discussion
                                        .threads
                                        .iter_mut()
                                        .find(|t| t.comments.iter().any(|c| c.id == id))
                                        .unwrap();
                                    for c in &mut thread.comments {
                                        if c.id == id {
                                            c.review_id = Some("submitted-review".into());
                                        }
                                    }
                                    discussion.comments.push(github::DiscussionComment {
                                        id: 900,
                                        permissions: Default::default(),
                                        author: "tester".into(),
                                        body: String::new(),
                                        created_at: "2026-10-22T00:00:00Z".into(),
                                        reactions: vec![],
                                        review_state: Some("COMMENTED".into()),
                                        review_id: Some("submitted-review".into()),
                                    });
                                    data.project_comments(None);
                                    data.rebuild_rows_anchored();
                                    cx.notify();
                                })
                            });
                        }
                        paint(&mut vcx);
                        let case = format!("split={}, conversation={conversation}, pending={pending}, reply={reply}, width={width}", mode == ViewMode::Split);
                        assert_eq!(
                            vcx.debug_bounds(card_selector).unwrap(),
                            before_card,
                            "card: {case}"
                        );
                        assert_eq!(
                            vcx.debug_bounds(follower_selector).unwrap(),
                            before_follower,
                            "follower: {case}"
                        );
                        if !conversation {
                            assert!(
                                vcx.debug_bounds("inline-comment-time-post-1")
                                    .unwrap()
                                    .size
                                    .height
                                    <= px(row_height()),
                                "timestamp must fit a single uniform row: {case}"
                            );
                        }
                        cx.update(|cx| {
                            let app = app.read(cx);
                            let data = app.active_data().unwrap();
                            assert_eq!(data.rows.len(), before_state.0, "rows: {case}");
                            assert_eq!(
                                data.scroll.0.borrow().base_handle.offset(),
                                before_state.1,
                                "diff viewport: {case}"
                            );
                            assert_eq!(
                                data.conv_scroll.offset(),
                                before_state.2,
                                "conversation viewport: {case}"
                            );
                            let mut keys: Vec<_> =
                                data.comment_text_layouts.keys().copied().collect();
                            keys.sort();
                            assert_eq!(keys, before_state.3, "Markdown measurements: {case}");
                            assert_eq!(
                                data.conv_markdown
                                    .borrow()
                                    .get(&format!("discussion-0-post-{token}"))
                                    .map(|state| state.entity_id()),
                                before_state.4,
                                "Markdown/media owner: {case}"
                            );
                            if !pending {
                                assert!(data.posts.pending.is_empty());
                            }
                        });
                        assert_eq!(
                            fake.posts().len(),
                            usize::from(!pending),
                            "exactly one published write, no retry: {case}"
                        );
                        assert_eq!(
                            fake.pending_create_calls.load(SeqCst),
                            usize::from(pending),
                            "exactly one pending write, no retry: {case}"
                        );
                    }
                }
            }
        }
    }
}

#[gpui::test]
fn pending_tray_acknowledgement_is_a_bounded_single_line_card(cx: &mut gpui::TestAppContext) {
    for reply in [false, true] {
        let fake = layout_fake();
        if reply {
            *fake.pending.lock().unwrap() = Some(pending_review());
        }
        let (window, app) = queue_window(cx, fake.clone());
        install_item(&app, layout_item(&fake, ViewMode::Split, false), cx);
        let mut vcx = gpui::VisualTestContext::from_window(window, cx);
        vcx.simulate_resize(size(px(850.), px(1000.)));
        let token = vcx.update(|window, cx| {
            app.update(cx, |app, cx| {
                app.sessions.entry(0).or_default().comments_expanded = true;
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
                app.composer()
                    .unwrap()
                    .input
                    .update(cx, |input, cx| input.set_value(&BODY.repeat(4), window, cx));
                let token = app.composer().unwrap().id;
                app.submit_composer(window, cx);
                assert!(app.composer().is_none());
                token
            })
        });
        paint(&mut vcx);
        let before = vcx.debug_bounds("pending-comments").unwrap();
        let card = vcx.debug_bounds("pending-comment-0-post-1").unwrap();
        assert_eq!(card.size.height, px(32.));
        let status = vcx.debug_bounds("pending-status").unwrap();
        assert!(status.size.height <= px(20.), "sync metadata must not wrap");
        vcx.run_until_parked();
        paint(&mut vcx);
        assert_eq!(vcx.debug_bounds("pending-comment-0-post-1").unwrap(), card);
        assert_eq!(vcx.debug_bounds("pending-status").unwrap(), status);
        assert_eq!(
            vcx.debug_bounds("pending-comments").unwrap(),
            before,
            "pending={token}, reply={reply}"
        );
    }
}

#[gpui::test]
fn metadata_acknowledgement_keeps_partially_visible_markdown_and_loaded_media_anchored(
    cx: &mut gpui::TestAppContext,
) {
    let requests = super::markdown_hosts::image_client(cx);
    let image =
        "<img src='https://github.com/user-attachments/assets/screen' width='120' height='80'>";
    for mode in [ViewMode::Unified, ViewMode::Split] {
        for conversation in [false, true] {
            for reply in [false, true] {
                let fake = layout_fake();
                fake.comments.lock().unwrap()[1].body = image.into();
                let (window, app) = queue_window(cx, fake.clone());
                install_item(&app, layout_item(&fake, mode, conversation), cx);
                let mut vcx = gpui::VisualTestContext::from_window(window, cx);
                vcx.simulate_resize(size(px(1100.), px(2400.)));
                // Load this image before the write: image arrival is a legitimate
                // resize, whereas re-binding metadata with an already loaded
                // image must preserve its owner and measured height.
                super::markdown_hosts::draw_settled(&mut vcx);
                let body = format!("{}\n\n{image}", format!("{BODY}\n\n").repeat(3));
                fake.stale_discussion.store(true, SeqCst);
                vcx.update(|window, cx| {
                    app.update(cx, |app, cx| {
                        let placement = if conversation && reply {
                            ComposerPlacement::ConversationThread("11".into())
                        } else {
                            ComposerPlacement::Diff
                        };
                        app.open_comment_composer(
                            placement,
                            reply.then_some(11),
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
                            .update(cx, |input, cx| input.set_value(&body, window, cx));
                        app.submit_composer(window, cx);
                        assert!(app.composer().is_none());
                    })
                });
                paint(&mut vcx);
                let selector = if conversation {
                    "discussion-0-post-1"
                } else {
                    "inline-text-0-post-1-0-card"
                };
                let whole = vcx.debug_bounds(selector).unwrap();
                let viewport = cx.update(|cx| {
                    let data = app.read(cx).active_data().unwrap();
                    if conversation {
                        data.conv_scroll.bounds()
                    } else {
                        data.scroll.0.borrow().base_handle.bounds()
                    }
                });
                vcx.simulate_resize(size(px(1100.), px(500.)));
                cx.update(|cx| app.update(cx, |app, cx| {
                    let data = app.active_data_mut().unwrap();
                    if conversation {
                        data.conv_scroll.set_offset(point(px(0.), viewport.top() - whole.top() - px(72.25)));
                    } else {
                        let first = data.rows.iter().position(|row| matches!(row, Row::CommentBody { text, slot: 0, .. } if text.comment_id == CommentKey::Post(1))).unwrap();
                        let mut scroll = data.scroll.0.borrow_mut(); scroll.deferred_scroll_to_item = None;
                        scroll.base_handle.set_offset(point(px(0.), -px((first + 1) as f32 * row_height() + 3.25)));
                    }
                    cx.notify();
                }));
                paint(&mut vcx);
                let before = vcx.debug_bounds(selector).unwrap();
                let state = cx.update(|cx| {
                    let data = app.read(cx).active_data().unwrap();
                    let viewport = if conversation {
                        data.conv_scroll.bounds()
                    } else {
                        data.scroll.0.borrow().base_handle.bounds()
                    };
                    assert!(
                        before.top() < viewport.top() && before.bottom() > viewport.top(),
                        "a partially visible body is required"
                    );
                    (
                        data.rows.len(),
                        data.scroll.0.borrow().base_handle.offset(),
                        data.conv_scroll.offset(),
                        data.conv_markdown
                            .borrow()
                            .get("discussion-0-post-1")
                            .map(|s| s.entity_id()),
                    )
                });
                assert!(fake.posts().is_empty());
                let image_requests = requests
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|url| url.ends_with("/screen"))
                    .count();
                vcx.run_until_parked();
                paint(&mut vcx);
                fake.comment_permissions.lock().unwrap().insert(
                    100,
                    github::CommentPermissions {
                        node_id: "fake-created-100".into(),
                        can_edit: true,
                        can_delete: true,
                        can_react: true,
                    },
                );
                fake.reactions.lock().unwrap().insert(
                    "fake-created-100".into(),
                    vec![github::Reaction {
                        content: "THUMBS_UP".into(),
                        count: 2,
                        viewer_has_reacted: false,
                    }],
                );
                fake.stale_discussion.store(false, SeqCst);
                cx.update(|cx| app.update(cx, |app, cx| app.refetch_comments(0, loop_loc(), cx)));
                vcx.run_until_parked();
                paint(&mut vcx);
                assert_eq!(
                    vcx.debug_bounds(selector).unwrap(),
                    before,
                    "split={}, conversation={conversation}, reply={reply}",
                    mode == ViewMode::Split
                );
                cx.update(|cx| {
                    let app = app.read(cx);
                    let data = app.active_data().unwrap();
                    assert_eq!(data.rows.len(), state.0);
                    assert_eq!(data.scroll.0.borrow().base_handle.offset(), state.1);
                    assert_eq!(data.conv_scroll.offset(), state.2);
                    assert_eq!(
                        data.conv_markdown
                            .borrow()
                            .get("discussion-0-post-1")
                            .map(|s| s.entity_id()),
                        state.3
                    );
                    assert!(data.posts.pending.is_empty());
                });
                assert_eq!(fake.posts().len(), 1);
                assert_eq!(
                    requests
                        .lock()
                        .unwrap()
                        .iter()
                        .filter(|url| url.ends_with("/screen"))
                        .count(),
                    image_requests,
                    "binding an identity must not reload media"
                );
            }
        }
    }
    assert!(
        requests
            .lock()
            .unwrap()
            .iter()
            .any(|url| url.ends_with("/screen")),
        "real media loader exercised through the fake HTTP client"
    );
}

#[gpui::test]
fn deleting_resolved_roots_preserves_disclosure_historical_anchors_and_surviving_counts(
    cx: &mut gpui::TestAppContext,
) {
    for mode in [ViewMode::Unified, ViewMode::Split] {
        for pending_reply in [false, true] {
            for expanded in [false, true] {
                for outdated in [false, true] {
                    let fake = layout_fake();
                    if !pending_reply {
                        fake.comments.lock().unwrap().push(rc(
                            12,
                            "a.rs",
                            Some("RIGHT"),
                            Some(1),
                            "survivor",
                            "alice",
                            "2026-01-01T00:01:00Z",
                            Some(11),
                        ));
                    }
                    fake.comment_permissions
                        .lock()
                        .unwrap()
                        .insert(11, permitted_comment("node-11", true, true));
                    fake.resolutions.lock().unwrap().insert("11".into(), true);
                    let mut metadata =
                        github::GitHubClient::pr_discussion(fake.as_ref(), &loop_loc())
                            .unwrap()
                            .threads
                            .into_iter()
                            .find(|t| t.id == "11")
                            .unwrap();
                    if outdated {
                        metadata.line = None;
                        metadata.start_line = None;
                        metadata.is_outdated = true;
                        metadata.original_line = Some(7);
                        metadata.original_start_line = Some(5);
                    }
                    fake.thread_metadata
                        .lock()
                        .unwrap()
                        .insert("11".into(), metadata);
                    if pending_reply {
                        let mut p = pending_review();
                        p.comments.push(github::PendingComment {
                            id: 12,
                            node_id: "pending-12".into(),
                            path: "a.rs".into(),
                            side: CommentSide::Right,
                            line: (!outdated).then_some(1),
                            start_line: None,
                            body: "survivor".into(),
                            in_reply_to_id: Some(11),
                        });
                        *fake.pending.lock().unwrap() = Some(p);
                    }
                    let (window, app) = queue_window(cx, fake.clone());
                    install_item(&app, layout_item(&fake, mode, false), cx);
                    let mut vcx = gpui::VisualTestContext::from_window(window, cx);
                    vcx.update(|window, cx| {
                        app.update(cx, |app, cx| {
                            app.sessions.entry(0).or_default().state = ReviewSession {
                                pending: fake.pending.lock().unwrap().clone(),
                                verified: true,
                            };
                            if expanded {
                                app.active_data_mut()
                                    .unwrap()
                                    .expanded_threads
                                    .insert("11".into());
                            }
                            app.sync_pending_comments(0);
                            app.delete_comment(
                                CommentTarget::Published(github::CommentTarget::ReviewComment(
                                    "node-11".into(),
                                )),
                                window,
                                cx,
                            );
                            assert!(app.sessions[&0].busy);
                            assert!(fake.posts().is_empty());
                        })
                    });
                    let check = |app: &ReviewApp| {
                        let data = app.active_data().unwrap();
                        assert_eq!(data.expanded_threads.contains("11"), expanded);
                        let survivor = if pending_reply {
                            CommentKey::Pending(12)
                        } else {
                            CommentKey::Inline(12)
                        };
                        let header = data.rows.iter().find_map(|row| match row {
                            Row::CommentHeader {
                                comment_id,
                                collapsed,
                                ..
                            } if *comment_id == survivor => Some(*collapsed),
                            _ => None,
                        });
                        if outdated {
                            assert!(header.is_none());
                        } else {
                            assert_eq!(
                                header,
                                Some(!expanded),
                                "split={}, pending_reply={pending_reply}, expanded={expanded}",
                                mode == ViewMode::Split
                            );
                        }
                        assert_eq!(
                            data.comments.as_ref().unwrap().counts["a.rs"],
                            if outdated { (1, 1) } else { (2, 0) }
                        );
                        let projection = app.active_comment_projection().unwrap();
                        let thread = projection
                            .conversation()
                            .into_iter()
                            .find_map(|entry| match entry {
                                comments::Entry::Thread(t) if t.key.0 == "11" => Some(t),
                                comments::Entry::Review(_, ts) => {
                                    ts.into_iter().find(|t| t.key.0 == "11")
                                }
                                _ => None,
                            })
                            .unwrap();
                        assert_eq!(thread.comments.len(), 1);
                        assert_eq!(projection.reply_target(thread.confirmed), Some(12));
                        assert!(thread.confirmed.is_resolved);
                        assert_eq!(thread.confirmed.is_outdated, outdated);
                        if outdated {
                            assert_eq!(thread.confirmed.original_line, Some(7));
                            assert_eq!(thread.confirmed.original_start_line, Some(5));
                        }
                    };
                    cx.update(|cx| check(app.read(cx)));
                    vcx.run_until_parked();
                    cx.update(|cx| check(app.read(cx)));
                    assert_eq!(fake.mutation_calls.load(SeqCst), 1);
                    if pending_reply && expanded && mode == ViewMode::Unified {
                        cx.update(|cx| {
                            app.update(cx, |app, cx| {
                                app.flip_pane(ItemPane::Conversation, cx);
                            })
                        });
                        paint(&mut vcx);
                        let reply = vcx
                            .debug_bounds("conversation-compose-0-12")
                            .expect("reply on the retained thread shell");
                        vcx.simulate_click(reply.center(), gpui::Modifiers::default());
                        vcx.update(|window, cx| {
                            app.update(cx, |app, cx| {
                                assert_eq!(
                                    app.composer().unwrap().reply_to,
                                    Some(12),
                                    "never target the deleted root"
                                );
                                app.composer().unwrap().input.update(cx, |input, cx| {
                                    input.set_value("follow-up survivor", window, cx)
                                });
                                app.submit_composer(window, cx);
                                assert!(app.composer().is_none());
                            })
                        });
                        vcx.run_until_parked();
                        cx.update(|cx| {
                            let app = app.read(cx);
                            assert!(
                                app.sessions[&0].error.is_none(),
                                "{:?}",
                                app.sessions[&0].error
                            );
                            let pending = app.sessions[&0].state.pending.as_ref().unwrap();
                            assert_eq!(pending.comments.len(), 2);
                            assert_eq!(
                                pending
                                    .comments
                                    .iter()
                                    .find(|c| c.body == "follow-up survivor")
                                    .unwrap()
                                    .in_reply_to_id,
                                Some(12)
                            );
                            assert_eq!(app.active_comment_projection().unwrap().pending_count(), 2);
                        });
                        assert_eq!(fake.pending_create_calls.load(SeqCst), 1);
                    }
                }
            }
        }
    }
}

#[gpui::test]
fn inline_completion_after_switching_prs_cannot_replace_the_new_pr_editor_or_focus(
    cx: &mut gpui::TestAppContext,
) {
    for pending in [false, true] {
        for operation in 0..3 {
            // create / edit / delete
            for rejected in [false, true] {
                let fake = layout_fake();
                fake.comment_permissions
                    .lock()
                    .unwrap()
                    .insert(11, permitted_comment("node-11", true, true));
                if pending {
                    let mut review = pending_review();
                    review.comments.push(github::PendingComment {
                        id: 11,
                        node_id: "node-11".into(),
                        path: "a.rs".into(),
                        side: CommentSide::Right,
                        line: Some(1),
                        start_line: None,
                        body: "original pending".into(),
                        in_reply_to_id: None,
                    });
                    *fake.pending.lock().unwrap() = Some(review);
                }
                let (window, app) = queue_window(cx, fake.clone());
                install_item(&app, layout_item(&fake, ViewMode::Unified, false), cx);
                let mut vcx = gpui::VisualTestContext::from_window(window, cx);
                fake.fail.store(rejected, SeqCst);
                fake.fail_pending_create.store(rejected, SeqCst);
                let (token, new_id, untouched) = vcx.update(|window, cx| {
                    app.update(cx, |app, cx| {
                        app.sessions.entry(0).or_default().state = ReviewSession {
                            pending: fake.pending.lock().unwrap().clone(),
                            verified: true,
                        };
                        app.sync_pending_comments(0);
                        let target = if pending {
                            CommentTarget::Pending(11)
                        } else {
                            CommentTarget::Published(github::CommentTarget::ReviewComment(
                                "node-11".into(),
                            ))
                        };
                        match operation {
                            0 => {
                                app.open_composer(
                                    None,
                                    "a.rs".into(),
                                    CommentSide::Right,
                                    1,
                                    0,
                                    window,
                                    cx,
                                );
                                app.composer_mut().unwrap().pending = pending;
                            }
                            1 => app.edit_comment(target, ComposerPlacement::Diff, window, cx),
                            _ => app.delete_comment(target, window, cx),
                        }
                        let token = if operation != 2 {
                            let composer = app.composer().unwrap();
                            let token = composer.id;
                            composer.input.update(cx, |input, cx| {
                                input.set_value("submitted on first PR", window, cx)
                            });
                            app.submit_composer(window, cx);
                            assert!(app.composer().is_none());
                            token
                        } else {
                            app.active_data().unwrap().posts.changes.active[0].token
                        };
                        assert!(app.sessions[&0].busy);
                        assert!(fake.posts().is_empty());
                        let other = layout_fake();
                        let mut item = layout_item(&other, ViewMode::Split, false);
                        item.id = 1;
                        let mut loc = loop_loc();
                        loc.number += 1;
                        item.source = Source::Pr(loc);
                        let untouched = match &item.state {
                            ItemState::Ready(data) => {
                                serde_json::to_value(&data.discussion).unwrap()
                            }
                            _ => unreachable!(),
                        };
                        app.items.push(item);
                        app.next_id = 2;
                        app.activate(1, window, cx);
                        app.open_composer(
                            None,
                            "a.rs".into(),
                            CommentSide::Right,
                            2,
                            0,
                            window,
                            cx,
                        );
                        app.composer()
                            .unwrap()
                            .input
                            .update(cx, |input, cx| input.set_value("new PR draft", window, cx));
                        let new_id = app.composer().unwrap().id;
                        assert!(app
                            .composer()
                            .unwrap()
                            .input
                            .focus_handle(cx)
                            .is_focused(window));
                        (token, new_id, untouched)
                    })
                });
                vcx.run_until_parked();
                vcx.update(|window, cx| {
                    app.update(cx, |app, cx| {
                        assert_eq!(app.active_item().unwrap().id, 1);
                        assert_eq!(app.composer().unwrap().id, new_id);
                        assert_eq!(
                            app.composer().unwrap().input.read(cx).value(),
                            "new PR draft"
                        );
                        assert!(app
                            .composer()
                            .unwrap()
                            .input
                            .focus_handle(cx)
                            .is_focused(window));
                        assert_eq!(
                            serde_json::to_value(&app.active_data().unwrap().discussion).unwrap(),
                            untouched
                        );
                        assert!(!app.sessions[&0].busy);
                        if rejected {
                            if operation == 0 {
                                app.restore_comment_draft(0, token, window, cx);
                            } else {
                                app.restore_comment_change(0, token, window, cx);
                            }
                            assert_eq!(app.composer().unwrap().id, new_id);
                            assert!(app
                                .composer()
                                .unwrap()
                                .input
                                .focus_handle(cx)
                                .is_focused(window));
                        }
                        let ItemState::Ready(first) = &app.items[0].state else {
                            unreachable!()
                        };
                        assert!(!first.posts.has_uncertain());
                        if !rejected {
                            assert!(first.posts.pending.is_empty());
                            assert!(first.posts.changes.active.is_empty());
                        }
                    })
                });
                let writes = if pending {
                    fake.pending_create_calls.load(SeqCst) + fake.mutation_calls.load(SeqCst)
                } else {
                    fake.posts().len()
                };
                assert_eq!(
                    writes, 1,
                    "one write: pending={pending}, operation={operation}, rejected={rejected}"
                );
            }
        }
    }
}
