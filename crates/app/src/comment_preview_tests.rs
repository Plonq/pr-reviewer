// Included in markdown_hosts to share its real-window Markdown fixtures.

fn click_preview_control(vcx: &mut gpui::VisualTestContext, selector: &'static str) {
    draw_settled(vcx);
    let bounds = vcx
        .debug_bounds(selector)
        .unwrap_or_else(|| panic!("missing {selector}"));
    vcx.simulate_click(bounds.center(), Default::default());
    draw_settled(vcx);
}

#[gpui::test]
fn comment_previews_render_without_posting_and_preserve_the_editor(cx: &mut gpui::TestAppContext) {
    let requests = image_client(cx);
    for surface in ["diff", "conversation", "reply"] {
        let fake = FakePr::with_threads(vec![rc(
            11,
            "a.rs",
            Some("RIGHT"),
            Some(1),
            "Original",
            "alice",
            "2026-01-01T00:00:00Z",
            None,
        )]);
        let (window, app) = queue_window(cx, fake.clone());
        install_item(&app, commenting_item(&fake), cx);
        let mut vcx = gpui::VisualTestContext::from_window(window, cx);
        vcx.simulate_resize(size(px(1500.), px(1200.)));
        let body = format!(
            "**Bold 🦀** and `code`, :rocket: @alice #12.\n\n![screen](https://github.com/user-attachments/assets/preview-{surface})\n\n{}",
            "- A list item with **formatting**\n".repeat(80),
        );
        let (input, uploads) = vcx.update(|window, cx| {
            app.update(cx, |app, cx| {
                let placement = match surface {
                    "diff" => {
                        app.items[0].pane = ItemPane::Changes;
                        ComposerPlacement::Diff
                    }
                    "reply" => ComposerPlacement::ConversationThread(
                        app.active_data()
                            .unwrap()
                            .discussion
                            .as_ref()
                            .unwrap()
                            .threads[0]
                            .id
                            .clone(),
                    ),
                    _ => ComposerPlacement::ConversationMain,
                };
                app.open_comment_composer(
                    placement,
                    (surface == "reply").then_some(11),
                    "a.rs".into(),
                    CommentSide::Right,
                    1,
                    1,
                    window,
                    cx,
                );
                let composer = app.composer().unwrap();
                composer.input.update(cx, |input, cx| {
                    input.set_value(&body, window, cx);
                    input.set_selected_range(2..6, cx);
                });
                (composer.input.clone(), composer.uploads.clone())
            })
        });
        click_preview_control(&mut vcx, "comment-preview");
        let preview = vcx.debug_bounds("comment-preview-body").unwrap();
        assert!(
            preview.size.height <= px(240.),
            "long previews must stay bounded"
        );
        assert!(vcx.debug_bounds("composer-preview-1-text").is_some());
        assert!(vcx.debug_bounds("comment-editor-field").is_none());
        vcx.simulate_input("must not edit a hidden editor");
        vcx.update(|_, cx| {
            let composer = app.read(cx).composer().unwrap();
            assert!(composer.preview);
            assert_eq!(composer.input, input);
            assert_eq!(composer.uploads, uploads);
            assert_eq!(input.read(cx).value().as_str(), body);
            assert_eq!(input.read(cx).selected_range(), 2..6);
        });
        assert!(fake.posts().is_empty(), "preview is not a submission");
        click_preview_control(&mut vcx, "comment-write");
        vcx.update(|window, cx| {
            assert!(!app.read(cx).composer().unwrap().preview);
            assert_eq!(input.read(cx).selected_range(), 2..6);
            assert!(input.focus_handle(cx).is_focused(window));
        });
        // The next preview must render the updated draft, not cached old Markdown.
        vcx.update(|window, cx| {
            input.update(cx, |input, cx| {
                input.set_value(
                    "![new](https://github.com/user-attachments/assets/updated-preview)",
                    window,
                    cx,
                );
            })
        });
        click_preview_control(&mut vcx, "comment-preview");
        vcx.simulate_keystrokes("escape");
        vcx.update(|_, cx| assert!(app.read(cx).composer().is_none()));
        assert!(fake.posts().is_empty());
    }
    let requests = requests.lock().unwrap();
    for surface in ["diff", "conversation", "reply"] {
        assert!(requests
            .iter()
            .any(|url| url.ends_with(&format!("preview-{surface}"))));
    }
    assert!(requests.iter().any(|url| url.ends_with("updated-preview")));
}

