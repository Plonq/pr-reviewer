//! The review queue: PR filtering, quick-filter chips, and row rendering.

use crate::threads::short_age;
use crate::{theme, ReviewApp};
use gpui::component::{avatar::Avatar, tag::Tag, Icon, IconName, Sizable as _};
use gpui::{div, prelude::*, px, Hsla, Rgba, SharedString};

/// A bounded reading surface, not an edge-to-edge spreadsheet. Below the
/// minimum width the queue deliberately keeps its desktop column layout.
pub(crate) const QUEUE_MAX_WIDTH: f32 = 1200.;
pub(crate) const QUEUE_MIN_WIDTH: f32 = 1040.;
const AUTHOR_WIDTH: f32 = 152.;
const STATUS_WIDTH: f32 = 160.;
const CHANGES_WIDTH: f32 = 112.;
const AGE_WIDTH: f32 = 136.;

/// Shared geometry for the header and every row. Only the PR column grows;
/// descriptive content wraps vertically instead of stealing another column's space.
fn columns() -> gpui::Div {
    div().w_full().flex().items_start().gap(px(20.)).px_3()
}

fn cell(width: f32) -> gpui::Div {
    div()
        .w(px(width))
        .flex_shrink_0()
        .min_w_0()
        .flex()
        .flex_col()
        .gap_1()
}

pub(crate) fn queue_header(cx: &gpui::App) -> gpui::AnyElement {
    columns()
        .py_2()
        .flex_shrink_0()
        .border_b_1()
        .border_color(theme::surface0(cx))
        .text_size(px(11.))
        .text_color(theme::subtext(cx))
        .child(div().flex_1().min_w_0().child("Pull request"))
        .child(
            cell(AUTHOR_WIDTH)
                .debug_selector(|| "queue-header-author".into())
                .child("Author"),
        )
        .child(
            cell(STATUS_WIDTH)
                .debug_selector(|| "queue-header-status".into())
                .child("Status"),
        )
        .child(
            cell(CHANGES_WIDTH)
                .debug_selector(|| "queue-header-changes".into())
                .text_right()
                .child("Changes"),
        )
        .child(
            cell(AGE_WIDTH)
                .debug_selector(|| "queue-header-age".into())
                .text_right()
                .child("Age"),
        )
        .into_any_element()
}

/// The CI verdict's paint: green opens without fear, red means
/// expect breakage, yellow means checks still running, gray means no
/// signal — no checks configured, or the enrichment couldn't see them.
fn ci_color(ci: github::CiState, cx: &gpui::App) -> Rgba {
    match ci {
        github::CiState::Pass => theme::green(cx),
        github::CiState::Fail => theme::red(cx),
        github::CiState::Pending => theme::yellow(cx),
        github::CiState::NoChecks => theme::overlay0(cx),
    }
}

/// A CI verdict needs a label and a distinct icon, not color alone.
fn ci_label(ci: github::CiState) -> (IconName, &'static str) {
    match ci {
        github::CiState::Pass => (IconName::CircleCheck, "CI passed"),
        github::CiState::Fail => (IconName::CircleX, "CI failed"),
        github::CiState::Pending => (IconName::LoaderCircle, "CI running"),
        github::CiState::NoChecks => (IconName::Minus, "No CI signal"),
    }
}

/// Both status lines reserve the same icon slot, even when there is no icon.
/// Text stays aligned across CI/review lines and across every verdict.
fn status_line(
    kind: &'static str,
    pos: usize,
    icon: Option<IconName>,
    label: &'static str,
    color: Rgba,
) -> gpui::Div {
    div()
        .flex()
        .items_center()
        .gap_1()
        .text_color(color)
        .child(
            div()
                .size(px(12.))
                .flex_shrink_0()
                .when_some(icon, |slot, icon| slot.child(Icon::new(icon).size(px(12.)))),
        )
        .child(
            div()
                .debug_selector(move || format!("queue-{kind}-label-{pos}"))
                .flex_1()
                .min_w_0()
                .child(label),
        )
}

