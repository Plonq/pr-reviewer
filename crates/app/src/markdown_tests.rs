use super::*;

pub(super) fn image_client(cx: &mut gpui::TestAppContext) -> Arc<std::sync::Mutex<Vec<String>>> {
    let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen = requests.clone();
    let mut png = std::io::Cursor::new(Vec::new());
    image::DynamicImage::new_rgb8(240, 160).write_to(&mut png, image::ImageFormat::Png).unwrap();
    let png = png.into_inner();
    let mut gif = Vec::new();
    {
        let mut encoder = image::codecs::gif::GifEncoder::new(&mut gif);
        encoder.encode_frames([
            image::Frame::new(image::RgbaImage::new(120, 80)),
            image::Frame::new(image::RgbaImage::new(120, 80)),
        ]).unwrap();
    }
    cx.update(|cx| cx.set_http_client(http_client::FakeHttpClient::create(move |req| {
        let url = req.uri().to_string();
        seen.lock().unwrap().push(url.clone());
        let bytes = if url.ends_with("undecodable-badge.svg") {
            b"not an image".to_vec()
        } else if url.ends_with("diagram") {
            br#"<svg xmlns="http://www.w3.org/2000/svg" width="200" height="80"><rect width="200" height="80"/></svg>"#.to_vec()
        } else if url.ends_with("animation") { gif.clone() } else { png.clone() };
        let status = if url.ends_with("missing-badge.svg") { 404 } else { 200 };
        async move { Ok(http_client::http::Response::builder().status(status)
            // Deliberately uninformative MIME and extension: decoding is byte-based.
            .header("content-type", "application/octet-stream")
            .body(http_client::AsyncBody::from(bytes)).unwrap()) }
    })));
    requests
}

pub(super) fn draw_settled(cx: &mut gpui::VisualTestContext) {
    for _ in 0..8 {
        cx.update(|window, cx| { window.draw(cx).clear(cx); });
        cx.run_until_parked();
    }
}

#[gpui::test]
fn failed_inline_badges_stay_inside_their_reserved_bounds(cx: &mut gpui::TestAppContext) {
    image_client(cx);
    for asset in ["undecodable-badge.svg", "missing-badge.svg"] {
        let url = format!("https://example.test/{asset}");
        let source = format!(
            "[![Rust]({url})](https://example.test/language) 🦀\n\n\
             # Remove the doc comment on `POLL_INTERVAL`\n\n\
             > *TL;DR*: **one thing only** — this removes a comment.\n\n\
             > Following prose should remain readable.\n\n\
             | Aspect | Before | After |\n| :--- | :---: | :---: |\n\
             | Doc comment | prose | *removed* |\n\n```rust\nconst VALUE: u32 = 1;\n```"
        );
        let (window, app) = queue_window(cx, Arc::new(FakePr::default()));
        let mut item = loop_pr_item(vec![rc(
            11,
            "a.rs",
            Some("RIGHT"),
            Some(1),
            &source,
            "alice",
            "2026-01-01T00:00:00Z",
            None,
        )]);
        let ItemState::Ready(data) = &mut item.state else {
            unreachable!()
        };
        data.pr_meta.as_mut().unwrap().body = source;
        data.discussion = Some(github::PrDiscussion::default());
        install_item(&app, item, cx);
        let mut vcx = gpui::VisualTestContext::from_window(window, cx);
        vcx.simulate_resize(size(px(1500.), px(1800.)));
        for (mode, image_selector, fallback_selector, content_selector) in [
            (
                Some(ViewMode::Unified),
                "inline-text-0-11-0-image-1",
                "inline-text-0-11-0-image-1-fallback",
                "inline-text-0-11-0-content",
            ),
            (
                Some(ViewMode::Split),
                "inline-text-0-11-0-image-1",
                "inline-text-0-11-0-image-1-fallback",
                "inline-text-0-11-0-content",
            ),
            (
                None,
                "conversation-0-image-1",
                "conversation-0-image-1-fallback",
                "conversation-0-text",
            ),
        ] {
            if let Some(mode) = mode {
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
            } else {
                vcx.dispatch_action(ShowConversation);
            }
            draw_settled(&mut vcx);
            let image = vcx.debug_bounds(image_selector).unwrap();
            let fallback = vcx.debug_bounds(fallback_selector).unwrap();
            let content = vcx.debug_bounds(content_selector).unwrap();
            assert!(image.size.width > px(0.) && image.size.height > px(0.));
            // The linked fallback inherits link underlines. Inspect their actual
            // clipped paint bounds, since unchanged document height hid this bug.
            vcx.update(|window, _| {
                let image = image.scale(window.scale_factor());
                let content = content.scale(window.scale_factor());
                for underline in window.painted_underlines() {
                    let painted = underline.bounds.intersect(&underline.content_mask.bounds);
                    if painted.intersects(&content) && painted.right() > image.left()
                        && painted.left() < image.right() && painted.top() >= image.top()
                    {
                        assert!(painted.bottom() <= image.bottom(),
                            "{asset} {image_selector}: fallback painted beyond inline image: {painted:?} in {image:?}");
                    }
                }
            });
            assert_eq!(image.intersect(&fallback), fallback,
                "{asset} {image_selector}: fallback must fit the reserved slot: {fallback:?} in {image:?}");
            vcx.simulate_click(image.center(), Default::default());
            draw_settled(&mut vcx);
            cx.update(|cx| {
                let viewer = app.read(cx).image_viewer.as_ref().unwrap();
                assert_eq!(viewer.target.url.as_ref(), url);
                assert_eq!(viewer.target.alt.as_ref(), "Rust");
                assert_eq!(
                    viewer.target.link.as_deref(),
                    Some("https://example.test/language")
                );
            });
            assert!(
                cx.opened_url().is_none(),
                "image activation must not follow the parent link"
            );
            assert!(vcx.debug_bounds("viewer-error").is_some());
            vcx.simulate_keystrokes("escape");
            draw_settled(&mut vcx);
        }
    }
}

