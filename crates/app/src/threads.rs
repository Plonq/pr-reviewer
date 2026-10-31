//! App-owned review comments and threads, anchored indexing,
//! comment-body wrapping, and comment age formatting.

use crate::{rows::ViewMode, SPLIT_DIVIDER};
use std::collections::HashMap;
use std::ops::Range;

/// Comments (including old cached ones) already carry a GitHub login. The
/// public avatar endpoint accepts both user and bot logins, so displaying a
/// face needs no extra API request or cache schema change. The shared image
/// loader caches these URLs for offline use; initials remain the fallback.
pub(crate) fn author_avatar(
    author: &str,
    account: &crate::AccountStatus,
) -> gpui::component::avatar::Avatar {
    use gpui::{component::Sizable as _, prelude::*};
    gpui::component::avatar::Avatar::new()
        .name(author.to_owned())
        .xsmall()
        .when_some(author_avatar_url(author, account), |avatar, url| {
            avatar.src(url)
        })
}

fn author_avatar_url(author: &str, account: &crate::AccountStatus) -> Option<String> {
    // Pending and local drafts say "you", not a GitHub login. Never fetch the
    // avatar of the unrelated GitHub account literally named "you".
    if author == "you" {
        return match account {
            crate::AccountStatus::SignedIn(viewer) => Some(viewer.avatar_url.clone()),
            _ => None,
        };
    }
    let login = author.strip_suffix("[bot]").unwrap_or(author);
    if login.is_empty()
        || !login
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-')
    {
        return None;
    }
    let author = author.replace("[bot]", "%5Bbot%5D");
    Some(format!(
        "https://avatars.githubusercontent.com/{author}?s=64"
    ))
}

/// Which diff side a review comment anchors to, GitHub's LEFT/RIGHT.
pub(crate) use github::DiffSide as CommentSide;

/// The comment's display age and legacy lexical ordering key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CommentTime {
    Posted(String),
    Draft,
    Pending,
}

impl CommentTime {
    fn sort_key(&self) -> &str {
        match self {
            Self::Posted(raw) => raw,
            Self::Draft | Self::Pending => "draft",
        }
    }

    pub(crate) fn short_age(&self, now: i64) -> String {
        match self {
            Self::Posted(raw) => short_age(raw, now),
            Self::Draft | Self::Pending => "draft".to_string(),
        }
    }
}

/// A comment used by the app. Local drafts and PR comments live in separate
/// item collections, so their numeric identities never need to be combined.
#[derive(Clone, Debug)]
pub(crate) struct ReviewComment {
    pub(crate) id: u64,
    pub(crate) path: String,
    pub(crate) line: Option<u64>,
    pub(crate) side: CommentSide,
    pub(crate) start_line: Option<u64>,
    pub(crate) body: String,
    pub(crate) author: String,
    pub(crate) time: CommentTime,
    pub(crate) in_reply_to_id: Option<u64>,
}

impl From<github::ReviewComment> for ReviewComment {
    fn from(comment: github::ReviewComment) -> Self {
        let position = crate::comment_anchor::display_line(comment.line, comment.start_line);
        Self {
            id: comment.id,
            path: comment.path,
            line: position.map(|(_, line)| line),
            side: if comment.side.as_deref() == Some("LEFT") {
                CommentSide::Left
            } else {
                CommentSide::Right
            },
            start_line: position.and_then(|(start, _)| start),
            body: comment.body,
            author: comment.user.login,
            time: CommentTime::Posted(comment.created_at),
            in_reply_to_id: comment.in_reply_to_id,
        }
    }
}

/// One review thread: the top-level comment plus its replies, in lexical
/// timestamp order (then numeric id), including unanchored/outdated roots.
#[derive(Debug)]
pub(crate) struct CommentThread {
    pub(crate) root: ReviewComment,
    pub(crate) replies: Vec<ReviewComment>,
}

/// One file's threads, keyed by anchor.
pub(crate) type FileAnchors = HashMap<(CommentSide, u64), Vec<CommentThread>>;

/// Review comments grouped for row building.
#[derive(Debug, Default)]
pub(crate) struct CommentIndex {
    /// path → (side, line) → threads in root-created order.
    pub(crate) threads: HashMap<String, FileAnchors>,
    /// path → (anchored comment count, outdated comment count).
    pub(crate) counts: HashMap<String, (usize, usize)>,
    /// Retained so replacing pending drafts can rebuild accurate counts
    /// without losing genuinely outdated published threads.
    unanchored: Vec<CommentThread>,
}

