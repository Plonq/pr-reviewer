//! App-owned, pure comment projection. Confirmed discussion/session storage stays
//! with its existing owners; transient posts, identity aliases and read/write
//! reconciliation barriers live here.
//!
//! Conversation shows published entries and pending replies; the tray also shows
//! pending roots. Inline creation overlays feed all applicable surfaces from the
//! same projection. No projected data is passed to the persistence layer.

#[cfg(test)]
mod creation_tests;
pub(crate) mod index;
#[cfg(test)]
mod mutation_tests;
pub(crate) mod mutations;
pub(crate) mod posting;
pub(crate) mod sync;
#[cfg(test)]
mod tests;

use github::{DiscussionComment, PendingComment, PendingReview, PrDiscussion, ReviewThread};
use std::collections::{HashMap, HashSet};

/// UI identity is not an API target. In particular local post tokens never
/// occupy a reserved numeric range of GitHub comment IDs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
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

impl From<u64> for CommentKey {
    fn from(id: u64) -> Self {
        Self::Inline(id)
    }
}
#[cfg(test)]
impl PartialEq<u64> for CommentKey {
    fn eq(&self, id: &u64) -> bool {
        matches!(self, Self::Inline(n) | Self::Pending(n) if n == id)
    }
}
impl std::fmt::Display for CommentKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Inline(id) | Self::Pending(id) | Self::Issue(id) | Self::Review(id) => {
                write!(f, "{id}")
            }
            Self::Post(token) => write!(f, "post-{token}"),
        }
    }
}

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
    CreatedThread(&'a posting::Post, Vec<Comment<'a>>),
}

pub(crate) struct Projection<'a> {
    discussion: Option<&'a PrDiscussion>,
    posts: &'a posting::Posts,
    pub pending: Vec<&'a PendingComment>,
    all_pending: Vec<&'a PendingComment>,
}

impl<'a> Projection<'a> {
    pub fn new(
        discussion: Option<&'a PrDiscussion>,
        pending: Option<&'a PendingReview>,
        posts: &'a posting::Posts,
    ) -> Self {
        let mut seen = HashSet::new();
        let all_pending: Vec<_> = pending
            .into_iter()
            .flat_map(|r| &r.comments)
            .filter(|c| seen.insert(c.id))
            .collect();
        Self {
            discussion,
            posts,
            pending: all_pending
                .iter()
                .copied()
                .filter(|c| !posts.changes.hidden(CommentKey::Pending(c.id)))
                .collect(),
            all_pending,
        }
    }

