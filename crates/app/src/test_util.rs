//! Shared test fixtures used by more than one module's tests.

use crate::minimap::{MinimapKind, MinimapRow};
use crate::rows::{FileUpgrade, Row};
use crate::threads::{group_comments, CommentIndex, ReviewComment};
use diff_core::{diff_texts, DiffRow, FileDiff, FileStatus, Hunk, PrDiff};
use gpui::SharedString;
use std::collections::HashMap;
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
        old_count: rows
            .iter()
            .filter(|row| !matches!(row, DiffRow::Added { .. }))
            .count() as u32,
        new_start,
        new_count: rows
            .iter()
            .filter(|row| !matches!(row, DiffRow::Removed { .. }))
            .count() as u32,
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

/// An app comment fixture, distinct from the raw API fixture `rc`.
pub(crate) fn app_rc(
    id: u64,
    path: &str,
    side: Option<&str>,
    line: Option<u64>,
    body: &str,
    author: &str,
    created_at: &str,
    reply_to: Option<u64>,
) -> ReviewComment {
    rc(id, path, side, line, body, author, created_at, reply_to).into()
}

pub(crate) fn pr(number: u64, title: &str, author: &str, branch: &str) -> github::PrSummary {
    github::PrSummary {
        number,
        title: title.to_string(),
        author: github::Author {
            login: author.to_string(),
        },
        state: "OPEN".to_string(),
        is_draft: false,
        head_ref_name: branch.to_string(),
        updated_at: "2026-07-01T00:00:00Z".to_string(),
    }
}

/// A queue row for the chip tests: a fully-formed [`QueueItem`] with
/// only the fields the chips derive from filled in.
pub(crate) fn queued(number: u64, repository: &str, author: &str) -> github::QueueItem {
    github::QueueItem {
        number,
        title: format!("pr {number}"),
        author: author.to_string(),
        author_avatar_url: None,
        repository: repository.to_string(),
        is_draft: false,
        review_state: github::ReviewState::Awaiting,
        ci: github::CiState::NoChecks,
        additions: None,
        deletions: None,
        comments: 0,
        labels: Vec::new(),
        updated_at: String::new(),
    }
}

/// A 20-line file with line 10 changed, re-diffed with context 3: one
/// hunk covering lines 7..=13, hidden gaps of 6 lines above and 7 below.
pub(crate) fn upgraded_diff() -> (PrDiff, HashMap<usize, FileUpgrade>) {
    let old: String = (1..=20).map(|i| format!("line {i}\n")).collect();
    let new = old.replace("line 10\n", "line ten\n");
    let hunks = diff_texts(&old, &new, 3);
    let new_lines: Vec<SharedString> = new.lines().map(|l| l.to_string().into()).collect();
    let n = new_lines.len();
    let file = FileDiff {
        old_path: Some("a.txt".into()),
        new_path: Some("a.txt".into()),
        status: FileStatus::Modified,
        hunks,
        additions: 1,
        deletions: 1,
    };
    let upgrade = FileUpgrade {
        new_lines,
        old_spans: vec![Vec::new(); 20],
        new_spans: vec![Vec::new(); n],
    };
    (PrDiff { files: vec![file] }, HashMap::from([(0, upgrade)]))
}

pub(crate) fn row_name(row: &Row) -> &'static str {
    match row {
        Row::FileHeader { .. } => "FileHeader",
        Row::Binary => "Binary",
        Row::Fold { .. } => "Fold",
        Row::LargeFile { .. } => "LargeFile",
        Row::Continuation(_) => "Continuation",
        Row::Line { .. } => "Line",
        Row::SplitLine { .. } => "SplitLine",
        Row::CommentHeader { .. } => "CommentHeader",
        Row::CommentBody { .. } => "CommentBody",
        Row::CommentActions { .. } => "CommentActions",
    }
}

/// Two text files — a.rs with two hunks, b.rs with one — the smallest
/// change set that exercises navigation across a file boundary.
pub(crate) fn two_file_diff() -> PrDiff {
    let file = |path: &str, hunks: Vec<Hunk>| FileDiff {
        old_path: Some(path.into()),
        new_path: Some(path.into()),
        status: FileStatus::Modified,
        hunks,
        additions: 1,
        deletions: 1,
    };
    PrDiff {
        files: vec![
            file(
                "a.rs",
                vec![
                    hunk(1, 1, vec![add(1, "one", Vec::new())]),
                    hunk(10, 10, vec![add(10, "two", Vec::new())]),
                ],
            ),
            file("b.rs", vec![hunk(1, 1, vec![rem(1, "three", Vec::new())])]),
        ],
    }
}

/// Comments for sample_diff's a.rs: a RIGHT thread (with one reply) on
/// added line 2 ("new1"), a LEFT thread on removed line 2 ("old1"), and
/// one outdated comment.
pub(crate) fn sample_comments() -> CommentIndex {
    group_comments(vec![
        app_rc(
            1,
            "a.rs",
            Some("RIGHT"),
            Some(2),
            "on new1",
            "alice",
            "2026-01-01T00:00:00Z",
            None,
        ),
        app_rc(
            2,
            "a.rs",
            Some("RIGHT"),
            Some(2),
            "reply",
            "bob",
            "2026-01-02T00:00:00Z",
            Some(1),
        ),
        app_rc(
            3,
            "a.rs",
            Some("LEFT"),
            Some(2),
            "on old1",
            "carol",
            "2026-01-03T00:00:00Z",
            None,
        ),
        app_rc(
            4,
            "a.rs",
            None,
            None,
            "outdated",
            "dave",
            "2026-01-04T00:00:00Z",
            None,
        ),
    ])
}
