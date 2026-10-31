use super::*;
use crate::rows::{build_presented_rows, RowOptions, RowsView, ViewMode};

fn patch() -> PrDiff {
    diff_core::parse_patch("diff --git a/needle.txt b/needle.txt\n--- a/needle.txt\n+++ b/needle.txt\n@@ -1,3 +1,3 @@\n shared\n-old é世界 old\n+new é世界 new\n tail\ndiff --git a/other b/other\n--- a/other\n+++ b/other\n@@ -1 +1 @@\n-old\n+elsewhere\n")
}

fn find(diff: &PrDiff, scope: SearchScope, query: &str) -> Matches {
    Matches::find(diff, &HashMap::new(), &HashSet::new(), scope, 0, query)
}

fn rows(
    diff: &PrDiff,
    upgrades: &HashMap<usize, FileUpgrade>,
    mode: ViewMode,
    wrap: bool,
    file: usize,
) -> Vec<Row> {
    build_presented_rows(
        RowsView {
            diff,
            upgrades,
            comments: None,
            options: RowOptions {
                mode,
                file_ix: file,
                comments_visible: false,
                comment_wrap: 80,
            },
        },
        store::DiffPreferences {
            soft_wrap: Some(wrap),
            ..Default::default()
        },
        &HashSet::new(),
        6,
    )
    .rows
}

#[test]
fn literal_unicode_search_excludes_headers_and_obeys_scope() {
    let diff = patch();
    for query in ["", "needle", "NEW", ".*", "sharedold"] {
        assert!(
            find(&diff, SearchScope::AllFiles, query)
                .occurrences
                .is_empty(),
            "{query}"
        );
    }
    assert!(find(&diff, SearchScope::CurrentFile, "elsewhere")
        .occurrences
        .is_empty());
    assert_eq!(
        find(&diff, SearchScope::CurrentFile, "old")
            .occurrences
            .len(),
        2
    );
    let found = find(&diff, SearchScope::CurrentFile, "é世界");
    assert_eq!(found.occurrences.len(), 2);
    assert!(found.occurrences[0].removed);
    assert!(!found.occurrences[1].removed);
    assert_eq!(
        find(&diff, SearchScope::AllFiles, "old").occurrences.len(),
        3
    );
    for scope in [SearchScope::AllFiles, SearchScope::ChangedLines] {
        let found = find(&diff, scope, "elsewhere");
        assert_eq!(found.occurrences.len(), 1);
        assert_eq!(found.occurrences[0].file, 1);
    }
    assert_eq!(
        find(&diff, SearchScope::AllFiles, "shared")
            .occurrences
            .len(),
        1
    );
    assert!(find(&diff, SearchScope::ChangedLines, "shared")
        .occurrences
        .is_empty());
}

#[test]
fn projections_are_unicode_safe_across_wraps_in_both_views() {
    let diff = patch();
    for mode in [ViewMode::Unified, ViewMode::Split] {
        for wrap in [false, true] {
            let rows = rows(&diff, &HashMap::new(), mode, wrap, 0);
            let mut found = find(&diff, SearchScope::AllFiles, "é世界");
            found.project(&rows, 0);
            for ix in 0..2 {
                let spans: Vec<_> = found
                    .by_row
                    .iter()
                    .flatten()
                    .filter(|(id, _)| *id == ix)
                    .map(|(_, s)| s)
                    .collect();
                assert_eq!(spans.len(), if wrap { 2 } else { 1 });
                let text: String = spans
                    .iter()
                    .map(|s| &row_side_text(&rows[s.row], s.side).unwrap()[s.range.clone()])
                    .collect();
                assert_eq!(text, "é世界");
            }
            // Reprojection to another file removes all stale highlights.
            found.project(&rows, 1);
            assert!(found.by_row.iter().all(Vec::is_empty));
        }
    }
}

