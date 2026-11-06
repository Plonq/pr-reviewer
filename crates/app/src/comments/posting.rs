//! Transient Conversation posts and exact-receipt identity aliases.
//! Never written to the GitHub cache; no remote identities are fabricated.

use github::{DiscussionComment, PrDiscussion};
use std::collections::HashMap;

pub(crate) struct Post {
    pub token: u64,
    pub reply_to: Option<u64>,
    pub author: String,
    pub body: String,
    pub status: Status,
    receipt: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Status {
    Posting,
    Syncing,
}

impl Status {
    pub fn label(self) -> &'static str {
        match self {
            Self::Posting => "Posting…",
            Self::Syncing => "Posted · syncing…",
        }
    }
}

#[derive(Default)]
pub(crate) struct Posts {
    pub pending: Vec<Post>,
    // Retain the view identity after loading, so markdown/media update in place.
    loaded: HashMap<super::CommentKey, super::CommentKey>,
}

impl Posts {
    pub fn begin(&mut self, token: u64, reply_to: Option<u64>, author: String, body: String) {
        self.pending.push(Post {
            token,
            reply_to,
            receipt: None,
            author,
            body,
            status: Status::Posting,
        });
    }

    pub fn complete(&mut self, token: u64, receipt: Option<u64>) {
        // A failure keeps the composer retryable; a saved draft is already in
        // the review session. Neither should leave an optimistic published card.
        self.pending.retain_mut(|post| {
            if post.token != token {
                return true;
            }
            post.receipt = receipt;
            post.status = Status::Syncing;
            receipt.is_some()
        });
    }

    pub fn reconcile(&mut self, discussion: &PrDiscussion) {
        self.pending.retain(|post| {
            let Some(id) = post.receipt else {
                return true;
            };
            let found = match post.reply_to {
                None => discussion
                    .comments
                    .iter()
                    .any(|c| c.id == id && c.review_state.is_none()),
                Some(root) => discussion.threads.iter().any(|thread| {
                    thread.comments.first().is_some_and(|c| c.id == root)
                        && thread.comments.iter().any(|c| c.id == id)
                }),
            };
            if found {
                let remote = if post.reply_to.is_some() {
                    super::CommentKey::Inline(id)
                } else {
                    super::CommentKey::Issue(id)
                };
                self.loaded
                    .insert(remote, super::CommentKey::Post(post.token));
            }
            !found
        });
    }

    pub fn identity(&self, comment: &DiscussionComment, inline: bool) -> super::CommentKey {
        let remote = if inline {
            super::CommentKey::Inline(comment.id)
        } else if comment.review_state.is_some() {
            super::CommentKey::Review(comment.id)
        } else {
            super::CommentKey::Issue(comment.id)
        };
        self.loaded.get(&remote).cloned().unwrap_or(remote)
    }

    #[cfg(test)]
    pub fn key(&self, kind: &str, comment: &DiscussionComment) -> String {
        self.identity(comment, kind == "review").markdown_key()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn comment(id: u64) -> DiscussionComment {
        DiscussionComment {
            permissions: Default::default(),
            id,
            author: "tester".into(),
            body: "same text".into(),
            created_at: "2026-10-26T00:00:00Z".into(),
            reactions: Vec::new(),
            review_state: None,
            review_id: None,
        }
    }

    #[test]
    fn exact_receipts_replace_placeholders_without_matching_older_identical_text() {
        let mut posts = Posts::default();
        posts.begin(1, None, "tester".into(), "same text".into());
        let key = super::super::CommentKey::Post(posts.pending[0].token).markdown_key();
        let mut discussion = PrDiscussion {
            comments: vec![comment(50)],
            threads: vec![],
        };
        posts.reconcile(&discussion);
        assert_eq!(
            posts.pending.len(),
            1,
            "fetching while the post is in flight does not remove it"
        );
        posts.complete(1, Some(100));
        posts.reconcile(&discussion);
        assert_eq!(
            posts.pending.len(),
            1,
            "a stale fetch or identical older body does not remove it"
        );
        assert_eq!(posts.pending[0].status.label(), "Posted · syncing…");
        discussion.comments.push(comment(100));
        posts.reconcile(&discussion);
        assert!(posts.pending.is_empty());
        assert_eq!(posts.key("main", &discussion.comments[1]), key);
        assert_ne!(posts.key("main", &discussion.comments[0]), key);
    }

    #[test]
    fn multiple_identical_posts_reconcile_independently() {
        let mut posts = Posts::default();
        for token in [1, 2] {
            posts.begin(token, None, "tester".into(), "same text".into());
            posts.complete(token, Some(token + 100));
        }
        posts.reconcile(&PrDiscussion {
            comments: vec![comment(102)],
            threads: vec![],
        });
        assert_eq!(posts.pending.len(), 1);
        assert_eq!(posts.pending[0].token, 1);
    }

    #[test]
    fn replies_only_reconcile_in_their_original_thread() {
        let mut posts = Posts::default();
        posts.begin(1, Some(11), "tester".into(), "same text".into());
        posts.complete(1, Some(100));
        let mut discussion = PrDiscussion {
            comments: vec![comment(100)],
            threads: vec![],
        };
        posts.reconcile(&discussion);
        assert_eq!(
            posts.pending.len(),
            1,
            "main and review comments have separate identities"
        );
        discussion.threads = PrDiscussion::from_review_comments(vec![github::ReviewComment {
            id: 11,
            path: "old.rs".into(),
            side: None,
            line: None,
            start_line: None,
            body: "root".into(),
            user: github::Author {
                login: "alice".into(),
            },
            created_at: "2026-10-25T00:00:00Z".into(),
            in_reply_to_id: None,
        }])
        .threads;
        discussion.threads[0].comments.push(comment(100));
        posts.reconcile(&discussion);
        assert!(posts.pending.is_empty());
        assert_eq!(posts.key("review", &comment(100)), "post-1");
        assert_ne!(posts.key("main", &comment(100)), "post-1");
    }

    #[test]
    fn failed_posts_and_saved_drafts_remove_only_their_own_placeholder() {
        let mut posts = Posts::default();
        posts.begin(1, None, "tester".into(), "first".into());
        posts.begin(2, None, "tester".into(), "second".into());
        posts.complete(1, None);
        assert_eq!(posts.pending.len(), 1);
        assert_eq!(posts.pending[0].token, 2);
    }
}