#[gpui::test]
fn github_single_newlines_match_explicit_breaks_in_inline_and_persistent_prose(cx: &mut gpui::TestAppContext) {
    let (window, app) = queue_window(cx, Arc::new(FakePr::default()));
    let multiline = "line1\nline2\nline3";
    let single = "line1 line2 line3";
    let mut item = loop_pr_item(vec![
        rc(11, "a.rs", Some("RIGHT"), Some(1), multiline, "alice", "2026-01-01T00:00:00Z", None),
        rc(13, "a.rs", Some("RIGHT"), Some(1), "line1  \nline2  \nline3", "bob", "2026-01-01T00:00:00Z", None),
        rc(15, "a.rs", Some("RIGHT"), Some(1), single, "carol", "2026-01-01T00:00:00Z", None),
    ]);
    let ItemState::Ready(data) = &mut item.state else { unreachable!() };
    data.pr_meta.as_mut().unwrap().body = multiline.into();
    install_item(&app, item, cx);
    let mut vcx = gpui::VisualTestContext::from_window(window, cx);
    vcx.simulate_resize(size(px(1500.), px(1800.)));
    for mode in [ViewMode::Unified, ViewMode::Split] {
        cx.update(|cx| app.update(cx, |app, cx| {
            let data = app.active_data_mut().unwrap();
            data.mode = mode;
            data.rebuild_rows_anchored();
            data.scroll.0.borrow_mut().deferred_scroll_to_item = None;
            data.scroll.0.borrow().base_handle.set_offset(point(px(0.), px(0.)));
            cx.notify();
        }));
        draw_settled(&mut vcx);
        let body = vcx.debug_bounds("inline-text-0-11-0-content").unwrap();
        let explicit = vcx.debug_bounds("inline-text-0-13-0-content").unwrap();
        let one_line = vcx.debug_bounds("inline-text-0-15-0-content").unwrap();
        assert_eq!(body.size.height, explicit.size.height);
        assert!(body.size.height >= one_line.size.height * 2.9, "three visual lines, not one");
        assert!(body.bottom() <= vcx.debug_bounds("inline-text-0-11-0-card").unwrap().bottom());
        // Native prose selection must still copy real line endings, without
        // the backslashes introduced only for the Markdown parser.
        let start = body.origin + point(px(1.), px(8.));
        let end = body.origin + point(px(90.), body.size.height - px(5.));
        vcx.simulate_event(MouseDownEvent {
            position: start, button: MouseButton::Left, click_count: 1,
            ..Default::default()
        });
        vcx.simulate_mouse_move(end, MouseButton::Left, Default::default());
        vcx.simulate_mouse_up(end, MouseButton::Left, Default::default());
        vcx.update(|window, cx| {
            let selected = gpui::base::TextSelection::selected_text(window, cx);
            assert!(selected.contains("line1\nline2\nline3"), "{selected:?}");
        });
    }
    vcx.dispatch_action(ShowConversation);
    draw_settled(&mut vcx);
    let three_lines = vcx.debug_bounds("conversation-0-text").unwrap().size.height;
    let owner = cx.update(|cx| app.read(cx).active_data().unwrap().conv_markdown.borrow()
        .get("conversation-0").unwrap().state.entity_id());
    for source in [single, multiline] {
        cx.update(|cx| app.update(cx, |app, cx| {
            app.active_data_mut().unwrap().pr_meta.as_mut().unwrap().body = source.into();
            cx.notify();
        }));
        draw_settled(&mut vcx);
        let height = vcx.debug_bounds("conversation-0-text").unwrap().size.height;
        if source == single {
            assert!(three_lines >= height * 2.9);
        } else {
            assert_eq!(height, three_lines);
        }
        cx.update(|cx| assert_eq!(app.read(cx).active_data().unwrap().conv_markdown.borrow()
            .get("conversation-0").unwrap().state.entity_id(), owner, "edits reuse the document owner"));
    }
}

