//! The sidebar file tree: paths flattened depth-first, collapse/visibility, and row rendering.

use std::collections::HashSet;

use crate::{theme, threads::CommentIndex, ItemData, ReviewApp};
use diff_core::{FileDiff, FileStatus};
use fuzzy_matcher::{skim::SkimMatcherV2, FuzzyMatcher};
use gpui::component::{checkbox::Checkbox, Disableable as _, Icon, Sizable as _};
use gpui::{div, prelude::*, px, Hsla, SharedString};

/// Row height of sidebar file-tree entries.
const TREE_ROW_HEIGHT: f32 = 24.0;

/// One row of the file tree, flattened depth-first for a uniform_list.
/// Directories precede files at each level; both are name-sorted.
#[derive(Debug, PartialEq)]
pub(crate) struct TreeEntry {
    pub(crate) depth: usize,
    /// Display name: a file name, or a compressed single-child directory
    /// chain ("src/core/flags").
    pub(crate) name: SharedString,
    pub(crate) kind: TreeEntryKind,
}

#[derive(Debug, PartialEq)]
pub(crate) enum TreeEntryKind {
    /// Full path of the chain's deepest directory — the collapse-state key.
    Dir { path: String },
    /// Index into `PrDiff::files` (and thus `ItemData::file_rows`).
    File { file_ix: usize },
}

/// Group file paths (diff order) into a depth-first tree. Directory chains
/// with a single child directory and no files of their own compress into one
/// entry, GitHub-style.
pub(crate) fn build_tree(paths: &[&str]) -> Vec<TreeEntry> {
    #[derive(Default)]
    struct DirNode {
        dirs: std::collections::BTreeMap<String, DirNode>,
        files: Vec<(String, usize)>,
    }
    let mut root = DirNode::default();
    for (file_ix, path) in paths.iter().enumerate() {
        let (dirs, name) = match path.rsplit_once('/') {
            Some((dirs, name)) => (Some(dirs), name),
            None => (None, *path),
        };
        let mut node = &mut root;
        for part in dirs.into_iter().flat_map(|dirs| dirs.split('/')) {
            node = node.dirs.entry(part.to_string()).or_default();
        }
        node.files.push((name.to_string(), file_ix));
    }
    fn flatten(node: DirNode, prefix: &str, depth: usize, out: &mut Vec<TreeEntry>) {
        for (name, mut child) in node.dirs {
            let mut label = name;
            let mut path = if prefix.is_empty() {
                label.clone()
            } else {
                format!("{prefix}/{label}")
            };
            while child.files.is_empty() && child.dirs.len() == 1 {
                let (next_name, next) = child.dirs.into_iter().next().unwrap();
                label.push('/');
                label.push_str(&next_name);
                path.push('/');
                path.push_str(&next_name);
                child = next;
            }
            out.push(TreeEntry {
                depth,
                name: label.into(),
                kind: TreeEntryKind::Dir { path: path.clone() },
            });
            flatten(child, &path, depth + 1, out);
        }
        let mut files = node.files;
        files.sort();
        for (name, file_ix) in files {
            out.push(TreeEntry {
                depth,
                name: name.into(),
                kind: TreeEntryKind::File { file_ix },
            });
        }
    }
    let mut out = Vec::new();
    flatten(root, "", 0, &mut out);
    out
}

/// Indices of the entries visible given the collapsed directories: everything
/// deeper than a collapsed dir (until the next entry at its depth) is hidden.
pub(crate) fn visible_entries(entries: &[TreeEntry], collapsed: &HashSet<String>) -> Vec<usize> {
    let mut out = Vec::with_capacity(entries.len());
    let mut hide_deeper_than: Option<usize> = None;
    for (ix, entry) in entries.iter().enumerate() {
        if let Some(depth) = hide_deeper_than {
            if entry.depth > depth {
                continue;
            }
            hide_deeper_than = None;
        }
        out.push(ix);
        if let TreeEntryKind::Dir { path } = &entry.kind {
            if collapsed.contains(path) {
                hide_deeper_than = Some(entry.depth);
            }
        }
    }
    out
}

