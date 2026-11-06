// Included in main.rs's tests so these exercise the real action/focus chain.

fn bind_line_selection_keys(cx: &mut gpui::TestAppContext) {
    cx.update(|cx| {
        cx.bind_keys(selection::keybindings());
        cx.bind_keys([
            KeyBinding::new("shift-c", NewComment, Some("ReviewApp")),
            KeyBinding::new("escape", ClearSelection, Some("ReviewApp")),
            KeyBinding::new("cmd-c", CopySelection, Some("ReviewApp")),
        ]);
    });
}

#[gpui::test]
fn keyboard_line_selection_extends_shrinks_reverses_and_comments(cx: &mut gpui::TestAppContext) {
    bind_line_selection_keys(cx);
    let (window, app) = queue_window(cx, FakePr::with_threads(Vec::new()));
    let mut vcx = gpui::VisualTestContext::from_window(window, cx);
    install_item(&app, range_pr_item(), cx);
    vcx.update(|window, cx| {
        let focus = app.read(cx).focus_handle.clone();
        window.focus(&focus, cx);
    });
    vcx.run_until_parked();

    vcx.simulate_keystrokes("down shift-down");
    vcx.update(|_, cx| {
        let app = app.read(cx);
        let data = app.active_data().unwrap();
        assert_eq!(data.cursor, 3);
        let selection = data.selection.unwrap();
        assert!(selection.linewise);
        for (ix, row) in data.rows.iter().enumerate() {
            assert!(
                selection.text_range(ix, row).is_none(),
                "keyboard line selection must not highlight code"
            );
        }
        assert!(
            row_selection_range(&selection, 2, &data.rows[2]).is_some(),
            "keep the selected-line gutter cue"
        );
        assert_eq!(
            selection_text(&data.selection.unwrap(), &data.rows),
            "two\nthree"
        );
        assert!(app.composer().is_none());
    });
    vcx.simulate_keystrokes("shift-up");
    vcx.update(|_, cx| {
        let data = app.read(cx).active_data().unwrap();
        assert_eq!(selection_text(&data.selection.unwrap(), &data.rows), "two");
    });
    vcx.simulate_keystrokes("shift-up shift-up");
    vcx.update(|_, cx| {
        let data = app.read(cx).active_data().unwrap();
        assert_eq!(data.cursor, 1, "clamps at the first source line");
        assert_eq!(
            selection_text(&data.selection.unwrap(), &data.rows),
            "one\ntwo"
        );
        assert!(data.selection.unwrap().anchor > data.selection.unwrap().head);
    });
    vcx.simulate_keystrokes("cmd-c");
    vcx.update(|_, cx| {
        assert_eq!(
            cx.read_from_clipboard().unwrap().text().unwrap(),
            "one\ntwo"
        )
    });
    vcx.simulate_keystrokes("shift-c");
    vcx.update(|window, cx| {
        let app = app.read(cx);
        let composer = app.composer().unwrap();
        assert_eq!((composer.start_line, composer.line), (Some(1), 2));
        assert_eq!(composer.side, CommentSide::Right);
        assert!(composer.input.focus_handle(cx).is_focused(window));
    });
    // Arrow keys in the editor edit text, not the diff's selection.
    vcx.simulate_input("first\nsecond");
    vcx.simulate_keystrokes("up shift-down");
    vcx.update(|window, cx| {
        app.update(cx, |app, cx| {
            assert_eq!(app.active_data().unwrap().cursor, 1);
            app.close_composer(window, cx);
        });
    });
    vcx.simulate_keystrokes("escape");
    vcx.update(|_, cx| assert!(app.read(cx).active_data().unwrap().selection.is_none()));
    vcx.simulate_keystrokes("down shift-down down");
    vcx.update(|_, cx| {
        let data = app.read(cx).active_data().unwrap();
        assert!(
            data.selection.is_none(),
            "an unmodified arrow clears the range"
        );
        assert_eq!(data.cursor, 4);
    });
}

#[gpui::test]
fn keyboard_ranges_skip_comment_rows_and_keep_the_clicked_split_side(
    cx: &mut gpui::TestAppContext,
) {
    bind_line_selection_keys(cx);
    let (window, app) = queue_window(cx, FakePr::with_threads(Vec::new()));
    let mut vcx = gpui::VisualTestContext::from_window(window, cx);
    let mut item = range_pr_item();
    let ItemState::Ready(data) = &mut item.state else {
        unreachable!()
    };
    data.diff = diff_core::parse_patch("diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1,3 +1,2 @@\n-old one\n-old two\n-old three\n+new one\n+new two\n");
    data.anchor_hunks = data
        .diff
        .files
        .iter()
        .map(|file| file.hunks.clone())
        .collect();
    data.mode = ViewMode::Split;
    data.set_rows(data.row_build());
    data.rows.insert(
        2,
        Row::CommentEditor {
            half: Some(CommentSide::Left),
        },
    );
    data.cursor = 1;
    data.cursor_side = Some(SelSide::Left);
    install_item(&app, item, cx);
    vcx.update(|window, cx| {
        let focus = app.read(cx).focus_handle.clone();
        window.focus(&focus, cx);
    });
    vcx.run_until_parked();
    vcx.simulate_keystrokes("shift-down shift-down shift-down");
    vcx.update(|_, cx| {
        let data = app.read(cx).active_data().unwrap();
        let selection = data.selection.unwrap();
        assert_eq!(selection.side, SelSide::Left);
        assert!(selection.linewise);
        for (ix, row) in data.rows.iter().enumerate() {
            assert!(selection.text_range(ix, row).is_none());
        }
        assert_eq!(data.cursor, 4);
        assert_eq!(
            selection_text(&selection, &data.rows),
            "old one\nold two\nold three"
        );
        let scroll = data.scroll.0.borrow();
        let viewport = scroll.base_handle.bounds();
        let top =
            viewport.top() + scroll.base_handle.offset().y + px(data.cursor as f32 * row_height());
        assert!(top >= viewport.top() && top + px(row_height()) <= viewport.bottom());
    });
    vcx.simulate_keystrokes("shift-c");
    vcx.update(|_, cx| {
        let composer = app.read(cx).composer().unwrap();
        assert_eq!((composer.start_line, composer.line), (Some(1), 3));
        assert_eq!(composer.side, CommentSide::Left);
    });
}