impl CommentIndex {
    /// GitHub's current LEFT coordinates refer to the full PR's base, not a
    /// previous review commit. Retain counts/history, but never paint these at
    /// coincidentally matching incremental-diff line numbers.
    pub(crate) fn hide_left_anchors(&mut self) {
        let threads = std::mem::take(&mut self.threads);
        let mut all: Vec<_> = threads
            .into_values()
            .flat_map(|anchors| anchors.into_values().flatten())
            .chain(std::mem::take(&mut self.unanchored))
            .collect();
        for thread in &mut all {
            if thread.root.side == CommentSide::Left {
                thread.root.line = None;
                thread.root.start_line = None;
            }
        }
        *self = index_threads(all);
    }

    /// Replace only the pending-review layer. GitHub may also include these
    /// ids in its comments response; the session's copy wins without duplicates.
    pub(crate) fn replace_pending(&mut self, pending: Option<&github::PendingReview>) {
        let old = std::mem::take(self);
        let mut comments: Vec<_> = old
            .threads
            .into_values()
            .flat_map(|anchors| anchors.into_values().flatten())
            .chain(old.unanchored)
            .flat_map(|thread| std::iter::once(thread.root).chain(thread.replies))
            .filter(|comment| {
                comment.time != CommentTime::Pending
                    && !pending.is_some_and(|review| {
                        review.comments.iter().any(|draft| draft.id == comment.id)
                    })
            })
            .collect();
        if let Some(review) = pending {
            comments.extend(review.comments.iter().map(|comment| {
                let position =
                    crate::comment_anchor::display_line(comment.line, comment.start_line);
                ReviewComment {
                    id: comment.id,
                    path: comment.path.clone(),
                    line: position.map(|(_, line)| line),
                    side: comment.side,
                    start_line: position.and_then(|(start, _)| start),
                    body: comment.body.clone(),
                    author: "you".into(),
                    time: CommentTime::Pending,
                    in_reply_to_id: comment.in_reply_to_id,
                }
            }));
        }
        *self = group_comments(comments);
    }
}

/// Assemble global-order threads for both export and anchored row indexing.
/// Replies whose parents have not yet been encountered are dropped, even if
/// their parent appears later in the sorted input.
pub(crate) fn assemble_threads(mut comments: Vec<ReviewComment>) -> Vec<CommentThread> {
    comments.sort_by(|a, b| {
        a.time
            .sort_key()
            .cmp(b.time.sort_key())
            .then(a.id.cmp(&b.id))
    });
    let mut threads: Vec<CommentThread> = Vec::new();
    // Comment id (root or reply) → thread index, so replies-to-replies still
    // land in the right thread.
    let mut thread_of: HashMap<u64, usize> = HashMap::new();
    for comment in comments {
        match comment.in_reply_to_id {
            None => {
                thread_of.insert(comment.id, threads.len());
                threads.push(CommentThread {
                    root: comment,
                    replies: Vec::new(),
                });
            }
            Some(parent) => {
                if let Some(&ix) = thread_of.get(&parent) {
                    thread_of.insert(comment.id, ix);
                    threads[ix].replies.push(comment);
                }
                // Orphaned reply (parent not fetched): skip.
            }
        }
    }
    threads
}

/// Index assembled threads by their root anchor. Outdated roots and their
/// replies contribute to counts but have no rows.
pub(crate) fn index_threads(threads: Vec<CommentThread>) -> CommentIndex {
    let mut index = CommentIndex::default();
    for thread in threads {
        let size = 1 + thread.replies.len();
        let counts = index.counts.entry(thread.root.path.clone()).or_default();
        let Some(line) = thread.root.line else {
            counts.1 += size;
            index.unanchored.push(thread);
            continue;
        };
        counts.0 += size;
        index
            .threads
            .entry(thread.root.path.clone())
            .or_default()
            .entry((thread.root.side, line))
            .or_default()
            .push(thread);
    }
    index
}

/// Assemble and index comments for row building.
pub(crate) fn group_comments(comments: Vec<ReviewComment>) -> CommentIndex {
    index_threads(assemble_threads(comments))
}

/// Widest a comment body ever wraps, however roomy the pane: past ~80 columns
/// prose gets hard to track. Also the fallback before the pane is measured.
pub(crate) const COMMENT_WRAP_CHARS: usize = 80;
/// Never wrap narrower than this, however cramped the pane — below it every
/// word lands on its own line, which is worse than clipping.
const MIN_COMMENT_WRAP_CHARS: usize = 24;

