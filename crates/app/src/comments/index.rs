//! Pure comment grouping and anchored indexing. Local reviews use the same grouping rules.

use std::collections::HashMap;

/// Which diff side a review comment anchors to, GitHub's LEFT/RIGHT.
pub(crate) use github::DiffSide as CommentSide;

/// The comment's display age and legacy lexical ordering key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CommentTime {
    Posted(String),
    Draft,
    Pending,
    Posting(super::posting::Status, bool),
}

impl CommentTime {
    pub(crate) fn sort_key(&self) -> &str {
        match self {
            Self::Posted(raw) => raw,
            Self::Draft | Self::Pending => "draft",
            Self::Posting(_, _) => "posting",
        }
    }

    pub(crate) fn short_age(&self, now: i64) -> String {
        match self {
            Self::Posted(raw) => crate::threads::short_age(raw, now),
            Self::Draft | Self::Pending => "draft".to_string(),
            Self::Posting(status, _) => status.label().into(),
        }
    }
}

/// A card used by the app. Its identity is a stable UI key, not an API target;
/// rendering adapters resolve confirmed remote identities through the projection.
#[derive(Clone, Debug)]
pub(crate) struct ReviewComment {
    pub(crate) id: super::CommentKey,
    pub(crate) path: String,
    pub(crate) line: Option<u64>,
    pub(crate) side: CommentSide,
    pub(crate) start_line: Option<u64>,
    pub(crate) body: String,
    pub(crate) author: String,
    pub(crate) time: CommentTime,
    pub(crate) in_reply_to_id: Option<super::CommentKey>,
}

impl From<github::ReviewComment> for ReviewComment {
    fn from(comment: github::ReviewComment) -> Self {
        let position = crate::comment_anchor::display_line(comment.line, comment.start_line);
        Self {
            id: super::CommentKey::Inline(comment.id),
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
            in_reply_to_id: comment.in_reply_to_id.map(super::CommentKey::Inline),
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

    /// Compatibility adapter for the existing index parity tests. Runtime
    /// consumers project from confirmed sources rather than appending layers.
    #[cfg(test)]
    pub(crate) fn replace_pending(&mut self, pending: Option<&github::PendingReview>) {
        let old = std::mem::take(self);
        let comments: Vec<_> = old
            .threads
            .into_values()
            .flat_map(|anchors| anchors.into_values().flatten())
            .chain(old.unanchored)
            .flat_map(|thread| std::iter::once(thread.root).chain(thread.replies))
            .collect();
        *self = group_comments(super::merge_pending(
            comments,
            pending.into_iter().flat_map(|review| &review.comments),
        ));
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
    let mut thread_of: HashMap<super::CommentKey, usize> = HashMap::new();
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
