use super::*;
use crate::rows::Cell;
use crate::selection::RowCol;

fn line(no: u32, kind: LineKind, text: &str) -> Row {
    Row::Line {
        old_no: (kind != LineKind::Added).then_some(no),
        new_no: (kind != LineKind::Removed).then_some(no),
        kind,
        text: text.to_string().into(),
        intra: Vec::new(),
        syntax: Vec::new(),
    }
}

fn split(no: u32, left: Option<&str>, right: Option<&str>) -> Row {
    let cell = |text: &str| Cell {
        no,
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

fn selection(side: SelSide, start: (usize, usize), end: (usize, usize)) -> Selection {
    Selection {
        side,
        anchor: RowCol {
            row: start.0,
            col: start.1,
        },
        head: RowCol {
            row: end.0,
            col: end.1,
        },
        linewise: false,
    }
}

fn mark(row: usize, side: SelSide, columns: Range<usize>) -> MinimapMatch {
    MinimapMatch {
        row,
        side,
        range: (columns.start as f32 / 160.)..(columns.end as f32 / 160.),
    }
}

#[test]
fn literal_unicode_substrings_match_both_split_sides_in_byte_coordinates() {
    let rows = vec![
        split(1, Some("x [hé] [hé]"), Some("[hé] new")),
        split(2, None, Some("z[hé]")),
    ];
    // Backward character selection, not a word or a regular expression.
    let matches = SelectionMatches::find(&rows, Some(selection(SelSide::Left, (0, 6), (0, 2))));
    assert_eq!(matches.ranges(0, SelSide::Left), vec![2..7, 8..13]);
    assert_eq!(matches.ranges(0, SelSide::Right), vec![0..5]);
    assert_eq!(matches.ranges(1, SelSide::Right), vec![1..6]);
    assert!(matches.ranges(1, SelSide::Left).is_empty());
    assert_eq!(
        matches.marks,
        vec![
            mark(0, SelSide::Left, 2..6),
            mark(0, SelSide::Left, 7..11),
            mark(0, SelSide::Right, 0..4),
            mark(1, SelSide::Right, 1..5),
        ]
    );
    let from_right = SelectionMatches::find(&rows, Some(selection(SelSide::Right, (0, 0), (0, 4))));
    assert_eq!(from_right.by_row, matches.by_row);
}

#[test]
fn unified_context_is_deduplicated_and_added_and_removed_text_both_match() {
    let rows = vec![
        line(1, LineKind::Context, "foo foo"),
        line(2, LineKind::Removed, "foo"),
        line(2, LineKind::Added, "Foo foo"),
    ];
    let matches = SelectionMatches::find(&rows, Some(selection(SelSide::Unified, (0, 0), (0, 3))));
    assert_eq!(matches.ranges(0, SelSide::Unified), vec![0..3, 4..7]);
    assert_eq!(matches.ranges(1, SelSide::Unified), vec![0..3]);
    assert_eq!(matches.ranges(2, SelSide::Unified), vec![4..7]);
    assert_eq!(matches.marks.len(), 4);
}

#[test]
fn matches_project_across_unequal_soft_wraps_and_repeated_occurrences() {
    let rows = vec![
        split(1, Some("hé"), Some("xhé")),
        Row::Continuation(Box::new(split(1, Some("llo!"), Some("ll")))),
        Row::Continuation(Box::new(split(1, None, Some("o!")))),
        split(2, Some("héllo héllo"), None),
    ];
    let matches = SelectionMatches::find(&rows, Some(selection(SelSide::Left, (0, 0), (1, 3))));
    assert_eq!(matches.ranges(0, SelSide::Left), vec![0..3]);
    assert_eq!(matches.ranges(1, SelSide::Left), vec![0..3]);
    assert_eq!(matches.ranges(0, SelSide::Right), vec![1..4]);
    assert_eq!(matches.ranges(1, SelSide::Right), vec![0..2]);
    assert_eq!(matches.ranges(2, SelSide::Right), vec![0..1]);
    assert_eq!(matches.ranges(3, SelSide::Left), vec![0..6, 7..13]);
    assert_eq!(
        matches.marks,
        vec![
            mark(0, SelSide::Left, 0..2),
            mark(0, SelSide::Right, 1..3),
            mark(1, SelSide::Left, 0..3),
            mark(1, SelSide::Right, 0..2),
            mark(2, SelSide::Right, 0..1),
            mark(3, SelSide::Left, 0..5),
            mark(3, SelSide::Left, 6..11),
        ]
    );
}

#[test]
fn multiline_matches_follow_source_streams_but_not_folds_or_hunk_gaps() {
    let rows = vec![
        line(1, LineKind::Context, "foo"),
        Row::CommentEditor { half: None, top: false },
        line(2, LineKind::Context, "bar"),
        line(3, LineKind::Removed, "foo"),
        line(4, LineKind::Removed, "bar"),
        line(3, LineKind::Added, "foo"),
        line(4, LineKind::Added, "bar"),
        line(5, LineKind::Context, "foo"),
        Row::Fold { key: 6, count: 10 },
        line(16, LineKind::Context, "bar"),
        line(17, LineKind::Context, "foo"),
        line(30, LineKind::Context, "bar"),
    ];
    let matches = SelectionMatches::find(&rows, Some(selection(SelSide::Unified, (0, 0), (2, 3))));
    assert_eq!(
        matches
            .marks
            .iter()
            .map(|mark| mark.row)
            .collect::<Vec<_>>(),
        vec![0, 2, 3, 4, 5, 6]
    );
    assert_eq!(matches.ranges(0, SelSide::Unified), vec![0..3]);
    assert!(matches.ranges(1, SelSide::Unified).is_empty());
    // Removed+added rows are adjacent visually, but not in either source.
    let mixed_rows = vec![
        line(1, LineKind::Removed, "before"),
        line(1, LineKind::Added, "after"),
    ];
    let mixed = SelectionMatches::find(
        &mixed_rows,
        Some(selection(SelSide::Unified, (0, 0), (1, 5))),
    );
    assert!(mixed.marks.is_empty());
}

#[test]
fn no_text_selection_means_no_matches() {
    let rows = vec![
        line(1, LineKind::Context, "   "),
        line(2, LineKind::Context, "foo"),
    ];
    let empty = selection(SelSide::Unified, (1, 0), (1, 0));
    let whitespace = selection(SelSide::Unified, (0, 0), (0, 3));
    let linewise = Selection {
        linewise: true,
        ..selection(SelSide::Unified, (1, 0), (1, 3))
    };
    for selection in [None, Some(empty), Some(whitespace), Some(linewise)] {
        assert!(SelectionMatches::find(&rows, selection).marks.is_empty());
    }
    assert!(SelectionMatches::find(&[], Some(empty)).marks.is_empty());
}
