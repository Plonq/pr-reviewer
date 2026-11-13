use super::*;

#[test]
fn update_menu_contains_the_manual_action_and_disables_it_without_sparkle() {
    for enabled in [true, false] {
        let menu = updater::app_menu(enabled);
        assert_eq!(menu.name.as_ref(), "Gander");
        let gpui::MenuItem::Action {
            name,
            action,
            disabled,
            ..
        } = &menu.items[0]
        else {
            panic!("expected the check-for-updates menu action");
        };
        assert_eq!(name.as_ref(), "Check for Updates…");
        assert!(action.as_any().is::<updater::CheckForUpdates>());
        assert_eq!(*disabled, !enabled);
        assert!(
            matches!(&menu.items[2], gpui::MenuItem::Action { action, .. } if action.as_any().is::<Quit>())
        );
    }
}

#[gpui::test]
fn manual_update_action_works_with_modal_focus_and_keeps_drafts(cx: &mut gpui::TestAppContext) {
    let (window, app) = queue_window(cx, Arc::new(FakePr::default()));
    let mut vcx = gpui::VisualTestContext::from_window(window, cx);
    vcx.update(|window, cx| {
        updater::init_menu(&app, cx);
        app.update(cx, |app, cx| {
            app.open_quick_open(window, cx);
            app.open_input
                .update(cx, |input, cx| input.set_value("unfinished", window, cx));
        });
    });
    draw_window(&mut vcx);
    assert!(vcx.debug_bounds("update-status").is_none());
    vcx.update(|_, cx| cx.dispatch_action(&updater::CheckForUpdates));
    draw_window(&mut vcx);
    vcx.update(|window, cx| {
        let app = app.read(cx);
        assert_eq!(app.updater.state, 4);
        assert!(app.quick_open_visible);
        assert!(app.open_input.focus_handle(cx).is_focused(window));
        assert_eq!(app.open_input.read(cx).value().as_str(), "unfinished");
        assert!(
            session::read(&app.store).is_none(),
            "checking must not save or restart the session"
        );
    });
    assert!(vcx.debug_bounds("update-status").is_some());
    assert!(vcx.debug_bounds("update-button").is_none());

    // No-update status is transient, and automatic checks stay completely quiet.
    for state in [5, 0] {
        vcx.update(|_, cx| app.update(cx, |app, cx| app.set_update_status(state, None, cx)));
        draw_window(&mut vcx);
        assert_eq!(vcx.debug_bounds("update-status").is_some(), state == 5);
        assert!(vcx.debug_bounds("update-button").is_none());
    }
}

#[gpui::test]
fn repeated_manual_checks_never_install_or_restart_a_staged_update(cx: &mut gpui::TestAppContext) {
    let (window, app) = queue_window(cx, Arc::new(FakePr::default()));
    let mut vcx = gpui::VisualTestContext::from_window(window, cx);
    vcx.update(|_, cx| {
        updater::init_menu(&app, cx);
    });
    for state in [1, 2, 4, 6] {
        vcx.update(|_, cx| {
            app.update(cx, |app, cx| app.set_update_status(state, None, cx));
            cx.dispatch_action(&updater::CheckForUpdates);
            cx.dispatch_action(&updater::CheckForUpdates);
        });
        draw_window(&mut vcx);
        vcx.update(|_, cx| {
            assert_eq!(app.read(cx).updater.state, state);
            assert!(session::read(&app.read(cx).store).is_none());
        });
    }
}

#[gpui::test]
fn update_error_button_retries_via_the_manual_check_path(cx: &mut gpui::TestAppContext) {
    let (window, app) = queue_window(cx, Arc::new(FakePr::default()));
    let mut vcx = gpui::VisualTestContext::from_window(window, cx);
    vcx.update(|_, cx| {
        app.update(cx, |app, cx| {
            app.set_update_status(3, Some("couldn't download the update feed".into()), cx);
        })
    });
    draw_window(&mut vcx);
    let button = vcx.debug_bounds("update-button").unwrap();
    vcx.simulate_click(button.center(), gpui::Modifiers::default());
    draw_window(&mut vcx);
    vcx.update(|_, cx| {
        assert_eq!(app.read(cx).updater.state, 4);
        assert!(app.read(cx).updater.error.is_none());
    });
    assert!(vcx.debug_bounds("update-button").is_none());
    assert!(vcx.debug_bounds("update-status").is_some());
}