#[gpui::test]
fn stock_inline_markdown_measures_images_in_both_sides_and_partial_virtualization(cx: &mut gpui::TestAppContext) {
    let requests = image_client(cx);
    let (window, app) = queue_window(cx, Arc::new(FakePr::default()));
    let mut vcx = gpui::VisualTestContext::from_window(window, cx);
    vcx.simulate_resize(size(px(1500.), px(1800.)));
    let mut item = loop_pr_item(vec![
        rc(11, "a.rs", Some("RIGHT"), Some(1), "Before **screen**\n\n![screen](https://github.com/user-attachments/assets/screen)\n\n![diagram](https://github.com/user-attachments/assets/diagram)\n\nAfter `code`.", "alice", "2026-01-01T00:00:00Z", None),
        rc(13, "a.rs", Some("LEFT"), Some(1), "<img src='https://github.com/user-attachments/assets/old-screen'>", "carol", "2026-01-01T00:00:00Z", None),
    ]);
    let ItemState::Ready(data) = &mut item.state else { unreachable!() };
    data.diff = diff_core::parse_patch("diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1,2 +1,2 @@\n-old\n+new\n tail\n");
    data.set_rows(data.row_build());
    install_item(&app, item, cx);
    for mode in [ViewMode::Unified, ViewMode::Split] {
        cx.update(|cx| app.update(cx, |app, cx| {
            let data = app.active_data_mut().unwrap();
            data.mode = mode;
            data.rebuild_rows_anchored();
            data.scroll.0.borrow_mut().deferred_scroll_to_item = None;
            data.scroll.0.borrow().base_handle.set_offset(point(px(0.), px(0.)));
            cx.notify();
        }));
        draw_settled(&mut vcx);
        let card = vcx.debug_bounds("inline-text-0-11-0-card").unwrap();
        let content = vcx.debug_bounds("inline-text-0-11-0-content").unwrap();
        assert!(content.size.height > px(row_height()));
        assert!(content.bottom() <= card.bottom(), "async content must fit its reservation");
        let (viewport, first) = cx.update(|cx| {
            let data = app.read(cx).active_data().unwrap();
            let first = data.rows.iter().position(|row| matches!(row,
                Row::CommentBody { text, slot: 0, .. } if text.comment_id == 11)).unwrap();
            (data.scroll.0.borrow().base_handle.bounds(), first)
        });
        if mode == ViewMode::Split {
            assert!(card.left() > viewport.center().x);
            assert!(vcx.debug_bounds("inline-text-0-13-0-card").unwrap().right() <= viewport.center().x);
        }
        vcx.simulate_resize(size(px(1500.), px(200.)));
        cx.update(|cx| app.update(cx, |app, cx| {
            app.active_data_mut().unwrap().scroll.0.borrow().base_handle
                .set_offset(point(px(0.), -px((first + 1) as f32 * row_height())));
            cx.notify();
        }));
        draw_settled(&mut vcx);
        assert!(vcx.debug_bounds("inline-text-0-11-0-content").is_some(),
            "a body stays mounted when its first reservation row is virtualized away");
        vcx.simulate_resize(size(px(1500.), px(1800.)));
    }
    let requests = requests.lock().unwrap();
    for name in ["screen", "diagram", "old-screen"] {
        assert!(requests.iter().any(|url| url.ends_with(name)), "stock image loader requested {name}");
    }
}