#[gpui::test]
fn extending_a_keyboard_range_reveals_the_head_without_jumping_visible_lines(
    cx: &mut gpui::TestAppContext,
) {
    bind_line_selection_keys(cx);
    let (window, app) = queue_window(cx, FakePr::with_threads(Vec::new()));
    let mut vcx = gpui::VisualTestContext::from_window(window, cx);
    let mut item = range_pr_item();
    let ItemState::Ready(data) = &mut item.state else {
        unreachable!()
    };
    let mut patch =
        String::from("diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -0,0 +1,100 @@\n");
    for line in 1..=100 {
        patch.push_str(&format!("+line {line}\n"));
    }
    data.diff = diff_core::parse_patch(&patch);
    data.set_rows(data.row_build());
    data.scroll.scroll_to_item_strict(0, ScrollStrategy::Top);
    install_item(&app, item, cx);
    vcx.update(|window, cx| {
        let focus = app.read(cx).focus_handle.clone();
        window.focus(&focus, cx);
    });
    vcx.run_until_parked();
    let offset = vcx.update(|_, cx| {
        app.read(cx)
            .active_data()
            .unwrap()
            .scroll
            .0
            .borrow()
            .base_handle
            .offset()
    });
    vcx.simulate_keystrokes("shift-down");
    vcx.run_until_parked();
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
            offset
        );
    });
    for _ in 0..80 {
        vcx.simulate_keystrokes("shift-down");
    }
    vcx.run_until_parked();
    vcx.update(|_, cx| {
        let data = app.read(cx).active_data().unwrap();
        assert_eq!(data.cursor, 82);
        assert_eq!(data.selection.unwrap().ordered().0.row, 1);
        let scroll = data.scroll.0.borrow();
        let top = scroll.base_handle.offset().y + px(data.cursor as f32 * row_height());
        assert!(scroll.base_handle.offset().y < offset.y);
        assert!(top >= px(0.));
        assert!(top + px(row_height()) <= scroll.base_handle.bounds().size.height);
    });
}

#[gpui::test]
fn keyboard_navigation_from_a_header_handles_deletion_only_split_files(
    cx: &mut gpui::TestAppContext,
) {
    let (window, app) = queue_window(cx, FakePr::with_threads(Vec::new()));
    let mut vcx = gpui::VisualTestContext::from_window(window, cx);
    let mut item = range_pr_item();
    let ItemState::Ready(data) = &mut item.state else {
        unreachable!()
    };
    data.diff = diff_core::parse_patch("diff --git a/a.rs b/a.rs\ndeleted file mode 100644\n--- a/a.rs\n+++ /dev/null\n@@ -1,2 +0,0 @@\n-one\n-two\n");
    data.mode = ViewMode::Split;
    data.set_rows(data.row_build());
    data.cursor = 0;
    data.cursor_side = None;
    install_item(&app, item, cx);
    vcx.update(|_, cx| {
        app.update(cx, |app, cx| {
            app.select_line(1, true, cx);
            let data = app.active_data().unwrap();
            assert_eq!(data.cursor_side, Some(SelSide::Left));
            assert_eq!(selection_text(&data.selection.unwrap(), &data.rows), "one");
            app.select_line(1, true, cx);
            let data = app.active_data().unwrap();
            assert_eq!(
                selection_text(&data.selection.unwrap(), &data.rows),
                "one\ntwo"
            );
        });
    });
}

#[gpui::test]
fn line_navigation_ignores_non_code_views_and_clears_ranges_on_jumps(
    cx: &mut gpui::TestAppContext,
) {
    let (window, app) = queue_window(cx, FakePr::with_threads(Vec::new()));
    let mut vcx = gpui::VisualTestContext::from_window(window, cx);
    install_item(&app, range_pr_item(), cx);
    vcx.update(|_, cx| {
        app.update(cx, |app, cx| {
            app.select_line(1, true, cx);
            assert!(app.active_data().unwrap().selection.is_some());
            app.jump(3, cx);
            assert!(app.active_data().unwrap().selection.is_none());
            app.show_queue = true;
            app.select_line(1, true, cx);
            assert_eq!(app.active_data().unwrap().cursor, 3);
            app.show_queue = false;
            app.items[0].pane = ItemPane::Conversation;
            app.select_line(1, true, cx);
            assert_eq!(app.active_data().unwrap().cursor, 3);
            assert!(app.active_data().unwrap().selection.is_none());
        });
    });
}
