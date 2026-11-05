use super::*;

fn filter_window(
    cx: &mut gpui::TestAppContext,
    paths: &[&str],
) -> (gpui::VisualTestContext, gpui::Entity<ReviewApp>) {
    let (window, app) = queue_window(cx, Arc::new(FakePr::default()));
    cx.update(|cx| {
        cx.bind_keys(tree::keybindings());
        cx.bind_keys([KeyBinding::new("/", FocusTreeFilter, Some("ReviewApp"))]);
    });
    let mut item = loop_pr_item(Vec::new());
    let mut diff = two_file_diff();
    diff.files = paths
        .iter()
        .map(|path| {
            let mut file = two_file_diff().files.remove(0);
            file.old_path = Some((*path).into());
            file.new_path = Some((*path).into());
            file
        })
        .collect();
    item.state = ItemState::Ready(file_mode_data(diff));
    install_item(&app, item, cx);
    cx.update(|cx| {
        app.update(cx, |app, _| {
            app.active_data_mut().unwrap().show_file(0, None)
        });
    });
    let mut vcx = gpui::VisualTestContext::from_window(window, cx);
    draw_window(&mut vcx);
    vcx.simulate_keystrokes("/");
    draw_window(&mut vcx);
    (vcx, app)
}

fn highlighted(app: &ReviewApp, cx: &gpui::App) -> Option<String> {
    app.active_data()?
        .tree_view
        .as_ref()?
        .state
        .read(cx)
        .selected_item()
        .map(|item| item.id.to_string())
}

#[gpui::test]
fn filter_arrows_move_existing_highlight_over_files_without_opening_them(
    cx: &mut gpui::TestAppContext,
) {
    let (mut vcx, app) = filter_window(
        cx,
        &[
            "main.rs",
            "src/main_test.rs",
            "src/main.rs",
            "tests/main.rs",
            "docs/guide.md",
        ],
    );
    vcx.simulate_input("main.rs");
    draw_window(&mut vcx);
    assert!(vcx.debug_bounds("file-filter-hint").is_some());
    // The first visible file wins, even though root main.rs has the best
    // fuzzy score. The diff stays on that root file while we choose.
    vcx.update(|window, cx| {
        let app = app.read(cx);
        assert_eq!(highlighted(app, cx).as_deref(), Some("file:src/main.rs"));
        assert_eq!(app.active_data().unwrap().file_ix, 0);
        assert!(app.tree_filter_input.focus_handle(cx).is_focused(window));
    });
    for (key, path) in [
        ("up", "src/main.rs"),
        ("down", "src/main_test.rs"),
        ("down", "tests/main.rs"), // Skip the tests directory heading.
        ("down", "main.rs"),
        ("down", "main.rs"), // Clamp at the end.
        ("up", "tests/main.rs"),
        ("up", "src/main_test.rs"), // Skip the heading in reverse too.
        ("down", "tests/main.rs"),
    ] {
        vcx.simulate_keystrokes(key);
        draw_window(&mut vcx);
        vcx.update(|window, cx| {
            let app = app.read(cx);
            assert_eq!(highlighted(app, cx), Some(format!("file:{path}")));
            let data = app.active_data().unwrap();
            assert_eq!(data.file_ix, 0, "arrows must not preview files");
            assert!(!data.file_history.can_go(NavigationDirection::Back));
            assert!(app.tree_filter_input.focus_handle(cx).is_focused(window));
        });
    }
    // Left/right still move the input caret, not the result highlight.
    vcx.simulate_keystrokes("left");
    vcx.update(|_, cx| {
        assert_eq!(
            app.read(cx).tree_filter_input.read(cx).cursor(),
            "main.rs".len() - 1
        );
        assert_eq!(
            highlighted(app.read(cx), cx).as_deref(),
            Some("file:tests/main.rs")
        );
    });
    vcx.simulate_keystrokes("right enter");
    draw_window(&mut vcx);
    vcx.update(|window, cx| {
        let app = app.read(cx);
        assert_eq!(app.active_data().unwrap().file_ix, 3);
        assert!(app
            .active_data()
            .unwrap()
            .file_history
            .can_go(NavigationDirection::Back));
        assert_eq!(app.tree_filter_input.read(cx).value(), "main.rs");
        assert!(app.focus_handle.is_focused(window));
    });
}

