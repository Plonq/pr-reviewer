//! App chrome: the centered placeholder, the window title, and the cmd-k palette.

use crate::{theme, ReviewApp};
use gpui::{div, prelude::*, px, Hsla, SharedString};
use std::path::PathBuf;

pub(crate) fn centered_message(text: SharedString, color: gpui::Rgba) -> gpui::AnyElement {
    div()
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .text_color(color)
        .child(text)
        .into_any_element()
}

pub(crate) fn app_title(detail: Option<String>) -> gpui::AnyElement {
    let mut title = div()
        .flex()
        .items_center()
        .gap_2()
        .flex_1()
        .min_w_0()
        .child(
            div()
                .font_weight(gpui::FontWeight::BOLD)
                .child(SharedString::from("Gander")),
        );
    if let Some(detail) = detail {
        title = title.child(
            div()
                .text_color(theme::subtext())
                .truncate()
                .child(SharedString::from(detail)),
        );
    }
    title.into_any_element()
}

/// The cmd-k command palette: a staged flow for opening things.
pub(crate) enum PaletteStep {
    /// Step 1: pick what to open.
    Sources { selected: usize },
    /// Step 2 (GitHub path): type `owner/repo`.
    RepoInput { error: Option<SharedString> },
    /// Step 3 (GitHub path): pick a PR from the repo's open list.
    PrList { repo: String, prs: PrListState },
    /// Local path: pick the base ref for an already-open local item.
    LocalBaseList {
        item_id: u64,
        repo_root: PathBuf,
        current: String,
        bases: BaseListState,
    },
}

pub(crate) enum PrListState {
    Loading,
    Loaded {
        all: Vec<github::PrSummary>,
        /// Indices into `all`, fuzzy-filtered by the query, best match first.
        filtered: Vec<usize>,
        /// Index into `filtered`.
        selected: usize,
    },
    Failed(String),
}

pub(crate) enum BaseListState {
    Loading,
    Loaded {
        all: Vec<String>,
        /// Indices into `all`, fuzzy-filtered by the query, best match first.
        filtered: Vec<usize>,
        /// Index into `filtered`.
        selected: usize,
    },
    Failed(String),
}

pub(crate) const PALETTE_SOURCES: [&str; 2] = ["Open GitHub pull request", "Open local folder"];
pub(crate) const SOURCE_PR: usize = 0;
pub(crate) const SOURCE_FOLDER: usize = 1;
pub(crate) const PALETTE_ROW_HEIGHT: f32 = 30.0;

/// Step-1 options matching the query (case-insensitive substring), as indices
/// into PALETTE_SOURCES. Empty query keeps both.
pub(crate) fn filtered_sources(query: &str) -> Vec<usize> {
    let q = query.trim().to_lowercase();
    PALETTE_SOURCES
        .iter()
        .enumerate()
        .filter(|(_, label)| q.is_empty() || label.to_lowercase().contains(&q))
        .map(|(ix, _)| ix)
        .collect()
}

/// One row of the palette's PR list: state dot, #number, title, author, head
/// branch. Clicking opens the PR just like enter does.
pub(crate) fn palette_pr_row(
    pr: &github::PrSummary,
    pos: usize,
    selected: bool,
    entity: gpui::Entity<ReviewApp>,
) -> gpui::AnyElement {
    let dot = if pr.is_draft {
        theme::overlay0()
    } else {
        theme::green()
    };
    div()
        .id(("palette-pr", pos))
        .mx_1()
        .px_2()
        .h(px(PALETTE_ROW_HEIGHT))
        .rounded_md()
        .flex()
        .items_center()
        .gap_2()
        .cursor_pointer()
        .when(selected, |row| row.bg(theme::surface0()))
        .when(!selected, |row| {
            row.hover(|style| style.bg(Hsla::from(theme::surface0()).opacity(0.5)))
        })
        .on_click(move |_, window, cx| {
            entity.update(cx, |this, cx| this.palette_open_pr_row(pos, window, cx));
        })
        .child(
            div()
                .w(px(8.))
                .h(px(8.))
                .flex_shrink_0()
                .rounded_full()
                .bg(dot),
        )
        .child(
            div()
                .flex_shrink_0()
                .text_color(theme::subtext())
                .child(SharedString::from(format!("#{}", pr.number))),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_color(theme::text())
                .child(SharedString::from(pr.title.clone())),
        )
        .child(
            div()
                .flex_shrink_0()
                .text_color(theme::subtext())
                .child(SharedString::from(pr.author.login.clone())),
        )
        .child(
            div()
                .flex_shrink_0()
                .max_w(px(140.))
                .truncate()
                .text_color(theme::overlay0())
                .child(SharedString::from(pr.head_ref_name.clone())),
        )
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_options_filter_by_substring() {
        assert_eq!(filtered_sources(""), vec![0, 1]);
        assert_eq!(filtered_sources("pull"), vec![0]);
        assert_eq!(filtered_sources("FOLDER"), vec![1]);
        assert_eq!(filtered_sources("open"), vec![0, 1]);
        assert!(filtered_sources("nope").is_empty());
    }
}
