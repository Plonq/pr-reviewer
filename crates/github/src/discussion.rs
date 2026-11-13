//! Published PR discussion, independent of diff-row indexing and pending drafts.

use crate::{Author, ReviewComment};

/// Opaque identity and API family for a published comment mutation.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum CommentTarget {
    Issue(String),
    Review(String),
    ReviewComment(String),
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CommentPermissions {
    pub node_id: String,
    pub can_edit: bool,
    pub can_delete: bool,
    #[serde(default)]
    pub can_react: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Reaction {
    pub content: String,
    pub count: u64,
    #[serde(default)]
    pub viewer_has_reacted: bool,
}

impl Reaction {
    pub const KINDS: [&'static str; 8] = [
        "THUMBS_UP",
        "THUMBS_DOWN",
        "LAUGH",
        "HOORAY",
        "CONFUSED",
        "HEART",
        "ROCKET",
        "EYES",
    ];

    pub fn emoji(&self) -> &str {
        match self.content.as_str() {
            "THUMBS_UP" => "👍",
            "THUMBS_DOWN" => "👎",
            "LAUGH" => "😄",
            "HOORAY" => "🎉",
            "CONFUSED" => "😕",
            "HEART" => "❤️",
            "ROCKET" => "🚀",
            "EYES" => "👀",
            other => other,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DiscussionComment {
    pub id: u64,
    /// Missing permissions in older caches deliberately default to read-only.
    #[serde(default)]
    pub permissions: CommentPermissions,
    pub author: String,
    pub body: String,
    pub created_at: String,
    pub reactions: Vec<Reaction>,
    /// Present for review summaries (including bodyless approvals).
    pub review_state: Option<String>,
    /// GraphQL review identity: the summary's own id, or an inline comment's
    /// owning review. Replies do not move a thread into a later review.
    #[serde(default)]
    pub review_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ReviewCodeContext {
    /// Immutable blob revision and coordinates from GitHub's positioning API.
    pub commit_id: String,
    pub line: u64,
    pub start_line: Option<u64>,
    pub first_line: u64,
    pub lines: Vec<String>,
}

impl ReviewCodeContext {
    pub(crate) fn from_blob(
        commit_id: &str,
        start_line: Option<u64>,
        line: u64,
        text: &str,
    ) -> Option<Self> {
        let start = start_line.unwrap_or(line);
        if start == 0 || start > line {
            return None;
        }
        let first_line = start.saturating_sub(3).max(line.saturating_sub(19)).max(1);
        let count = usize::try_from(line - first_line + 1).ok()?;
        let lines: Vec<_> = text
            .lines()
            .skip(usize::try_from(first_line - 1).ok()?)
            .take(count)
            .map(str::to_owned)
            .collect();
        // Do not present unrelated code if the captured line isn't in the blob.
        if lines.len() != count {
            return None;
        }
        Some(Self {
            commit_id: commit_id.into(),
            line,
            start_line,
            first_line,
            lines,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ReviewThread {
    pub id: String,
    pub is_resolved: bool,
    /// Older cached discussions are read-only until refreshed.
    #[serde(default)]
    pub viewer_can_resolve: bool,
    #[serde(default)]
    pub viewer_can_unresolve: bool,
    pub is_outdated: bool,
    pub path: String,
    pub side: String,
    pub line: Option<u64>,
    pub start_line: Option<u64>,
    /// Kept for the conversation's historical anchor, never used in diff rows.
    pub original_line: Option<u64>,
    pub original_start_line: Option<u64>,
    /// The root comment's captured patch, not the PR's current file contents.
    #[serde(default)]
    pub diff_hunk: String,
    /// Outside-diff comments have no hunk; retain a bounded excerpt from their
    /// captured blob so the conversation also works offline.
    #[serde(default)]
    pub code_context: Option<ReviewCodeContext>,
    pub comments: Vec<DiscussionComment>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PrDiscussion {
    /// Issue comments and submitted review summaries. Thread replies live only
    /// under their thread, so merging the timeline cannot duplicate them.
    pub comments: Vec<DiscussionComment>,
    pub threads: Vec<ReviewThread>,
    #[serde(default)]
    pub events: Vec<DiscussionEvent>,
}

/// Non-comment activity from GitHub's timeline. Kept separate from mutable
/// comments so events never acquire comment actions or markdown interpretation.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DiscussionEvent {
    pub id: String,
    pub actor: String,
    pub created_at: String,
    pub description: String,
}

impl PrDiscussion {
    /// Compatibility for backends that only expose flat review comments.
    pub fn from_review_comments(comments: Vec<ReviewComment>) -> Self {
        let parents: std::collections::HashMap<_, _> =
            comments.iter().map(|c| (c.id, c.in_reply_to_id)).collect();
        let mut discussion = Self::default();
        for root in comments.iter().filter(|c| c.in_reply_to_id.is_none()) {
            let mut members: Vec<_> = comments
                .iter()
                .filter(|c| {
                    let mut id = c.id;
                    for _ in 0..=comments.len() {
                        if id == root.id {
                            return true;
                        }
                        match parents.get(&id).copied().flatten() {
                            Some(parent) => id = parent,
                            None => break,
                        }
                    }
                    false
                })
                .map(|c| DiscussionComment {
                    id: c.id,
                    permissions: CommentPermissions::default(),
                    author: c.user.login.clone(),
                    body: c.body.clone(),
                    created_at: c.created_at.clone(),
                    reactions: Vec::new(),
                    review_state: None,
                    review_id: None,
                })
                .collect();
            // Preserve root identity even for a malformed early timestamp on
            // a reply; only replies are sorted against one another.
            members.sort_by(|a, b| {
                (a.id != root.id)
                    .cmp(&(b.id != root.id))
                    .then(a.created_at.cmp(&b.created_at))
                    .then(a.id.cmp(&b.id))
            });
            discussion.threads.push(ReviewThread {
                id: root.id.to_string(),
                is_resolved: false,
                viewer_can_resolve: false,
                viewer_can_unresolve: false,
                is_outdated: root.line.is_none(),
                path: root.path.clone(),
                side: root.side.clone().unwrap_or_else(|| "RIGHT".into()),
                line: root.line,
                start_line: root.start_line,
                original_line: None,
                original_start_line: None,
                diff_hunk: String::new(),
                code_context: None,
                comments: members,
            });
        }
        discussion
    }

    /// The diff's existing flat-comment contract, derived from the same fetch.
    pub fn review_comments(&self) -> Vec<ReviewComment> {
        self.threads
            .iter()
            .flat_map(|thread| {
                let root = thread.comments.first().map(|c| c.id);
                thread
                    .comments
                    .iter()
                    .enumerate()
                    .map(move |(ix, c)| ReviewComment {
                        id: c.id,
                        path: thread.path.clone(),
                        line: thread.line,
                        start_line: thread.start_line,
                        side: Some(thread.side.clone()),
                        body: c.body.clone(),
                        user: Author {
                            login: c.author.clone(),
                        },
                        created_at: c.created_at.clone(),
                        in_reply_to_id: if ix == 0 { None } else { root },
                    })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_snapshots_without_review_identity_or_code_still_load() {
        let old = serde_json::json!({
            "comments": [],
            "threads": [{
                "id": "T1", "is_resolved": false, "is_outdated": true,
                "path": "old.rs", "side": "LEFT", "line": null, "start_line": null,
                "original_line": 3, "original_start_line": null,
                "comments": [{"id": 1, "author": "alice", "body": "old comment",
                    "created_at": "2026-01-01T00:00:00Z", "reactions": [], "review_state": null}]
            }]
        });
        let discussion: PrDiscussion = serde_json::from_value(old).unwrap();
        assert!(discussion.events.is_empty());
        assert!(discussion.threads[0].diff_hunk.is_empty());
        assert!(discussion.threads[0].code_context.is_none());
        assert!(discussion.threads[0].comments[0].review_id.is_none());
        assert!(!discussion.threads[0].viewer_can_resolve);
        assert!(!discussion.threads[0].viewer_can_unresolve);
        assert_eq!(
            serde_json::from_value::<PrDiscussion>(serde_json::to_value(&discussion).unwrap())
                .unwrap(),
            discussion
        );
    }

    #[test]
    fn blob_context_is_bounded_and_requires_the_captured_line() {
        let text = (1..=100)
            .map(|n| format!("line {n}\r\n"))
            .collect::<String>();
        let context = ReviewCodeContext::from_blob("old-commit", Some(2), 90, &text).unwrap();
        assert_eq!(context.first_line, 71);
        assert_eq!(context.lines.len(), 20);
        assert_eq!(context.lines.last().unwrap(), "line 90");
        assert_eq!(context.commit_id, "old-commit");
        assert!(ReviewCodeContext::from_blob("old", None, 101, &text).is_none());
        assert!(ReviewCodeContext::from_blob("old", None, 0, &text).is_none());
        assert!(ReviewCodeContext::from_blob("old", Some(10), 9, &text).is_none());
        let empty_line = ReviewCodeContext::from_blob("old", None, 2, "one\n\nthree\n").unwrap();
        assert_eq!(empty_line.lines, ["one", ""]);
    }

    #[test]
    fn old_cached_reactions_and_permissions_default_to_read_only() {
        let reaction: Reaction =
            serde_json::from_value(serde_json::json!({"content":"HEART", "count":3})).unwrap();
        assert!(!reaction.viewer_has_reacted);
        let permissions: CommentPermissions = serde_json::from_value(
            serde_json::json!({"node_id":"IC1", "can_edit":true, "can_delete":true}),
        )
        .unwrap();
        assert!(!permissions.can_react);
    }

    #[test]
    fn all_github_reaction_kinds_have_display_glyphs() {
        for (content, emoji) in [
            ("THUMBS_UP", "👍"),
            ("THUMBS_DOWN", "👎"),
            ("LAUGH", "😄"),
            ("HOORAY", "🎉"),
            ("CONFUSED", "😕"),
            ("HEART", "❤️"),
            ("ROCKET", "🚀"),
            ("EYES", "👀"),
            ("FUTURE", "FUTURE"),
        ] {
            assert_eq!(
                Reaction {
                    content: content.into(),
                    count: 2,
                    viewer_has_reacted: false,
                }
                .emoji(),
                emoji
            );
        }
    }
}
