// Included alongside the stock Markdown/image layout regressions.
fn viewer_conversation(
    cx: &mut gpui::TestAppContext,
    body: &str,
) -> (gpui::AnyWindowHandle, gpui::Entity<ReviewApp>) {
    let (window, app) = queue_window(cx, Arc::new(FakePr::default()));
    let mut item = loop_pr_item(Vec::new());
    item.pane = ItemPane::Conversation;
    let ItemState::Ready(data) = &mut item.state else {
        unreachable!()
    };
    data.pr_meta.as_mut().unwrap().body = body.into();
    data.discussion = Some(github::PrDiscussion::default());
    install_item(&app, item, cx);
    (window, app)
}

fn click_selector(vcx: &mut gpui::VisualTestContext, selector: &'static str) {
    let bounds = vcx
        .debug_bounds(selector)
        .unwrap_or_else(|| panic!("missing {selector}"));
    vcx.simulate_click(bounds.center(), gpui::Modifiers::default());
    draw_settled(vcx);
}

#[gpui::test]
fn image_viewer_is_below_header_modal_and_restores_review_state(cx: &mut gpui::TestAppContext) {
    image_client(cx);
    let body = format!(
        "![screen](https://github.com/user-attachments/assets/screen)\n\n{}",
        "Paragraph.\n\n".repeat(60)
    );
    let (window, app) = viewer_conversation(cx, &body);
    let mut vcx = gpui::VisualTestContext::from_window(window, cx);
    vcx.simulate_resize(size(px(1200.), px(800.)));
    draw_settled(&mut vcx);
    let selection = Selection {
        side: SelSide::Right,
        linewise: false,
        anchor: RowCol { row: 1, col: 0 },
        head: RowCol { row: 1, col: 2 },
    };
    vcx.update(|window, cx| {
        app.update(cx, |app, cx| {
            let prior = app.tree_filter_input.focus_handle(cx);
            window.focus(&prior, cx);
            let data = app.active_data_mut().unwrap();
            data.selection = Some(selection);
            data.conv_scroll.set_offset(point(px(0.), px(-20.)));
            cx.bind_keys([
                KeyBinding::new("cmd-o", OpenInput, None),
                KeyBinding::new("cmd-w", CloseItem, None),
                KeyBinding::new("j", NextFile, Some("ReviewApp")),
            ]);
            cx.notify();
        });
    });
    draw_settled(&mut vcx);
    let offset = cx.update(|cx| app.read(cx).active_data().unwrap().conv_scroll.offset());
    click_selector(&mut vcx, "conversation-0-image-0");
    let overlay = vcx.debug_bounds("image-viewer").unwrap();
    let header = APP_HEADER_HEIGHT;
    assert_eq!(overlay.origin, point(px(0.), header));
    assert_eq!(overlay.size, size(px(1200.), px(800.) - header));
    let toolbar = vcx.debug_bounds("viewer-toolbar").unwrap();
    assert_eq!(toolbar.top(), overlay.top());
    for selector in ["viewer-out", "viewer-fit", "viewer-browser", "viewer-close"] {
        let bounds = vcx.debug_bounds(selector).unwrap();
        assert!(
            bounds.left() >= toolbar.left()
                && bounds.right() <= toolbar.right()
                && bounds.top() >= toolbar.top()
                && bounds.bottom() <= toolbar.bottom(),
            "image toolbar control outside its bar: {selector}: {bounds:?} in {toolbar:?}"
        );
    }
    assert!(vcx.debug_bounds("viewer-image").is_some());
    vcx.simulate_keystrokes("cmd-o cmd-w j tab");
    vcx.dispatch_action(ToggleView);
    vcx.simulate_event(gpui::ScrollEvent {
        position: overlay.center(),
        delta: gpui::ScrollDelta::Pixels(point(px(0.), px(40.))),
        ..Default::default()
    });
    draw_settled(&mut vcx);
    cx.update(|cx| {
        let app = app.read(cx);
        assert!(app.image_viewer.is_some());
        assert!(!app.quick_open_visible);
        assert_eq!(app.items.len(), 1);
        assert!(app.active_data().unwrap().mode == ViewMode::Unified);
        assert_eq!(app.active_data().unwrap().selection, Some(selection));
        assert_eq!(app.active_data().unwrap().conv_scroll.offset(), offset);
    });
    click_selector(&mut vcx, "viewer-image");
    assert!(
        vcx.debug_bounds("image-viewer").is_some(),
        "image click must not dismiss"
    );
    vcx.simulate_keystrokes("escape");
    draw_settled(&mut vcx);
    assert!(vcx.debug_bounds("image-viewer").is_none());
    vcx.update(|window, cx| {
        let app = app.read(cx);
        assert!(app.tree_filter_input.focus_handle(cx).is_focused(window));
        assert_eq!(app.active_data().unwrap().selection, Some(selection));
        assert_eq!(app.active_data().unwrap().conv_scroll.offset(), offset);
    });
    assert!(cx.opened_url().is_none());
}