#[gpui::test]
fn preview_scrolling_does_not_scroll_the_diff_even_at_preview_boundaries(
    cx: &mut gpui::TestAppContext,
) {
    for mode in [ViewMode::Unified, ViewMode::Split] {
        let (window, app) = queue_window(cx, Arc::new(FakePr::default()));
        let mut item = loop_pr_item(vec![]);
        let ItemState::Ready(data) = &mut item.state else {
            unreachable!()
        };
        data.mode = mode;
        data.diff = diff_core::parse_patch(&format!(
            "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1,201 +1,201 @@\n-old\n+new\n{}",
            " context\n".repeat(200),
        ));
        data.set_rows(data.row_build());
        install_item(&app, item, cx);
        let mut vcx = gpui::VisualTestContext::from_window(window, cx);
        vcx.simulate_resize(size(px(1500.), px(900.)));
        vcx.update(|window, cx| {
            app.update(cx, |app, cx| {
                app.open_comment_composer(
                    ComposerPlacement::Diff,
                    None,
                    "a.rs".into(),
                    CommentSide::Right,
                    1,
                    1,
                    window,
                    cx,
                );
                app.composer().unwrap().input.update(cx, |input, cx| {
                    input.set_value("Preview paragraph.\n\n".repeat(80), window, cx);
                });
            })
        });
        click_preview_control(&mut vcx, "comment-preview");
        let before_diff = vcx.update(|_, cx| {
            app.read(cx)
                .active_data()
                .unwrap()
                .scroll
                .0
                .borrow()
                .base_handle
                .offset()
        });
        let before_content = vcx.debug_bounds("composer-preview-1-text").unwrap();
        let preview = vcx.debug_bounds("comment-preview-body").unwrap();
        vcx.simulate_event(gpui::ScrollEvent {
            position: preview.center(),
            delta: gpui::ScrollDelta::Pixels(point(px(0.), px(-60.))),
            ..Default::default()
        });
        draw_settled(&mut vcx);
        let after_content = vcx.debug_bounds("composer-preview-1-text").unwrap();
        assert!(
            after_content.top() < before_content.top(),
            "the preview itself must scroll"
        );
        vcx.update(|_, cx| {
            assert_eq!(
                app.read(cx)
                    .active_data()
                    .unwrap()
                    .scroll
                    .0
                    .borrow()
                    .base_handle
                    .offset(),
                before_diff,
                "scrolling inside the preview must not also scroll the diff",
            )
        });
        // Repeated gestures at the top and bottom must not chain to the diff.
        for delta in [-100_000., -60., 100_000., 60.] {
            vcx.simulate_event(gpui::ScrollEvent {
                position: preview.center(),
                delta: gpui::ScrollDelta::Pixels(point(px(0.), px(delta))),
                ..Default::default()
            });
            draw_settled(&mut vcx);
            vcx.update(|_, cx| {
                assert_eq!(
                    app.read(cx)
                        .active_data()
                        .unwrap()
                        .scroll
                        .0
                        .borrow()
                        .base_handle
                        .offset(),
                    before_diff,
                )
            });
        }
        // Empty previews should contain wheel gestures too.
        vcx.update(|window, cx| {
            let input = app.read(cx).composer().unwrap().input.clone();
            input.update(cx, |input, cx| input.set_value("", window, cx));
        });
        draw_settled(&mut vcx);
        let preview = vcx.debug_bounds("comment-preview-body").unwrap();
        vcx.simulate_event(gpui::ScrollEvent {
            position: preview.center(),
            delta: gpui::ScrollDelta::Pixels(point(px(0.), px(-60.))),
            ..Default::default()
        });
        draw_settled(&mut vcx);
        vcx.update(|_, cx| {
            assert_eq!(
                app.read(cx)
                    .active_data()
                    .unwrap()
                    .scroll
                    .0
                    .borrow()
                    .base_handle
                    .offset(),
                before_diff,
            )
        });
    }
}

#[gpui::test]
fn review_summary_previews_handle_empty_drafts_and_submit_only_on_request(
    cx: &mut gpui::TestAppContext,
) {
    let fake = FakePr::with_threads(vec![]);
    let (window, app) = queue_window(cx, fake.clone());
    install_item(&app, loop_pr_item(vec![]), cx);
    let mut vcx = gpui::VisualTestContext::from_window(window, cx);
    vcx.simulate_resize(size(px(1200.), px(1000.)));
    vcx.update(|window, cx| app.update(cx, |app, cx| app.open_review(window, cx)));
    click_preview_control(&mut vcx, "comment-preview");
    assert!(vcx.debug_bounds("comment-preview-body").is_some());
    assert!(
        vcx.debug_bounds("review-preview-text").is_none(),
        "empty drafts have a placeholder"
    );
    assert!(fake.posts().is_empty());
    click_preview_control(&mut vcx, "comment-write");
    vcx.simulate_input("**Looks good** 🦀");
    click_preview_control(&mut vcx, "comment-preview");
    assert!(vcx.debug_bounds("review-preview-text").is_some());
    vcx.update(|_, cx| {
        let review = app.read(cx).review.as_ref().unwrap();
        assert!(review.preview);
        assert_eq!(review.input.read(cx).value().as_str(), "**Looks good** 🦀");
        assert!(!review.in_flight);
    });
    assert!(fake.posts().is_empty());
    vcx.simulate_keystrokes("cmd-enter");
    vcx.run_until_parked();
    assert_eq!(
        fake.posts(),
        vec![Post::Review {
            verdict: github::ReviewVerdict::Approve,
            body: "**Looks good** 🦀".into(),
        }]
    );
}