#[gpui::test]
fn inline_comment_drags_select_prose_without_moving_the_diff(cx: &mut gpui::TestAppContext) {
    let (window, app) = queue_window(cx, Arc::new(FakePr::default()));
    let mut item = loop_pr_item(vec![rc(
        11, "a.rs", Some("RIGHT"), Some(1),
        "Alpha bravo charlie delta echo foxtrot golf hotel.",
        "alice", "2026-01-01T00:00:00Z", None,
    )]);
    let ItemState::Ready(data) = &mut item.state else { unreachable!() };
    let context = " context\n".repeat(200);
    data.diff = diff_core::parse_patch(&format!(
        "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1,201 +1,201 @@\n-old\n+new\n{context}"
    ));
    data.set_rows(data.row_build());
    install_item(&app, item, cx);
    let mut vcx = gpui::VisualTestContext::from_window(window, cx);
    for mode in [ViewMode::Unified, ViewMode::Split] {
        cx.update(|cx| app.update(cx, |app, cx| {
            let data = app.active_data_mut().unwrap();
            data.mode = mode;
            data.rebuild_rows_anchored();
            data.scroll.0.borrow_mut().deferred_scroll_to_item = None;
            data.scroll.0.borrow().base_handle.set_offset(point(px(0.), -px(row_height())));
            cx.notify();
        }));
        draw_settled(&mut vcx);
        for click_count in [1, 2] {
            let body = vcx.debug_bounds("inline-text-0-11-0-content").unwrap();
            let start = body.origin + point(px(8.), px(10.));
            let end = start + point(px(150.), px(0.));
            let before = cx.update(|cx| app.read(cx).active_data().unwrap()
                .scroll.0.borrow().base_handle.offset());
            vcx.simulate_event(MouseDownEvent {
                position: start, button: MouseButton::Left, click_count,
                ..Default::default()
            });
            vcx.simulate_mouse_move(end, MouseButton::Left, Default::default());
            // Give selection auto-scroll time to run: the pointer is near the
            // comment's top, but nowhere near the diff viewport's top.
            vcx.executor().advance_clock(std::time::Duration::from_millis(150));
            draw_settled(&mut vcx);
            vcx.simulate_mouse_up(end, MouseButton::Left, Default::default());
            vcx.update(|window, cx| {
                let selected = gpui::base::TextSelection::selected_text(window, cx);
                assert!(selected.contains("bravo"), "click count {click_count}: {selected:?}");
                let app = app.read(cx);
                assert!(app.drag_anchor.is_none());
                assert!(app.active_data().unwrap().selection.is_none());
                assert_eq!(app.active_data().unwrap().scroll.0.borrow().base_handle.offset(), before);
            });
        }
    }
}