#[gpui::test]
fn image_viewer_zoom_drag_resize_fit_and_all_dismissals(cx: &mut gpui::TestAppContext) {
    image_client(cx);
    let (window, app) = viewer_conversation(
        cx,
        "![screen](https://github.com/user-attachments/assets/screen)",
    );
    let mut vcx = gpui::VisualTestContext::from_window(window, cx);
    vcx.simulate_resize(size(px(900.), px(600.)));
    draw_settled(&mut vcx);
    click_selector(&mut vcx, "conversation-0-image-0");
    for _ in 0..5 {
        click_selector(&mut vcx, "viewer-in");
    }
    let center = point(px(450.), px(320.));
    vcx.simulate_mouse_down(center, MouseButton::Left, Default::default());
    vcx.simulate_mouse_move(
        center + point(px(80.), px(40.)),
        MouseButton::Left,
        Default::default(),
    );
    vcx.simulate_mouse_up(
        center + point(px(80.), px(40.)),
        MouseButton::Left,
        Default::default(),
    );
    draw_settled(&mut vcx);
    cx.update(|cx| {
        assert_eq!(
            app.read(cx).image_viewer.as_ref().unwrap().transform.pan,
            point(80., 40.)
        )
    });
    vcx.simulate_resize(size(px(1100.), px(900.)));
    draw_settled(&mut vcx);
    assert_eq!(
        vcx.debug_bounds("image-viewer").unwrap().size,
        size(px(1100.), px(900.) - APP_HEADER_HEIGHT)
    );
    vcx.simulate_resize(size(px(500.), px(400.)));
    draw_settled(&mut vcx);
    let close = vcx.debug_bounds("viewer-close").unwrap();
    assert!(
        close.right() <= px(500.) && close.bottom() <= APP_HEADER_HEIGHT + px(92.),
        "toolbar must remain usable in narrow windows"
    );
    click_selector(&mut vcx, "viewer-fit");
    cx.update(|cx| {
        let t = &app.read(cx).image_viewer.as_ref().unwrap().transform;
        assert_eq!(t.zoom, 1.);
        assert_eq!(t.pan, point(0., 0.));
    });
    let backdrop = vcx.debug_bounds("viewer-backdrop").unwrap();
    vcx.simulate_click(backdrop.origin + point(px(4.), px(4.)), Default::default());
    draw_settled(&mut vcx);
    assert!(vcx.debug_bounds("image-viewer").is_none());
    click_selector(&mut vcx, "conversation-0-image-0");
    click_selector(&mut vcx, "viewer-close");
    assert!(vcx.debug_bounds("image-viewer").is_none());
}