    pub fn pending_comment(&self, id: u64) -> Option<&'a PendingComment> {
        self.pending
            .iter()
            .copied()
            .find(|comment| comment.id == id)
    }

    pub fn published(&self, target: &github::CommentTarget) -> Option<&'a DiscussionComment> {
        published(self.discussion?, target).filter(|c| {
            !self.posts.changes.hidden(published_key(
                c,
                matches!(target, github::CommentTarget::ReviewComment(_)),
            ))
        })
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
                .find(|c| c.id == id && !self.posts.changes.hidden(CommentKey::Inline(c.id)))
                .map(Comment::Published)
        })
    }

    pub fn reply_target(&self, thread: &'a ReviewThread) -> Option<u64> {
        self.thread(thread)
            .comments
            .into_iter()
            .find_map(|c| match c {
                Comment::Published(c) => Some(c.id),
                Comment::Pending(c) => Some(c.id),
                Comment::Posting(_) => None,
            })
    }

    pub fn key(&self, comment: Comment<'_>, inline: bool) -> CommentKey {
        match comment {
            Comment::Pending(c) => self.posts.alias(CommentKey::Pending(c.id)),
            Comment::Posting(post) => CommentKey::Post(post.token),
            Comment::Published(c) => self.posts.identity(c, inline),
        }
    }

    pub fn body(&self, comment: Comment<'a>, inline: bool) -> &'a str {
        match comment {
            Comment::Published(c) => self.posts.changes.body(published_key(c, inline), &c.body),
            Comment::Pending(c) => self.posts.changes.body(CommentKey::Pending(c.id), &c.body),
            Comment::Posting(p) => &p.body,
        }
    }

    pub(crate) fn threads(&self) -> Vec<&'a ReviewThread> {
        let mut threads: Vec<_> = self
            .discussion
            .into_iter()
            .flat_map(|d| &d.threads)
            .filter(|t| !t.comments.is_empty())
            .collect();
        threads.extend(self.posts.changes.threads.values().filter(|t| {
            !self.discussion.is_some_and(|d| {
                d.threads
                    .iter()
                    .any(|current| current.id == t.id && !current.comments.is_empty())
            })
        }));
        threads
    }

    fn placement(&self, thread: &'a ReviewThread) -> &'a ReviewThread {
        self.posts.changes.threads.get(&thread.id).unwrap_or(thread)
    }

    fn belongs(&self, thread: &ReviewThread, mut parent: u64) -> bool {
        let mut seen = HashSet::new();
        while seen.insert(parent) {
            if thread.comments.iter().any(|c| c.id == parent)
                || self.posts.changes.members.get(&parent) == Some(&thread.id)
            {
                return true;
            }
            let Some(next) = self
                .all_pending
                .iter()
                .find(|c| c.id == parent)
                .copied()
                .or_else(|| self.posts.changes.pending_roots.get(&parent))
                .and_then(|c| c.in_reply_to_id)
            else {
                return false;
            };
            parent = next;
        }
        false
    }

    fn thread(&self, thread: &'a ReviewThread) -> Thread<'a> {
        let actual = self.discussion.is_some_and(|d| {
            d.threads
                .iter()
                .any(|t| t.id == thread.id && !t.comments.is_empty())
        });
        let mut comments: Vec<_> = thread
            .comments
            .iter()
            .filter(|c| {
                actual
                    && !self.posts.changes.hidden(CommentKey::Inline(c.id))
                    && !self.all_pending.iter().any(|p| p.id == c.id)
            })
            .map(Comment::Published)
            .collect();
        comments.extend(
            self.posts
                .pending
                .iter()
                .filter(|post| post.reply_to.is_some_and(|id| self.belongs(thread, id)))
                .map(Comment::Posting),
        );
        comments.extend(
            self.pending
                .iter()
                .copied()
                .filter(|draft| {
                    draft
                        .in_reply_to_id
                        .is_some_and(|id| self.belongs(thread, id))
                })
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
            .filter(|c| !self.posts.changes.hidden(published_key(c, false)))
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
            .threads()
            .into_iter()
            .filter(|t| self.posts.thread_shell(t).is_none())
            .filter(|t| {
                let root = self.placement(t).comments[0].id;
                !self.all_pending.iter().any(|c| c.id == root)
                    && !self
                        .posts
                        .changes
                        .pending_roots
                        .get(&root)
                        .is_some_and(|c| c.in_reply_to_id.is_none())
            })
            .collect();
        threads
            .sort_by(|a, b| thread_order(self.placement(a)).cmp(&thread_order(self.placement(b))));
        for thread in threads {
            if self.thread(thread).comments.is_empty() {
                continue;
            }
            let review = self.placement(thread).comments[0]
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
                    || has_content(self.body(Comment::Published(comment), false))
            }
            _ => true,
        });
        entries.extend(
            self.posts
                .pending
                .iter()
                .filter(|post| post.reply_to.is_none() && post.anchor.is_none() && !post.pending)
                .map(|post| Entry::Comment(Comment::Posting(post))),
        );
        let mut roots: Vec<_> = self
            .posts
            .roots
            .iter()
            .chain(&self.posts.pending)
            .filter(|p| p.anchor.is_some() && p.reply_to.is_none() && !p.pending)
            .collect();
        roots.sort_by_key(|post| post.token);
        for post in roots {
            let thread = self
                .threads()
                .into_iter()
                .find(|t| self.posts.is_thread_shell(post, t));
            let comments = thread
                .map(|t| self.thread(t).comments)
                .unwrap_or_else(|| vec![Comment::Posting(post)]);
            if !comments.is_empty() {
                entries.push(Entry::CreatedThread(post, comments));
            }
        }
        entries
    }

    pub fn pending_entries(&self) -> Vec<Comment<'a>> {
        self.pending
            .iter()
            .copied()
            .map(Comment::Pending)
            .chain(
                self.posts
                    .pending
                    .iter()
                    .filter(|p| p.pending)
                    .map(Comment::Posting),
            )
            .collect()
    }

    pub fn pending_count(&self) -> usize {
        self.pending_entries().len()
    }
    pub fn has_pending(&self) -> bool {
        self.pending_count() > 0
    }

    pub fn remote_id(&self, key: CommentKey) -> Option<u64> {
        self.posts.remote_id(key)
    }
    pub fn inline_key(&self, id: u64) -> CommentKey {
        self.posts.alias(
            if self.all_pending.iter().any(|c| c.id == id)
                || self.posts.changes.pending_roots.contains_key(&id)
            {
                CommentKey::Pending(id)
            } else {
                CommentKey::Inline(id)
            },
        )
    }

    pub fn diff_index(&self) -> index::CommentIndex {
        let mut ghosts = HashSet::new();
        let mut published = Vec::new();
        for thread in self.threads() {
            let placement = self.placement(thread);
            let root = &placement.comments[0];
            let actual = self.discussion.is_some_and(|d| {
                d.threads
                    .iter()
                    .any(|t| t.id == thread.id && !t.comments.is_empty())
            });
            let mut members: Vec<_> = if actual {
                thread.comments.iter().collect()
            } else {
                Vec::new()
            };
            if !members.iter().any(|c| c.id == root.id) {
                ghosts.insert(CommentKey::Inline(root.id));
                members.insert(0, root);
            }
            let position = crate::comment_anchor::display_line(thread.line, thread.start_line);
            for c in members {
                published.push(index::ReviewComment {
                    id: CommentKey::Inline(c.id),
                    path: thread.path.clone(),
                    line: position.map(|(_, line)| line),
                    start_line: position.and_then(|(start, _)| start),
                    side: if thread.side == "LEFT" {
                        github::DiffSide::Left
                    } else {
                        github::DiffSide::Right
                    },
                    body: c.body.clone(),
                    author: c.author.clone(),
                    time: index::CommentTime::Posted(c.created_at.clone()),
                    in_reply_to_id: (c.id != root.id).then_some(CommentKey::Inline(root.id)),
                });
            }
        }
        let mut pending = self.all_pending.clone();
        for root in self
            .posts
            .changes
            .pending_roots
            .values()
            .filter(|c| !self.all_pending.iter().any(|p| p.id == c.id))
        {
            ghosts.insert(CommentKey::Pending(root.id));
            pending.push(root);
        }
        let mut comments = merge_pending(published, pending);
        let mut hidden = HashSet::new();
        for comment in &mut comments {
            let remote = comment.id;
            if self.posts.changes.hidden(remote) || ghosts.contains(&remote) {
                hidden.insert(self.posts.alias(remote));
            }
            comment.body = self.posts.changes.body(remote, &comment.body).to_owned();
            if let Some(CommentKey::Inline(parent)) = comment.in_reply_to_id {
                if let Some(thread) = self.threads().into_iter().find(|t| self.belongs(t, parent)) {
                    comment.in_reply_to_id =
                        Some(CommentKey::Inline(self.placement(thread).comments[0].id));
                }
            }
            comment.id = self.posts.alias(comment.id);
            if comment.time == index::CommentTime::Pending {
                if let Some(author) = self.posts.author(comment.id) {
                    comment.author = author.into();
                }
            }
            comment.in_reply_to_id = comment.in_reply_to_id.map(|id| match id {
                CommentKey::Inline(id) | CommentKey::Pending(id) => self.inline_key(id),
                _ => id,
            });
        }
        comments.extend(
            self.posts
                .pending
                .iter()
                .filter(|p| p.anchor.is_some())
                .map(|post| {
                    let anchor = post.anchor.as_ref().unwrap();
                    index::ReviewComment {
                        id: CommentKey::Post(post.token),
                        path: anchor.path.clone(),
                        line: Some(anchor.line),
                        side: anchor.side,
                        start_line: anchor.start_line,
                        body: post.body.clone(),
                        author: post.author.clone(),
                        time: index::CommentTime::Posting(post.status, post.pending),
                        in_reply_to_id: post.reply_to.map(|id| self.inline_key(id)),
                    }
                }),
        );
        let mut index = index::group_comments(index::retain_thread_members(comments, &hidden));
        // Resolve membership while this projection still has the pending
        // snapshot. Thin renderers must not reconstruct it from a deleted
        // confirmed root (or guess by shared coordinates).
        for thread in self.threads() {
            let root = index.all_threads().find_map(|t| {
                let belongs = self
                    .posts
                    .remote_id(t.root.id)
                    .is_some_and(|id| self.belongs(thread, id))
                    || self
                        .posts
                        .pending
                        .iter()
                        .find(|p| CommentKey::Post(p.token) == t.root.id)
                        .and_then(|p| p.reply_to)
                        .is_some_and(|parent| self.belongs(thread, parent));
                belongs.then_some(t.root.id)
            });
            if let Some(root) = root {
                index.thread_roots.insert(thread.id.clone(), root);
            }
        }
        index
    }
}

