use super::*;

fn image_client(cx: &mut gpui::TestAppContext) -> Arc<std::sync::Mutex<Vec<String>>> {
    let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen = requests.clone();
    let mut png = std::io::Cursor::new(Vec::new());
    image::DynamicImage::new_rgb8(240, 160).write_to(&mut png, image::ImageFormat::Png).unwrap();
    let png = png.into_inner();
    cx.update(|cx| cx.set_http_client(http_client::FakeHttpClient::create(move |req| {
        let url = req.uri().to_string();
        seen.lock().unwrap().push(url.clone());
        let bytes = if url.ends_with("diagram") {
            br#"<svg xmlns="http://www.w3.org/2000/svg" width="200" height="80"><rect width="200" height="80"/></svg>"#.to_vec()
        } else { png.clone() };
        async move { Ok(http_client::http::Response::builder().status(200)
            // Deliberately uninformative MIME and extension: decoding is byte-based.
            .header("content-type", "application/octet-stream")
            .body(http_client::AsyncBody::from(bytes)).unwrap()) }
    })));
    requests
}

fn draw_settled(cx: &mut gpui::VisualTestContext) {
    for _ in 0..8 {
        cx.update(|window, cx| { window.draw(cx).clear(cx); });
        cx.run_until_parked();
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
fn stock_conversation_markdown_loads_html_linked_and_extensionless_images(cx: &mut gpui::TestAppContext) {
    let requests = image_client(cx);
    let (window, app) = queue_window(cx, Arc::new(FakePr::default()));
    let mut vcx = gpui::VisualTestContext::from_window(window, cx);
    vcx.simulate_resize(size(px(1400.), px(1800.)));
    let mut item = loop_pr_item(Vec::new());
    item.pane = ItemPane::Conversation;
    let ItemState::Ready(data) = &mut item.state else { unreachable!() };
    data.pr_meta.as_mut().unwrap().body = "- first\n  - nested\n\n<img src='https://github.com/user-attachments/assets/html-screen'>\n\n[![diagram](https://github.com/user-attachments/assets/diagram)](https://example.com/full)\n\n| Before | After |\n| --- | --- |\n| a | b |\n\n![omitted][ref]\n\n[ref]: https://example.com/reference-only".into();
    let mut discussion = conversation::tests::sample_discussion();
    discussion.threads.clear();
    discussion.comments.truncate(1);
    discussion.comments[0].body = "![comment](https://github.com/user-attachments/assets/comment-screen)".into();
    data.discussion = Some(discussion);
    install_item(&app, item, cx);
    draw_settled(&mut vcx);
    let requests = requests.lock().unwrap();
    for name in ["html-screen", "diagram", "comment-screen"] {
        assert!(requests.iter().any(|url| url.ends_with(name)), "stock image loader requested {name}");
    }
    assert!(!requests.iter().any(|url| url.ends_with("reference-only")), "reference images are intentionally omitted by stock Markdown");
}