#[gpui::test]
fn conversation_keeps_deep_scroll_across_pane_switches(cx: &mut gpui::TestAppContext) {
    let (window, app) = queue_window(cx, Arc::new(FakePr::default()));
    let mut item = loop_pr_item(Vec::new());
    item.pane = ItemPane::Conversation;
    let ItemState::Ready(data) = &mut item.state else { unreachable!() };
    data.pr_meta.as_mut().unwrap().body = format!("```mermaid\nflowchart TD\nA[Review] --> B[Respond]\n```\n\n{}", "Description paragraph.\n\n".repeat(300));
    let mut discussion = conversation::tests::sample_discussion();
    discussion.threads.clear();
    let template = discussion.comments[0].clone();
    discussion.comments = (1..=30).map(|id| {
        let mut comment = template.clone();
        comment.id = id;
        comment.body = format!("Comment {id}.\n\n{}", "A long comment paragraph with enough source to parse asynchronously.\n\n".repeat(80));
        comment
    }).collect();
    data.discussion = Some(discussion);
    install_item(&app, item, cx);
    let mut vcx = gpui::VisualTestContext::from_window(window, cx);
    draw_settled(&mut vcx);
    for y in [-120., -6000., -16000.] {
        vcx.update(|window, cx| {
            app.update(cx, |app, cx| {
                app.active_data().unwrap().conv_scroll.set_offset(point(px(0.), px(y)));
                cx.notify();
            });
            let _ = window.draw(cx);
        });
        let expected = cx.update(|cx| app.read(cx).active_data().unwrap().conv_scroll.offset());
        assert_eq!(expected.y, px(y), "the fixture must have enough scroll range");
        for _ in 0..2 {
            vcx.dispatch_action(ShowChanges);
            draw_settled(&mut vcx);
            vcx.dispatch_action(ShowConversation);
            // Check the very first frame too, before async Markdown can parse.
            vcx.update(|window, cx| {
                let _ = window.draw(cx);
                assert_eq!(app.read(cx).active_data().unwrap().conv_scroll.offset(), expected);
            });
            draw_settled(&mut vcx);
            cx.update(|cx| assert_eq!(app.read(cx).active_data().unwrap().conv_scroll.offset(), expected));
        }
    }
}

#[gpui::test]
fn reference_images_render_inline_and_block_and_open_the_viewer(cx: &mut gpui::TestAppContext) {
    let requests = image_client(cx);
    for (body, selector, linked) in [
        ("[![asset][ref]][dest]", "conversation-0-image-1", true),
        ("Before ![asset][] after", "conversation-0-image-7", false),
        ("Before ![asset] after", "conversation-0-image-7", false),
    ] {
        let source = format!("{body}\n\n[ref]: https://example.com/screen\n[asset]: https://example.com/screen\n[dest]: https://example.com/original");
        let (window, app) = viewer_conversation(cx, &source);
        let mut vcx = gpui::VisualTestContext::from_window(window, cx);
        draw_settled(&mut vcx);
        click_selector(&mut vcx, selector);
        cx.update(|cx| {
            let viewer = app.read(cx).image_viewer.as_ref().unwrap();
            assert_eq!(viewer.target.url.as_ref(), "https://example.com/screen");
            assert_eq!(viewer.target.alt.as_ref(), "asset");
            assert_eq!(viewer.target.link.as_deref(), linked.then_some("https://example.com/original"));
        });
        click_selector(&mut vcx, "viewer-close");
    }
    assert!(requests.lock().unwrap().iter().any(|url| url.ends_with("/screen")));
}

