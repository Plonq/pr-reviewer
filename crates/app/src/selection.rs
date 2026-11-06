//! Selection over diff rows: mouse/keyboard anchoring, source-line navigation,
//! per-row range math, and copy text.

use std::ops::Range;
use unicode_segmentation::UnicodeSegmentation;

use crate::rows::Row;

pub(crate) fn keybindings() -> [gpui::KeyBinding; 4] {
    use crate::{ExtendNextLine, ExtendPreviousLine, SelectNextLine, SelectPreviousLine};
    [
        gpui::KeyBinding::new("up", SelectPreviousLine, Some("ReviewApp")),
        gpui::KeyBinding::new("down", SelectNextLine, Some("ReviewApp")),
        gpui::KeyBinding::new("shift-up", ExtendPreviousLine, Some("ReviewApp")),
        gpui::KeyBinding::new("shift-down", ExtendNextLine, Some("ReviewApp")),
    ]
}

/// Which text stream a selection runs through. Split selections are locked to
/// the side where the drag started, like GitHub; the other side paints nothing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum SelSide {
    Unified,
    Left,
    Right,
}

/// A point in text space: display row index + char index into that row's text
/// (char, not byte — convert to byte offsets only when slicing). Ordered by
/// (row, col), which is exactly document order.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub(crate) struct RowCol {
    pub(crate) row: usize,
    pub(crate) col: usize,
}

/// Anchor stays where selection started; head follows the mouse or keyboard.
/// The ordered pair is derived on use, so backward selection needs no special casing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Selection {
    pub(crate) side: SelSide,
    pub(crate) anchor: RowCol,
    pub(crate) head: RowCol,
    /// Whole-line navigation selects gutters, not individual code characters.
    pub(crate) linewise: bool,
}

impl Selection {
    /// Character-level paint range. Linewise selections still supply gutter
    /// cues, copy text and comment anchors, but leave code backgrounds alone.
    pub(crate) fn text_range(&self, row_ix: usize, row: &Row) -> Option<Range<usize>> {
        if self.linewise {
            None
        } else {
            row_selection_range(self, row_ix, row)
        }
    }

    pub(crate) fn ordered(&self) -> (RowCol, RowCol) {
        if self.anchor <= self.head {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        }
    }
}

/// The first display row of the logical source line containing `row`.
pub(crate) fn source_line_start(rows: &[Row], mut row: usize) -> usize {
    while row > 0 && matches!(rows.get(row), Some(Row::Continuation(_))) {
        row -= 1;
    }
    row
}

/// Select complete source lines, including every wrapped fragment. Endpoint
/// orientation retains the original anchor when extending back through it.
pub(crate) fn line_selection(
    rows: &[Row],
    side: SelSide,
    anchor: usize,
    head: usize,
) -> Option<Selection> {
    let anchor = source_line_start(rows, anchor);
    let head = source_line_start(rows, head);
    let start = RowCol {
        row: anchor.min(head),
        col: 0,
    };
    let mut last = anchor.max(head);
    row_side_text(rows.get(start.row)?, side)?;
    row_side_text(rows.get(last)?, side)?;
    while matches!(rows.get(last + 1), Some(Row::Continuation(_))) {
        last += 1;
    }
    // Unequal split wrapping can leave the last fragment absent on this side.
    let end = (anchor.max(head)..=last).rev().find_map(|row| {
        row_side_text(&rows[row], side).map(|text| RowCol {
            row,
            col: text.chars().count(),
        })
    })?;
    Some(if head < anchor {
        Selection {
            side,
            anchor: end,
            head: start,
            linewise: true,
        }
    } else {
        Selection {
            side,
            anchor: start,
            head: end,
            linewise: true,
        }
    })
}

/// The next source line on a text stream, skipping headers, comments, folds,
/// absent split cells and visual continuations. Stops at the file's ends.
pub(crate) fn next_source_line(
    rows: &[Row],
    side: SelSide,
    row: usize,
    direction: i32,
) -> Option<usize> {
    let row = source_line_start(rows, row);
    let selectable = |ix: &usize| {
        !matches!(rows[*ix], Row::Continuation(_)) && row_side_text(&rows[*ix], side).is_some()
    };
    if direction > 0 {
        (row.saturating_add(1)..rows.len()).find(selectable)
    } else {
        (0..row.min(rows.len())).rev().find(selectable)
    }
}

