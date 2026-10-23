//! Phase-2 full-content upgrade: background refetch and re-diff of whole files.

use crate::rows::FileUpgrade;
use diff_core::{diff_texts, DiffRow, FileStatus, Hunk};
use gpui::SharedString;

/// Never upgrade more than this many files per item; the rest stay on the
/// perfectly serviceable patch-derived view.
const MAX_UPGRADE_FILES: usize = 400;
/// Per-side byte cap, checked on fetched text before line-ending normalization.
const MAX_UPGRADE_BLOB_BYTES: usize = 1024 * 1024;
/// gh/git are one subprocess per call, so a small worker pool is plenty.
const UPGRADE_WORKERS: usize = 4;

/// Where to fetch full file contents from, snapshotted when an item loads.
pub(crate) enum UpgradeSource {
    Pr {
        loc: github::PrLocator,
        base_oid: String,
        head_oid: String,
    },
    Local(git::LocalSource),
}

/// One file's fetch inputs, snapshotted from the parsed patch (paths and
/// statuses are already known — no extra API call needed).
pub(crate) struct UpgradeJob {
    pub(crate) file_ix: usize,
    pub(crate) old_path: Option<String>,
    pub(crate) new_path: Option<String>,
    pub(crate) status: FileStatus,
}

/// One file's completed upgrade: authoritative hunks plus the render state.
pub(crate) struct UpgradedFile {
    pub(crate) file_ix: usize,
    pub(crate) hunks: Vec<Hunk>,
    pub(crate) additions: u32,
    pub(crate) deletions: u32,
    pub(crate) upgrade: FileUpgrade,
}

/// Blocking fetch + re-diff + whole-file highlight for every eligible file;
/// runs on the background executor. Files that fail on either side are simply
/// missing from the result and keep their patch-derived view.
pub(crate) fn run_upgrade(
    github: &dyn github::GitHubClient,
    source: &UpgradeSource,
    jobs: Vec<UpgradeJob>,
) -> Vec<UpgradedFile> {
    match source {
        UpgradeSource::Pr {
            loc,
            base_oid,
            head_oid,
        } => run_with_source(
            &PrContent {
                github,
                loc,
                base_oid,
                head_oid,
            },
            jobs,
        ),
        UpgradeSource::Local(source) => run_with_source(&LocalContent { source }, jobs),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Side {
    Old,
    New,
}

/// A fetched side's usable text. None includes absence, non-UTF-8 and fetch
/// failures; Some(empty) is a usable empty file. Adapters own backend routing
/// and diagnostics, while the common pipeline enforces presence and size.
trait ContentSource: Sync {
    fn fetch(&self, path: &str, side: Side) -> Option<String>;
}

struct PrContent<'a> {
    github: &'a dyn github::GitHubClient,
    loc: &'a github::PrLocator,
    base_oid: &'a str,
    head_oid: &'a str,
}

impl ContentSource for PrContent<'_> {
    fn fetch(&self, path: &str, side: Side) -> Option<String> {
        let oid = match side {
            Side::Old => self.base_oid,
            Side::New => self.head_oid,
        };
        match self.github.file_at(self.loc, oid, path) {
            Ok(text) => text,
            Err(err) => {
                eprintln!("gander: {path}: {err}");
                None
            }
        }
    }
}

struct LocalContent<'a> {
    source: &'a git::LocalSource,
}

impl ContentSource for LocalContent<'_> {
    fn fetch(&self, path: &str, side: Side) -> Option<String> {
        match side {
            Side::Old => git::file_at_base(self.source, path),
            Side::New => {
                String::from_utf8(std::fs::read(self.source.repo_root.join(path)).ok()?).ok()
            }
        }
    }
}