#[gpui::test]
fn oversized_conversation_images_reserve_only_their_scaled_height(cx: &mut gpui::TestAppContext) {
    image_client(cx);
    // Large screenshots appear as adjacent image tags in the conversation.
    let body = (0..7)
        .map(|i| format!("<img width=\"3024\" height=\"1964\" alt=\"screenshot\" src=\"https://github.com/user-attachments/assets/large-{i}\" />"))
        .collect::<Vec<_>>()
        .join("\n");
    let (window, _) = viewer_conversation(cx, &body);
    let mut vcx = gpui::VisualTestContext::from_window(window, cx);
    for width in [1200., 800., 1600.] {
        vcx.simulate_resize(size(px(width), px(5000.)));
        draw_settled(&mut vcx);
        let mut previous: Option<gpui::Bounds<gpui::Pixels>> = None;
        for (offset, _) in body.match_indices("<img") {
            let selector = Box::leak(format!("conversation-0-image-{offset}").into_boxed_str());
            let bounds = vcx.debug_bounds(selector).unwrap();
            assert!(bounds.size.width < px(3024.));
            // The fake HTTP client returns 240x160 images. Use the decoded
            // aspect ratio, not potentially stale HTML height attributes.
            assert!(
                (bounds.size.height - bounds.size.width / 1.5).abs() < px(1.),
                "image must shrink in both dimensions: {bounds:?}"
            );
            if let Some(previous) = previous {
                let gap = bounds.top() - previous.bottom();
                assert!(gap >= px(0.) && gap <= px(16.), "image gap: {gap:?}");
            }
            previous = Some(bounds);
        }
        let text = vcx.debug_bounds("conversation-0-text").unwrap();
        assert!(
            text.bottom() - previous.unwrap().bottom() < px(32.),
            "no trailing image reservation: text={text:?}, last={previous:?}"
        );
    }
}

#[gpui::test]
fn github_paste_html_images_preserve_dimensions_and_open_the_viewer(cx: &mut gpui::TestAppContext) {
    image_client(cx);
    // This is the markup GitHub generates for ordinary pasted screenshots.
    let body = "<img width=\"120\" height=\"80\" alt=\"Image\" src=\"https://github.com/user-attachments/assets/pasted\" />";
    let (window, app) = viewer_conversation(cx, body);
    let mut vcx = gpui::VisualTestContext::from_window(window, cx);
    draw_settled(&mut vcx);
    assert_eq!(
        vcx.debug_bounds("conversation-0-image-0").unwrap().size,
        size(px(120.), px(80.))
    );
    click_selector(&mut vcx, "conversation-0-image-0");
    cx.update(|cx| {
        let viewer = app.read(cx).image_viewer.as_ref().unwrap();
        assert_eq!(
            viewer.target.url.as_ref(),
            "https://github.com/user-attachments/assets/pasted"
        );
        assert_eq!(viewer.target.alt.as_ref(), "Image");
    });
    assert_eq!(
        vcx.debug_bounds("viewer-image").unwrap().size,
        size(px(240.), px(160.)),
        "viewer uses the full image, not HTML's reduced display size"
    );
    assert!(cx.opened_url().is_none());
    vcx.simulate_keystrokes("escape");
    draw_settled(&mut vcx);
    assert!(vcx.debug_bounds("image-viewer").is_none());
}

#[gpui::test]
fn github_paste_html_in_quotes_lists_tables_mixed_links_and_adjacent_images(
    cx: &mut gpui::TestAppContext,
) {
    image_client(cx);
    let body = "> <img width=\"120\" height=\"80\" alt=\"quote\" src=\"https://github.com/user-attachments/assets/quote\" />\n\n- <img width='120' height='80' alt='list' src='https://github.com/user-attachments/assets/list'>\n\n| Image |\n| --- |\n| <img width='120' height='80' alt='table' src='https://github.com/user-attachments/assets/table'> |\n\n[before <img width='120' height='80' alt='linked' src='https://github.com/user-attachments/assets/linked'> after](https://example.com/original)\n\n<img width='120' height='80' alt='first' src='https://github.com/user-attachments/assets/first'>\n<img width='120' height='80' alt='second' src='https://github.com/user-attachments/assets/second'>";
    let (window, app) = viewer_conversation(cx, body);
    let mut vcx = gpui::VisualTestContext::from_window(window, cx);
    vcx.simulate_resize(size(px(1400.), px(2400.)));
    draw_settled(&mut vcx);
    let positions: Vec<_> = body
        .match_indices("<img")
        .map(|(position, _)| position)
        .collect();
    for (alt, offset) in ["quote", "list", "table", "linked", "first", "second"]
        .into_iter()
        .zip(positions)
    {
        let selector = Box::leak(format!("conversation-0-image-{offset}").into_boxed_str());
        assert_eq!(
            vcx.debug_bounds(selector).unwrap().size,
            size(px(120.), px(80.)),
            "{alt}"
        );
        click_selector(&mut vcx, selector);
        cx.update(|cx| {
            let viewer = app.read(cx).image_viewer.as_ref().unwrap();
            assert_eq!(viewer.target.alt.as_ref(), alt);
            if alt == "linked" {
                assert_eq!(
                    viewer.target.link.as_deref(),
                    Some("https://example.com/original")
                );
            }
        });
        assert!(cx.opened_url().is_none());
        vcx.simulate_keystrokes("escape");
        draw_settled(&mut vcx);
    }
}