/// A drag retains its original point or word, even when the pointer reverses.
#[derive(Clone, Copy, Debug)]
pub(crate) enum SelectionDrag {
    Character { side: SelSide, anchor: RowCol },
    Word(Selection),
}

impl SelectionDrag {
    pub(crate) fn side(self) -> SelSide {
        match self {
            Self::Character { side, .. } => side,
            Self::Word(selection) => selection.side,
        }
    }

    pub(crate) fn selection(self, rows: &[Row], head: RowCol) -> Option<Selection> {
        match self {
            Self::Character { side, anchor } => (head != anchor).then_some(Selection {
                side,
                anchor,
                head,
                linewise: false,
            }),
            Self::Word(initial) => {
                // A drag can move past a line's end or through non-text rows.
                let col = rows
                    .get(head.row)
                    .and_then(|row| row_side_text(row, initial.side))
                    .map(|text| head.col.min(text.chars().count().saturating_sub(1)))
                    .unwrap_or(head.col);
                let word = word_selection(rows, initial.side, RowCol { col, ..head });
                let (start, end) = word.map(|word| word.ordered()).unwrap_or((head, head));
                Some(if head < initial.anchor {
                    Selection {
                        anchor: initial.head,
                        head: start,
                        ..initial
                    }
                } else {
                    Selection {
                        head: end.max(initial.head),
                        ..initial
                    }
                })
            }
        }
    }
}

/// Select the Unicode word-boundary segment under a character, not the nearest
/// caret. Periods always delimit words; punctuation and whitespace are selectable
/// too. Wrapped fragments are joined before finding boundaries, so a visual line
/// break never splits a word.
pub(crate) fn word_selection(rows: &[Row], side: SelSide, hit: RowCol) -> Option<Selection> {
    let text = row_side_text(rows.get(hit.row)?, side)?;
    if hit.col >= text.chars().count() {
        return None;
    }
    let mut first = hit.row;
    while first > 0 && matches!(rows[first], Row::Continuation(_)) {
        first -= 1;
    }
    let mut last = hit.row + 1;
    while last < rows.len() && matches!(rows[last], Row::Continuation(_)) {
        last += 1;
    }
    let mut text = String::new();
    let mut hit_col = hit.col;
    let mut fragments = Vec::new();
    for (ix, row) in rows.iter().enumerate().take(last).skip(first) {
        if let Some(fragment) = row_side_text(row, side) {
            let len = fragment.chars().count();
            if ix < hit.row {
                hit_col += len;
            }
            fragments.push((ix, len));
            text.push_str(fragment);
        }
    }
    let mut start = 0;
    // Unicode word boundaries retain internal periods in e.g. `foo.bar` and
    // `3.14`. Split after periods first so each becomes trailing punctuation,
    // which Unicode segmentation separates from the preceding word.
    for segment in text
        .split_inclusive('.')
        .flat_map(|part| part.split_word_bounds())
    {
        let end = start + segment.chars().count();
        if hit_col < end {
            let point = |mut col: usize, is_end: bool| {
                for &(row, len) in &fragments {
                    if col < len || (is_end && col == len) {
                        return RowCol { row, col };
                    }
                    col -= len;
                }
                unreachable!("word boundary is within the source line")
            };
            return Some(Selection {
                side,
                anchor: point(start, false),
                head: point(end, true),
                linewise: false,
            });
        }
        start = end;
    }
    None
}

/// The text a selection on `side` runs through for this row, if any. Rows
/// that aren't text (headers, spacers, binary) and absent split cells yield
/// None: they're selectable-through but contribute nothing.
pub(crate) fn row_side_text(row: &Row, side: SelSide) -> Option<&str> {
    match (crate::rows::source_row(row), side) {
        (Row::Line { text, .. }, SelSide::Unified) => Some(text.as_ref()),
        (Row::SplitLine { left, .. }, SelSide::Left) => left.as_ref().map(|c| c.text.as_ref()),
        (Row::SplitLine { right, .. }, SelSide::Right) => right.as_ref().map(|c| c.text.as_ref()),
        _ => None,
    }
}