fn published_key(comment: &DiscussionComment, inline: bool) -> CommentKey {
    if inline {
        CommentKey::Inline(comment.id)
    } else if comment.review_state.is_some() {
        CommentKey::Review(comment.id)
    } else {
        CommentKey::Issue(comment.id)
    }
}

pub(crate) fn published_node(target: &github::CommentTarget) -> &str {
    match target {
        github::CommentTarget::Issue(id)
        | github::CommentTarget::Review(id)
        | github::CommentTarget::ReviewComment(id) => id,
    }
}

fn published<'a>(
    discussion: &'a PrDiscussion,
    target: &github::CommentTarget,
) -> Option<&'a DiscussionComment> {
    let (node, inline) = match target {
        github::CommentTarget::Issue(node) | github::CommentTarget::Review(node) => (node, false),
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
    published.retain(|c| c.time != index::CommentTime::Pending && !matches!(c.id, CommentKey::Inline(id) | CommentKey::Pending(id) if seen.contains(&id)));
    published.extend(pending.into_iter().map(|comment| {
        let position = crate::comment_anchor::display_line(comment.line, comment.start_line);
        index::ReviewComment {
            id: CommentKey::Pending(comment.id),
            path: comment.path.clone(),
            line: position.map(|(_, line)| line),
            side: comment.side,
            start_line: position.and_then(|(start, _)| start),
            body: comment.body.clone(),
            author: "you".into(),
            time: index::CommentTime::Pending,
            in_reply_to_id: comment.in_reply_to_id.map(CommentKey::Inline),
        }
    }));
    published
}