/// Chrome around a comment body inside its card: the gutter indent, the accent
/// border, and the horizontal padding (see `comment_row`).
const COMMENT_CARD_CHROME: f32 = 72. + 2. + 24.;

/// Columns a comment body can use given the measured list width — the whole
/// width in unified, one half in split. Wrapping is then only as narrow as the
/// pane forces, capped at [`COMMENT_WRAP_CHARS`] for readability.
pub(crate) fn comment_wrap_cols(list_width: f32, mode: ViewMode, char_width: f32) -> usize {
    if char_width <= 0. {
        return COMMENT_WRAP_CHARS;
    }
    let card = match mode {
        ViewMode::Unified => list_width,
        ViewMode::Split => (list_width - SPLIT_DIVIDER) / 2.,
    };
    let cols = ((card - COMMENT_CARD_CHROME) / char_width).floor();
    (cols.max(0.) as usize).clamp(MIN_COMMENT_WRAP_CHARS, COMMENT_WRAP_CHARS)
}

/// Soft-wrap `text` at `width` chars: explicit newlines are preserved, wraps
/// prefer the last space in range (the space is consumed), and a word longer
/// than the width hard-breaks on a char boundary.
pub(crate) fn wrap_body(text: &str, width: usize) -> Vec<String> {
    let mut out = Vec::new();
    for line in text.split('\n') {
        let mut line = line.strip_suffix('\r').unwrap_or(line);
        loop {
            // Byte offset of the (width+1)-th char; absent = the rest fits.
            let Some((cut, _)) = line.char_indices().nth(width) else {
                out.push(line.to_string());
                break;
            };
            match line[..cut].rfind(' ') {
                Some(space) if space > 0 => {
                    out.push(line[..space].to_string());
                    line = &line[space + 1..];
                }
                _ => {
                    out.push(line[..cut].to_string());
                    line = &line[cut..];
                }
            }
        }
    }
    out
}

/// Days since 1970-01-01 for a civil date (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// Unix seconds for an ISO-8601 UTC timestamp ("2026-07-01T12:34:56Z").
fn parse_iso_utc(s: &str) -> Option<i64> {
    if s.len() < 20 {
        return None;
    }
    let num = |range: Range<usize>| s.get(range)?.parse::<i64>().ok();
    Some(
        days_from_civil(num(0..4)?, num(5..7)?, num(8..10)?) * 86400
            + num(11..13)? * 3600
            + num(14..16)? * 60
            + num(17..19)?,
    )
}