#[gpui::test]
fn stock_conversation_markdown_loads_html_linked_and_extensionless_images(cx: &mut gpui::TestAppContext) {
    let requests = image_client(cx);
    let (window, app) = queue_window(cx, Arc::new(FakePr::default()));
    let mut vcx = gpui::VisualTestContext::from_window(window, cx);
    vcx.simulate_resize(size(px(1400.), px(1800.)));
    let mut item = loop_pr_item(Vec::new());
    item.pane = ItemPane::Conversation;
    let ItemState::Ready(data) = &mut item.state else { unreachable!() };
    data.pr_meta.as_mut().unwrap().body = "- first\n  - nested\n\n<img src='https://github.com/user-attachments/assets/html-screen'>\n\n[![diagram](https://github.com/user-attachments/assets/diagram)](https://example.com/full)\n\n| Before | After |\n| --- | --- |\n| a | b |\n\n![reference][ref]\n\n[ref]: https://example.com/reference-only".into();
    let mut discussion = conversation::tests::sample_discussion();
    discussion.threads.clear();
    discussion.comments.truncate(1);
    discussion.comments[0].body = "![comment](https://github.com/user-attachments/assets/comment-screen)".into();
    data.discussion = Some(discussion);
    install_item(&app, item, cx);
    draw_settled(&mut vcx);
    let requests = requests.lock().unwrap();
    for name in ["html-screen", "diagram", "comment-screen", "reference-only"] {
        assert!(requests.iter().any(|url| url.ends_with(name)), "stock image loader requested {name}");
    }
}

