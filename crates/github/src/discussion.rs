//! Published PR discussion, independent of diff-row indexing and pending drafts.

use crate::{Author, ReviewComment};

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Reaction {
    pub content: String,
    pub count: u64,
}

impl Reaction {
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
    pub author: String,
    pub body: String,
    pub created_at: String,
    pub reactions: Vec<Reaction>,
    /// Present for review summaries (including bodyless approvals).
    pub review_state: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ReviewThread {
    pub id: String,
    pub is_resolved: bool,
    pub is_outdated: bool,
    pub path: String,
    pub side: String,
    pub line: Option<u64>,
    pub start_line: Option<u64>,
    /// Kept for the conversation's historical anchor, never used in diff rows.
    pub original_line: Option<u64>,
    pub original_start_line: Option<u64>,
    pub comments: Vec<DiscussionComment>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PrDiscussion {
    /// Issue comments and submitted review summaries. Thread replies live only
    /// under their thread, so merging the timeline cannot duplicate them.
    pub comments: Vec<DiscussionComment>,
    pub threads: Vec<ReviewThread>,
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
                    author: c.user.login.clone(),
                    body: c.body.clone(),
                    created_at: c.created_at.clone(),
                    reactions: Vec::new(),
                    review_state: None,
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
                is_outdated: root.line.is_none(),
                path: root.path.clone(),
                side: root.side.clone().unwrap_or_else(|| "RIGHT".into()),
                line: root.line,
                start_line: root.start_line,
                original_line: None,
                original_start_line: None,
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
                    count: 2
                }
                .emoji(),
                emoji
            );
        }
    }
}