#[test]
fn full_file_gaps_and_split_context_are_indexed_once() {
    let old: String = (1..=30).map(|n| format!("line {n}\n")).collect();
    let new = old.replace("line 12\n", "changed 12\n");
    let diff = PrDiff {
        files: vec![diff_core::FileDiff {
            old_path: Some("a.txt".into()),
            new_path: Some("a.txt".into()),
            status: FileStatus::Modified,
            hunks: diff_core::diff_texts(&old, &new, 3),
            additions: 1,
            deletions: 1,
        }],
    };
    let upgrades = HashMap::from([(
        0,
        FileUpgrade {
            new_lines: new.lines().map(|line| line.to_string().into()).collect(),
            old_spans: vec![],
            new_spans: vec![],
        },
    )]);
    for query in ["line 1", "line 30"] {
        for mode in [ViewMode::Unified, ViewMode::Split] {
            let mut found = Matches::find(
                &diff,
                &upgrades,
                &HashSet::new(),
                SearchScope::AllFiles,
                0,
                query,
            );
            assert_eq!(found.patch_only, 0);
            let built = rows(&diff, &upgrades, mode, true, 0);
            found.project(&built, 0);
            for ix in 0..found.occurrences.len() {
                assert!(found.by_row.iter().flatten().any(|(id, _)| *id == ix));
            }
        }
    }
    let mut found = Matches::find(
        &diff,
        &upgrades,
        &HashSet::new(),
        SearchScope::AllFiles,
        0,
        "line 30",
    );
    assert_eq!(found.occurrences.len(), 1);
    found.project(&rows(&diff, &upgrades, ViewMode::Split, false, 0), 0);
    assert_eq!(found.by_row.iter().flatten().count(), 2);
    for query in ["line 1", "line 30"] {
        let found = Matches::find(
            &diff,
            &upgrades,
            &HashSet::new(),
            SearchScope::ChangedLines,
            0,
            query,
        );
        assert_eq!(found.occurrences.len(), usize::from(query == "line 1")); // removed line 12
    }
}

#[test]
fn navigation_wraps_across_files_and_empty_results_are_safe() {
    let mut found = find(&patch(), SearchScope::AllFiles, "old");
    assert_eq!(found.current, Some(0));
    found.step(-1);
    assert_eq!(found.current, Some(2));
    assert_eq!(found.occurrences[2].file, 1);
    found.step(1);
    assert_eq!(found.current, Some(0));
    found.step(1);
    assert_eq!(found.current, Some(1));
    let mut empty = find(&patch(), SearchScope::AllFiles, "absent");
    empty.step(1);
    empty.step(-1);
    assert_eq!(empty.current, None);
}

#[test]
fn large_files_need_explicit_expansion_and_patch_only_coverage_is_reported() {
    let diff = patch();
    let upgrades = HashMap::from([(
        0,
        FileUpgrade {
            new_lines: (0..20_000).map(|_| "shared".into()).collect(),
            old_spans: vec![],
            new_spans: vec![],
        },
    )]);
    let found = Matches::find(
        &diff,
        &upgrades,
        &HashSet::new(),
        SearchScope::AllFiles,
        0,
        "é世界",
    );
    assert_eq!(found.skipped_large, 1);
    assert_eq!(found.patch_only, 1);
    assert!(found.occurrences.is_empty());
    let expanded = HashSet::from([(0, 0)]);
    let found = Matches::find(
        &diff,
        &upgrades,
        &expanded,
        SearchScope::AllFiles,
        0,
        "é世界",
    );
    assert_eq!(found.skipped_large, 0);
    assert_eq!(found.occurrences.len(), 2);
    let found = Matches::find(
        &diff,
        &upgrades,
        &expanded,
        SearchScope::ChangedLines,
        0,
        "é世界",
    );
    assert_eq!(found.patch_only, 0);
    assert_eq!(found.occurrences.len(), 2);
}

#[test]
fn added_deleted_renamed_and_binary_files() {
    let diff = diff_core::parse_patch("diff --git a/added b/added\nnew file mode 100644\n--- /dev/null\n+++ b/added\n@@ -0,0 +1 @@\n+needle\ndiff --git a/deleted b/deleted\ndeleted file mode 100644\n--- a/deleted\n+++ /dev/null\n@@ -1 +0,0 @@\n-needle\ndiff --git a/before b/after\nsimilarity index 50%\nrename from before\nrename to after\n--- a/before\n+++ b/after\n@@ -1 +1 @@\n-needle\n+needle new\ndiff --git a/binary b/binary\nBinary files a/binary and b/binary differ\n");
    let found = find(&diff, SearchScope::ChangedLines, "needle");
    assert_eq!(
        found
            .occurrences
            .iter()
            .map(|m| (m.file, m.removed))
            .collect::<Vec<_>>(),
        [(0, false), (1, true), (2, true), (2, false)]
    );
    assert_eq!(
        find(&diff, SearchScope::AllFiles, "binary")
            .occurrences
            .len(),
        0
    );
}