#[gpui::test]
fn mermaid_inline_comments_resize_async_in_unified_split_and_partial_virtualization(cx: &mut gpui::TestAppContext) {
    let requests = image_client(cx);
    let (window, app) = queue_window(cx, Arc::new(FakePr::default()));
    let mut vcx = gpui::VisualTestContext::from_window(window, cx);
    vcx.simulate_resize(size(px(1500.), px(1800.)));
    // Wider than a split pane: exercise real downscaling, not just a small
    // intrinsic diagram that fits unchanged in both modes.
    let source = "flowchart TD\nA[Window<br/>backgrounded] -. pause timer .-> B[Timer paused]\nB -. regain focus .-> C[Focus refresh]\nD[Window<br/>frontmost] -->|timer elapsed| E[Background poll<br/>every 180 s]\nE -->|run queue search| F((GitHub<br/>GraphQL))\nF -->|new PRs| G[Merge into<br/>queue view]\nC --> G\nF -->|no change| H[Keep existing<br/>queue]\nG --> D\nH --> D";
    let fenced = format!("```mermaid\n{source}\n```");
    let mut item = loop_pr_item(vec![
        rc(11, "a.rs", Some("RIGHT"), Some(1), &fenced, "alice", "2026-01-01T00:00:00Z", None),
        rc(13, "a.rs", Some("LEFT"), Some(1), &fenced, "bob", "2026-01-01T00:00:00Z", None),
        rc(15, "a.rs", Some("RIGHT"), Some(1), "```mermaid\nflowchart LR\nA[unclosed\n```", "carol", "2026-01-01T00:00:00Z", None),
    ]);
    let ItemState::Ready(data) = &mut item.state else { unreachable!() };
    data.diff = diff_core::parse_patch("diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1,2 +1,2 @@\n-old\n+new\n tail\n");
    data.set_rows(data.row_build());
    install_item(&app, item, cx);
    for mode in [ViewMode::Unified, ViewMode::Split] {
        cx.update(|cx| app.update(cx, |app, cx| {
            let data = app.active_data_mut().unwrap();
            data.mode = mode;
            data.rebuild_rows_anchored();
            data.scroll.0.borrow_mut().deferred_scroll_to_item = None;
            data.scroll.0.borrow().base_handle.set_offset(point(px(0.), px(0.)));
            cx.notify();
        }));
        draw_settled(&mut vcx);
        for (card, content, diagram) in [
            ("inline-text-0-11-0-card", "inline-text-0-11-0-content", "inline-text-0-11-0-mermaid-0-diagram"),
            ("inline-text-0-13-0-card", "inline-text-0-13-0-content", "inline-text-0-13-0-mermaid-0-diagram"),
        ] {
            let card = vcx.debug_bounds(card).unwrap();
            let content = vcx.debug_bounds(content).unwrap();
            let diagram = vcx.debug_bounds(diagram).unwrap();
            assert!(diagram.size.height > px(100.));
            assert!(diagram.size.width > px(50.));
            assert!(diagram.size.width <= content.size.width);
            assert!(content.bottom() <= card.bottom(), "async diagram must fit reserved rows");
        }
        assert!(vcx.debug_bounds("inline-text-0-15-0-mermaid-0-diagram").is_none(), "invalid source must not produce a diagram");
        let before = vcx.debug_bounds("inline-text-0-11-0-content").unwrap().size.height;
        let diagram = vcx.debug_bounds("inline-text-0-11-0-mermaid-0-diagram").unwrap();
        vcx.simulate_mouse_move(diagram.center(), None, Default::default());
        draw_settled(&mut vcx);
        assert_eq!(vcx.debug_bounds("inline-text-0-11-0-content").unwrap().size.height, before, "hover controls must not consume layout space");
        let toolbar = vcx.debug_bounds("inline-text-0-11-0-mermaid-0-toolbar").unwrap();
        assert!((toolbar.right() - (diagram.right() - px(4.))).abs() <= px(1.), "toolbar floats at diagram's right edge");
        assert!((toolbar.top() - (diagram.top() + px(4.))).abs() <= px(1.), "toolbar floats at diagram's top edge");
        let copy = vcx.debug_bounds("inline-text-0-11-0-mermaid-0-copy").unwrap();
        vcx.simulate_click(copy.center(), Default::default());
        cx.update(|cx| assert_eq!(cx.read_from_clipboard().unwrap().text().unwrap(), source));
        let copy_markdown = vcx.debug_bounds("inline-text-0-11-0-mermaid-0-copy-markdown").unwrap();
        vcx.simulate_click(copy_markdown.center(), Default::default());
        cx.update(|cx| assert_eq!(cx.read_from_clipboard().unwrap().text().unwrap(), fenced));
        let toggle = vcx.debug_bounds("inline-text-0-11-0-mermaid-0-source-toggle").unwrap();
        vcx.simulate_click(toggle.center(), Default::default());
        draw_settled(&mut vcx);
        let expanded = vcx.debug_bounds("inline-text-0-11-0-content").unwrap();
        assert_ne!(expanded.size.height, before, "source replacement updates reservation");
        assert!(expanded.bottom() <= vcx.debug_bounds("inline-text-0-11-0-card").unwrap().bottom());
        assert!(vcx.debug_bounds("inline-text-0-11-0-mermaid-0-diagram").is_none(), "source replaces rather than accompanies the diagram");
        assert!(vcx.debug_bounds("inline-text-0-11-0-mermaid-0-source-view").is_some());
        vcx.simulate_resize(size(px(950.), px(1800.)));
        draw_settled(&mut vcx);
        let source_view = vcx.debug_bounds("inline-text-0-11-0-mermaid-0-source-view").unwrap();
        let content = vcx.debug_bounds("inline-text-0-11-0-content").unwrap();
        assert!(source_view.right() <= content.right());
        assert!(content.bottom() <= vcx.debug_bounds("inline-text-0-11-0-card").unwrap().bottom(), "source replacement must fit reserved rows after resize");
        vcx.simulate_resize(size(px(1500.), px(1800.)));
        draw_settled(&mut vcx);
        let toggle = vcx.debug_bounds("inline-text-0-11-0-mermaid-0-source-toggle").unwrap();
        vcx.simulate_click(toggle.center(), Default::default());
        draw_settled(&mut vcx);
        assert!(vcx.debug_bounds("inline-text-0-11-0-mermaid-0-source-view").is_none());
        assert_eq!(vcx.debug_bounds("inline-text-0-11-0-content").unwrap().size.height, before, "Diagram toggle restores original geometry");
        let original = vcx.debug_bounds("inline-text-0-11-0-mermaid-0-diagram").unwrap();
        vcx.simulate_resize(size(px(950.), px(1800.)));
        draw_settled(&mut vcx);
        let narrow = vcx.debug_bounds("inline-text-0-11-0-mermaid-0-diagram").unwrap();
        let content = vcx.debug_bounds("inline-text-0-11-0-content").unwrap();
        assert!(narrow.right() <= content.right());
        if mode == ViewMode::Split {
            assert!(narrow.size.width < original.size.width - px(1.), "fixture must actually scale down in a narrower split pane");
        }
        let original_ratio = f32::from(original.size.width) / f32::from(original.size.height);
        assert!((content.size.height - narrow.size.height).abs() <= px(1.), "scaled diagram must not leave intrinsic-height whitespace in Markdown content");
        let card = vcx.debug_bounds("inline-text-0-11-0-card").unwrap();
        assert!(card.bottom() - content.bottom() < px(row_height()), "reserved rows may add rounding, not a large blank gap");
        let narrow_ratio = f32::from(narrow.size.width) / f32::from(narrow.size.height);
        assert!((original_ratio - narrow_ratio).abs() < 0.02, "fit must preserve diagram aspect ratio");
        vcx.simulate_resize(size(px(1500.), px(1800.)));
        draw_settled(&mut vcx);
        let first = cx.update(|cx| app.read(cx).active_data().unwrap().rows.iter().position(|row| matches!(row,
            Row::CommentBody { text, slot: 0, .. } if text.comment_id == 11)).unwrap());
        vcx.simulate_resize(size(px(1500.), px(200.)));
        cx.update(|cx| app.update(cx, |app, cx| {
            app.active_data_mut().unwrap().scroll.0.borrow().base_handle
                .set_offset(point(px(0.), -px((first + 1) as f32 * row_height())));
            cx.notify();
        }));
        draw_settled(&mut vcx);
        assert!(vcx.debug_bounds("inline-text-0-11-0-content").is_some());
        vcx.simulate_resize(size(px(1500.), px(1800.)));
    }
    let requests = requests.lock().unwrap();
    assert!(requests.iter().all(|url| url.starts_with("https://avatars.githubusercontent.com/")), "Mermaid must not use HTTP: {requests:?}");
}