#[gpui::test]
fn consecutive_block_images_have_spacing(cx: &mut gpui::TestAppContext) {
    image_client(cx);
    for body in [
        "<img width='120' height='80' src='https://github.com/first'>\n<img width='120' height='80' src='https://github.com/second'>",
        "<img width='120' height='80' src='https://github.com/first'>\n\n<img width='120' height='80' src='https://github.com/second'>",
        "![first](https://github.com/first)![second](https://github.com/second)",
        "![first](https://github.com/first)\n\n![second](https://github.com/second)",
    ] {
        let (window, _app) = viewer_conversation(cx, body);
        let mut vcx = gpui::VisualTestContext::from_window(window, cx);
        draw_settled(&mut vcx);
        let offset = body.rfind("<img").or_else(|| body.rfind("![")).unwrap();
        let second_selector =
            Box::leak(format!("conversation-0-image-{offset}").into_boxed_str());
        let first = vcx.debug_bounds("conversation-0-image-0").unwrap();
        let second = vcx.debug_bounds(second_selector).unwrap();
        assert!(
            second.top() - first.bottom() >= px(8.),
            "consecutive images need a visible gap: {first:?}, {second:?} for {body}"
        );
        assert_eq!(first.left(), second.left());
    }
}

#[gpui::test]
fn github_paste_image_with_a_following_caption_still_opens(cx: &mut gpui::TestAppContext) {
    image_client(cx);
    // No blank line: CommonMark delivers the image and caption as one HTML node.
    let body = "<img width=\"120\" height=\"80\" alt=\"Image\" src=\"https://github.com/user-attachments/assets/captioned\" />\nA caption without a blank line.";
    let (window, app) = viewer_conversation(cx, body);
    let mut vcx = gpui::VisualTestContext::from_window(window, cx);
    draw_settled(&mut vcx);
    let content = vcx
        .debug_bounds("conversation-0-captioned-image-0")
        .unwrap();
    vcx.simulate_click(content.origin + point(px(60.), px(40.)), Default::default());
    draw_settled(&mut vcx);
    cx.update(|cx| {
        assert_eq!(
            app.read(cx)
                .image_viewer
                .as_ref()
                .unwrap()
                .target
                .url
                .as_ref(),
            "https://github.com/user-attachments/assets/captioned"
        )
    });
    assert!(
        cx.opened_url().is_none(),
        "internal activation links must never open the browser"
    );
    vcx.simulate_keystrokes("escape");
    draw_settled(&mut vcx);
    let content = vcx
        .debug_bounds("conversation-0-captioned-image-0")
        .unwrap();
    // Text next to the image is NOT part of its activation target.
    vcx.simulate_click(
        content.origin + point(px(200.), px(40.)),
        Default::default(),
    );
    draw_settled(&mut vcx);
    assert!(vcx.debug_bounds("image-viewer").is_none());
}

#[gpui::test]
fn linked_markdown_images_prioritize_viewer_and_browser_actions_are_explicit(
    cx: &mut gpui::TestAppContext,
) {
    image_client(cx);
    let (window, _) = viewer_conversation(cx, "[![diagram](https://github.com/user-attachments/assets/diagram)](https://example.com/original)");
    let mut vcx = gpui::VisualTestContext::from_window(window, cx);
    draw_settled(&mut vcx);
    click_selector(&mut vcx, "conversation-0-image-1");
    assert!(cx.opened_url().is_none());
    assert_eq!(
        vcx.debug_bounds("viewer-image").unwrap().size,
        size(px(200.), px(80.)),
        "SVG keeps its aspect ratio"
    );
    click_selector(&mut vcx, "viewer-link");
    assert_eq!(
        cx.opened_url().as_deref(),
        Some("https://example.com/original")
    );
    click_selector(&mut vcx, "viewer-browser");
    assert_eq!(
        cx.opened_url().as_deref(),
        Some("https://github.com/user-attachments/assets/diagram")
    );
    assert!(vcx.debug_bounds("image-viewer").is_some());
}

