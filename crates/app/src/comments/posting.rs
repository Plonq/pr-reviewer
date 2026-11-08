//! Transient comment creates, recovery payloads and exact-receipt identity aliases.
//! Never written to the GitHub cache; no remote identities are fabricated.

use github::{DiscussionComment, PrDiscussion};
use std::collections::{HashMap, HashSet};

#[derive(Clone)]
pub(crate) struct Post {
    pub token: u64,
    pub reply_to: Option<u64>,
    pub author: String,
    pub body: String,
    pub status: Status,
    receipt: Option<u64>,
    thread_id: Option<String>,
    pub anchor: Option<github::CommentAnchor>,
    pub context: Vec<String>,
    pub pending: bool,
    pub command: Option<github::review_session::Command>,
    pub error: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Status {
    Posting,
    Syncing,
    Unverified,
}

impl Status {
    pub fn label(self) -> &'static str {
        match self {
            Self::Posting => "Posting…",
            Self::Syncing => "Posted · syncing…",
            Self::Unverified => "Unverified · check GitHub before retrying",
        }
    }
}

#[derive(Default)]
pub(crate) struct Posts {
    pub pending: Vec<Post>,
    pub roots: Vec<Post>,
    pub failed: Vec<Post>,
    pub changes: super::mutations::Changes,
    // Retain the view identity after loading, so markdown/media update in place.
    loaded: HashMap<super::CommentKey, super::CommentKey>,
    authors: HashMap<super::CommentKey, String>,
    public: HashSet<super::CommentKey>,
}

impl Posts {
    pub fn begin(&mut self, token: u64, reply_to: Option<u64>, author: String, body: String) {
        self.authors
            .insert(super::CommentKey::Post(token), author.clone());
        self.pending.push(Post {
            token,
            reply_to,
            receipt: None,
            thread_id: None,
            author,
            body,
            status: Status::Posting,
            anchor: None,
            context: Vec::new(),
            pending: false,
            command: None,
            error: None,
        });
    }

    #[cfg(test)]
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
        let mut roots = Vec::new();
        self.pending.retain_mut(|post| {
            let Some(id) = post.receipt else {
                return true;
            };
            let found = match post.reply_to {
                None if post.anchor.is_none() => discussion
                    .comments
                    .iter()
                    .any(|c| c.id == id && c.review_state.is_none()),
                None => discussion
                    .threads
                    .iter()
                    .any(|thread| thread.comments.first().is_some_and(|c| c.id == id)),
                Some(root) => discussion.threads.iter().any(|thread| {
                    thread.comments.first().is_some_and(|c| c.id == root)
                        && thread.comments.iter().any(|c| c.id == id)
                }),
            };
            if found {
                let remote = if post.anchor.is_some() || post.reply_to.is_some() {
                    super::CommentKey::Inline(id)
                } else {
                    super::CommentKey::Issue(id)
                };
                self.loaded
                    .insert(remote, super::CommentKey::Post(post.token));
                if post.anchor.is_some() && post.reply_to.is_none() && !post.pending {
                    post.thread_id = discussion
                        .threads
                        .iter()
                        .find(|t| t.comments.iter().any(|c| c.id == id))
                        .map(|t| t.id.clone());
                    roots.push(post.clone());
                }
            }
            !found
        });
        self.roots.extend(roots);
        self.roots.retain(|post| {
            discussion.threads.iter().any(|t| {
                post.thread_id.as_ref() == Some(&t.id)
                    || t.comments.iter().any(|c| Some(c.id) == post.receipt)
            }) || post
                .thread_id
                .as_ref()
                .is_some_and(|id| self.changes.threads.contains_key(id))
        });
    }

    pub fn begin_create(
        &mut self,
        token: u64,
        command: github::review_session::Command,
        anchor: Option<github::CommentAnchor>,
        pending: bool,
        author: String,
        body: String,
        reply_to: Option<u64>,
    ) {
        self.begin(token, reply_to, author, body);
        let post = self.pending.last_mut().unwrap();
        post.command = Some(command);
        post.anchor = anchor;
        post.pending = pending;
        if !pending {
            self.public.insert(super::CommentKey::Post(token));
        }
    }

    pub fn finish_create(
        &mut self,
        token: u64,
        result: &Result<github::MutationReceipt, github::ClientError>,
    ) {
        let Some(ix) = self.pending.iter().position(|p| p.token == token) else {
            return;
        };
        match result {
            Ok(receipt) => {
                let post = &mut self.pending[ix];
                post.status = Status::Syncing;
                let remote = match receipt {
                    github::MutationReceipt::PublishedCreated(c) => {
                        self.public.insert(super::CommentKey::Post(token));
                        post.receipt = Some(c.id);
                        if let Some(body) = &c.body {
                            post.body = body.clone();
                        }
                        if let Some(author) = &c.author {
                            post.author = author.clone();
                        }
                        if let Some(anchor) = &c.anchor {
                            post.anchor = Some(anchor.clone());
                        }
                        post.pending = false;
                        if post.anchor.is_some() || post.reply_to.is_some() {
                            super::CommentKey::Inline(c.id)
                        } else {
                            super::CommentKey::Issue(c.id)
                        }
                    }
                    github::MutationReceipt::PendingCreated { created, .. } => {
                        post.pending = true;
                        self.public.remove(&super::CommentKey::Post(token));
                        self.loaded.insert(
                            super::CommentKey::Inline(created.comment.id),
                            super::CommentKey::Post(token),
                        );
                        super::CommentKey::Pending(created.comment.id)
                    }
                    _ => return,
                };
                self.loaded.insert(remote, super::CommentKey::Post(token));
                if matches!(receipt, github::MutationReceipt::PendingCreated { .. }) {
                    self.pending.remove(ix);
                }
            }
            Err(error) if error.mutation_is_uncertain() => {
                let post = &mut self.pending[ix];
                post.status = Status::Unverified;
                post.error = Some(error.to_string());
            }
            Err(error) => {
                let mut post = self.pending.remove(ix);
                post.error = Some(error.to_string());
                self.failed.push(post);
            }
        }
    }

    pub fn has_uncertain(&self) -> bool {
        self.pending.iter().any(|p| p.status == Status::Unverified) || self.changes.has_uncertain()
    }

    pub fn remote_id(&self, key: super::CommentKey) -> Option<u64> {
        match key {
            super::CommentKey::Inline(id) | super::CommentKey::Pending(id) => Some(id),
            super::CommentKey::Post(_) => self.loaded.iter().find_map(|(remote, local)| {
                if *local != key {
                    return None;
                }
                match remote {
                    super::CommentKey::Inline(id) | super::CommentKey::Pending(id) => Some(*id),
                    _ => None,
                }
            }),
            _ => None,
        }
    }

    pub fn reserve_reactions(&self, key: super::CommentKey) -> bool {
        self.public.contains(&key)
    }

    pub fn author(&self, key: super::CommentKey) -> Option<&str> {
        self.authors.get(&key).map(String::as_str)
    }

    pub fn alias(&self, remote: super::CommentKey) -> super::CommentKey {
        self.loaded.get(&remote).copied().unwrap_or(remote)
    }

    pub fn is_thread_shell(&self, post: &Post, thread: &github::ReviewThread) -> bool {
        post.thread_id.as_ref() == Some(&thread.id)
            || thread.comments.iter().any(|c| Some(c.id) == post.receipt)
    }

    pub fn thread_shell(&self, thread: &github::ReviewThread) -> Option<&Post> {
        self.roots
            .iter()
            .chain(&self.pending)
            .find(|p| p.anchor.is_some() && p.reply_to.is_none() && self.is_thread_shell(p, thread))
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