fn ci_view(ci: github::CiState, pos: usize, cx: &gpui::App) -> impl IntoElement {
    let (icon, label) = ci_label(ci);
    status_line("ci", pos, Some(icon), label, ci_color(ci, cx))
}

/// Decided review states use the same outlined icons as CI. Awaiting keeps
/// an empty icon slot rather than drawing an extra attention-seeking mark.
fn review_state_view(state: github::ReviewState, pos: usize, cx: &gpui::App) -> gpui::Div {
    let (icon, label, color) = match state {
        github::ReviewState::Approved => {
            (Some(IconName::CircleCheck), "approved", theme::green(cx))
        }
        github::ReviewState::ChangesRequested => {
            (Some(IconName::CircleX), "changes requested", theme::red(cx))
        }
        github::ReviewState::Awaiting => (None, "awaiting review", theme::subtext(cx)),
    };
    status_line("review", pos, icon, label, color)
}

/// The diff size at a glance — `+151 −30`, additions in green, deletions
/// in red, the "big vs small" triage signal. `None` until the enrichment
/// has seen the diff, and rendered not at all rather than as a fake zero.
fn size_view(
    additions: Option<u64>,
    deletions: Option<u64>,
    cx: &gpui::App,
) -> Option<gpui::AnyElement> {
    let (additions, deletions) = (additions?, deletions?);
    Some(
        div()
            .w_full()
            .flex()
            .flex_wrap()
            .justify_end()
            .items_center()
            .gap_1()
            .text_size(px(12.))
            .child(
                div()
                    .max_w_full()
                    .text_color(theme::green(cx))
                    .child(SharedString::from(format!("+{additions}"))),
            )
            .child(
                div()
                    .max_w_full()
                    .text_color(theme::red(cx))
                    .child(SharedString::from(format!("−{deletions}"))),
            )
            .into_any_element(),
    )
}

/// The conversation count: a muted speech bubble beside the number, so
/// "has discussion happened here?" reads at a glance.
fn comment_count_view(comments: u64, cx: &gpui::App) -> impl IntoElement {
    div()
        .flex_shrink_0()
        .flex()
        .items_center()
        .gap_1()
        .child(
            Icon::default()
                .path("icons/gander/comment-16.svg")
                .xsmall()
                .text_color(theme::overlay0(cx)),
        )
        .child(
            div()
                .text_size(px(12.))
                .text_color(theme::subtext(cx))
                .child(SharedString::from(comments.to_string())),
        )
}

/// GitHub's label colors are six hex digits without `#` (`"1D76DB"`).
/// Anything else degrades to `None` and the chip falls back to neutral
/// colors — a malformed color never breaks a row.
fn label_color(color: &str) -> Option<Hsla> {
    let channel = |text: &str| u8::from_str_radix(text, 16).ok();
    let bytes = color.as_bytes();
    if bytes.len() != 6 {
        return None;
    }
    let (r, g, b) = (
        channel(std::str::from_utf8(&bytes[0..2]).ok()?)?,
        channel(std::str::from_utf8(&bytes[2..4]).ok()?)?,
        channel(std::str::from_utf8(&bytes[4..6]).ok()?)?,
    );
    Some(Hsla::from(Rgba {
        r: r as f32 / 255.0,
        g: g as f32 / 255.0,
        b: b as f32 / 255.0,
        a: 1.0,
    }))
}

/// Mix `a` toward `b` by `t` (0 = `a`, 1 = `b`). Hue takes the short way
/// around the wheel; alpha lerps like the rest.
fn mix_hsla(a: Hsla, b: Hsla, t: f32) -> Hsla {
    let mut hue = b.h - a.h;
    if hue > 0.5 {
        hue -= 1.0;
    } else if hue < -0.5 {
        hue += 1.0;
    }
    Hsla {
        h: (a.h + hue * t).rem_euclid(1.0),
        s: a.s + (b.s - a.s) * t,
        l: a.l + (b.l - a.l) * t,
        a: a.a + (b.a - a.a) * t,
    }
}