#[gpui::test]
fn markdown_plugin_covers_lists_tables_and_mixed_links_not_general_html_or_code(
    cx: &mut gpui::TestAppContext,
) {
    image_client(cx);
    let body = "- ![list](https://github.com/user-attachments/assets/list)\n\n| Image |\n| --- |\n| ![table](https://github.com/user-attachments/assets/table) |\n\n[before ![mixed](https://github.com/user-attachments/assets/mixed) after](https://example.com/mixed)\n\n<img src='https://example.com/html'>\n\n`![code](https://github.com/user-attachments/assets/code)`";
    let (window, app) = viewer_conversation(cx, body);
    let mut vcx = gpui::VisualTestContext::from_window(window, cx);
    vcx.simulate_resize(size(px(1400.), px(1800.)));
    draw_settled(&mut vcx);
    for alt in ["list", "table", "mixed"] {
        let selector = Box::leak(
            format!(
                "conversation-0-image-{}",
                body.find(&format!("![{alt}]")).unwrap()
            )
            .into_boxed_str(),
        );
        click_selector(&mut vcx, selector);
        cx.update(|cx| {
            let viewer = app.read(cx).image_viewer.as_ref().unwrap();
            assert_eq!(viewer.target.alt.as_ref(), alt);
            if alt == "mixed" {
                assert_eq!(
                    viewer.target.link.as_deref(),
                    Some("https://example.com/mixed")
                );
            }
        });
        assert!(cx.opened_url().is_none());
        vcx.simulate_keystrokes("escape");
        draw_settled(&mut vcx);
    }
    let code = Box::leak(
        format!("conversation-0-image-{}", body.find("![code]").unwrap()).into_boxed_str(),
    );
    assert!(vcx.debug_bounds(code).is_none());
    let html =
        Box::leak(format!("conversation-0-image-{}", body.find("<img").unwrap()).into_boxed_str());
    assert!(
        vcx.debug_bounds(html).is_none(),
        "general HTML stays on the stock renderer"
    );
}