fn run_with_source(source: &dyn ContentSource, mut jobs: Vec<UpgradeJob>) -> Vec<UpgradedFile> {
    if jobs.len() > MAX_UPGRADE_FILES {
        eprintln!(
            "gander: {} changed files; upgrading the first {MAX_UPGRADE_FILES} to full \
             contents, the rest stay patch-derived",
            jobs.len()
        );
        jobs.truncate(MAX_UPGRADE_FILES);
    }
    let queue = std::sync::Mutex::new(jobs.into_iter());
    let done = std::sync::Mutex::new(Vec::new());
    std::thread::scope(|scope| {
        for _ in 0..UPGRADE_WORKERS {
            scope.spawn(|| loop {
                let Some(job) = queue.lock().unwrap().next() else {
                    break;
                };
                if let Some(result) = upgrade_file(source, &job) {
                    done.lock().unwrap().push(result);
                }
            });
        }
    });
    let mut done = done.into_inner().unwrap();
    done.sort_by_key(|file| file.file_ix);
    done
}

/// Apply the common byte cap before normalization. Fetching still loads the
/// complete side; this is not a streaming memory bound.
fn fetch_side(source: &dyn ContentSource, path: &str, side: Side) -> Option<String> {
    let text = source.fetch(path, side)?;
    (text.len() <= MAX_UPGRADE_BLOB_BYTES).then_some(text)
}

