// Included beside the other real-window media regressions.
#[gpui::test]
fn local_svg_uses_display_resolution_and_invalid_svg_keeps_the_file_link(cx: &mut gpui::TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("small.svg"),
        br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 12 6"><path d="M0 0h12v6H0z" fill="blue"/></svg>"#).unwrap();
    std::fs::write(dir.path().join("bad.svg"), b"not an SVG").unwrap();
    let local = git::LocalSource {
        repo_root: dir.path().to_owned(), branch: "main".into(), base_ref: None,
        base_label: "HEAD".into(), base_oid: None,
    };
    let (window, app) = queue_window(cx, Arc::new(FakePr::default()));
    let mut item = loop_pr_item(Vec::new());
    item.pane = ItemPane::Changes;
    item.source = Source::Local(local.clone());
    let ItemState::Ready(data) = &mut item.state else { unreachable!() };
    data.pr_meta = None;
    data.diff = diff_core::parse_patch(concat!(
        "diff --git a/small.svg b/small.svg\nnew file mode 100644\n--- /dev/null\n+++ b/small.svg\n@@ -0,0 +1 @@\n+svg\n",
        "diff --git a/bad.svg b/bad.svg\nnew file mode 100644\n--- /dev/null\n+++ b/bad.svg\n@@ -0,0 +1 @@\n+bad\n",
    ));
    data.file_media = file_media::prepare(&data.diff, &item.source, &LoadedMeta::Local(local), None, None);
    data.file_ix = 0;
    data.rebuild_tree();
    data.set_rows(data.row_build());
    install_item(&app, item, cx);
    let mut vcx = gpui::VisualTestContext::from_window(window, cx);
    vcx.simulate_resize(size(px(1400.), px(800.)));
    draw_settled(&mut vcx);
    let viewport = vcx.debug_bounds("file-media-After-viewport").unwrap();
    vcx.update(|window, cx| {
        let resource = gpui::Resource::from(dir.path().join("small.svg"));
        let raster = svg_image::cached_raster(&resource, window, cx).unwrap();
        let fitted_width = f32::from(viewport.size.width).min(f32::from(viewport.size.height) * 2.);
        assert_eq!(raster.size(0).width.0, (fitted_width * window.scale_factor()).ceil() as i32);
        assert!(raster.size(0).width.0 > 24);
    });
    cx.update(|cx| app.update(cx, |app, cx| {
        let data = app.active_data_mut().unwrap();
        data.file_ix = 1;
        data.set_rows(data.row_build());
        cx.notify();
    }));
    draw_settled(&mut vcx);
    assert!(vcx.debug_bounds("file-media-After-error").is_some());
    assert!(vcx.debug_bounds("file-media-After-link").is_some());
    click_selector(&mut vcx, "file-media-After-link");
    assert_eq!(cx.opened_url().unwrap(), http_client::Url::from_file_path(dir.path().join("bad.svg")).unwrap().as_str());
    // Even a broken local SVG can be inspected as source.
    click_selector(&mut vcx, "svg-code");
    draw_settled(&mut vcx);
    assert!(vcx.debug_bounds("code-diff-pane").is_some());
    assert!(vcx.debug_bounds("file-media-After-error").is_none());
}