#[gpui::test]
fn image_viewer_activates_markdown_and_github_paste_in_both_changes_views(
    cx: &mut gpui::TestAppContext,
) {
    image_client(cx);
    let (window, app) = queue_window(cx, Arc::new(FakePr::default()));
    let mut item = loop_pr_item(vec![
        rc(
            11,
            "a.rs",
            Some("RIGHT"),
            Some(1),
            "![right](https://github.com/user-attachments/assets/right)",
            "alice",
            "2026-01-01T00:00:00Z",
            None,
        ),
        rc(
            13,
            "a.rs",
            Some("LEFT"),
            Some(1),
            "<img width=\"240\" height=\"160\" alt=\"left\" src=\"https://github.com/user-attachments/assets/left\" />",
            "bob",
            "2026-01-01T00:00:00Z",
            None,
        ),
    ]);
    let ItemState::Ready(data) = &mut item.state else {
        unreachable!()
    };
    data.diff = diff_core::parse_patch(
        "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1,2 +1,2 @@\n-old\n+new\n tail\n",
    );
    data.set_rows(data.row_build());
    install_item(&app, item, cx);
    let mut vcx = gpui::VisualTestContext::from_window(window, cx);
    vcx.simulate_resize(size(px(1500.), px(1800.)));
    for mode in [ViewMode::Unified, ViewMode::Split] {
        cx.update(|cx| {
            app.update(cx, |app, cx| {
                let data = app.active_data_mut().unwrap();
                data.mode = mode;
                data.rebuild_rows_anchored();
                data.scroll.0.borrow_mut().deferred_scroll_to_item = None;
                data.scroll
                    .0
                    .borrow()
                    .base_handle
                    .set_offset(point(px(0.), px(0.)));
                cx.notify();
            })
        });
        draw_settled(&mut vcx);
        for selector in ["inline-text-0-11-0-image-0", "inline-text-0-13-0-image-0"] {
            let before = cx.update(|cx| {
                let data = app.read(cx).active_data().unwrap();
                (data.scroll.0.borrow().base_handle.offset(), data.rows.len())
            });
            click_selector(&mut vcx, selector);
            assert!(
                vcx.debug_bounds("image-viewer").is_some(),
                "{selector}: {:?}, split={}",
                vcx.debug_bounds(selector),
                mode == ViewMode::Split
            );
            vcx.simulate_keystrokes("escape");
            draw_settled(&mut vcx);
            cx.update(|cx| {
                let data = app.read(cx).active_data().unwrap();
                assert_eq!(
                    (data.scroll.0.borrow().base_handle.offset(), data.rows.len()),
                    before
                );
            });
        }
        vcx.simulate_resize(size(px(1500.), px(400.)));
        cx.update(|cx| {
            app.update(cx, |app, cx| {
                let data = app.active_data_mut().unwrap();
                let first = data
                    .rows
                    .iter()
                    .position(|row| {
                        matches!(row,
                Row::CommentBody { text, slot: 0, .. } if text.comment_id == 11)
                    })
                    .unwrap();
                data.scroll
                    .0
                    .borrow()
                    .base_handle
                    .set_offset(point(px(0.), -px((first + 1) as f32 * row_height())));
                cx.notify();
            })
        });
        draw_settled(&mut vcx);
        let viewport = cx.update(|cx| {
            app.read(cx)
                .active_data()
                .unwrap()
                .scroll
                .0
                .borrow()
                .base_handle
                .bounds()
        });
        let image = vcx.debug_bounds("inline-text-0-11-0-image-0").unwrap();
        let visible = image.intersect(&viewport);
        assert!(visible.size.height > px(0.));
        assert!(
            image.top() < viewport.top(),
            "image's top reservation is virtualized away"
        );
        vcx.simulate_click(visible.center(), Default::default());
        draw_settled(&mut vcx);
        assert!(
            vcx.debug_bounds("image-viewer").is_some(),
            "partially virtualized images stay clickable"
        );
        vcx.simulate_keystrokes("escape");
        draw_settled(&mut vcx);
        vcx.simulate_resize(size(px(1500.), px(1800.)));
    }
}

#[gpui::test]
fn viewer_does_not_stack_and_closes_when_origin_review_closes(cx: &mut gpui::TestAppContext) {
    image_client(cx);
    let (window, app) = viewer_conversation(
        cx,
        "![screen](https://github.com/user-attachments/assets/screen)",
    );
    let mut vcx = gpui::VisualTestContext::from_window(window, cx);
    draw_settled(&mut vcx);
    click_selector(&mut vcx, "conversation-0-image-0");
    vcx.update(|window, cx| {
        app.update(cx, |app, cx| {
            app.open_quick_open(window, cx);
            app.open_review(window, cx);
            assert!(!app.quick_open_visible);
            assert!(app.review.is_none());
            app.close_item(app.active, cx);
        })
    });
    draw_settled(&mut vcx);
    assert!(vcx.debug_bounds("image-viewer").is_none());
    vcx.update(|window, cx| assert!(app.read(cx).focus_handle.is_focused(window)));
}