fn upgrade_file(source: &dyn ContentSource, job: &UpgradeJob) -> Option<UpgradedFile> {
    // Presence must match the patch's story: Added has no old side, Deleted no
    // new side, everything else needs both. A wanted-but-missing side (404,
    // binary, huge) keeps the whole file patch-derived.
    let old_text = match (job.status, &job.old_path) {
        (FileStatus::Added, _) => String::new(),
        (_, Some(path)) => fetch_side(source, path, Side::Old)?,
        (_, None) => return None,
    };
    let new_text = match (job.status, &job.new_path) {
        (FileStatus::Deleted, _) => String::new(),
        (_, Some(path)) => fetch_side(source, path, Side::New)?,
        (_, None) => return None,
    };
    // Normalize CRLF→LF up front: hunks, span tables, and gap rows must all
    // index the same line/byte space (diff_texts itself is ending-agnostic).
    let normalize = |text: String| {
        if text.contains('\r') {
            text.replace("\r\n", "\n")
        } else {
            text
        }
    };
    let old_text = normalize(old_text);
    let new_text = normalize(new_text);

    let hunks = diff_texts(&old_text, &new_text, 3);
    let (additions, deletions) =
        hunks
            .iter()
            .flat_map(|hunk| &hunk.rows)
            .fold((0, 0), |(a, d), row| match row {
                DiffRow::Added { .. } => (a + 1, d),
                DiffRow::Removed { .. } => (a, d + 1),
                DiffRow::Context { .. } => (a, d),
            });

    let path = job.new_path.as_deref().or(job.old_path.as_deref())?;
    let lang = syntax::language_for_path(path);
    let spans = |text: &str| match lang {
        Some(lang) if !text.is_empty() => syntax::highlight_lines(lang, text),
        _ => Vec::new(),
    };
    Some(UpgradedFile {
        file_ix: job.file_ix,
        hunks,
        additions,
        deletions,
        upgrade: FileUpgrade {
            old_spans: spans(&old_text),
            new_spans: spans(&new_text),
            new_lines: new_text
                .lines()
                .map(|line| SharedString::from(line.to_string()))
                .collect(),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rows::{build_rows, Row, RowOptions, RowsView, ViewMode};
    use diff_core::{FileDiff, PrDiff};
    use std::collections::{HashMap, HashSet};
    use std::sync::{Condvar, Mutex};
    use std::time::Duration;

    #[derive(Default)]
    struct FakeSource {
        text: HashMap<(String, Side), String>,
        calls: Mutex<Vec<(String, Side)>>,
    }

    impl FakeSource {
        fn new(entries: &[(&str, Side, &str)]) -> Self {
            Self {
                text: entries
                    .iter()
                    .map(|(path, side, text)| ((path.to_string(), *side), text.to_string()))
                    .collect(),
                ..Self::default()
            }
        }

        fn calls(&self) -> Vec<(String, Side)> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl ContentSource for FakeSource {
        fn fetch(&self, path: &str, side: Side) -> Option<String> {
            self.calls.lock().unwrap().push((path.to_string(), side));
            self.text.get(&(path.to_string(), side)).cloned()
        }
    }

    fn job(status: FileStatus, old: Option<&str>, new: Option<&str>) -> UpgradeJob {
        UpgradeJob {
            file_ix: 0,
            old_path: old.map(str::to_string),
            new_path: new.map(str::to_string),
            status,
        }
    }

    fn lines(file: &UpgradedFile) -> Vec<&str> {
        file.upgrade.new_lines.iter().map(|s| s.as_ref()).collect()
    }

    #[test]
    fn added_and_deleted_synthesize_only_the_absent_side() {
        let source = FakeSource::new(&[
            ("new.txt", Side::New, "new\n"),
            ("old.txt", Side::Old, "old\n"),
        ]);
        let added = upgrade_file(
            &source,
            &job(FileStatus::Added, Some("ignored"), Some("new.txt")),
        )
        .unwrap();
        let deleted = upgrade_file(
            &source,
            &job(FileStatus::Deleted, Some("old.txt"), Some("ignored")),
        )
        .unwrap();
        assert_eq!((added.additions, added.deletions), (1, 0));
        assert_eq!((deleted.additions, deleted.deletions), (0, 1));
        assert_eq!(lines(&added), ["new"]);
        assert!(lines(&deleted).is_empty());
        assert_eq!(
            source.calls(),
            [("new.txt".into(), Side::New), ("old.txt".into(), Side::Old)]
        );
    }

    #[test]
    fn required_paths_and_unusable_sides_short_circuit_in_old_new_order() {
        let cases = [
            (FileStatus::Added, None, None, vec![]),
            (FileStatus::Deleted, None, None, vec![]),
            (FileStatus::Modified, None, Some("new"), vec![]),
            (
                FileStatus::Renamed,
                Some("missing"),
                Some("new"),
                vec![("missing", Side::Old)],
            ),
            (
                FileStatus::Modified,
                Some("old"),
                None,
                vec![("old", Side::Old)],
            ),
            (
                FileStatus::Modified,
                Some("old"),
                Some("missing"),
                vec![("old", Side::Old), ("missing", Side::New)],
            ),
        ];
        for (status, old, new, expected) in cases {
            let source = FakeSource::new(&[("old", Side::Old, ""), ("new", Side::New, "")]);
            assert!(run_with_source(&source, vec![job(status, old, new)]).is_empty());
            assert_eq!(
                source.calls(),
                expected
                    .into_iter()
                    .map(|(p, s)| (p.to_string(), s))
                    .collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn present_empty_files_are_successful_including_renames_and_binary_status() {
        for status in [
            FileStatus::Modified,
            FileStatus::Renamed,
            FileStatus::Binary,
        ] {
            let source = FakeSource::new(&[("before", Side::Old, ""), ("after", Side::New, "")]);
            let result = run_with_source(&source, vec![job(status, Some("before"), Some("after"))]);
            assert_eq!(result.len(), 1);
            assert_eq!((result[0].additions, result[0].deletions), (0, 0));
            assert!(result[0].hunks.is_empty());
            assert!(lines(&result[0]).is_empty());
            assert!(result[0].upgrade.old_spans.is_empty());
            assert!(result[0].upgrade.new_spans.is_empty());
            assert_eq!(
                source.calls(),
                [("before".into(), Side::Old), ("after".into(), Side::New)]
            );
        }
        for (status, old, new, side) in [
            (FileStatus::Added, None, Some("empty"), Side::New),
            (FileStatus::Deleted, Some("empty"), None, Side::Old),
        ] {
            let source = FakeSource::new(&[("empty", side, "")]);
            assert_eq!(
                run_with_source(&source, vec![job(status, old, new)]).len(),
                1
            );
        }
    }

    #[test]
    fn renamed_paths_route_independently_and_new_path_selects_language() {
        let source = FakeSource::new(&[
            ("before.txt", Side::Old, "fn old() {}\n"),
            ("after.rs", Side::New, "fn new() {}\n"),
        ]);
        let result = upgrade_file(
            &source,
            &job(FileStatus::Renamed, Some("before.txt"), Some("after.rs")),
        )
        .unwrap();
        assert_eq!(
            source.calls(),
            [
                ("before.txt".into(), Side::Old),
                ("after.rs".into(), Side::New)
            ]
        );
        assert_eq!((result.additions, result.deletions), (1, 1));
        assert!(!result.upgrade.old_spans[0].is_empty());
        assert!(!result.upgrade.new_spans[0].is_empty());
    }

    #[test]
    fn deleted_file_falls_back_to_old_path_for_highlighting() {
        let source = FakeSource::new(&[("old.rs", Side::Old, "fn old() {}\n")]);
        let result =
            upgrade_file(&source, &job(FileStatus::Deleted, Some("old.rs"), None)).unwrap();
        assert!(!result.upgrade.old_spans[0].is_empty());
        assert!(result.upgrade.new_spans.is_empty());
    }

    #[test]
    fn byte_cap_accepts_exact_limit_and_rejects_one_more_on_either_side() {
        // Multibyte text pins bytes, not character count.
        let exact = "é".repeat(MAX_UPGRADE_BLOB_BYTES / 2);
        let over = format!("{exact}x");
        let source = FakeSource::new(&[("exact", Side::New, &exact), ("over", Side::New, &over)]);
        assert!(upgrade_file(&source, &job(FileStatus::Added, None, Some("exact"))).is_some());
        assert!(upgrade_file(&source, &job(FileStatus::Added, None, Some("over"))).is_none());
        let source = FakeSource::new(&[("exact", Side::Old, &exact), ("over", Side::Old, &over)]);
        assert!(upgrade_file(&source, &job(FileStatus::Deleted, Some("exact"), None)).is_some());
        assert!(upgrade_file(
            &source,
            &job(FileStatus::Modified, Some("over"), Some("unused"))
        )
        .is_none());
        assert_eq!(
            source.calls(),
            [("exact".into(), Side::Old), ("over".into(), Side::Old)]
        );
    }

    #[test]
    fn cap_is_enforced_before_crlf_normalization() {
        let exact = format!("{}\r\n", "x".repeat(MAX_UPGRADE_BLOB_BYTES - 2));
        let over = format!("{exact}x"); // Normalized size would fit exactly.
        let source = FakeSource::new(&[("exact", Side::New, &exact), ("over", Side::New, &over)]);
        let result = upgrade_file(&source, &job(FileStatus::Added, None, Some("exact"))).unwrap();
        assert_eq!(lines(&result)[0].len(), MAX_UPGRADE_BLOB_BYTES - 2);
        assert!(upgrade_file(&source, &job(FileStatus::Added, None, Some("over"))).is_none());
    }

    #[test]
    fn normalization_preserves_lone_cr_nul_and_rust_lines_semantics() {
        for (text, expected) in [
            ("", vec![]),
            ("\n", vec![""]),
            ("a", vec!["a"]),
            ("a\n", vec!["a"]),
            ("a\n\n", vec!["a", ""]),
            ("a\r\nb\r\n", vec!["a", "b"]),
            ("a\rb\r", vec!["a\rb\r"]),
            ("a\0b\n", vec!["a\0b"]),
        ] {
            let source = FakeSource::new(&[("file.txt", Side::New, text)]);
            let result =
                upgrade_file(&source, &job(FileStatus::Added, None, Some("file.txt"))).unwrap();
            assert_eq!(lines(&result), expected, "{text:?}");
            assert!(result.upgrade.new_spans.is_empty()); // Unknown language.
        }
    }

    #[test]
    fn crlf_hunks_counts_spans_and_row_gaps_share_normalized_coordinates() {
        let old: String = (1..=20).map(|i| format!("let n = {i};\r\n")).collect();
        let new = old.replace("let n = 10;", "let n = 100;");
        let source = FakeSource::new(&[("a.rs", Side::Old, &old), ("a.rs", Side::New, &new)]);
        let result = upgrade_file(
            &source,
            &job(FileStatus::Modified, Some("a.rs"), Some("a.rs")),
        )
        .unwrap();
        assert_eq!((result.additions, result.deletions), (1, 1));
        assert_eq!(result.hunks.len(), 1);
        let hunk = &result.hunks[0];
        assert_eq!(
            (
                hunk.old_start,
                hunk.new_start,
                hunk.old_count,
                hunk.new_count
            ),
            (7, 7, 7, 7)
        );
        // Highlight tables include an empty entry after the final newline;
        // new_lines intentionally follows Rust's .lines() semantics instead.
        assert_eq!(result.upgrade.old_spans.len(), 21);
        assert_eq!(result.upgrade.new_spans.len(), 21);
        assert!(result.upgrade.old_spans[20].is_empty());
        assert!(result.upgrade.new_spans[20].is_empty());
        assert_eq!(result.upgrade.new_lines.len(), 20);
        for row in &hunk.rows {
            let (no, text, spans) = match row {
                DiffRow::Added { new_no, text, .. } | DiffRow::Context { new_no, text, .. } => {
                    (*new_no, text, &result.upgrade.new_spans)
                }
                DiffRow::Removed { old_no, text, .. } => (*old_no, text, &result.upgrade.old_spans),
            };
            assert!(!text.contains('\r'));
            assert!(!spans[no as usize - 1].is_empty());
            assert!(spans[no as usize - 1]
                .iter()
                .all(|(range, _)| range.end <= text.len()));
        }
        assert!(lines(&result).iter().all(|line| !line.contains('\r')));
        let diff = PrDiff {
            files: vec![FileDiff {
                old_path: Some("a.rs".into()),
                new_path: Some("a.rs".into()),
                status: FileStatus::Modified,
                hunks: result.hunks,
                additions: result.additions,
                deletions: result.deletions,
            }],
        };
        let upgrades = HashMap::from([(0, result.upgrade)]);
        let built = build_rows(RowsView {
            diff: &diff,
            upgrades: &upgrades,
            comments: None,
            options: RowOptions {
                mode: ViewMode::Unified,
                file_ix: 0,
                comments_visible: false,
                comment_wrap: 80,
            },
        });
        assert_eq!(built.file_rows, [0]);
        let rendered: Vec<_> = built
            .rows
            .iter()
            .filter_map(|row| match row {
                Row::Line {
                    new_no: Some(no),
                    text,
                    ..
                } => Some((*no, text.as_ref())),
                _ => None,
            })
            .collect();
        assert_eq!(rendered.len(), 20);
        for ((no, text), expected) in rendered.iter().zip(new.replace("\r\n", "\n").lines()) {
            assert_eq!(*text, expected);
            assert_eq!(*text, upgrades[&0].new_lines[*no as usize - 1].as_ref());
        }
    }

    #[test]
    fn line_ending_only_changes_produce_no_hunks() {
        let source = FakeSource::new(&[
            ("a.txt", Side::Old, "a\r\nb\r\n"),
            ("a.txt", Side::New, "a\nb\n"),
        ]);
        let result = upgrade_file(
            &source,
            &job(FileStatus::Modified, Some("a.txt"), Some("a.txt")),
        )
        .unwrap();
        assert!(result.hunks.is_empty());
        assert_eq!((result.additions, result.deletions), (0, 0));
        assert_eq!(lines(&result), ["a", "b"]);
    }

    #[test]
    fn pool_has_exactly_four_workers_and_sequential_per_file_reads() {
        #[derive(Default)]
        struct State {
            threads: HashSet<std::thread::ThreadId>,
            arrivals: usize,
            active: usize,
            peak: usize,
            old_finished: HashSet<String>,
        }
        #[derive(Default)]
        struct PoolSource {
            state: Mutex<State>,
            ready: Condvar,
        }
        impl ContentSource for PoolSource {
            fn fetch(&self, path: &str, side: Side) -> Option<String> {
                let mut state = self.state.lock().unwrap();
                state.threads.insert(std::thread::current().id());
                state.active += 1;
                state.peak = state.peak.max(state.active);
                state.arrivals += 1;
                self.ready.notify_all();
                // Force the first wave to overlap without an unbounded barrier
                // hang if a regression reduces the worker count.
                let (mut state, timeout) = self
                    .ready
                    .wait_timeout_while(state, Duration::from_secs(5), |s| s.arrivals < 4)
                    .unwrap();
                assert!(!timeout.timed_out(), "four workers never arrived");
                if side == Side::New {
                    assert!(state.old_finished.contains(path));
                }
                drop(state);
                std::thread::yield_now();
                state = self.state.lock().unwrap();
                if side == Side::Old {
                    assert!(state.old_finished.insert(path.to_string()));
                }
                state.active -= 1;
                Some(String::new())
            }
        }
        let source = PoolSource::default();
        let jobs = (0..12)
            .map(|ix| UpgradeJob {
                file_ix: ix,
                ..job(
                    FileStatus::Modified,
                    Some(&ix.to_string()),
                    Some(&ix.to_string()),
                )
            })
            .collect();
        assert_eq!(run_with_source(&source, jobs).len(), 12);
        let state = source.state.lock().unwrap();
        assert_eq!(state.threads.len(), 4);
        assert_eq!(state.peak, 4);
        assert_eq!(state.active, 0);
        assert_eq!(state.arrivals, 24);
    }

    #[test]
    fn out_of_order_fetches_return_sorted_successes() {
        #[derive(Default)]
        struct ReorderedSource {
            returned: Mutex<Vec<String>>,
            ready: Condvar,
        }
        impl ContentSource for ReorderedSource {
            fn fetch(&self, path: &str, side: Side) -> Option<String> {
                assert_eq!(side, Side::New);
                let returned = self.returned.lock().unwrap();
                let mut returned = if path == "first" {
                    let (returned, timeout) = self
                        .ready
                        .wait_timeout_while(returned, Duration::from_secs(5), |r| r.is_empty())
                        .unwrap();
                    assert!(!timeout.timed_out());
                    returned
                } else {
                    returned
                };
                returned.push(path.to_string());
                self.ready.notify_all();
                Some(String::new())
            }
        }
        let source = ReorderedSource::default();
        let results = run_with_source(
            &source,
            vec![
                UpgradeJob {
                    file_ix: 2,
                    ..job(FileStatus::Added, None, Some("first"))
                },
                UpgradeJob {
                    file_ix: 9,
                    ..job(FileStatus::Added, None, Some("second"))
                },
            ],
        );
        assert_eq!(*source.returned.lock().unwrap(), ["second", "first"]);
        assert_eq!(
            results.iter().map(|r| r.file_ix).collect::<Vec<_>>(),
            [2, 9]
        );
    }

    #[test]
    fn first_four_hundred_jobs_are_considered_and_failure_does_not_refill_or_cancel() {
        let mut source = FakeSource::default();
        let jobs = (0..405)
            .map(|ix| {
                if ix != 1 {
                    source
                        .text
                        .insert((ix.to_string(), Side::New), String::new());
                }
                UpgradeJob {
                    file_ix: 1000 - ix,
                    ..job(FileStatus::Added, None, Some(&ix.to_string()))
                }
            })
            .collect();
        let result = run_with_source(&source, jobs);
        assert_eq!(result.len(), 399);
        assert!(result
            .windows(2)
            .all(|pair| pair[0].file_ix < pair[1].file_ix));
        assert!(result.iter().all(|r| r.file_ix >= 601 && r.file_ix != 999));
        let calls = source.calls();
        assert_eq!(calls.len(), 400);
        let considered: HashSet<_> = calls
            .iter()
            .map(|(p, side)| {
                assert_eq!(*side, Side::New);
                p.parse::<usize>().unwrap()
            })
            .collect();
        assert_eq!(considered, (0..400).collect());
    }

    #[derive(Default)]
    struct FileClient {
        calls: Mutex<Vec<(String, String, u64, String, String)>>,
    }

    impl github::GitHubClient for FileClient {
        fn file_at(
            &self,
            loc: &github::PrLocator,
            oid: &str,
            path: &str,
        ) -> Result<Option<String>, github::ClientError> {
            self.calls.lock().unwrap().push((
                loc.owner.clone(),
                loc.repo.clone(),
                loc.number,
                oid.to_string(),
                path.to_string(),
            ));
            match path {
                "missing" => Ok(None),
                "error" => Err(github::ClientError::Failed("test failure".into())),
                "empty" => Ok(Some(String::new())),
                _ => Ok(Some(format!("{oid}:{path}\n"))),
            }
        }
        fn viewer(&self) -> Result<github::Viewer, github::ClientError> {
            unreachable!()
        }
        fn search_prs(
            &self,
            _: &github::QueueSearch,
        ) -> Result<Vec<github::QueueItem>, github::ClientError> {
            unreachable!()
        }
        fn resolve_pr(&self, _: &str) -> Result<github::PrLocator, github::ClientError> {
            unreachable!()
        }
        fn pr_meta(&self, _: &github::PrLocator) -> Result<github::PrMeta, github::ClientError> {
            unreachable!()
        }
        fn list_prs(
            &self,
            _: &str,
            _: &str,
        ) -> Result<Vec<github::PrSummary>, github::ClientError> {
            unreachable!()
        }
        fn pr_patch(&self, _: &github::PrLocator) -> Result<String, github::ClientError> {
            unreachable!()
        }
        fn review_comments(
            &self,
            _: &github::PrLocator,
        ) -> Result<Vec<github::ReviewComment>, github::ClientError> {
            unreachable!()
        }
        fn mentionable_users(
            &self,
            _: &github::PrLocator,
        ) -> Result<Vec<github::Mention>, github::ClientError> {
            unreachable!()
        }
        fn post_review_comment(
            &self,
            _: &github::PrLocator,
            _: &str,
            _: &str,
            _: &str,
            _: u64,
            _: &str,
        ) -> Result<(), github::ClientError> {
            unreachable!()
        }
        fn post_reply(
            &self,
            _: &github::PrLocator,
            _: u64,
            _: &str,
        ) -> Result<(), github::ClientError> {
            unreachable!()
        }
        fn submit_review(
            &self,
            _: &github::PrLocator,
            _: github::ReviewVerdict,
            _: &str,
        ) -> Result<(), github::ClientError> {
            unreachable!()
        }
    }

    #[test]
    fn pr_adapter_routes_captured_oids_and_rename_paths_through_public_entry() {
        let client = FileClient::default();
        let loc = github::PrLocator {
            owner: "owner".into(),
            repo: "repo".into(),
            number: 42,
        };
        let source = UpgradeSource::Pr {
            loc,
            base_oid: "captured-base".into(),
            head_oid: "captured-head".into(),
        };
        let results = run_upgrade(
            &client,
            &source,
            vec![job(FileStatus::Renamed, Some("old.txt"), Some("new.txt"))],
        );
        assert_eq!(results.len(), 1);
        assert_eq!(lines(&results[0]), ["captured-head:new.txt"]);
        assert_eq!(
            *client.calls.lock().unwrap(),
            [
                (
                    "owner".into(),
                    "repo".into(),
                    42,
                    "captured-base".into(),
                    "old.txt".into()
                ),
                (
                    "owner".into(),
                    "repo".into(),
                    42,
                    "captured-head".into(),
                    "new.txt".into()
                ),
            ]
        );
    }

    #[test]
    fn pr_adapter_preserves_empty_none_and_error_policy() {
        let client = FileClient::default();
        let loc = github::PrLocator {
            owner: "o".into(),
            repo: "r".into(),
            number: 1,
        };
        let source = PrContent {
            github: &client,
            loc: &loc,
            base_oid: "base",
            head_oid: "head",
        };
        assert_eq!(source.fetch("empty", Side::Old), Some(String::new()));
        assert_eq!(source.fetch("missing", Side::Old), None);
        assert_eq!(source.fetch("error", Side::New), None);
    }

    struct TempRepo(std::path::PathBuf);

    impl TempRepo {
        fn new() -> Self {
            static NEXT_REPO: std::sync::atomic::AtomicUsize =
                std::sync::atomic::AtomicUsize::new(0);
            let sequence = NEXT_REPO.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let unique = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "gander-upgrade-{}-{unique}-{sequence}",
                std::process::id()
            ));
            std::fs::create_dir(&path).unwrap();
            let repo = Self(path);
            repo.git(&["init", "-b", "main"]);
            repo.git(&["config", "user.email", "test@example.com"]);
            repo.git(&["config", "user.name", "Test"]);
            repo.git(&["config", "commit.gpgsign", "false"]);
            repo
        }

        fn git(&self, args: &[&str]) {
            let output = std::process::Command::new("git")
                .arg("-C")
                .arg(&self.0)
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{args:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }

    impl Drop for TempRepo {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[test]
    fn local_adapter_reads_captured_base_but_current_working_tree() {
        let repo = TempRepo::new();
        std::fs::write(repo.0.join("old.rs"), "fn base() {}\n").unwrap();
        repo.git(&["add", "."]);
        repo.git(&["commit", "-m", "base"]);
        let source = git::resolve_local_with_base(&repo.0, Some("HEAD")).unwrap();
        std::fs::write(repo.0.join("old.rs"), "fn later_commit() {}\n").unwrap();
        repo.git(&["add", "."]);
        repo.git(&["commit", "-m", "move HEAD"]);
        std::fs::rename(repo.0.join("old.rs"), repo.0.join("new.rs")).unwrap();
        std::fs::write(repo.0.join("new.rs"), "fn working_tree() {}\n").unwrap();
        let adapter = LocalContent { source: &source };
        assert_eq!(
            adapter.fetch("old.rs", Side::Old).as_deref(),
            Some("fn base() {}\n")
        );
        assert_eq!(adapter.fetch("old.rs", Side::New), None);
        assert_eq!(adapter.fetch("new.rs", Side::Old), None);
        assert_eq!(
            adapter.fetch("new.rs", Side::New).as_deref(),
            Some("fn working_tree() {}\n")
        );
        let source = UpgradeSource::Local(source);
        let client = FileClient::default(); // No GitHub call is made for local upgrades.
        let results = run_upgrade(
            &client,
            &source,
            vec![job(FileStatus::Renamed, Some("old.rs"), Some("new.rs"))],
        );
        assert_eq!(results.len(), 1);
        assert_eq!(lines(&results[0]), ["fn working_tree() {}"]);
        assert!(client.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn local_adapter_collapses_missing_non_utf8_and_no_base_but_keeps_empty() {
        let repo = TempRepo::new();
        std::fs::write(repo.0.join("empty"), "").unwrap();
        std::fs::write(repo.0.join("non-utf8"), [0xff]).unwrap();
        let source = git::LocalSource {
            repo_root: repo.0.clone(),
            branch: "main".into(),
            base_ref: None,
            base_label: "HEAD".into(),
            base_oid: None,
        };
        let adapter = LocalContent { source: &source };
        assert_eq!(adapter.fetch("empty", Side::New), Some(String::new()));
        assert_eq!(adapter.fetch("empty", Side::Old), None);
        assert_eq!(adapter.fetch("non-utf8", Side::New), None);
        assert_eq!(adapter.fetch("missing", Side::New), None);
        assert_eq!(adapter.fetch(".", Side::New), None);
        repo.git(&["add", "."]);
        repo.git(&["commit", "-m", "files"]);
        let source = git::resolve_local_with_base(&repo.0, Some("HEAD")).unwrap();
        let adapter = LocalContent { source: &source };
        assert_eq!(adapter.fetch("empty", Side::Old), Some(String::new()));
        assert_eq!(adapter.fetch("non-utf8", Side::Old), None);
        assert_eq!(adapter.fetch("missing", Side::Old), None);
    }
}
