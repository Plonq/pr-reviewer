use super::*;

#[gpui::test]
fn update_restart_restores_reviews_editors_and_view_once(cx: &mut gpui::TestAppContext) {
    let fake = Arc::new(FakePr::default());
    let store = local_store::LocalStore::in_memory();
    let (window, app) = queue_window_with_store(cx, fake.clone(), store.clone());
    install_item(&app, commenting_item(&fake), cx);
    let mut vcx = gpui::VisualTestContext::from_window(window, cx);
    vcx.update(|window, cx| {
        app.update(cx, |app, cx| {
            app.items[0].pane = ItemPane::Changes;
            app.show_queue = false;
            app.minimap_visible = false;
            app.open_composer(None, "a.rs".into(), CommentSide::Right, 1, 0, window, cx);
            app.composer().unwrap().input.update(cx, |input, cx| {
                input.set_value("unfinished inline comment", window, cx);
            });
            app.open_review(window, cx);
            app.review.as_ref().unwrap().input.update(cx, |input, cx| {
                input.set_value("unfinished summary", window, cx);
            });
            app.close_review(window, cx);
            app.updater.state = 1;
            assert!(!app.update_blocked(cx));
            app.install_update(cx);
            assert_eq!(app.updater.state, 2);
            assert!(update_session::read(&store).is_some());
            // Don't let this test window's release overwrite the consumed
            // snapshot when it is eventually destroyed.
            app.updater.state = 0;
        });
    });
    let (_, restored) = queue_window_with_store(cx, fake.clone(), store.clone());
    cx.update(|cx| {
        let app = restored.read(cx);
        assert_eq!(app.items.len(), 1);
        assert_eq!(app.active, 0);
        assert!(!app.show_queue);
        assert!(!app.minimap_visible);
        assert_eq!(app.items[0].pane, ItemPane::Changes);
        assert_eq!(app.composers.len(), 1);
        let composer = &app.composers[0];
        assert_eq!(
            composer.input.read(cx).value().as_str(),
            "unfinished inline comment"
        );
        assert_eq!(composer.path, "a.rs");
        assert_eq!(composer.line, 1);
        assert!(!composer.in_flight);
        assert_eq!(
            app.items[0]
                .review_draft
                .as_ref()
                .unwrap()
                .input
                .read(cx)
                .value()
                .as_str(),
            "unfinished summary"
        );
        assert!(update_session::read(&store).is_none());
    });
    let (_, fresh) = queue_window_with_store(cx, fake, store);
    cx.update(|cx| assert!(fresh.read(cx).items.is_empty()));
}

#[gpui::test]
fn update_restart_is_blocked_by_writes_and_store_failure(cx: &mut gpui::TestAppContext) {
    let (window, app) = queue_window(cx, Arc::new(FakePr::default()));
    let mut vcx = gpui::VisualTestContext::from_window(window, cx);
    vcx.update(|_, cx| {
        app.update(cx, |app, cx| {
            app.updater.state = 1;
            app.sessions.insert(
                42,
                SessionUi {
                    busy: true,
                    ..Default::default()
                },
            );
            assert!(app.update_blocked(cx));
            app.install_update(cx);
            assert_eq!(app.updater.state, 1);
            assert!(update_session::read(&app.store).is_none());
            app.sessions.clear();
            app.store_error = Some("disk unavailable".into());
            app.install_update(cx);
            assert_eq!(app.updater.state, 1);
            assert!(update_session::read(&app.store).is_none());
            app.updater.state = 0;
        })
    });
}

#[test]
fn malformed_restart_snapshot_does_not_crash_launch() {
    let store = local_store::LocalStore::in_memory();
    store.set_setting("update_restart_v1", serde_json::json!({"items": "broken"}));
    assert!(update_session::read(&store).is_none());
}

#[gpui::test]
fn update_button_is_quiet_until_ready_and_restarts_through_click(cx: &mut gpui::TestAppContext) {
    let (window, app) = queue_window(cx, Arc::new(FakePr::default()));
    let mut vcx = gpui::VisualTestContext::from_window(window, cx);
    draw_window(&mut vcx);
    assert!(vcx.debug_bounds("update-button").is_none());
    vcx.update(|_, cx| {
        app.update(cx, |app, cx| {
            app.updater.state = 1;
            cx.notify();
        })
    });
    draw_window(&mut vcx);
    let button = vcx
        .debug_bounds("update-button")
        .expect("quiet header button");
    assert!(button.size.height <= APP_HEADER_HEIGHT);
    vcx.simulate_click(button.center(), gpui::Modifiers::default());
    vcx.update(|_, cx| {
        assert_eq!(app.read(cx).updater.state, 2);
        assert!(update_session::read(&app.read(cx).store).is_some());
    });
}

#[gpui::test]
fn update_restart_keeps_incremental_baseline_and_position(cx: &mut gpui::TestAppContext) {
    let fake = since_review_fake();
    let store = local_store::LocalStore::in_memory();
    let (window, app) = queue_window_with_store(cx, fake.clone(), store.clone());
    let mut vcx = gpui::VisualTestContext::from_window(window, cx);
    vcx.update(|_, cx| app.update(cx, |app, cx| app.open_item(Source::Pr(loop_loc()), cx)));
    vcx.run_until_parked();
    vcx.update(|window, cx| app.update(cx, |app, cx| app.switch_review_scope(true, window, cx)));
    vcx.run_until_parked();
    let prior = vcx.update(|_, cx| {
        app.update(cx, |app, cx| {
            let data = app.active_data().unwrap();
            assert!(data.review_base.is_some());
            let prior = (data.review_base.clone(), data.review_position());
            app.save_update_session(cx).unwrap();
            prior
        })
    });
    let (_, restored) = queue_window_with_store(cx, fake, store);
    cx.update(|cx| {
        let data = restored.read(cx).active_data().unwrap();
        assert_eq!(data.review_base, prior.0);
        assert_eq!(data.review_position(), prior.1);
    });
}

#[gpui::test]
fn staged_update_saves_restart_session_on_normal_window_close(cx: &mut gpui::TestAppContext) {
    let store = local_store::LocalStore::in_memory();
    let (window, app) = queue_window_with_store(cx, Arc::new(FakePr::default()), store.clone());
    let mut vcx = gpui::VisualTestContext::from_window(window, cx);
    vcx.update(|window, cx| {
        app.update(cx, |app, _| {
            app.updater.state = 1;
            app.minimap_visible = false;
        });
        window.remove_window();
    });
    drop(vcx);
    drop(app);
    cx.update(|_| {});
    cx.run_until_parked();
    assert!(update_session::read(&store).is_some());
    let (_, app) = queue_window_with_store(cx, Arc::new(FakePr::default()), store);
    cx.update(|cx| assert!(!app.read(cx).minimap_visible));
}
