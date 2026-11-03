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
}