#[gpui::test]
fn mermaid_persistent_description_ignores_old_results_after_edit_and_pr_switch(cx: &mut gpui::TestAppContext) {
    let (window, app) = queue_window(cx, Arc::new(FakePr::default()));
    let mut item = loop_pr_item(Vec::new());
    item.pane = ItemPane::Conversation;
    let ItemState::Ready(data) = &mut item.state else { unreachable!() };
    data.pr_meta.as_mut().unwrap().body = "```mermaid\nflowchart LR\nA[Original]-->B\n```".into();
    install_item(&app, item, cx);
    let mut vcx = gpui::VisualTestContext::from_window(window, cx);
    // Mount, but deliberately don't settle worker/parser completions.
    vcx.update(|window, cx| { window.draw(cx).clear(cx); });
    cx.update(|cx| app.update(cx, |app, cx| {
        app.active_data_mut().unwrap().pr_meta.as_mut().unwrap().body = "```mermaid\nflowchart LR\nA[broken\n```".into();
        let mut second = loop_pr_item(Vec::new());
        second.id = 1;
        second.pane = ItemPane::Conversation;
        let ItemState::Ready(data) = &mut second.state else { unreachable!() };
        data.pr_meta.as_mut().unwrap().body = "```mermaid\nflowchart TD\nC[Other PR]-->D\n```".into();
        app.items.push(second);
        app.active = 1;
        cx.notify();
    }));
    draw_settled(&mut vcx);
    assert!(vcx.debug_bounds("conversation-1-mermaid-0-diagram").is_some());
    assert!(vcx.debug_bounds("conversation-0-mermaid-0-diagram").is_none());
    cx.update(|cx| app.update(cx, |app, cx| { app.active = 0; cx.notify(); }));
    draw_settled(&mut vcx);
    assert!(vcx.debug_bounds("conversation-0-mermaid-0").is_some());
    assert!(vcx.debug_bounds("conversation-0-mermaid-0-diagram").is_none(), "original source's completion cannot replace an edit");
    cx.update(|cx| app.update(cx, |app, cx| {
        app.active_data_mut().unwrap().pr_meta.as_mut().unwrap().body = "```mermaid\nsequenceDiagram\nAlice->>Bob: Edited\n```".into();
        cx.notify();
    }));
    draw_settled(&mut vcx);
    assert!(vcx.debug_bounds("conversation-0-mermaid-0-diagram").is_some());
}
