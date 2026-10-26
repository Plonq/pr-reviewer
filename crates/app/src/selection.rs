//! Selection state over the diff rows: drag anchoring, per-row range math, and copy text.

use std::ops::Range;
use unicode_segmentation::UnicodeSegmentation;

use crate::rows::Row;

/// Which text stream a selection runs through. Split selections are locked to
/// the side where the drag started, like GitHub; the other side paints nothing.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub(crate) enum SelSide {
    Unified,
    Left,
    Right,
    /// One inline comment's contiguous text block, inclusive display rows.
    Comment {
        first: usize,
        last: usize,
    },
}

impl SelSide {
    pub(crate) fn comment_at(rows: &[Row], row: usize) -> Option<Self> {
        if !matches!(rows.get(row), Some(Row::CommentBody { .. })) {
            return None;
        }
        let mut first = row;
        while first > 0 && matches!(rows[first - 1], Row::CommentBody { .. }) {
            first -= 1;
        }
        let mut last = row;
        while matches!(rows.get(last + 1), Some(Row::CommentBody { .. })) {
            last += 1;
        }
        Some(Self::Comment { first, last })
    }
}

/// A point in text space: display row index + char index into that row's text
/// (char, not byte — convert to byte offsets only when slicing). Ordered by
/// (row, col), which is exactly document order.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub(crate) struct RowCol {
    pub(crate) row: usize,
    pub(crate) col: usize,
}

/// Anchor stays where the drag started; head follows the mouse. The ordered
/// pair is derived on use, so dragging upward needs no special casing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Selection {
    pub(crate) side: SelSide,
    pub(crate) anchor: RowCol,
    pub(crate) head: RowCol,
}

impl Selection {
    pub(crate) fn ordered(&self) -> (RowCol, RowCol) {
        if self.anchor <= self.head {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        }
    }
}

/// Character drags start at a caret; word drags retain the entire initial word.
#[derive(Clone, Copy, Debug)]
pub(crate) enum SelectionDrag {
    Characters(SelSide, RowCol),
    Words(Selection),
}

impl SelectionDrag {
    pub(crate) fn side(self) -> SelSide {
        match self {
            Self::Characters(side, _) => side,
            Self::Words(selection) => selection.side,
        }
    }

    pub(crate) fn selection(self, head: RowCol, rows: &[Row]) -> Option<Selection> {
        let head = match self.side() {
            SelSide::Comment { first, .. } if head.row < first => RowCol { row: first, col: 0 },
            SelSide::Comment { last, .. } if head.row > last => RowCol {
                row: last,
                col: rows
                    .get(last)
                    .and_then(|row| row_side_text(row, self.side()))
                    .map_or(0, |text| text.chars().count()),
            },
            _ => head,
        };
        match self {
            Self::Characters(side, anchor) => {
                (head != anchor).then_some(Selection { side, anchor, head })
            }
            Self::Words(initial) => {
                let target = word_selection(rows, initial.side, head).unwrap_or(Selection {
                    side: initial.side,
                    anchor: head,
                    head,
                });
                Some(if head < initial.anchor {
                    Selection {
                        anchor: initial.head,
                        head: target.anchor,
                        ..initial
                    }
                } else if head >= initial.head {
                    Selection {
                        head: target.head,
                        ..initial
                    }
                } else {
                    initial
                })
            }
        }
    }
}

