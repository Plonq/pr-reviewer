// Included beside the Markdown media tests to share the real-window helpers.
#[gpui::test]
fn diff_images_are_side_by_side_in_both_modes_and_other_media_only_link(cx: &mut gpui::TestAppContext) {
    let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen = requests.clone();
    let mut png = std::io::Cursor::new(Vec::new());
    image::DynamicImage::new_rgb8(120, 80).write_to(&mut png, image::ImageFormat::Png).unwrap();
    let png = png.into_inner();
    cx.update(|cx| cx.set_http_client(http_client::FakeHttpClient::create(move |req| {
        let url = req.uri().to_string();
        seen.lock().unwrap().push(url.clone());
        let bytes = if url.ends_with(".svg") {
            br#"<svg xmlns="http://www.w3.org/2000/svg" width="200" height="80"><rect width="200" height="80" fill="red"/></svg>"#.to_vec()
        } else { png.clone() };
        async move { Ok(http_client::http::Response::builder().status(200)
            .body(http_client::AsyncBody::from(bytes)).unwrap()) }
    })));
    let (window, app) = queue_window(cx, Arc::new(FakePr::default()));
    let mut item = loop_pr_item(Vec::new());
    item.pane = ItemPane::Changes;
    let ItemState::Ready(data) = &mut item.state else { unreachable!() };
    data.diff = diff_core::parse_patch(concat!(
        "diff --git a/before.svg b/after.png\nsimilarity index 100%\nrename from before.svg\nrename to after.png\n",
        "diff --git a/movie.mp4 b/movie.mp4\nBinary files a/movie.mp4 and b/movie.mp4 differ\n",
        "diff --git a/added.png b/added.png\nnew file mode 100644\nBinary files /dev/null and b/added.png differ\n",
        "diff --git a/deleted.svg b/deleted.svg\ndeleted file mode 100644\n--- a/deleted.svg\n+++ /dev/null\n@@ -1 +0,0 @@\n-<svg/>\n",
    ));
    data.file_ix = 0;
    data.file_media = file_media::prepare(&data.diff, &item.source,
        &LoadedMeta::Pr(data.pr_meta.clone().unwrap()), Some("base-commit"), None);
    data.rebuild_tree();
    data.set_rows(data.row_build());
    install_item(&app, item, cx);
    let mut vcx = gpui::VisualTestContext::from_window(window, cx);
    vcx.simulate_resize(size(px(1200.), px(800.)));
    for mode in [ViewMode::Unified, ViewMode::Split] {
        cx.update(|cx| app.update(cx, |app, cx| {
            let data = app.active_data_mut().unwrap();
            data.file_ix = 0;
            data.mode = mode;
            data.set_rows(data.row_build());
            cx.notify();
        }));
        draw_settled(&mut vcx);
        let left = vcx.debug_bounds("file-media-Before").unwrap();
        let right = vcx.debug_bounds("file-media-After").unwrap();
        assert!(left.size.width > px(100.) && left.size.height > px(100.));
        assert!(left.right() <= right.left());
        assert_eq!(left.top(), right.top());
        assert!((f32::from(left.size.width - right.size.width)).abs() < 1.);
        assert!(left.bottom() <= px(800.) && right.right() <= px(1200.));
        for selector in ["file-media-Before-error", "file-media-After-error", "file-media-Before-loading", "file-media-After-loading"] {
            assert!(vcx.debug_bounds(selector).is_none());
        }
        // Verify successful decoding, not merely reserved image rectangles.
        vcx.update(|window, cx| {
            let uri = gpui::Resource::Uri("https://raw.githubusercontent.com/acme/widgets/base-commit/before.svg".into());
            assert!(matches!(window.get_asset::<gpui::ImgResourceLoader>(&uri, cx), Some(Ok(_))));
        });
    }
    click_selector(&mut vcx, "file-media-Before-link");
    assert!(cx.opened_url().unwrap().ends_with("/base-commit/before.svg"));
    let before_video = requests.lock().unwrap().len();
    for (ix, old_link, new_link) in [(1, true, true), (2, false, true), (3, true, false)] {
        cx.update(|cx| app.update(cx, |app, cx| {
            let data = app.active_data_mut().unwrap();
            data.file_ix = ix;
            data.set_rows(data.row_build());
            cx.notify();
        }));
        draw_settled(&mut vcx);
        assert_eq!(vcx.debug_bounds("file-media-Before-link").is_some(), old_link);
        assert_eq!(vcx.debug_bounds("file-media-After-link").is_some(), new_link);
        if ix == 1 {
            assert_eq!(requests.lock().unwrap().len(), before_video, "video must not enter the image loader");
        }
    }
}