#[gpui::test]
fn svg_toggle_uses_full_code_diff_in_both_modes_and_remembers_each_file(cx: &mut gpui::TestAppContext) {
    const OLD: &str = "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"20\" height=\"8\">\n<rect width=\"20\" height=\"8\" fill=\"red\"/>\n<!-- untouched context -->\n</svg>\n";
    let new = OLD.replace("red", "blue");
    cx.update(|cx| cx.set_http_client(http_client::FakeHttpClient::create(|_| async {
        Ok(http_client::http::Response::builder().status(200)
            .body(http_client::AsyncBody::from(OLD)).unwrap())
    })));
    let fake = Arc::new(FakePr {
        pr_merge_base: Some("svg-base".into()),
        patch: Some(concat!(
            "diff --git a/changed.svg b/changed.svg\n--- a/changed.svg\n+++ b/changed.svg\n@@ -2 +2 @@\n-old\n+new\n",
            "diff --git a/added.svg b/added.svg\nnew file mode 100644\n--- /dev/null\n+++ b/added.svg\n@@ -0,0 +1 @@\n+new\n",
            "diff --git a/deleted.svg b/deleted.svg\ndeleted file mode 100644\n--- a/deleted.svg\n+++ /dev/null\n@@ -1 +0,0 @@\n-old\n",
            "diff --git a/before.svg b/renamed.SVG\nsimilarity index 100%\nrename from before.svg\nrename to renamed.SVG\n",
            "diff --git a/photo.png b/photo.png\nBinary files a/photo.png and b/photo.png differ\n",
        ).into()),
        file_contents: HashMap::from([
            (("svg-base".into(), "changed.svg".into()), OLD.into()),
            (("dead10cc".into(), "changed.svg".into()), new.clone()),
            (("dead10cc".into(), "added.svg".into()), new),
            (("svg-base".into(), "deleted.svg".into()), OLD.into()),
            (("svg-base".into(), "before.svg".into()), OLD.into()),
            (("dead10cc".into(), "renamed.SVG".into()), OLD.into()),
        ]),
        ..Default::default()
    });
    let (window, app) = queue_window(cx, fake.clone());
    let mut item = loop_pr_item(Vec::new());
    item.pane = ItemPane::Changes;
    let source = item.source.clone();
    let loaded = fetch_item(fake.clone(), &source, ViewMode::Unified, 0).unwrap();
    for ix in 0..4 {
        assert!(loaded.upgrades.contains_key(&ix), "SVG {ix} should have full-file code context");
    }
    assert!(!loaded.upgrades.contains_key(&4), "raster images must remain media-only");
    item.install(loaded);
    install_item(&app, item, cx);
    let mut vcx = gpui::VisualTestContext::from_window(window, cx);
    vcx.simulate_resize(size(px(1400.), px(800.)));
    for ix in 0..4 {
        cx.update(|cx| app.update(cx, |app, cx| {
            app.active_data_mut().unwrap().open_file(ix, None);
            cx.notify();
        }));
        draw_settled(&mut vcx);
        assert!(vcx.debug_bounds("file-media-Before").is_some(), "each SVG starts rendered");
        assert!(vcx.debug_bounds("svg-rendered").is_some());
        assert!(vcx.debug_bounds("svg-code").is_some());
        for mode in [ViewMode::Unified, ViewMode::Split] {
            cx.update(|cx| app.update(cx, |app, cx| {
                let data = app.active_data_mut().unwrap();
                data.mode = mode;
                data.rebuild_rows_anchored();
                cx.notify();
            }));
            click_selector(&mut vcx, "svg-code");
            draw_settled(&mut vcx);
            assert!(vcx.debug_bounds("file-media-Before").is_none());
            assert!(vcx.debug_bounds("code-diff-pane").is_some());
            vcx.update(|window, cx| {
                let app = app.read(cx);
                let data = app.active_data().unwrap();
                assert!(app.focus_handle.is_focused(window));
                assert!(!data.shows_file_media());
                assert!(!data.rows.iter().any(|row| matches!(row, Row::Binary)));
                assert!(data.rows.iter().any(|row| matches!(row,
                    Row::Line { text, .. } if text.contains("untouched context")) || matches!(row,
                    Row::SplitLine { left, right } if left.iter().chain(right).any(|cell| cell.text.contains("untouched context")))));
                assert!(data.mode == mode);
            });
            click_selector(&mut vcx, "svg-rendered");
            draw_settled(&mut vcx);
            assert!(vcx.debug_bounds("file-media-Before").is_some());
            assert!(vcx.debug_bounds("code-diff-pane").is_none());
        }
        // Leave this file in code mode, then visit a different file.
        click_selector(&mut vcx, "svg-code");
        draw_settled(&mut vcx);
    }
    cx.update(|cx| app.update(cx, |app, cx| {
        app.active_data_mut().unwrap().open_file(4, None);
        cx.notify();
    }));
    draw_settled(&mut vcx);
    assert!(vcx.debug_bounds("svg-code").is_none());
    assert!(vcx.debug_bounds("svg-rendered").is_none());
    assert!(vcx.debug_bounds("file-media-Before").is_some());
    cx.update(|cx| app.update(cx, |app, cx| {
        app.active_data_mut().unwrap().open_file(0, None);
        cx.notify();
    }));
    draw_settled(&mut vcx);
    assert!(vcx.debug_bounds("code-diff-pane").is_some(), "returning remembers code view");
    let loaded = fetch_item(fake, &source, ViewMode::Split, 0).unwrap();
    cx.update(|cx| app.update(cx, |app, cx| {
        app.items[app.active].install(loaded);
        cx.notify();
    }));
    draw_settled(&mut vcx);
    assert!(vcx.debug_bounds("code-diff-pane").is_some(), "refresh preserves the choice");
}