#[gpui::test]
fn filter_enter_opens_first_visible_match_rather_than_best_fuzzy_score(
    cx: &mut gpui::TestAppContext,
) {
    let (mut vcx, app) = filter_window(cx, &["main.rs", "src/main_test.rs", "src/main.rs"]);
    vcx.simulate_input("main.rs");
    vcx.simulate_keystrokes("enter");
    vcx.update(|window, cx| {
        let app = app.read(cx);
        assert_eq!(app.active_data().unwrap().file_ix, 2);
        assert!(app.focus_handle.is_focused(window));
        assert_eq!(app.tree_filter_input.read(cx).value(), "main.rs");
    });
}

#[gpui::test]
fn filter_query_changes_reset_highlight_and_empty_results_cannot_open_stale_files(
    cx: &mut gpui::TestAppContext,
) {
    let (mut vcx, app) = filter_window(cx, &["root.rs", "src/a.rs", "src/b.rs"]);
    vcx.simulate_input("src");
    vcx.simulate_keystrokes("down");
    vcx.update(|_, cx| {
        assert_eq!(
            highlighted(app.read(cx), cx).as_deref(),
            Some("file:src/b.rs")
        );
    });
    // Refine the query without clearing it first. Selection resets even if
    // the previously highlighted file still matches.
    vcx.simulate_input("/");
    vcx.update(|_, cx| {
        assert_eq!(
            highlighted(app.read(cx), cx).as_deref(),
            Some("file:src/a.rs")
        );
    });
    vcx.simulate_input("zzz");
    vcx.simulate_keystrokes("down up enter");
    vcx.update(|window, cx| {
        let app = app.read(cx);
        assert_eq!(highlighted(app, cx), None);
        assert_eq!(app.active_data().unwrap().file_ix, 0);
        assert!(app.tree_filter_input.focus_handle(cx).is_focused(window));
    });
    // Recover from no matches, then cancel without opening the candidate.
    vcx.simulate_keystrokes("cmd-a");
    vcx.simulate_input("src/b");
    vcx.update(|_, cx| {
        assert_eq!(
            highlighted(app.read(cx), cx).as_deref(),
            Some("file:src/b.rs")
        );
    });
    vcx.simulate_keystrokes("escape");
    vcx.update(|window, cx| {
        let app = app.read(cx);
        assert!(app.tree_filter_input.read(cx).value().is_empty());
        assert_eq!(app.active_data().unwrap().file_ix, 0);
        assert!(app.focus_handle.is_focused(window));
    });
    // Empty-query Enter remains a no-op rather than opening the old highlight.
    vcx.simulate_keystrokes("/ enter");
    vcx.update(|window, cx| {
        let app = app.read(cx);
        assert_eq!(app.active_data().unwrap().file_ix, 0);
        assert!(app.tree_filter_input.focus_handle(cx).is_focused(window));
    });
}

#[gpui::test]
fn filter_highlight_scrolls_into_view_and_enter_without_arrows_opens_first_visible_file(
    cx: &mut gpui::TestAppContext,
) {
    let paths: Vec<_> = (0..80).map(|ix| format!("src/file-{ix:02}.rs")).collect();
    let paths: Vec<_> = paths.iter().map(String::as_str).collect();
    let (mut vcx, app) = filter_window(cx, &paths);
    vcx.simulate_input("file");
    for _ in 0..79 {
        vcx.simulate_keystrokes("down");
    }
    draw_window(&mut vcx);
    vcx.update(|window, cx| {
        let app = app.read(cx);
        assert_eq!(highlighted(app, cx).as_deref(), Some("file:src/file-79.rs"));
        assert_eq!(app.active_data().unwrap().file_ix, 0);
        assert!(app.tree_filter_input.focus_handle(cx).is_focused(window));
    });
    let row = vcx.debug_bounds("tree-row-80").unwrap();
    let sidebar = vcx.debug_bounds("sidebar").unwrap();
    assert!(row.top() >= sidebar.top());
    assert!(row.bottom() <= sidebar.bottom());
    // Query changes reset the highlight, and Enter commits the first visible
    // file without needing an arrow keystroke.
    vcx.simulate_keystrokes("cmd-a");
    vcx.simulate_input("file-0");
    vcx.simulate_keystrokes("enter");
    vcx.update(|window, cx| {
        let app = app.read(cx);
        assert_eq!(app.active_data().unwrap().file_ix, 0);
        assert!(app.focus_handle.is_focused(window));
    });
}