#[gpui::test]
fn viewer_loading_failure_retry_reuses_gpui_loader(cx: &mut gpui::TestAppContext) {
    let (send, receive) = futures::channel::oneshot::channel::<()>();
    let receive = Arc::new(std::sync::Mutex::new(Some(receive)));
    cx.update(|cx| {
        cx.set_http_client(http_client::FakeHttpClient::create(move |_| {
            let receive = receive.lock().unwrap().take().unwrap();
            async move {
                receive.await.unwrap();
                Ok(http_client::http::Response::builder()
                    .status(500)
                    .body(http_client::AsyncBody::from("unavailable"))
                    .unwrap())
            }
        }))
    });
    let (window, app) = viewer_conversation(
        cx,
        "![screen](https://github.com/user-attachments/assets/screen)",
    );
    let mut vcx = gpui::VisualTestContext::from_window(window, cx);
    draw_settled(&mut vcx);
    vcx.update(|window, cx| {
        app.update(cx, |app, cx| {
            app.open_image(
                crate::image_viewer::ImageTarget {
                    url: "https://github.com/user-attachments/assets/screen".into(),
                    alt: "screen".into(),
                    link: None,
                },
                0,
                window,
                cx,
            )
        })
    });
    draw_settled(&mut vcx);
    assert!(vcx.debug_bounds("viewer-loading").is_some());
    send.send(()).unwrap();
    draw_settled(&mut vcx);
    assert!(vcx.debug_bounds("viewer-error").is_some());
    assert!(cx.opened_url().is_none());
    vcx.simulate_keystrokes("escape");
    draw_settled(&mut vcx);
    click_selector(&mut vcx, "conversation-0-image-0");
    assert!(
        vcx.debug_bounds("viewer-error").is_some(),
        "failed inline images remain inspectable"
    );
    let requests = image_client(cx);
    click_selector(&mut vcx, "viewer-retry");
    assert!(vcx.debug_bounds("viewer-image").is_some());
    assert!(vcx.debug_bounds("viewer-error").is_none());
    assert_eq!(requests.lock().unwrap().len(), 1);
}

#[gpui::test]
fn animated_markdown_images_keep_frames_and_share_the_inline_asset(cx: &mut gpui::TestAppContext) {
    let requests = image_client(cx);
    let (window, _) = viewer_conversation(
        cx,
        "![animation](https://github.com/user-attachments/assets/animation)",
    );
    let mut vcx = gpui::VisualTestContext::from_window(window, cx);
    draw_settled(&mut vcx);
    click_selector(&mut vcx, "conversation-0-image-0");
    assert_eq!(
        vcx.debug_bounds("viewer-image").unwrap().size,
        size(px(120.), px(80.))
    );
    vcx.update(|window, cx| {
        let image = window
            .get_asset::<svg_image::ImageLoader>(
                &gpui::Resource::Uri("https://github.com/user-attachments/assets/animation".into()),
                cx,
            )
            .unwrap()
            .unwrap();
        assert_eq!(
            image.frame_count(),
            2,
            "do not flatten animated images to a single frame"
        );
    });
    assert_eq!(
        requests
            .lock()
            .unwrap()
            .iter()
            .filter(|url| url.ends_with("animation"))
            .count(),
        1,
        "opening an already loaded image must not start a second downloader"
    );
}

#[gpui::test]
fn conversation_comments_and_review_threads_open_github_paste_images(
    cx: &mut gpui::TestAppContext,
) {
    image_client(cx);
    let (window, app) = viewer_conversation(cx, "Description");
    cx.update(|cx| {
        app.update(cx, |app, cx| {
            let data = app.active_data_mut().unwrap();
            let mut discussion = conversation::tests::sample_discussion();
            discussion.comments.truncate(1);
            discussion.comments[0].body =
                "<img width=\"240\" height=\"160\" alt=\"comment\" src=\"https://github.com/user-attachments/assets/comment\" />".into();
            discussion.threads[0].is_resolved = false;
            discussion.threads[0].is_outdated = false;
            discussion.threads[0].comments.truncate(1);
            discussion.threads[0].comments[0].body =
                "<img width=\"240\" height=\"160\" alt=\"thread\" src=\"https://github.com/user-attachments/assets/thread\" />".into();
            data.discussion = Some(discussion);
            cx.notify();
        })
    });
    let mut vcx = gpui::VisualTestContext::from_window(window, cx);
    vcx.simulate_resize(size(px(1400.), px(2400.)));
    draw_settled(&mut vcx);
    for selector in [
        "discussion-0-main-false-1-image-0",
        "discussion-0-review-false-1-image-0",
    ] {
        click_selector(&mut vcx, selector);
        assert!(vcx.debug_bounds("image-viewer").is_some());
        vcx.simulate_keystrokes("escape");
        draw_settled(&mut vcx);
    }
}