/// Compact "3d ago"-style age of an ISO timestamp relative to `now` (unix
/// seconds). Unparseable input renders as-is.
pub(crate) fn short_age(iso: &str, now: i64) -> String {
    let Some(t) = parse_iso_utc(iso) else {
        return iso.to_string();
    };
    let d = now - t;
    if d < 60 {
        "just now".to_string()
    } else if d < 3600 {
        format!("{}m ago", d / 60)
    } else if d < 86400 {
        format!("{}h ago", d / 3600)
    } else if d < 365 * 86400 {
        format!("{}d ago", d / 86400)
    } else {
        format!("{}y ago", d / (365 * 86400))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::{app_rc as rc, rc as api_rc};

    #[test]
    fn comment_avatar_urls_support_users_and_bots_but_not_missing_authors() {
        let account = crate::AccountStatus::Checking;
        assert_eq!(
            author_avatar_url("octocat", &account).as_deref(),
            Some("https://avatars.githubusercontent.com/octocat?s=64")
        );
        assert_eq!(
            author_avatar_url("dependabot[bot]", &account).as_deref(),
            Some("https://avatars.githubusercontent.com/dependabot%5Bbot%5D?s=64")
        );
        for author in ["", "[bot]", "a/b", "a?b", "a b", "you"] {
            assert_eq!(author_avatar_url(author, &account), None);
        }
    }

    #[test]
    fn draft_avatars_use_the_viewer_not_the_account_named_you() {
        let account = crate::AccountStatus::SignedIn(github::Viewer {
            login: "alice".into(),
            avatar_url: "https://avatars.githubusercontent.com/u/1?s=64".into(),
        });
        assert_eq!(
            author_avatar_url("you", &account).as_deref(),
            Some("https://avatars.githubusercontent.com/u/1?s=64")
        );
    }

    #[test]
    fn review_comment_conversion_preserves_raw_fields_and_defaults_side() {
        for (side, expected) in [
            (Some("LEFT"), CommentSide::Left),
            (Some("RIGHT"), CommentSide::Right),
            (Some("left"), CommentSide::Right),
            (Some("unknown"), CommentSide::Right),
            (None, CommentSide::Right),
        ] {
            let mut raw = api_rc(
                u64::MAX,
                "old/renamed.rs",
                side,
                Some(42),
                "body\nunchanged",
                "alice",
                "raw malformed timestamp",
                Some(u64::MAX - 1),
            );
            raw.start_line = Some(39);
            let comment = ReviewComment::from(raw);
            assert_eq!(comment.id, u64::MAX);
            assert_eq!(comment.path, "old/renamed.rs");
            assert_eq!(comment.line, Some(42));
            assert_eq!(comment.start_line, Some(39));
            assert_eq!(comment.side, expected);
            assert_eq!(comment.body, "body\nunchanged");
            assert_eq!(comment.author, "alice");
            assert_eq!(comment.in_reply_to_id, Some(u64::MAX - 1));
            assert_eq!(
                comment.time,
                CommentTime::Posted("raw malformed timestamp".into())
            );
            assert_eq!(comment.time.short_age(0), "raw malformed timestamp");
            assert_eq!(comment.time.sort_key(), "raw malformed timestamp");
        }
    }

    #[test]
    fn assemble_threads_equal_timestamp_orders_roots_and_nested_replies_by_id() {
        let comment = |id, parent| rc(id, "a.rs", None, Some(2), "body", "author", "same", parent);
        let threads = assemble_threads(vec![
            comment(6, Some(4)),
            comment(5, None),
            comment(4, Some(3)),
            comment(2, None),
            comment(3, Some(1)),
            comment(1, None),
        ]);
        assert_eq!(
            threads.iter().map(|t| t.root.id).collect::<Vec<_>>(),
            vec![1, 2, 5]
        );
        assert_eq!(
            threads[0].replies.iter().map(|c| c.id).collect::<Vec<_>>(),
            vec![3, 4, 6]
        );
        let index = index_threads(threads);
        let anchored = &index.threads["a.rs"][&(CommentSide::Right, 2)];
        assert_eq!(
            anchored.iter().map(|t| t.root.id).collect::<Vec<_>>(),
            vec![1, 2, 5]
        );
        assert_eq!(index.counts["a.rs"], (6, 0));
    }

    #[test]
    fn assemble_threads_draft_uses_literal_age_and_legacy_sort_key_then_id() {
        let draft = |id, parent| ReviewComment {
            time: CommentTime::Draft,
            ..rc(
                id,
                "a.rs",
                Some("LEFT"),
                Some(2),
                "draft body",
                "you",
                "unused",
                parent,
            )
        };
        assert_eq!(CommentTime::Draft.short_age(0), "draft");
        assert_eq!(CommentTime::Draft.short_age(i64::MAX), "draft");
        let threads = assemble_threads(vec![
            draft(4, None),
            draft(3, Some(2)),
            draft(2, Some(1)),
            draft(1, None),
        ]);
        assert_eq!(
            threads.iter().map(|t| t.root.id).collect::<Vec<_>>(),
            vec![1, 4]
        );
        assert_eq!(
            threads[0].replies.iter().map(|c| c.id).collect::<Vec<_>>(),
            vec![2, 3]
        );
        assert_eq!(CommentTime::Draft.sort_key(), "draft");
    }

    #[test]
    fn pending_layer_replaces_duplicates_edits_and_deletes_without_losing_outdated_counts() {
        let mut index = group_comments(vec![
            rc(
                1,
                "a.rs",
                Some("RIGHT"),
                Some(7),
                "published",
                "alice",
                "a",
                None,
            ),
            rc(
                2,
                "a.rs",
                Some("LEFT"),
                None,
                "outdated",
                "alice",
                "b",
                None,
            ),
            rc(
                12,
                "a.rs",
                Some("LEFT"),
                Some(7),
                "stale copy",
                "you",
                "c",
                None,
            ),
        ]);
        let mut review = github::PendingReview {
            id: 91,
            node_id: "pending".into(),
            body: String::new(),
            comments: vec![
                github::PendingComment {
                    id: 12,
                    in_reply_to_id: None,
                    node_id: "draft_node".into(),
                    path: "a.rs".into(),
                    side: CommentSide::Left,
                    line: Some(7),
                    start_line: Some(5),
                    body: "draft".into(),
                },
                github::PendingComment {
                    id: 13,
                    in_reply_to_id: None,
                    node_id: "old_draft_node".into(),
                    path: "a.rs".into(),
                    side: CommentSide::Left,
                    line: None,
                    start_line: None,
                    body: "old draft".into(),
                },
            ],
        };
        for _ in 0..2 {
            index.replace_pending(Some(&review));
            assert_eq!(index.counts["a.rs"], (2, 2));
            let threads = &index.threads["a.rs"][&(CommentSide::Left, 7)];
            assert_eq!(threads.len(), 1);
            assert_eq!(threads[0].root.start_line, Some(5));
            assert_eq!(threads[0].root.time, CommentTime::Pending);
        }
        review.comments[0].body = "edited".into();
        review.comments.pop();
        index.replace_pending(Some(&review));
        assert_eq!(index.counts["a.rs"], (2, 1));
        assert_eq!(
            index.threads["a.rs"][&(CommentSide::Left, 7)][0].root.body,
            "edited"
        );
        index.replace_pending(None);
        assert_eq!(index.counts["a.rs"], (1, 1));
        assert!(!index.threads["a.rs"].contains_key(&(CommentSide::Left, 7)));
    }

    #[test]
    fn assemble_threads_drops_early_and_missing_parents_but_keeps_outdated_replies() {
        let comment =
            |id, time, parent| rc(id, "stale.rs", None, None, "body", "author", time, parent);
        let threads = assemble_threads(vec![
            comment(2, "a", Some(1)), // Parent is later in lexical timestamp order.
            comment(3, "c", Some(2)), // Descendant of the dropped reply is also dropped.
            comment(4, "c", Some(999)),
            comment(1, "b", None),
            comment(5, "d", Some(1)),
            comment(6, "e", Some(5)),
            comment(0, "b", Some(1)), // Equal time but lower id: also too early.
        ]);
        assert_eq!(threads.len(), 1);
        assert_eq!(threads[0].root.id, 1);
        assert_eq!(
            threads[0].replies.iter().map(|c| c.id).collect::<Vec<_>>(),
            vec![5, 6]
        );
        let index = index_threads(threads);
        assert!(index.threads.is_empty());
        assert_eq!(index.counts["stale.rs"], (0, 3));
    }

    #[test]
    fn wrap_preserves_newlines_and_breaks_at_spaces() {
        // Explicit newlines (and CRLF) survive; empty lines stay empty.
        assert_eq!(wrap_body("a\n\nb\r\nc", 10), vec!["a", "", "b", "c"]);
        // Fits exactly: no wrap.
        assert_eq!(wrap_body("abcde", 5), vec!["abcde"]);
        // Breaks at the last space in range; the space is consumed.
        assert_eq!(
            wrap_body("one two three four", 9),
            vec!["one two", "three", "four"]
        );
        // A word longer than the width hard-breaks; no space is invented.
        assert_eq!(wrap_body("abcdefghij", 4), vec!["abcd", "efgh", "ij"]);
        // Never splits inside a word when a space exists in range.
        assert_eq!(wrap_body("aa bbbb", 5), vec!["aa", "bbbb"]);
        // Multibyte chars: wraps on char boundaries, not bytes.
        assert_eq!(wrap_body("ééééé", 3), vec!["ééé", "éé"]);
        assert_eq!(wrap_body("éé éé", 3), vec!["éé", "éé"]);
    }

    #[test]
    fn short_age_buckets() {
        let now = parse_iso_utc("2026-07-09T12:00:00Z").unwrap();
        assert_eq!(
            CommentTime::Posted("2026-07-06T12:00:00Z".into()).short_age(now),
            "3d ago"
        );
        assert_eq!(short_age("2026-07-09T11:59:30Z", now), "just now");
        assert_eq!(short_age("2026-07-09T11:15:00Z", now), "45m ago");
        assert_eq!(short_age("2026-07-09T05:00:00Z", now), "7h ago");
        assert_eq!(short_age("2026-07-06T12:00:00Z", now), "3d ago");
        assert_eq!(short_age("2024-01-01T00:00:00Z", now), "2y ago");
        // Clock skew (future timestamp) degrades to "just now".
        assert_eq!(short_age("2026-07-09T12:05:00Z", now), "just now");
        // Unparseable input renders as-is.
        assert_eq!(short_age("garbage", now), "garbage");
    }

    #[test]
    fn parse_iso_utc_matches_known_epoch() {
        assert_eq!(parse_iso_utc("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_iso_utc("2001-09-09T01:46:40Z"), Some(1_000_000_000));
        assert_eq!(parse_iso_utc("2026-07-09"), None);
    }

    #[test]
    fn group_comments_threads_replies_orphans_and_outdated() {
        let index = group_comments(vec![
            // Deliberately out of order: grouping sorts by created_at.
            rc(
                2,
                "a.rs",
                Some("RIGHT"),
                Some(2),
                "reply",
                "bob",
                "2026-01-02T00:00:00Z",
                Some(1),
            ),
            rc(
                1,
                "a.rs",
                Some("RIGHT"),
                Some(2),
                "root",
                "alice",
                "2026-01-01T00:00:00Z",
                None,
            ),
            // Second thread at the same anchor, created later.
            rc(
                3,
                "a.rs",
                Some("RIGHT"),
                Some(2),
                "later",
                "carol",
                "2026-01-03T00:00:00Z",
                None,
            ),
            // LEFT-side thread on the same line number: distinct anchor.
            rc(
                4,
                "a.rs",
                Some("LEFT"),
                Some(2),
                "old side",
                "dave",
                "2026-01-04T00:00:00Z",
                None,
            ),
            // Outdated: no current line. Counts, never anchors.
            rc(
                5,
                "a.rs",
                None,
                None,
                "stale",
                "erin",
                "2026-01-05T00:00:00Z",
                None,
            ),
            // Orphaned reply: parent never fetched — dropped entirely.
            rc(
                6,
                "a.rs",
                Some("RIGHT"),
                Some(2),
                "orphan",
                "mallory",
                "2026-01-06T00:00:00Z",
                Some(999),
            ),
            // Reply-to-a-reply lands in the root's thread.
            rc(
                7,
                "a.rs",
                Some("RIGHT"),
                Some(2),
                "nested",
                "alice",
                "2026-01-07T00:00:00Z",
                Some(2),
            ),
            rc(
                8,
                "b.rs",
                Some("RIGHT"),
                Some(9),
                "other file",
                "bob",
                "2026-01-08T00:00:00Z",
                None,
            ),
        ]);
        let a = &index.threads["a.rs"];
        let right = &a[&(CommentSide::Right, 2)];
        assert_eq!(right.len(), 2);
        assert_eq!(right[0].root.id, 1);
        assert_eq!(
            right[0].replies.iter().map(|c| c.id).collect::<Vec<_>>(),
            vec![2, 7]
        );
        assert_eq!(right[1].root.id, 3);
        assert!(right[1].replies.is_empty());
        assert_eq!(a[&(CommentSide::Left, 2)][0].root.id, 4);
        // Counts: anchored = 3 (root+reply+nested) + 1 + 1 = 5 (the orphan
        // is dropped), outdated = 1.
        assert_eq!(index.counts["a.rs"], (5, 1));
        assert_eq!(index.counts["b.rs"], (1, 0));
        assert_eq!(index.threads["b.rs"][&(CommentSide::Right, 9)].len(), 1);
    }

    #[test]
    fn comment_wrap_cols_follows_the_pane_width() {
        let cw = 8.0; // 8px per column keeps the arithmetic obvious.
        let chrome = COMMENT_CARD_CHROME;
        // Unified uses the whole width; split only its half, so the same pane
        // yields roughly half the columns.
        assert_eq!(
            comment_wrap_cols(chrome + 40. * cw, ViewMode::Unified, cw),
            40
        );
        let split_pane = 2. * (chrome + 40. * cw) + SPLIT_DIVIDER;
        assert_eq!(comment_wrap_cols(split_pane, ViewMode::Split, cw), 40);
        // A roomy pane stops widening at the readability cap...
        assert_eq!(
            comment_wrap_cols(10_000., ViewMode::Unified, cw),
            COMMENT_WRAP_CHARS
        );
        // ...and a cramped one bottoms out rather than wrapping every word.
        assert_eq!(
            comment_wrap_cols(0., ViewMode::Split, cw),
            MIN_COMMENT_WRAP_CHARS
        );
        // Degenerate char width can't divide by zero.
        assert_eq!(
            comment_wrap_cols(800., ViewMode::Unified, 0.),
            COMMENT_WRAP_CHARS
        );
    }
}