#[gpui::test]
fn svg_safety_covers_named_and_raster_disguised_local_files(cx: &mut gpui::TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let fixture = dir.path().join("harmless.png");
    image::RgbaImage::from_pixel(2, 2, image::Rgba([255, 0, 0, 255])).save(&fixture).unwrap();
    let unsafe_svg = format!(r#"<svg xmlns="http://www.w3.org/2000/svg" width="20" height="10"><image href="{}" width="20" height="10"/></svg>"#, fixture.display());
    for name in ["blocked.svg", "disguised.png"] {
        std::fs::write(dir.path().join(name), &unsafe_svg).unwrap();
    }
    let local = git::LocalSource {
        repo_root: dir.path().to_owned(), branch: "main".into(), base_ref: None,
        base_label: "HEAD".into(), base_oid: None,
    };
    let (window, app) = queue_window(cx, Arc::new(FakePr::default()));
    let mut item = loop_pr_item(Vec::new());
    item.pane = ItemPane::Changes;
    item.source = Source::Local(local.clone());
    let ItemState::Ready(data) = &mut item.state else { unreachable!() };
    data.pr_meta = None;
    data.diff = diff_core::parse_patch(concat!(
        "diff --git a/blocked.svg b/blocked.svg\nnew file mode 100644\n--- /dev/null\n+++ b/blocked.svg\n@@ -0,0 +1 @@\n+svg\n",
        "diff --git a/disguised.png b/disguised.png\nnew file mode 100644\nBinary files /dev/null and b/disguised.png differ\n",
        "diff --git a/harmless.png b/harmless.png\nnew file mode 100644\nBinary files /dev/null and b/harmless.png differ\n",
    ));
    data.file_media = file_media::prepare(&data.diff, &item.source, &LoadedMeta::Local(local), None, None);
    data.rebuild_tree();
    data.set_rows(data.row_build());
    install_item(&app, item, cx);
    let mut vcx = gpui::VisualTestContext::from_window(window, cx);
    for (ix, name) in ["blocked.svg", "disguised.png", "harmless.png"].into_iter().enumerate() {
        cx.update(|cx| app.update(cx, |app, cx| {
            let data = app.active_data_mut().unwrap();
            data.file_ix = ix;
            data.set_rows(data.row_build());
            cx.notify();
        }));
        draw_settled(&mut vcx);
        assert_eq!(vcx.debug_bounds("file-media-After-error").is_some(), ix < 2, "{name}");
        assert!(vcx.debug_bounds("file-media-After-link").is_some());
        vcx.update(|window, cx| {
            let resource = gpui::Resource::from(dir.path().join(name));
            assert!(!cx.has_asset::<gpui::ImgResourceLoader>(&resource), "default decoder used for {name}");
            if ix > 0 {
                let image = window.get_asset::<svg_image::ImageLoader>(&resource, cx).unwrap();
                assert_eq!(image.is_ok(), ix == 2, "{name}");
            }
        });
    }
}

#[gpui::test]
fn svg_safety_covers_markdown_stock_html_captions_measurement_and_viewer_retry(cx: &mut gpui::TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let fixture = dir.path().join("harmless.png");
    image::RgbaImage::from_pixel(2, 2, image::Rgba([255, 0, 0, 255])).save(&fixture).unwrap();
    let unsafe_svg = format!(r#"<svg xmlns="http://www.w3.org/2000/svg" width="20" height="10"><image href="{}" width="20" height="10"/></svg>"#, fixture.display());
    let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen = requests.clone();
    let payload = unsafe_svg.into_bytes();
    cx.update(|cx| cx.set_http_client(http_client::FakeHttpClient::create(move |req| {
        seen.lock().unwrap().push(req.uri().to_string());
        let bytes = payload.clone();
        async move { Ok(http_client::http::Response::builder().status(200)
            .header("content-type", "image/png")
            .body(http_client::AsyncBody::from(bytes)).unwrap()) }
    })));
    let body = concat!(
        "![block](https://example.com/block.png)\n\n",
        "before ![inline](https://example.com/inline.png) after\n\n",
        "![reference][ref]\n\n[ref]: https://example.com/reference.png\n\n",
        "<div><img src='https://example.com/nested.png'></div>\n\n",
        "<video src='https://example.com/video.mp4'><img src='https://example.com/video-fallback.png'></video>\n\n",
        "<img src='https://github.com/user-attachments/assets/caption.png'>\nCaption\n\n",
        // These stay with Kit's stock parser, whose image hook must fail closed.
        "<div><img src='file:///tmp/harmless.png'><img src='data:image/svg+xml;base64,PHN2Zy8+'></div>\n",
    );
    let (window, _) = viewer_conversation(cx, body);
    let mut vcx = gpui::VisualTestContext::from_window(window, cx);
    vcx.simulate_resize(size(px(1400.), px(2400.)));
    draw_settled(&mut vcx);
    let urls = [
        "https://example.com/block.png",
        "https://example.com/inline.png",
        "https://example.com/reference.png",
        "https://example.com/nested.png",
        "https://example.com/video-fallback.png",
        "https://github.com/user-attachments/assets/caption.png",
    ];
    vcx.update(|window, cx| {
        for url in urls {
            let resource = gpui::Resource::Uri(url.into());
            assert!(window.get_asset::<svg_image::ImageLoader>(&resource, cx).unwrap_or_else(|| panic!("not loaded: {url}")).is_err(), "{url}");
            assert!(!cx.has_asset::<gpui::ImgResourceLoader>(&resource), "default decoder used for {url}");
        }
        for url in ["file:///tmp/harmless.png", "data:image/svg+xml;base64,PHN2Zy8+"] {
            assert!(!cx.has_asset::<gpui::ImgResourceLoader>(&gpui::Resource::Uri(url.into())));
            let gpui::ImageSource::Custom(load) = svg_image::remote_source(&url.into()) else { panic!("expected restricted source") };
            assert!(load(window, cx).unwrap().is_err());
            assert!(matches!(svg_image::markdown_source(&url.into()), gpui::ImageSource::Custom(_)));
        }
    });
    assert!(requests.lock().unwrap().iter().all(|url| url.starts_with("https://")));
    click_selector(&mut vcx, "conversation-0-image-0");
    assert!(vcx.debug_bounds("viewer-error").is_some());
    let before_retry = requests.lock().unwrap().iter().filter(|url| url.ends_with("block.png")).count();
    click_selector(&mut vcx, "viewer-retry");
    assert!(vcx.debug_bounds("viewer-error").is_some());
    assert_eq!(requests.lock().unwrap().iter().filter(|url| url.ends_with("block.png")).count(), before_retry + 1);
    vcx.update(|_, cx| assert!(!cx.has_asset::<gpui::ImgResourceLoader>(&gpui::Resource::Uri(urls[0].into()))));
}