/// File indices whose full path fuzzy-matches `query`, best score first.
/// An empty query keeps every file in diff order.
pub(crate) fn fuzzy_file_matches(paths: &[&str], query: &str) -> Vec<usize> {
    let query = query.trim();
    if query.is_empty() {
        return (0..paths.len()).collect();
    }
    let matcher = SkimMatcherV2::default();
    let mut scored: Vec<(i64, usize)> = paths
        .iter()
        .enumerate()
        .filter_map(|(ix, path)| matcher.fuzzy_match(path, query).map(|score| (score, ix)))
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    scored.into_iter().map(|(_, ix)| ix).collect()
}

/// Status label for file headers and color shared with tree entries.
pub(crate) fn status_style(status: FileStatus) -> (&'static str, gpui::Rgba) {
    match status {
        FileStatus::Added => ("added", theme::green()),
        FileStatus::Deleted => ("deleted", theme::red()),
        FileStatus::Modified => ("modified", theme::blue()),
        FileStatus::Renamed => ("renamed", theme::mauve()),
        FileStatus::Binary => ("binary", theme::peach()),
    }
}

/// One row of the sidebar's tree list: an index into `ItemData::tree`, or —
/// while the fuzzy filter is active — a matching file shown as its full path.
#[derive(Clone, Copy)]
pub(crate) enum TreeListRow {
    Entry(usize),
    FilteredFile(usize),
}

/// Count comments, not threads, including replies, outdated comments and drafts.
fn file_comment_count(file: &FileDiff, comments: Option<&CommentIndex>) -> usize {
    comments
        .and_then(|index| index.counts.get(file.display_path()))
        .map_or(0, |(anchored, outdated)| anchored + outdated)
}

