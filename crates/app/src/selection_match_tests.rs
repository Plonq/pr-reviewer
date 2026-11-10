// Included in main.rs's tests to exercise the real diff mouse and paint paths.
#[gpui::test]
fn selection_matches_paint_both_sides_clear_and_invalidate_with_rows(
    cx: &mut gpui::TestAppContext,
) {
    let (window, app) = queue_window(cx, FakePr::with_threads(Vec::new()));
    let mut vcx = gpui::VisualTestContext::from_window(window, cx);
    for (mode, side) in [
        (ViewMode::Unified, SelSide::Unified),
        (ViewMode::Split, SelSide::Left),
        (ViewMode::Split, SelSide::Right),
    ] {
        let mut item = range_pr_item();
        let ItemState::Ready(data) = &mut item.state else { unreachable!() };
        data.diff = diff_core::parse_patch("diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1 +1 @@\n-old needle needle\n+new needle\ndiff --git a/b.rs b/b.rs\n--- a/b.rs\n+++ b/b.rs\n@@ -0,0 +1 @@\n+needle\n");
        data.mode = mode;
        data.selection = None;
        data.set_rows(data.row_build());
        install_item(&app, item, cx);
        draw_window(&mut vcx);
        let code_id = match side {
            SelSide::Unified => "diff-code-1",
            SelSide::Left => "diff-code-left-1",
            SelSide::Right => "diff-code-right-1",
        };
        let bounds = vcx.debug_bounds(code_id).unwrap();
        let position = vcx.update(|window, cx| {
            let width = app.update(cx, |app, _| app.char_width(window));
            point(bounds.left() + width * 5., bounds.center().y)
        });
        let prior = vcx.update(|_, cx| {
            app.read(cx).active_data().unwrap().scroll.0.borrow().base_handle.offset()
        });
        vcx.simulate_event(MouseDownEvent {
            position,
            button: MouseButton::Left,
            click_count: 2,
            ..Default::default()
        });
        vcx.simulate_mouse_up(position, MouseButton::Left, gpui::Modifiers::default());
        vcx.update(|window, cx| { let _ = window.draw(cx); });
        assert!(vcx.debug_bounds("selection-match-outlines").is_some());
        let cached = vcx.update(|_, cx| {
            let data = app.read(cx).active_data().unwrap();
            assert_eq!(selection_text(&data.selection.unwrap(), &data.rows), "needle");
            assert!(data.search.is_none(), "passive matching must not open Find");
            assert_eq!(data.scroll.0.borrow().base_handle.offset(), prior);
            let matches = data.selection_matches();
            assert!(Rc::ptr_eq(&matches, &data.selection_matches()), "reuse the source scan on repaint");
            let (left_row, left_side, right_row, right_side) = if mode == ViewMode::Unified {
                (1, SelSide::Unified, 2, SelSide::Unified)
            } else {
                (1, SelSide::Left, 1, SelSide::Right)
            };
            let mut left_ranges = vec![4..10, 11..17];
            let mut right_ranges = vec![4..10];
            if side == SelSide::Right {
                right_ranges.clear();
            } else {
                left_ranges.remove(0);
            }
            assert_eq!(matches.ranges(left_row, left_side), left_ranges);
            assert_eq!(matches.ranges(right_row, right_side), right_ranges);
            let mut expected_marks = vec![
                minimap::MinimapMatch {
                    row: left_row, side: left_side, range: (4. / 160.)..(10. / 160.),
                },
                minimap::MinimapMatch {
                    row: left_row, side: left_side, range: (11. / 160.)..(17. / 160.),
                },
                minimap::MinimapMatch {
                    row: right_row, side: right_side, range: (4. / 160.)..(10. / 160.),
                },
            ];
            expected_marks.remove(if side == SelSide::Right { 2 } else { 0 });
            assert_eq!(matches.marks, expected_marks);
            matches
        });
        // A rebuild with the same selection must not retain old row spans.
        vcx.update(|_, cx| app.update(cx, |app, cx| {
            let data = app.active_data_mut().unwrap();
            data.set_rows(data.row_build());
            assert!(!Rc::ptr_eq(&cached, &data.selection_matches()));
            cx.notify();
        }));
        vcx.dispatch_action(ClearSelection);
        vcx.update(|window, cx| { let _ = window.draw(cx); });
        vcx.update(|_, cx| {
            assert!(app.read(cx).active_data().unwrap().selection_matches().marks.is_empty());
        });
        assert!(vcx.debug_bounds("selection-match-outlines").is_none());
        // Nor should a selection leak into another file with the same text.
        vcx.update(|_, cx| app.update(cx, |app, cx| {
            let data = app.active_data_mut().unwrap();
            data.selection = Some(Selection {
                side,
                anchor: RowCol { row: 1, col: 4 },
                head: RowCol { row: 1, col: 10 },
                linewise: false,
            });
            assert!(!data.selection_matches().marks.is_empty());
            data.show_file(1, None);
            assert!(data.selection_matches().marks.is_empty());
            cx.notify();
        }));
    }
}