/// Select the Unicode word (or whitespace/punctuation segment) under a glyph.
/// Columns are character indices, not nearest-caret positions. Soft wraps are
/// only visual boundaries: a word spanning continuations is selected in full.
pub(crate) fn word_selection(rows: &[Row], side: SelSide, hit: RowCol) -> Option<Selection> {
    if let SelSide::Comment { first, last } = side {
        if !(first..=last).contains(&hit.row) {
            return None;
        }
    }
    let text = row_side_text(rows.get(hit.row)?, side)?;
    if hit.col >= text.chars().count() {
        return None;
    }
    let mut first = hit.row;
    let continues = |row: usize| match &rows[row] {
        Row::Continuation(_) => true,
        // Hard-wrapped comment words have no separator. A consumed space or
        // explicit newline is a real word boundary, not part of the word.
        Row::CommentBody { separator, .. } => *separator == "",
        _ => false,
    };
    while first > 0 && continues(first) {
        first -= 1;
    }
    let mut last = hit.row + 1;
    while last < rows.len() && continues(last) {
        last += 1;
    }
    let lengths: Vec<_> = rows[first..last]
        .iter()
        .map(|row| row_side_text(row, side).unwrap_or_default().chars().count())
        .collect();
    let text: String = rows[first..last]
        .iter()
        .filter_map(|row| row_side_text(row, side))
        .collect();
    let col = lengths[..hit.row - first].iter().sum::<usize>() + hit.col;
    let mut start = 0;
    for segment in text.split_word_bounds() {
        let end = start + segment.chars().count();
        if col < end {
            let point = |mut col: usize, is_end: bool| {
                let mut row = first;
                for &len in &lengths[..lengths.len() - 1] {
                    if col < len || (is_end && col == len) {
                        break;
                    }
                    col -= len;
                    row += 1;
                }
                RowCol { row, col }
            };
            return Some(Selection {
                side,
                anchor: point(start, false),
                head: point(end, true),
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
        (Row::CommentBody { line, .. }, SelSide::Comment { .. }) => Some(line.as_ref()),
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
    if let SelSide::Comment { first, last } = sel.side {
        if !(first..=last).contains(&row_ix) {
            return None;
        }
    }
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
            if has_text {
                match &rows[ix] {
                    Row::Continuation(_) => {}
                    Row::CommentBody { separator, .. } => text_out.push_str(separator),
                    _ => text_out.push('\n'),
                }
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

    #[test]
    fn words_use_unicode_boundaries_and_character_columns() {
        let rows = vec![line("let café_name42 = cafe\u{301};  東京")];
        for (col, expected) in [
            (0, "let"),
            (3, " "),
            (4, "café_name42"),
            (14, "café_name42"),
            (16, "="),
            (18, "cafe\u{301}"),
            (22, "cafe\u{301}"),
            (23, ";"),
            (24, "  "),
            (26, "東"),
        ] {
            let selection = word_selection(&rows, SelSide::Unified, RowCol { row: 0, col })
                .expect("a glyph is selectable");
            assert_eq!(selection_text(&selection, &rows), expected, "column {col}");
        }
        for col in [28, 99] {
            assert!(word_selection(&rows, SelSide::Unified, RowCol { row: 0, col }).is_none());
        }
    }

    #[test]
    fn words_skip_non_text_empty_and_missing_split_cells() {
        let rows = vec![Row::Binary, line(""), split(Some("old_name"), None)];
        for (row, side) in [
            (0, SelSide::Unified),
            (1, SelSide::Unified),
            (2, SelSide::Right),
        ] {
            assert!(word_selection(&rows, side, RowCol { row, col: 0 }).is_none());
        }
        let selection = word_selection(&rows, SelSide::Left, RowCol { row: 2, col: 4 }).unwrap();
        assert_eq!(selection_text(&selection, &rows), "old_name");
    }

    #[test]
    fn words_span_soft_wraps_but_not_source_lines() {
        let rows = vec![
            line("let long_"),
            Row::Continuation(Box::new(line("name = 1"))),
            line("another"),
        ];
        for hit in [RowCol { row: 0, col: 8 }, RowCol { row: 1, col: 0 }] {
            let selection = word_selection(&rows, SelSide::Unified, hit).unwrap();
            assert_eq!(selection_text(&selection, &rows), "long_name");
            assert_eq!(selection.anchor, RowCol { row: 0, col: 4 });
            assert_eq!(selection.head, RowCol { row: 1, col: 4 });
        }
        let selection = word_selection(&rows, SelSide::Unified, RowCol { row: 2, col: 0 }).unwrap();
        assert_eq!(selection_text(&selection, &rows), "another");
    }

    #[test]
    fn word_drags_extend_in_both_directions_and_keep_the_initial_word() {
        let rows = vec![line("one two three")];
        let initial = word_selection(&rows, SelSide::Unified, RowCol { row: 0, col: 5 }).unwrap();
        let drag = SelectionDrag::Words(initial);
        for (col, expected) in [(5, "two"), (12, "two three"), (1, "one two"), (6, "two")] {
            let selection = drag.selection(RowCol { row: 0, col }, &rows).unwrap();
            assert_eq!(selection_text(&selection, &rows), expected);
        }
        let drag = SelectionDrag::Characters(SelSide::Unified, RowCol { row: 0, col: 4 });
        assert!(drag.selection(RowCol { row: 0, col: 4 }, &rows).is_none());
        let selection = drag.selection(RowCol { row: 0, col: 6 }, &rows).unwrap();
        assert_eq!(selection_text(&selection, &rows), "tw");
    }

    fn comment_body(text: &str, width: usize) -> Vec<Row> {
        crate::threads::wrap_body_with_separators(text, width)
            .into_iter()
            .map(|(separator, line)| Row::CommentBody {
                line: line.into(),
                half: None,
                separator,
            })
            .collect()
    }

    #[test]
    fn comment_word_selection_spans_hard_wraps_and_keeps_unicode_intact() {
        let rows = comment_body("a café_identifier42 tail", 9);
        let side = SelSide::comment_at(&rows, 0).unwrap();
        for hit in [RowCol { row: 1, col: 0 }, RowCol { row: 2, col: 2 }] {
            let selection = word_selection(&rows, side, hit).unwrap();
            assert_eq!(selection_text(&selection, &rows), "café_identifier42");
        }
    }

    #[test]
    fn copying_comment_text_restores_spaces_and_newlines_not_visual_wraps() {
        for text in [
            "one two three four",
            "café_identifier42",
            "first\n\nlast",
            "a  b  c",
        ] {
            let rows = comment_body(text, 5);
            let side = SelSide::comment_at(&rows, 0).unwrap();
            let selection = sel(side, (0, 0), (rows.len() - 1, 99));
            assert_eq!(selection_text(&selection, &rows), text);
        }
    }

    #[test]
    fn comment_drags_cannot_select_code_or_adjacent_comments() {
        let mut rows = vec![line("code before")];
        rows.extend(comment_body("one two\nthree", 80));
        rows.push(Row::Binary); // A header/action/media boundary ends the block.
        rows.extend(comment_body("other comment", 80));
        rows.push(line("code after"));
        let side = SelSide::comment_at(&rows, 1).unwrap();
        assert_eq!(side, SelSide::Comment { first: 1, last: 2 });
        let initial = word_selection(&rows, side, RowCol { row: 1, col: 5 }).unwrap();
        let drag = SelectionDrag::Words(initial);
        let before = drag.selection(RowCol { row: 0, col: 6 }, &rows).unwrap();
        assert_eq!(selection_text(&before, &rows), "one two");
        let after = drag.selection(RowCol { row: 5, col: 6 }, &rows).unwrap();
        assert_eq!(selection_text(&after, &rows), "two\nthree");
        assert!(row_selection_range(&after, 4, &rows[4]).is_none());
        assert!(row_side_text(&rows[1], SelSide::Unified).is_none());
        assert!(word_selection(&rows, side, RowCol { row: 4, col: 0 }).is_none());
    }
}