/// A label chip's `(background, text, border)` from GitHub's label color,
/// tuned for the palette: a tinted fill with the color itself as text —
/// lifted toward white in dark mode, deepened toward black in light, so
/// GitHub's darker label colors stay readable on both. A missing color
/// degrades to neutral grays, never breaks the row.
pub(crate) fn label_chip_colors(
    color: Option<Hsla>,
    light: bool,
    cx: &gpui::App,
) -> (Hsla, Hsla, Hsla) {
    let Some(color) = color else {
        let bg = Hsla::from(theme::surface0(cx)).opacity(0.4);
        let text = Hsla::from(theme::subtext(cx));
        return (bg, text, Hsla::from(theme::surface0(cx)));
    };
    let (text, toward) = if light {
        (
            mix_hsla(
                color,
                Hsla {
                    h: 0.0,
                    s: 0.0,
                    l: 0.0,
                    a: 1.0,
                },
                0.2,
            ),
            0.18,
        )
    } else {
        (
            mix_hsla(
                color,
                Hsla {
                    h: 0.0,
                    s: 0.0,
                    l: 1.0,
                    a: 1.0,
                },
                0.4,
            ),
            0.12,
        )
    };
    (
        color.opacity(toward),
        text,
        color.opacity(if light { 0.6 } else { 0.5 }),
    )
}

/// How many chips one filter dimension shows before its `+n` overflow: the
/// chip bar stays a bounded strip over the queue however wide the day's
/// contexts spread. The active chip is pinned to the front of its group, so it can
/// never fall off the capped end.
const QUEUE_CHIPS_SHOWN: usize = 8;

/// One quick-filter chip over the queue: the filter clicking it applies,
/// how many rows it covers, and whether it's its dimension's active value.
/// The chip paints the qualifier it compiles to — the bar doubles as a
/// readout of the search the next refresh runs.
pub(crate) struct QueueChip {
    pub(crate) filter: github::QueueFilter,
    pub(crate) count: usize,
    pub(crate) active: bool,
}

/// One filter dimension's chips: the values present in the queue, ordered
/// for triage — the active value first, then the heaviest, then
/// alphabetically — capped with a count of the rest. A dimension with a
/// single value and nothing active stays off the bar entirely: there's
/// nothing to narrow, and an inert chip would only look broken.
pub(crate) struct QueueChipGroup {
    pub(crate) kind: github::QueueFilterKind,
    pub(crate) chips: Vec<QueueChip>,
    pub(crate) hidden: usize,
}

/// The chip groups for the queue as it stands: one per dimension — org,
/// repo — its values derived from the rows the queue is currently
/// showing, each carrying the rows it covers and whether it's active.
/// Deriving from the current rows is what keeps the bar honest: a filtered
/// queue offers the values it still contains — the active chip included,
/// so it stays visible and clearable — and an unfiltered one offers
/// everything. Two rules keep every chip meaningful: a dimension appears
/// only once it can narrow (two or more values, or one while it's the
/// active filter — its chip is the one-glance readout), and a malformed
/// repository row degrades honestly, its whole string standing in as the
/// org rather than panicking or vanishing.
pub(crate) fn queue_chips(
    prs: &[github::QueueItem],
    search: &github::QueueSearch,
) -> Vec<QueueChipGroup> {
    use github::QueueFilterKind::{Org, Repo};
    [Org, Repo]
        .into_iter()
        .filter_map(|kind| {
            let mut counts: std::collections::BTreeMap<&str, usize> =
                std::collections::BTreeMap::new();
            for pr in prs {
                let value = match kind {
                    Org => pr
                        .repository
                        .split_once('/')
                        .map(|(owner, _)| owner)
                        .unwrap_or(pr.repository.as_str()),
                    Repo => pr.repository.as_str(),
                };
                *counts.entry(value).or_insert(0) += 1;
            }
            let active = search.filters().iter().find(|f| f.kind() == kind);
            // One value and nothing active: the chip couldn't narrow
            // anything, so it stays off the bar.
            if counts.len() < 2 && active.is_none() {
                return None;
            }
            let mut values: Vec<(&str, usize)> = counts.into_iter().collect();
            values.sort_by(|(a_value, a_count), (b_value, b_count)| {
                // The active value always leads its group — the chip must
                // stay visible, whatever its weight.
                let a_active = active.is_some_and(|f| f.value() == *a_value);
                let b_active = active.is_some_and(|f| f.value() == *b_value);
                b_active
                    .cmp(&a_active)
                    .then(b_count.cmp(a_count))
                    .then(a_value.cmp(b_value))
            });
            let (shown, hidden) = values.split_at(values.len().min(QUEUE_CHIPS_SHOWN));
            Some(QueueChipGroup {
                kind,
                chips: shown
                    .iter()
                    .map(|(value, count)| QueueChip {
                        filter: match kind {
                            Org => github::QueueFilter::org(*value),
                            Repo => github::QueueFilter::repo(*value),
                        },
                        count: *count,
                        active: active.is_some_and(|f| f.value() == *value),
                    })
                    .collect(),
                hidden: hidden.len(),
            })
        })
        .collect()
}