/// `current` marks the file the diff viewport is showing (surface0, like the
/// active sidebar item). Clicking a dir toggles collapse; clicking a file
/// jumps the diff to its header.
pub(crate) fn render_tree_row(
    row: TreeListRow,
    pos: usize,
    current: bool,
    data: &ItemData,
    entity: &gpui::Entity<ReviewApp>,
    reloading: bool,
) -> gpui::AnyElement {
    let comments = |file_ix: usize| {
        let count = file_comment_count(&data.diff.files[file_ix], data.comments.as_ref());
        (count > 0).then(|| {
            div()
                .debug_selector(move || format!("file-comments-{file_ix}"))
                .flex_shrink_0()
                .flex()
                .items_center()
                .gap_1()
                .text_size(px(10.))
                .text_color(theme::subtext())
                .child(
                    Icon::default()
                        .path("icons/gander/comment-16.svg")
                        .xsmall()
                        .text_color(theme::overlay0()),
                )
                .child(SharedString::from(count.to_string()))
        })
    };
    let checkbox = |file_ix: usize| {
        let path = data.diff.files[file_ix].display_path();
        let state = data.viewed.state(path);
        let entity = entity.clone();
        div()
            .id(("viewed-control", file_ix))
            .debug_selector(move || format!("viewed-{file_ix}"))
            .flex_shrink_0()
            .flex()
            .items_center()
            .pl_1()
            // A checkbox click must never also open its file, including
            // when disabled during a mutation or refresh.
            .on_click(|_, _, cx| cx.stop_propagation())
            .child(
                Checkbox::new(("file-viewed", file_ix))
                    .xsmall()
                    .checked(state == Some(github::FileViewedState::Viewed))
                    .disabled(
                        reloading
                            || state.is_none()
                            || data.viewed.pending.contains(path)
                            || data.viewed.uncertain.contains(path),
                    )
                    .on_click(move |_, window, cx| {
                        cx.stop_propagation();
                        entity.update(cx, |this, cx| {
                            window.focus(&this.focus_handle, cx);
                            this.toggle_file_viewed(file_ix, cx);
                        });
                    }),
            )
    };
    let stats = |file: &FileDiff| {
        div()
            .flex()
            .items_center()
            .gap_1()
            .flex_shrink_0()
            .text_size(px(10.))
            .child(
                div()
                    .text_color(Hsla::from(theme::green()).opacity(0.7))
                    .child(SharedString::from(format!("+{}", file.additions))),
            )
            .child(
                div()
                    .text_color(Hsla::from(theme::red()).opacity(0.7))
                    .child(SharedString::from(format!("−{}", file.deletions))),
            )
    };
    let entity = entity.clone();
    let base = div()
        .id(("tree-row", pos))
        .debug_selector(move || format!("tree-row-{pos}"))
        .h(px(TREE_ROW_HEIGHT))
        .w_full()
        .flex()
        .items_center()
        .gap_1()
        .pr_2()
        .cursor_pointer()
        .when(current, |row| row.bg(theme::surface0()))
        .when(!current, |row| {
            row.hover(|style| style.bg(Hsla::from(theme::surface0()).opacity(0.5)))
        });
    match row {
        TreeListRow::Entry(entry_ix) => {
            let entry = &data.tree[entry_ix];
            let indent = px(8. + entry.depth as f32 * 12.);
            let base = base.pl(indent).on_click(move |_, window, cx| {
                entity.update(cx, |this, cx| this.tree_entry_clicked(entry_ix, window, cx));
            });
            match &entry.kind {
                TreeEntryKind::Dir { path } => {
                    let chevron = if data.collapsed.contains(path) {
                        "▸"
                    } else {
                        "▾"
                    };
                    base.child(
                        div()
                            .w(px(12.))
                            .flex_shrink_0()
                            .text_color(theme::overlay0())
                            .child(SharedString::from(chevron)),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_color(theme::overlay0())
                            .child(entry.name.clone()),
                    )
                    .into_any_element()
                }
                TreeEntryKind::File { file_ix } => {
                    let file = &data.diff.files[*file_ix];
                    let (_, status_color) = status_style(file.status);
                    base.child(div().w(px(12.)).flex_shrink_0()) // aligns with dir chevrons
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_color(status_color)
                                .child(entry.name.clone()),
                        )
                        .children(comments(*file_ix))
                        .child(stats(file))
                        .when(data.pr_meta.is_some(), |row| row.child(checkbox(*file_ix)))
                        .into_any_element()
                }
            }
        }
        TreeListRow::FilteredFile(file_ix) => {
            let file = &data.diff.files[file_ix];
            let (_, status_color) = status_style(file.status);
            base.pl_2()
                .on_click(move |_, window, cx| {
                    entity.update(cx, |this, cx| this.jump_to_file(file_ix, window, cx));
                })
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_color(status_color)
                        .child(SharedString::from(file.display_path().to_string())),
                )
                .children(comments(file_ix))
                .child(stats(file))
                .when(data.pr_meta.is_some(), |row| row.child(checkbox(file_ix)))
                .into_any_element()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_comment_counts_include_outdated_and_use_display_path() {
        let mut file = crate::test_util::sample_diff().files.remove(0);
        let mut index = CommentIndex::default();
        index.counts.insert("a.rs".into(), (2, 3));
        index.counts.insert("renamed.rs".into(), (4, 0));
        assert_eq!(file_comment_count(&file, Some(&index)), 5);
        assert_eq!(file_comment_count(&file, None), 0);

        file.status = FileStatus::Renamed;
        file.new_path = Some("renamed.rs".into());
        assert_eq!(file_comment_count(&file, Some(&index)), 4);

        file.status = FileStatus::Deleted;
        file.new_path = None;
        assert_eq!(file_comment_count(&file, Some(&index)), 5);

        index.counts.insert("a.rs".into(), (0, 3));
        assert_eq!(file_comment_count(&file, Some(&index)), 3);
        index.counts.clear();
        assert_eq!(file_comment_count(&file, Some(&index)), 0);
    }

    /// (depth, display name, Some(file_ix) for files / None for dirs).
    fn flat(entries: &[TreeEntry]) -> Vec<(usize, &str, Option<usize>)> {
        entries
            .iter()
            .map(|e| {
                let file_ix = match &e.kind {
                    TreeEntryKind::Dir { .. } => None,
                    TreeEntryKind::File { file_ix } => Some(*file_ix),
                };
                (e.depth, e.name.as_ref(), file_ix)
            })
            .collect()
    }

    fn dir_paths(entries: &[TreeEntry]) -> Vec<&str> {
        entries
            .iter()
            .filter_map(|e| match &e.kind {
                TreeEntryKind::Dir { path } => Some(path.as_str()),
                TreeEntryKind::File { .. } => None,
            })
            .collect()
    }

    #[test]
    fn tree_nests_dirs_first_and_sorts_names() {
        let tree = build_tree(&["src/main.rs", "README.md", "src/lib.rs"]);
        assert_eq!(
            flat(&tree),
            vec![
                (0, "src", None),
                (1, "lib.rs", Some(2)),
                (1, "main.rs", Some(0)),
                (0, "README.md", Some(1)),
            ]
        );
        // No directories at all: a flat, sorted file list.
        let tree = build_tree(&["b.txt", "a.txt"]);
        assert_eq!(
            flat(&tree),
            vec![(0, "a.txt", Some(1)), (0, "b.txt", Some(0))]
        );
    }

    #[test]
    fn tree_compresses_single_child_dir_chains() {
        // core→flags has one child dir and no files: compressed. src has a
        // file of its own, so it is not folded into the chain.
        let tree = build_tree(&[
            "src/core/flags/defs.rs",
            "src/core/flags/parse.rs",
            "src/main.rs",
        ]);
        assert_eq!(
            flat(&tree),
            vec![
                (0, "src", None),
                (1, "core/flags", None),
                (2, "defs.rs", Some(0)),
                (2, "parse.rs", Some(1)),
                (1, "main.rs", Some(2)),
            ]
        );
        // The collapse key is the chain's full path.
        assert_eq!(dir_paths(&tree), vec!["src", "src/core/flags"]);

        // A chain starting at the root compresses too.
        let tree = build_tree(&["a/b/c.txt"]);
        assert_eq!(flat(&tree), vec![(0, "a/b", None), (1, "c.txt", Some(0))]);
        assert_eq!(dir_paths(&tree), vec!["a/b"]);

        // A dir with its own files stops the chain even with one child dir.
        let tree = build_tree(&["a/f.txt", "a/b/g.txt"]);
        assert_eq!(
            flat(&tree),
            vec![
                (0, "a", None),
                (1, "b", None),
                (2, "g.txt", Some(1)),
                (1, "f.txt", Some(0)),
            ]
        );
    }

    #[test]
    fn collapse_hides_subtrees() {
        let tree = build_tree(&[
            "src/core/flags/defs.rs",
            "src/core/flags/parse.rs",
            "src/main.rs",
            "README.md",
        ]);
        // flat: [src, core/flags, defs, parse, main.rs, README.md]
        let none = HashSet::new();
        assert_eq!(visible_entries(&tree, &none), vec![0, 1, 2, 3, 4, 5]);

        // Collapsing the inner chain hides its files but keeps the sibling.
        let inner = HashSet::from(["src/core/flags".to_string()]);
        assert_eq!(visible_entries(&tree, &inner), vec![0, 1, 4, 5]);

        // Collapsing src hides everything under it, collapsed child included.
        let outer = HashSet::from(["src".to_string(), "src/core/flags".to_string()]);
        assert_eq!(visible_entries(&tree, &outer), vec![0, 5]);
    }

    #[test]
    fn fuzzy_filter_flattens_and_ranks() {
        let paths = ["src/core/flags/defs.rs", "src/main.rs", "docs/guide.md"];
        assert_eq!(fuzzy_file_matches(&paths, "defs"), vec![0]);
        assert_eq!(fuzzy_file_matches(&paths, "guide"), vec![2]);
        assert!(fuzzy_file_matches(&paths, "zzzqqq").is_empty());
        // Empty / whitespace query: everything, diff order.
        assert_eq!(fuzzy_file_matches(&paths, ""), vec![0, 1, 2]);
        assert_eq!(fuzzy_file_matches(&paths, "  "), vec![0, 1, 2]);
        // Best score first: the contiguous match beats the spread-out one.
        let paths = ["main_test.rs", "main.rs"];
        assert_eq!(fuzzy_file_matches(&paths, "main.rs"), vec![1, 0]);
    }

    #[test]
    fn status_style_matches_file_header_tags() {
        assert_eq!(status_style(FileStatus::Added), ("added", theme::green()));
        assert_eq!(status_style(FileStatus::Deleted), ("deleted", theme::red()));
        assert_eq!(
            status_style(FileStatus::Modified),
            ("modified", theme::blue())
        );
        assert_eq!(
            status_style(FileStatus::Renamed),
            ("renamed", theme::mauve())
        );
        assert_eq!(status_style(FileStatus::Binary), ("binary", theme::peach()));
    }
}
