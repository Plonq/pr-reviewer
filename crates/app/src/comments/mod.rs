//! App-owned, pure comment projection. Confirmed discussion/session storage stays
//! with its existing owners; only transient posts and identity aliases live here.
//!
//! Surface adapters deliberately preserve today's visibility: Conversation shows
//! published entries and pending replies; the tray also shows pending roots;
//! Changes indexes confirmed published/pending inline comments, not in-flight
//! Conversation posts. No projected data is passed to the persistence layer.

pub(crate) mod index;
pub(crate) mod posting;
#[cfg(test)]
mod tests;

use github::{DiscussionComment, PendingComment, PendingReview, PrDiscussion, ReviewThread};
use std::collections::{HashMap, HashSet};

/// UI identity is not an API target. In particular local post tokens never
/// occupy a reserved numeric range of GitHub comment IDs.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum CommentKey {
    Issue(u64),
    Review(u64),
    Inline(u64),
    Pending(u64),
    Post(u64),
}

impl CommentKey {
    /// Retain the existing persistent Markdown/media keys across this refactor.
    pub fn markdown_key(&self) -> String {
        match self {
            Self::Issue(id) => format!("main-false-{id}"),
            Self::Review(id) => format!("main-true-{id}"),
            Self::Inline(id) => format!("review-false-{id}"),
            Self::Pending(id) => format!("draft-{id}"),
            Self::Post(token) => format!("post-{token}"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct ThreadKey(pub String);

#[derive(Clone, Copy)]
pub(crate) enum Comment<'a> {
    Published(&'a DiscussionComment),
    Pending(&'a PendingComment),
    Posting(&'a posting::Post),
}

pub(crate) struct Thread<'a> {
    pub key: ThreadKey,
    /// Confirmed anchor, permissions, resolution, context and review ownership.
    pub confirmed: &'a ReviewThread,
    pub comments: Vec<Comment<'a>>,
}

/// Review wrappers, thread ordering and placement are retained, not reconstructed
/// from a synthetic GitHub discussion containing transient comments.
pub(crate) enum Entry<'a> {
    Comment(Comment<'a>),
    Review(&'a DiscussionComment, Vec<Thread<'a>>),
    Thread(Thread<'a>),
}

pub(crate) struct Projection<'a> {
    discussion: Option<&'a PrDiscussion>,
    posts: &'a posting::Posts,
    pub pending: Vec<&'a PendingComment>,
}

impl<'a> Projection<'a> {
    pub fn new(
        discussion: Option<&'a PrDiscussion>,
        pending: Option<&'a PendingReview>,
        posts: &'a posting::Posts,
    ) -> Self {
        let mut seen = HashSet::new();
        Self {
            discussion,
            posts,
            pending: pending
                .into_iter()
                .flat_map(|review| &review.comments)
                .filter(|comment| seen.insert(comment.id))
                .collect(),
        }
    }

    pub fn pending_comment(&self, id: u64) -> Option<&'a PendingComment> {
        self.pending
            .iter()
            .copied()
            .find(|comment| comment.id == id)
    }

    pub fn published(&self, target: &github::CommentTarget) -> Option<&'a DiscussionComment> {
        let discussion = self.discussion?;
        let (node, inline) = match target {
            github::CommentTarget::Issue(node) | github::CommentTarget::Review(node) => {
                (node, false)
            }
            github::CommentTarget::ReviewComment(node) => (node, true),
        };
        if inline {
            discussion
                .threads
                .iter()
                .flat_map(|t| &t.comments)
                .find(|c| &c.permissions.node_id == node)
        } else {
            discussion
                .comments
                .iter()
                .find(|c| &c.permissions.node_id == node)
        }
    }

    pub fn thread_for_inline(&self, id: u64) -> Option<&'a ReviewThread> {
        self.discussion?
            .threads
            .iter()
            .find(|thread| thread.comments.iter().any(|c| c.id == id))
    }

    pub fn inline(&self, id: u64) -> Option<Comment<'a>> {
        self.pending_comment(id).map(Comment::Pending).or_else(|| {
            self.discussion?
                .threads
                .iter()
                .flat_map(|t| &t.comments)
                .find(|c| c.id == id)
                .map(Comment::Published)
        })
    }

    pub fn key(&self, comment: Comment<'_>, inline: bool) -> CommentKey {
        match comment {
            Comment::Pending(c) => CommentKey::Pending(c.id),
            Comment::Posting(post) => CommentKey::Post(post.token),
            Comment::Published(c) => self.posts.identity(c, inline),
        }
    }

    fn thread(&self, thread: &'a ReviewThread) -> Thread<'a> {
        let root = thread.comments[0].id;
        let mut comments: Vec<_> = thread
            .comments
            .iter()
            .filter(|c| self.pending_comment(c.id).is_none())
            .map(Comment::Published)
            .collect();
        comments.extend(
            self.posts
                .pending
                .iter()
                .filter(|post| post.reply_to == Some(root))
                .map(Comment::Posting),
        );
        comments.extend(
            self.pending
                .iter()
                .copied()
                .filter(|draft| draft.in_reply_to_id == Some(root))
                .map(Comment::Pending),
        );
        Thread {
            key: ThreadKey(thread.id.clone()),
            confirmed: thread,
            comments,
        }
    }

    pub fn conversation(&self) -> Vec<Entry<'a>> {
        let mut reviews = HashMap::new();
        let mut entries: Vec<_> = self
            .discussion
            .into_iter()
            .flat_map(|d| &d.comments)
            .enumerate()
            .map(|(ix, comment)| {
                if comment.review_state.is_some() {
                    if let Some(id) = &comment.review_id {
                        reviews.insert(id.as_str(), ix);
                    }
                    Entry::Review(comment, Vec::new())
                } else {
                    Entry::Comment(Comment::Published(comment))
                }
            })
            .collect();
        // A session-owned pending root belongs to Changes/the tray, even if
        // it is also returned in the discussion. Do not leave an empty public
        // thread shell after deduplicating that root.
        let mut threads: Vec<_> = self
            .discussion
            .into_iter()
            .flat_map(|d| &d.threads)
            .filter(|t| {
                t.comments
                    .first()
                    .is_some_and(|root| self.pending_comment(root.id).is_none())
            })
            .collect();
        threads.sort_by(|a, b| thread_order(a).cmp(&thread_order(b)));
        for thread in threads {
            let review = thread.comments[0]
                .review_id
                .as_deref()
                .and_then(|id| reviews.get(id));
            if let Some(Entry::Review(_, threads)) = review.and_then(|&ix| entries.get_mut(ix)) {
                threads.push(self.thread(thread));
            } else {
                entries.push(Entry::Thread(self.thread(thread)));
            }
        }
        entries.sort_by(|a, b| entry_order(a).cmp(&entry_order(b)));
        entries.retain(|entry| match entry {
            Entry::Review(comment, threads) => {
                !threads.is_empty()
                    || !matches!(comment.review_state.as_deref(), Some("COMMENTED"))
                    || has_content(&comment.body)
            }
            _ => true,
        });
        entries.extend(
            self.posts
                .pending
                .iter()
                .filter(|post| post.reply_to.is_none())
                .map(|post| Entry::Comment(Comment::Posting(post))),
        );
        entries
    }

    pub fn diff_index(&self) -> index::CommentIndex {
        let published = self
            .discussion
            .into_iter()
            .flat_map(|d| d.review_comments())
            .map(index::ReviewComment::from)
            .collect();
        index::group_comments(merge_pending(published, self.pending.iter().copied()))
    }
}

/// Whitespace and HTML-only metadata do not warrant a review summary card.
pub(crate) fn has_content(mut body: &str) -> bool {
    loop {
        body = body.trim();
        let Some(rest) = body.strip_prefix("<!--") else {
            return !body.is_empty();
        };
        let Some(end) = rest.find("-->") else {
            return false;
        };
        body = &rest[end + 3..];
    }
}

fn thread_order(thread: &ReviewThread) -> (&str, u8, u64, &str) {
    (&thread.comments[0].created_at, 2, 0, &thread.id)
}

fn entry_order<'a>(entry: &'a Entry<'_>) -> (&'a str, u8, u64, &'a str) {
    match entry {
        Entry::Comment(Comment::Published(c)) => (&c.created_at, 0, c.id, ""),
        Entry::Review(c, _) => (&c.created_at, 1, c.id, ""),
        Entry::Thread(t) => thread_order(t.confirmed),
        _ => unreachable!("transient entries are appended after sorting"),
    }
}

/// The pending session is authoritative when GitHub returns the same inline ID
/// in both responses. Also used by the compatibility index parity tests.
fn merge_pending<'a>(
    mut published: Vec<index::ReviewComment>,
    pending: impl IntoIterator<Item = &'a PendingComment>,
) -> Vec<index::ReviewComment> {
    let mut seen = HashSet::new();
    let pending: Vec<_> = pending.into_iter().filter(|c| seen.insert(c.id)).collect();
    published.retain(|c| c.time != index::CommentTime::Pending && !seen.contains(&c.id));
    published.extend(pending.into_iter().map(|comment| {
        let position = crate::comment_anchor::display_line(comment.line, comment.start_line);
        index::ReviewComment {
            id: comment.id,
            path: comment.path.clone(),
            line: position.map(|(_, line)| line),
            side: comment.side,
            start_line: position.and_then(|(start, _)| start),
            body: comment.body.clone(),
            author: "you".into(),
            time: index::CommentTime::Pending,
            in_reply_to_id: comment.in_reply_to_id,
        }
    }));
    published
}
