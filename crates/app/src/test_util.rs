//! Shared test fixtures used by more than one module's tests.

use crate::minimap::{MinimapKind, MinimapRow};
use diff_core::{DiffRow, FileDiff, FileStatus, Hunk, PrDiff};
use std::ops::Range;

pub(crate) fn ctx(old_no: u32, new_no: u32, text: &str) -> DiffRow {
    DiffRow::Context {
        old_no,
        new_no,
        text: text.to_string(),
    }
}

pub(crate) fn add(new_no: u32, text: &str, intra: Vec<Range<usize>>) -> DiffRow {
    DiffRow::Added {
        new_no,
        text: text.to_string(),
        intra,
    }
}

pub(crate) fn rem(old_no: u32, text: &str, intra: Vec<Range<usize>>) -> DiffRow {
    DiffRow::Removed {
        old_no,
        text: text.to_string(),
        intra,
    }
}

pub(crate) fn hunk(old_start: u32, new_start: u32, rows: Vec<DiffRow>) -> Hunk {
    Hunk {
        old_start,
        old_count: 0,
        new_start,
        new_count: 0,
        section: String::new(),
        rows,
    }
}

pub(crate) fn sample_diff() -> PrDiff {
    PrDiff {
        files: vec![
            FileDiff {
                old_path: Some("a.rs".into()),
                new_path: Some("a.rs".into()),
                status: FileStatus::Modified,
                hunks: vec![
                    // Equal-count modified run, flanked by context.
                    hunk(
                        1,
                        1,
                        vec![
                            ctx(1, 1, "ctx"),
                            rem(2, "old1", vec![0..3]),
                            rem(3, "old2", Vec::new()),
                            add(2, "new1", vec![0..3]),
                            add(3, "new2", Vec::new()),
                            ctx(4, 4, "tail"),
                        ],
                    ),
                    // Unequal run (2 removed, 1 added) + a lone added run.
                    hunk(
                        10,
                        10,
                        vec![
                            rem(10, "r1", Vec::new()),
                            rem(11, "r2", Vec::new()),
                            add(10, "a1", Vec::new()),
                            ctx(12, 11, "c"),
                            add(12, "lone", Vec::new()),
                        ],
                    ),
                ],
                additions: 4,
                deletions: 4,
            },
            FileDiff {
                old_path: Some("b.png".into()),
                new_path: Some("b.png".into()),
                status: FileStatus::Binary,
                hunks: Vec::new(),
                additions: 0,
                deletions: 0,
            },
        ],
    }
}

pub(crate) fn mrow(kind: MinimapKind, len_frac: f32) -> MinimapRow {
    MinimapRow { kind, len_frac }
}

pub(crate) fn rc(
    id: u64,
    path: &str,
    side: Option<&str>,
    line: Option<u64>,
    body: &str,
    author: &str,
    created_at: &str,
    reply_to: Option<u64>,
) -> github::ReviewComment {
    github::ReviewComment {
        id,
        path: path.to_string(),
        line,
        side: side.map(str::to_string),
        start_line: None,
        body: body.to_string(),
        user: github::Author {
            login: author.to_string(),
        },
        created_at: created_at.to_string(),
        in_reply_to_id: reply_to,
    }
}
