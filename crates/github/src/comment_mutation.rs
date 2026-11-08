//! Exact mutation outcomes, independent of a subsequent (possibly stale) read.
use crate::{CommentAnchor, CommentTarget, PendingComment};

/// Fields actually returned by creation. REST has no thread node ID; GraphQL
/// can supply one. Missing metadata is not an invented permission or timestamp.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PublishedComment {
    pub id: u64,
    pub node_id: String,
    pub body: Option<String>,
    pub author: Option<String>,
    pub created_at: Option<String>,
    pub anchor: Option<CommentAnchor>,
    pub in_reply_to_id: Option<u64>,
    pub thread_id: Option<String>,
    pub review_node_id: Option<String>,
    pub review_id: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingCommentReceipt {
    pub comment: PendingComment,
    pub thread_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MutationTarget {
    Published(CommentTarget),
    Pending {
        review_id: u64,
        id: u64,
        node_id: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MutationReceipt {
    NoCommentChange,
    PublishedCreated(PublishedComment),
    PendingCreated {
        review_id: u64,
        review_node_id: String,
        created: PendingCommentReceipt,
    },
    Edited {
        target: MutationTarget,
        body: String,
    },
    Deleted {
        target: MutationTarget,
    },
}

impl MutationReceipt {
    pub fn published_id(&self) -> Option<u64> {
        match self {
            Self::PublishedCreated(comment) => Some(comment.id),
            _ => None,
        }
    }
}