/// Byte offset of char index `col`, clamped to the end of the text.
fn char_to_byte(text: &str, col: usize) -> usize {
    text.char_indices()
        .nth(col)
        .map(|(byte, _)| byte)
        .unwrap_or(text.len())
}

/// The selected byte range within display row `row_ix`, or None if the row
/// contributes nothing (outside the selection, not a text row, or the selected
/// side is absent). The range can be empty (e.g. a selected empty line): copy
/// keeps it as an empty line, painting skips it.
pub(crate) fn row_selection_range(
    sel: &Selection,
    row_ix: usize,
    row: &Row,
) -> Option<Range<usize>> {
    let (start, end) = sel.ordered();
    if row_ix < start.row || row_ix > end.row {
        return None;
    }
    let text = row_side_text(row, sel.side)?;
    let chars = text.chars().count();
    let start_col = if row_ix == start.row {
        start.col.min(chars)
    } else {
        0
    };
    let end_col = if row_ix == end.row {
        end.col.min(chars)
    } else {
        chars
    };
    if start_col > end_col {
        return None;
    }
    Some(char_to_byte(text, start_col)..char_to_byte(text, end_col))
}

/// The selected text: each contributing row's selected substring, joined with
/// newlines. Header/spacer rows and absent split cells are skipped entirely
/// (no blank line for them).
pub(crate) fn selection_text(sel: &Selection, rows: &[Row]) -> String {
    let (start, end) = sel.ordered();
    let mut text_out = String::new();
    let mut has_text = false;
    for ix in start.row..=end.row.min(rows.len().saturating_sub(1)) {
        if let Some(range) = row_selection_range(sel, ix, &rows[ix]) {
            let text = row_side_text(&rows[ix], sel.side).unwrap_or_default();
            if has_text && !matches!(&rows[ix], Row::Continuation(_)) {
                text_out.push('\n');
            }
            text_out.push_str(&text[range]);
            has_text = true;
        }
    }
    text_out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rows::{Cell, LineKind, Row};

    fn line(text: &str) -> Row {
        Row::Line {
            old_no: Some(1),
            new_no: Some(1),
            kind: LineKind::Context,
            text: text.to_string().into(),
            intra: Vec::new(),
            syntax: Vec::new(),
        }
    }

    fn split(left: Option<&str>, right: Option<&str>) -> Row {
        let cell = |text: &str| Cell {
            no: 1,
            kind: LineKind::Context,
            text: text.to_string().into(),
            intra: Vec::new(),
            syntax: Vec::new(),
        };
        Row::SplitLine {
            left: left.map(cell),
            right: right.map(cell),
        }
    }

    fn sel(side: SelSide, anchor: (usize, usize), head: (usize, usize)) -> Selection {
        Selection {
            side,
            linewise: false,
            anchor: RowCol {
                row: anchor.0,
                col: anchor.1,
            },
            head: RowCol {
                row: head.0,
                col: head.1,
            },
        }
    }

    #[test]
    fn line_ranges_include_wrapped_unicode_and_empty_lines_in_both_directions() {
        let rows = vec![
            Row::Binary,
            line("hé"),
            Row::Continuation(Box::new(line("llo"))),
            Row::Binary,
            line(""),
            line("last"),
        ];
        for (anchor, head) in [(1, 5), (5, 1), (2, 5), (5, 2)] {
            let selection = line_selection(&rows, SelSide::Unified, anchor, head).unwrap();
            assert_eq!(selection_text(&selection, &rows), "héllo\n\nlast");
            assert!(selection.linewise);
            for (ix, row) in rows.iter().enumerate() {
                assert!(selection.text_range(ix, row).is_none());
            }
        }
        let selection = line_selection(&rows, SelSide::Unified, 1, 2).unwrap();
        assert_eq!(
            selection.ordered(),
            (RowCol { row: 1, col: 0 }, RowCol { row: 2, col: 3 })
        );
        assert_eq!(selection_text(&selection, &rows), "héllo");
        assert!(line_selection(&rows, SelSide::Unified, 0, 1).is_none());
        assert!(line_selection(&rows, SelSide::Unified, 1, 99).is_none());
        assert_eq!(next_source_line(&rows, SelSide::Unified, 1, 1), Some(4));
        assert_eq!(next_source_line(&rows, SelSide::Unified, 2, 1), Some(4));
        assert_eq!(next_source_line(&rows, SelSide::Unified, 4, -1), Some(1));
        assert_eq!(next_source_line(&rows, SelSide::Unified, 1, -1), None);
        assert_eq!(next_source_line(&rows, SelSide::Unified, 5, 1), None);
        assert_eq!(next_source_line(&[], SelSide::Unified, 0, 1), None);
    }

    #[test]
    fn line_navigation_and_ranges_stay_on_the_selected_split_side() {
        let rows = vec![
            split(Some("left"), Some("ri")),
            Row::Continuation(Box::new(split(None, Some("ght")))),
            split(Some("left only"), None),
            Row::Binary,
            split(None, Some("right only")),
        ];
        assert_eq!(next_source_line(&rows, SelSide::Right, 0, 1), Some(4));
        assert_eq!(next_source_line(&rows, SelSide::Left, 0, 1), Some(2));
        assert_eq!(next_source_line(&rows, SelSide::Right, 4, -1), Some(0));
        let left = line_selection(&rows, SelSide::Left, 0, 0).unwrap();
        assert_eq!(selection_text(&left, &rows), "left");
        assert!(left.text_range(0, &rows[0]).is_none());
        let right = line_selection(&rows, SelSide::Right, 4, 0).unwrap();
        assert_eq!(selection_text(&right, &rows), "right\nright only");
        assert!(right.text_range(0, &rows[0]).is_none());
        assert!(right.text_range(1, &rows[1]).is_none());
        assert!(right.text_range(4, &rows[4]).is_none());
        assert!(line_selection(&rows, SelSide::Right, 0, 2).is_none());
    }

    #[test]
    fn words_use_unicode_boundaries_and_character_columns() {
        for (text, ranges) in [
            ("foo_bar42(x)", vec![0..9, 9..10, 10..11, 11..12]),
            ("foo.bar", vec![0..3, 3..4, 4..7]),
            ("3.14", vec![0..1, 1..2, 2..4]),
            (".foo..bar.", vec![0..1, 1..4, 4..5, 5..6, 6..9, 9..10]),
            ("héllo.world", vec![0..5, 5..6, 6..11]),
            ("héllo 世界", vec![0..5, 5..6, 6..7, 7..8]),
            ("cafe\u{301} ok", vec![0..5, 5..6, 6..8]),
            ("  one\t two", vec![0..2, 2..5, 5..6, 6..7, 7..10]),
        ] {
            let rows = vec![line(text)];
            for range in ranges {
                for col in range.clone() {
                    let word =
                        word_selection(&rows, SelSide::Unified, RowCol { row: 0, col }).unwrap();
                    assert_eq!(
                        word,
                        sel(SelSide::Unified, (0, range.start), (0, range.end)),
                        "{text:?}, col {col}"
                    );
                    let bytes = char_to_byte(text, range.start)..char_to_byte(text, range.end);
                    assert_eq!(word.text_range(0, &rows[0]), Some(bytes.clone()));
                    assert_eq!(selection_text(&word, &rows), &text[bytes]);
                }
            }
        }
    }

    #[test]
    fn periods_delimit_wrapped_words_and_word_drag_endpoints() {
        let rows = vec![line("foo.ba"), Row::Continuation(Box::new(line("r.baz")))];
        let bar = word_selection(&rows, SelSide::Unified, RowCol { row: 1, col: 0 }).unwrap();
        assert_eq!(bar, sel(SelSide::Unified, (0, 4), (1, 1)));
        assert_eq!(selection_text(&bar, &rows), "bar");
        let period = word_selection(&rows, SelSide::Unified, RowCol { row: 1, col: 1 }).unwrap();
        assert_eq!(selection_text(&period, &rows), ".");
        let foo = word_selection(&rows, SelSide::Unified, RowCol { row: 0, col: 0 }).unwrap();
        let dragged = SelectionDrag::Word(foo)
            .selection(&rows, RowCol { row: 0, col: 4 })
            .unwrap();
        assert_eq!(selection_text(&dragged, &rows), "foo.bar");
    }

    #[test]
    fn words_ignore_non_text_empty_and_out_of_range_hits() {
        let rows = vec![
            line(""),
            line("word"),
            Row::Binary,
            split(None, Some("right")),
        ];
        for (side, row, col) in [
            (SelSide::Unified, 0, 0),
            (SelSide::Unified, 1, 4),
            (SelSide::Unified, 1, usize::MAX),
            (SelSide::Unified, 2, 0),
            (SelSide::Unified, 99, 0),
            (SelSide::Left, 3, 0),
        ] {
            assert!(word_selection(&rows, side, RowCol { row, col }).is_none());
        }
        let word = word_selection(&rows, SelSide::Right, RowCol { row: 3, col: 2 }).unwrap();
        assert_eq!(selection_text(&word, &rows), "right");
    }

    #[test]
    fn words_span_wrapped_fragments_but_not_source_lines_or_split_sides() {
        for side in [SelSide::Unified, SelSide::Left, SelSide::Right] {
            let row = |text: &str| match side {
                SelSide::Unified => line(text),
                SelSide::Left => split(Some(text), Some("other")),
                SelSide::Right => split(Some("other"), Some(text)),
            };
            let rows = vec![
                row("a hel"),
                Row::Continuation(Box::new(row("lo_w"))),
                Row::Continuation(Box::new(row("orld!"))),
                row("next"),
            ];
            for (row, col) in [(0, 3), (1, 0), (2, 2)] {
                let word = word_selection(&rows, side, RowCol { row, col }).unwrap();
                assert_eq!(word, sel(side, (0, 2), (2, 4)));
                assert_eq!(selection_text(&word, &rows), "hello_world");
            }
            let word = word_selection(&rows, side, RowCol { row: 2, col: 4 }).unwrap();
            assert_eq!(selection_text(&word, &rows), "!");
        }
        // Exact wrap boundaries: start on the next fragment, end on the prior one.
        let rows = vec![
            line("one "),
            Row::Continuation(Box::new(line("two "))),
            Row::Continuation(Box::new(line("three"))),
        ];
        let word = word_selection(&rows, SelSide::Unified, RowCol { row: 1, col: 0 }).unwrap();
        assert_eq!(word, sel(SelSide::Unified, (1, 0), (1, 3)));
    }

    #[test]
    fn word_drags_extend_in_whole_words_and_retain_the_initial_word() {
        let rows = vec![line("one two three"), Row::Binary, line("four")];
        let initial = word_selection(&rows, SelSide::Unified, RowCol { row: 0, col: 5 }).unwrap();
        let drag = SelectionDrag::Word(initial);
        for (row, col, expected) in [
            (0, 5, "two"),
            (0, 0, "one two"),
            (0, 11, "two three"),
            (0, 99, "two three"),
            (2, 1, "two three\nfour"),
            (0, 4, "two"),
        ] {
            let selection = drag.selection(&rows, RowCol { row, col }).unwrap();
            assert_eq!(selection_text(&selection, &rows), expected);
        }
        let drag = SelectionDrag::Character {
            side: SelSide::Unified,
            anchor: RowCol { row: 0, col: 4 },
        };
        assert!(drag.selection(&rows, RowCol { row: 0, col: 4 }).is_none());
        let selection = drag.selection(&rows, RowCol { row: 0, col: 5 }).unwrap();
        assert_eq!(selection_text(&selection, &rows), "t");
    }

    #[test]
    fn selection_ordered_swaps_backward_drags() {
        let forward = sel(SelSide::Unified, (1, 3), (4, 2));
        let backward = sel(SelSide::Unified, (4, 2), (1, 3));
        assert_eq!(forward.ordered(), backward.ordered());
        // Same row, backward drag: ordered by column.
        let same_row = sel(SelSide::Unified, (2, 7), (2, 1));
        let (start, end) = same_row.ordered();
        assert_eq!((start.col, end.col), (1, 7));
    }

    #[test]
    fn char_to_byte_multibyte() {
        let text = "let s = \"héllo\";";
        assert_eq!(char_to_byte(text, 0), 0);
        assert_eq!(char_to_byte(text, 10), 10); // 'é'
        assert_eq!(char_to_byte(text, 11), 12); // first 'l': 'é' is 2 bytes
        assert_eq!(char_to_byte(text, 99), text.len()); // clamped
        assert_eq!(
            &text[char_to_byte(text, 8)..char_to_byte(text, 14)],
            "\"héllo"
        );
    }

    #[test]
    fn row_range_unified_multi_row() {
        let rows = vec![line("first line"), line("middle"), line("last line")];
        let sel = sel(SelSide::Unified, (0, 6), (2, 4));
        // First row: from col 6 to end of text.
        assert_eq!(row_selection_range(&sel, 0, &rows[0]), Some(6..10));
        // Middle row: fully selected, col 0 to end.
        assert_eq!(row_selection_range(&sel, 1, &rows[1]), Some(0..6));
        // Last row: col 0 to col 4.
        assert_eq!(row_selection_range(&sel, 2, &rows[2]), Some(0..4));
        // Outside the selection.
        assert_eq!(row_selection_range(&sel, 3, &rows[0]), None);
    }

    #[test]
    fn row_range_skips_non_text_rows() {
        let sel = sel(SelSide::Unified, (0, 0), (2, 3));
        // Non-text rows inside the span contribute nothing…
        assert_eq!(row_selection_range(&sel, 1, &Row::Binary), None);
        // …and split rows never match a Unified-side selection.
        assert_eq!(
            row_selection_range(&sel, 1, &split(Some("x"), Some("y"))),
            None
        );
    }

    #[test]
    fn row_range_split_sides_and_absent_cells() {
        let rows = vec![
            split(Some("left one"), Some("right one")),
            split(None, Some("right only")),
            split(Some("left only"), None),
        ];
        let right = sel(SelSide::Right, (0, 6), (2, 4));
        assert_eq!(row_selection_range(&right, 0, &rows[0]), Some(6..9));
        assert_eq!(row_selection_range(&right, 1, &rows[1]), Some(0..10));
        // Selected side absent: contributes nothing.
        assert_eq!(row_selection_range(&right, 2, &rows[2]), None);
        let left = sel(SelSide::Left, (0, 5), (1, 3));
        assert_eq!(row_selection_range(&left, 0, &rows[0]), Some(5..8));
        assert_eq!(row_selection_range(&left, 1, &rows[1]), None);
    }

    #[test]
    fn row_range_clamps_columns_to_text() {
        let rows = vec![line("ab"), line("cdef")];
        // Anchor col way past the end of a short line.
        let sel = sel(SelSide::Unified, (0, 99), (1, 2));
        assert_eq!(row_selection_range(&sel, 0, &rows[0]), Some(2..2)); // empty
        assert_eq!(row_selection_range(&sel, 1, &rows[1]), Some(0..2));
    }

    #[test]
    fn copy_assembles_contributing_rows() {
        let rows = vec![
            line("fn main() {"),
            Row::Binary,
            line(""),
            line("    body();"),
            line("}"),
        ];
        // From col 3 of row 0 through col 1 of row 4: the binary row is
        // skipped (no blank line for it), the empty line survives.
        let forward = sel(SelSide::Unified, (0, 3), (4, 1));
        assert_eq!(
            selection_text(&forward, &rows),
            "main() {\n\n    body();\n}"
        );
        // Backward drag over one row.
        let backward = sel(SelSide::Unified, (0, 7), (0, 3));
        assert_eq!(selection_text(&backward, &rows), "main");
    }

    #[test]
    fn copy_split_takes_locked_side_only() {
        let rows = vec![
            split(Some("old a"), Some("new a")),
            split(None, Some("new b")),
            split(Some("old c"), None),
        ];
        let right = sel(SelSide::Right, (0, 0), (2, 5));
        assert_eq!(selection_text(&right, &rows), "new a\nnew b");
        let left = sel(SelSide::Left, (0, 0), (2, 5));
        assert_eq!(selection_text(&left, &rows), "old a\nold c");
    }

    #[test]
    fn copy_multibyte_slice() {
        let rows = vec![line("let s = \"héllo\";")];
        let quoted = sel(SelSide::Unified, (0, 8), (0, 14));
        assert_eq!(selection_text(&quoted, &rows), "\"héllo");
    }
}