/// The PR's age and the age of the viewer's outstanding direct review request.
/// Missing history and old caches stay unknown rather than using PR activity.
fn queue_ages(pr: &github::QueueItem, now: i64) -> [String; 2] {
    [
        (
            "Requested",
            pr.review_requested_at.as_deref().unwrap_or_default(),
        ),
        ("Opened", pr.created_at.as_str()),
    ]
    .map(|(label, timestamp)| {
        let age = if timestamp.is_empty() {
            "unknown".to_string()
        } else {
            short_age(timestamp, now)
        };
        format!("{label} {age}")
    })
}

/// Multi-line, variable-height table rows: every field keeps its column,
/// while titles, identities and labels can grow without ellipses. All labels
/// are shown for this prototype, inline with the repository and PR number.
pub(crate) fn queue_row(
    pr: &github::QueueItem,
    pos: usize,
    selected: bool,
    now: i64,
    entity: gpui::Entity<ReviewApp>,
    cx: &gpui::App,
) -> gpui::AnyElement {
    columns()
        .id(("queue-row", pos))
        .debug_selector(move || format!("queue-row-{pos}"))
        .relative()
        .py_2()
        .border_b_1()
        .border_color(Hsla::from(theme::surface0(cx)).opacity(0.6))
        .text_size(px(12.))
        .line_height(px(18.))
        .whitespace_normal()
        .cursor_pointer()
        .when(selected, |row| {
            row.bg(theme::surface0(cx)).child(
                div()
                    .debug_selector(move || format!("queue-selection-{pos}"))
                    .absolute()
                    .left_0()
                    .top_2()
                    .bottom_2()
                    .w(px(2.))
                    .rounded_full()
                    .bg(theme::blue(cx)),
            )
        })
        .when(!selected, |row| {
            row.hover(|style| style.bg(Hsla::from(theme::surface0(cx)).opacity(0.3)))
        })
        .on_click(move |_, window, cx| {
            entity.update(cx, |this, cx| {
                window.focus(&this.focus_handle, cx);
                this.queue_open(pos, cx);
            });
        })
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    div()
                        .debug_selector(move || format!("queue-title-{pos}"))
                        .w_full()
                        .text_size(px(14.))
                        .line_height(px(20.))
                        .text_color(if pr.is_draft {
                            theme::subtext(cx)
                        } else {
                            theme::text(cx)
                        })
                        .child(SharedString::from(pr.title.clone())),
                )
                .child(
                    div()
                        .w_full()
                        .flex()
                        .flex_wrap()
                        .items_center()
                        .gap_x_2()
                        .gap_y_1()
                        .child(
                            div()
                                .debug_selector(move || format!("queue-repository-{pos}"))
                                .flex_shrink_0()
                                .max_w_full()
                                .text_color(theme::subtext(cx))
                                .child(SharedString::from(format!(
                                    "{} · #{}",
                                    pr.repository, pr.number
                                ))),
                        )
                        .when(pr.is_draft, |metadata| {
                            metadata.child(Tag::secondary().xsmall().child("Draft"))
                        })
                        .children(pr.labels.iter().enumerate().map(|(ix, label)| {
                            let (bg, text, border) = label_chip_colors(
                                label_color(&label.color),
                                theme::is_light(cx),
                                cx,
                            );
                            div()
                                .debug_selector(move || format!("queue-label-{pos}-{ix}"))
                                .max_w_full()
                                .child(
                                    Tag::custom(bg, text, border).xsmall().max_w_full().child(
                                        div()
                                            .min_w_0()
                                            .child(SharedString::from(label.name.clone())),
                                    ),
                                )
                        })),
                ),
        )
        .child(
            cell(AUTHOR_WIDTH)
                .debug_selector(move || format!("queue-author-cell-{pos}"))
                .child(
                    div()
                        .flex()
                        .items_start()
                        .gap_2()
                        .child(
                            div().flex_shrink_0().child(
                                match &pr.author_avatar_url {
                                    Some(url) => Avatar::new().src(url.clone()),
                                    None => Avatar::new(),
                                }
                                .name(pr.author.clone())
                                .xsmall(),
                            ),
                        )
                        .child(
                            div()
                                .debug_selector(move || format!("queue-author-{pos}"))
                                .flex_1()
                                .min_w_0()
                                .text_color(theme::subtext(cx))
                                .child(SharedString::from(pr.author.clone())),
                        ),
                ),
        )
        .child(
            cell(STATUS_WIDTH)
                .debug_selector(move || format!("queue-status-{pos}"))
                .child(ci_view(pr.ci, pos, cx))
                .child(review_state_view(pr.review_state, pos, cx)),
        )
        .child(
            cell(CHANGES_WIDTH)
                .debug_selector(move || format!("queue-changes-{pos}"))
                .items_end()
                .child(
                    div()
                        .debug_selector(move || format!("queue-size-{pos}"))
                        .w_full()
                        .min_h(px(18.))
                        .text_right()
                        .when_some(size_view(pr.additions, pr.deletions, cx), |cell, size| {
                            cell.child(size)
                        })
                        .when(pr.additions.is_none() || pr.deletions.is_none(), |cell| {
                            cell.text_color(theme::overlay0(cx)).child("—")
                        }),
                )
                .child(
                    div()
                        .debug_selector(move || format!("queue-comments-{pos}"))
                        .child(comment_count_view(pr.comments, cx)),
                ),
        )
        .child(
            cell(AGE_WIDTH)
                .debug_selector(move || format!("queue-age-{pos}"))
                .text_right()
                .text_color(theme::subtext(cx))
                .children(
                    queue_ages(pr, now)
                        .into_iter()
                        .enumerate()
                        .map(|(ix, age)| {
                            div()
                                .debug_selector(move || format!("queue-age-{pos}-{ix}"))
                                .w_full()
                                .when(
                                    ix == 0
                                        && pr
                                            .review_requested_at
                                            .as_deref()
                                            .is_some_and(|timestamp| !timestamp.is_empty()),
                                    |age| age.text_color(theme::text(cx)),
                                )
                                .child(SharedString::from(age))
                        }),
                ),
        )
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::queued;

    #[test]
    fn queue_ages_use_the_viewers_request_not_last_activity() {
        let mut pr = queued(1, "acme/repo", "alice");
        pr.created_at = "1970-01-01T12:00:00Z".into();
        pr.review_requested_at = Some("1970-01-04T11:15:00Z".into());
        pr.updated_at = "1970-01-04T11:59:00Z".into();
        let now = 3 * 86400 + 12 * 3600; // 1970-01-04T12:00:00Z
        assert_eq!(queue_ages(&pr, now), ["Requested 45m ago", "Opened 3d ago"]);
    }

    #[test]
    fn queue_ages_do_not_invent_times_for_old_caches_or_team_requests() {
        let mut pr = queued(1, "acme/repo", "alice");
        pr.updated_at = "1970-01-04T11:15:00Z".into();
        let now = 3 * 86400 + 12 * 3600;
        assert_eq!(
            queue_ages(&pr, now),
            ["Requested unknown", "Opened unknown"]
        );
    }

    /// A queue row's label colors: GitHub's six-hex form parses, and only
    /// that form — anything else degrades the chip to neutral, never
    /// breaks the row.
    #[test]
    fn queue_label_colors_parse_githubs_hex() {
        assert_eq!(
            label_color("1D76DB"),
            Some(Hsla::from(Rgba {
                r: 0x1d as f32 / 255.0,
                g: 0x76 as f32 / 255.0,
                b: 0xdb as f32 / 255.0,
                a: 1.0,
            }))
        );
        for malformed in ["", "1D76D", "1D76DBB", "#1D76DB", "GHIJKL", "1D 6DB"] {
            assert_eq!(
                label_color(malformed),
                None,
                "{malformed:?} isn't a label color"
            );
        }
    }

    #[gpui::test]
    fn queue_label_chips_stay_readable_in_both_palettes(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            gpui::init(cx);
            // A GitHub label color, mid-tone blue.
            let blue = label_color("1D76DB").unwrap();
            // Dark mode: text lifts toward white, fill stays tinted.
            let (bg, text, border) = label_chip_colors(Some(blue), false, cx);
            assert!(
                text.l > blue.l,
                "dark-mode text must be lighter than the base"
            );
            assert_eq!(bg.a, 0.12);
            assert!(border.a > 0.0);
            // Light mode: text deepens toward black instead.
            let (_, text, _) = label_chip_colors(Some(blue), true, cx);
            assert!(
                text.l < blue.l,
                "light-mode text must be darker than the base"
            );
            // No color at all: neutral grays, so the chip still reads.
            let (bg, text, _) = label_chip_colors(None, false, cx);
            assert_eq!(bg, Hsla::from(theme::surface0(cx)).opacity(0.4));
            assert_eq!(text, Hsla::from(theme::subtext(cx)));
        });
    }

    #[test]
    fn queue_label_mixing_takes_the_short_way_round() {
        let red = Hsla {
            h: 0.95,
            s: 1.0,
            l: 0.5,
            a: 1.0,
        };
        let yellow = Hsla {
            h: 0.05,
            s: 1.0,
            l: 0.5,
            a: 1.0,
        };
        // Near-equality: hues wrap through rem_euclid, so endpoints land a
        // float's whisker off the originals — visible to ==, not to eyes.
        let close = |a: Hsla, b: Hsla| {
            (a.h - b.h).abs() < 1e-6
                && (a.s - b.s).abs() < 1e-6
                && (a.l - b.l).abs() < 1e-6
                && (a.a - b.a).abs() < 1e-6
        };
        // Endpoints are (near enough to) exact.
        assert!(close(mix_hsla(red, yellow, 0.0), red));
        assert!(close(mix_hsla(red, yellow, 1.0), yellow));
        // Halfway wraps through 1.0 → 0.0 (red → yellow the short way),
        // not the long way through green.
        assert_eq!(mix_hsla(red, yellow, 0.5).h, 0.0);
        // Alpha lerps with the rest.
        assert_eq!(mix_hsla(red, Hsla { a: 0.0, ..yellow }, 0.5).a, 0.5);
    }

    #[test]
    fn ci_verdicts_have_distinct_icons_and_explicit_labels() {
        assert!(matches!(
            ci_label(github::CiState::Pass),
            (IconName::CircleCheck, "CI passed")
        ));
        assert!(matches!(
            ci_label(github::CiState::Fail),
            (IconName::CircleX, "CI failed")
        ));
        assert!(matches!(
            ci_label(github::CiState::Pending),
            (IconName::LoaderCircle, "CI running")
        ));
        assert!(matches!(
            ci_label(github::CiState::NoChecks),
            (IconName::Minus, "No CI signal")
        ));
    }

    /// The CI palette wiring: each verdict paints its own color —
    /// the acceptance "green vs red" rides on exactly these mappings.
    #[gpui::test]
    fn queue_ci_verdicts_paint_their_colors(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            gpui::init(cx);
            assert_eq!(ci_color(github::CiState::Pass, cx), theme::green(cx));
            assert_eq!(ci_color(github::CiState::Fail, cx), theme::red(cx));
            assert_eq!(ci_color(github::CiState::Pending, cx), theme::yellow(cx));
            assert_eq!(ci_color(github::CiState::NoChecks, cx), theme::overlay0(cx));
        });
    }

    /// Size stays absent until the enrichment has seen both halves — a
    /// half-known PR shows nothing rather than a misleading number.
    #[gpui::test]
    fn queue_size_waits_for_the_whole_diff(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            gpui::init(cx);
            assert!(size_view(None, None, cx).is_none());
            assert!(size_view(Some(151), None, cx).is_none());
            assert!(size_view(None, Some(30), cx).is_none());
            assert!(size_view(Some(151), Some(30), cx).is_some());
        });
    }

    /// The `kind` group from a derivation — the bar skips dimensions with
    /// nothing to offer, so positions in the vec aren't stable.
    fn group(groups: &[QueueChipGroup], kind: github::QueueFilterKind) -> &QueueChipGroup {
        groups
            .iter()
            .find(|group| group.kind == kind)
            .expect("the kind must be on the bar")
    }

    /// Chips derive from the rows the queue is showing: one group per
    /// dimension that can narrow, each value with the rows it covers,
    /// heaviest first with alphabetical ties.
    #[test]
    fn queue_chips_derive_from_the_current_rows() {
        let prs = vec![
            queued(1, "acme/widgets", "alice"),
            queued(2, "acme/widgets", "bob"),
            queued(3, "acme/gadgets", "alice"),
            queued(4, "globex/thing", "bob"),
        ];
        let groups = queue_chips(&prs, &github::QueueSearch::review_requested());
        // Orgs in kind order first, then repos — every
        // dimension here has two or more values.
        assert_eq!(
            groups.iter().map(|group| group.kind).collect::<Vec<_>>(),
            vec![github::QueueFilterKind::Org, github::QueueFilterKind::Repo]
        );
        // Heaviest first, whatever the alphabetical order would say.
        let org = group(&groups, github::QueueFilterKind::Org);
        assert_eq!(org.chips[0].filter, github::QueueFilter::org("acme"));
        assert_eq!(org.chips[0].count, 3);
        assert!(!org.chips[0].active);
        assert_eq!(org.chips[1].filter, github::QueueFilter::org("globex"));
        let repo = group(&groups, github::QueueFilterKind::Repo);
        assert_eq!(
            repo.chips[0].filter,
            github::QueueFilter::repo("acme/widgets")
        );
        assert_eq!(repo.chips[0].count, 2);
        assert_eq!(
            repo.chips[1].filter,
            github::QueueFilter::repo("acme/gadgets")
        );
        assert_eq!(
            repo.chips[2].filter,
            github::QueueFilter::repo("globex/thing")
        );
        assert_eq!(groups.iter().map(|group| group.hidden).sum::<usize>(), 0);
    }

    /// A dimension with a single value has nothing to narrow: it stays
    /// off the bar instead of offering a chip that couldn't visibly change
    /// anything — but while it's the active filter its chip returns, as
    /// the one-glance readout of what's filtering the queue.
    #[test]
    fn queue_chips_stay_off_the_bar_until_they_can_narrow() {
        let prs = vec![
            queued(1, "acme/widgets", "alice"),
            queued(2, "acme/widgets", "bob"),
        ];
        // One org, one repo: different authors must not offer filters.
        let groups = queue_chips(&prs, &github::QueueSearch::review_requested());
        assert!(groups.is_empty());
        // Active brings a single-value dimension back — marked, and the
        // only chip in its group.
        let search =
            github::QueueSearch::review_requested().filtered_by(github::QueueFilter::org("acme"));
        let groups = queue_chips(&prs, &search);
        let org = group(&groups, github::QueueFilterKind::Org);
        assert_eq!(org.chips.len(), 1);
        assert!(org.chips[0].active);
        assert_eq!(org.chips[0].filter, github::QueueFilter::org("acme"));
        assert_eq!(org.hidden, 0);
    }

    /// The active chip pins to the front of its group — even when it's the
    /// lightest value there — and reads back marked, so the one-glance
    /// rule holds under the cap too.
    #[test]
    fn queue_chips_pin_the_active_value_first() {
        let prs = vec![
            queued(1, "acme/widgets", "alice"),
            queued(2, "acme/widgets", "alice"),
            queued(3, "acme/gadgets", "bob"),
        ];
        let search = github::QueueSearch::review_requested()
            .filtered_by(github::QueueFilter::repo("acme/gadgets"));
        let groups = queue_chips(&prs, &search);
        let repos = group(&groups, github::QueueFilterKind::Repo);
        assert_eq!(
            repos.chips[0].filter,
            github::QueueFilter::repo("acme/gadgets")
        );
        assert_eq!(repos.chips[0].count, 1);
        assert!(repos.chips[0].active);
        assert_eq!(
            repos.chips[1].filter,
            github::QueueFilter::repo("acme/widgets")
        );
        assert!(!repos.chips[1].active);
    }

    /// What the queue holds after narrowing to one repo: the bar offers
    /// only that repo — the active chip, visible and marked. The org
    /// dimension has one value and nothing active, so it leaves the bar.
    #[test]
    fn queue_chips_derive_from_the_filtered_queue() {
        let prs = vec![queued(3, "acme/gadgets", "alice")];
        let search = github::QueueSearch::review_requested()
            .filtered_by(github::QueueFilter::repo("acme/gadgets"));
        let groups = queue_chips(&prs, &search);
        assert_eq!(groups.len(), 1, "only the active repo's dimension remains");
        let repo = group(&groups, github::QueueFilterKind::Repo);
        assert_eq!(
            repo.chips[0].filter,
            github::QueueFilter::repo("acme/gadgets")
        );
        assert!(repo.chips[0].active);
        // With the repo chip lifted too, a single row can't offer
        // anything to narrow: the bar empties.
        let groups = queue_chips(
            &prs,
            &search.clone().without_filter(github::QueueFilterKind::Repo),
        );
        assert!(
            groups.is_empty(),
            "a single row can't offer anything to narrow"
        );
    }

    /// Ten repositories: the bar shows the cap and reports the rest — bounded
    /// strip, honest count. All counts equal here, so alphabetical order
    /// decides what shows.
    #[test]
    fn queue_chips_cap_each_group_with_an_overflow_count() {
        let prs: Vec<_> = (0..10)
            .map(|n| queued(n, &format!("acme/repo{n:02}"), "alice"))
            .collect();
        let groups = queue_chips(&prs, &github::QueueSearch::review_requested());
        let repos = group(&groups, github::QueueFilterKind::Repo);
        assert_eq!(repos.chips.len(), QUEUE_CHIPS_SHOWN);
        assert_eq!(repos.hidden, 10 - QUEUE_CHIPS_SHOWN);
        assert_eq!(
            repos.chips[0].filter,
            github::QueueFilter::repo("acme/repo00")
        );
        assert_eq!(
            repos.chips[QUEUE_CHIPS_SHOWN - 1].filter,
            github::QueueFilter::repo("acme/repo07")
        );
    }

    /// A malformed repository row degrades honestly: the whole string
    /// stands in as the org rather than panicking or vanishing — and it
    /// counts as its own value, so a queue mixing malformed and normal
    /// rows still offers both.
    #[test]
    fn queue_chips_degrade_a_malformed_repository() {
        let prs = vec![
            queued(1, "not-a-slug", "alice"),
            queued(2, "acme/widgets", "alice"),
        ];
        let groups = queue_chips(&prs, &github::QueueSearch::review_requested());
        let org = group(&groups, github::QueueFilterKind::Org);
        assert_eq!(org.chips.len(), 2);
        assert_eq!(org.chips[0].filter, github::QueueFilter::org("acme"));
        assert_eq!(org.chips[1].filter, github::QueueFilter::org("not-a-slug"));
        let repo = group(&groups, github::QueueFilterKind::Repo);
        assert_eq!(
            repo.chips[0].filter,
            github::QueueFilter::repo("acme/widgets")
        );
        assert_eq!(
            repo.chips[1].filter,
            github::QueueFilter::repo("not-a-slug")
        );
    }
}
