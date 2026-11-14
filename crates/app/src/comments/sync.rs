//! Ordering and positive-acknowledgement barriers for confirmed reads. This is
//! not an optimistic operation queue: only successful writes are retained here.
use github::{CommentTarget, MutationReceipt, MutationTarget, PrDiscussion};
#[cfg(test)]
mod tests;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ReadTicket {
    request: u64,
    revision: u64,
    during_write: bool,
}

#[derive(Default)]
pub(crate) struct SyncState {
    request: u64,
    revision: u64,
    writing: bool,
    awaiting: Vec<MutationReceipt>,
}

impl SyncState {
    pub fn is_writing(&self) -> bool {
        self.writing
    }

    /// Shared ordering for comments-only reads and full refreshes.
    pub fn begin_read(&mut self) -> ReadTicket {
        self.request += 1;
        ReadTicket {
            request: self.request,
            revision: self.revision,
            during_write: self.writing,
        }
    }

    pub fn begin_write(&mut self) {
        debug_assert!(
            !self.writing,
            "per-PR single flight is enforced by SessionUi"
        );
        self.writing = true;
        self.revision += 1;
    }

    pub fn finish_write(&mut self, receipt: Option<&MutationReceipt>) {
        self.writing = false;
        // Also invalidate reads started during a rejected/partially successful
        // write. Such a read cannot prove the resulting session or discussion.
        self.revision += 1;
        let Some(receipt) = receipt.filter(|r| published_effect(r)) else {
            return;
        };
        self.awaiting.retain(|old| !supersedes(receipt, old));
        self.awaiting.push(receipt.clone());
    }

    pub fn eligible(&self, ticket: ReadTicket) -> bool {
        ticket.request == self.request
            && ticket.revision == self.revision
            && !ticket.during_write
            && !self.writing
    }

    /// Reject the entire comment snapshot rather than caching a stale body or
    /// deletion resurrection. Metadata/diff from a full fetch can still install.
    /// A failed read never calls this, so it cannot acknowledge a write.
    pub fn accept(&mut self, ticket: ReadTicket, discussion: &PrDiscussion) -> bool {
        self.accept_with_changes(ticket, discussion, None)
    }

    pub fn accept_projected(
        &mut self,
        ticket: ReadTicket,
        discussion: &PrDiscussion,
        changes: &super::mutations::Changes,
    ) -> bool {
        self.accept_with_changes(ticket, discussion, Some(changes))
    }

    fn accept_with_changes(
        &mut self,
        ticket: ReadTicket,
        discussion: &PrDiscussion,
        changes: Option<&super::mutations::Changes>,
    ) -> bool {
        if !self.eligible(ticket) {
            return false;
        }
        if !self.awaiting.iter().all(|old| {
            if acknowledged(old, discussion) {
                return true;
            }
            let Some(changes) = changes else {
                return false;
            };
            // A newer unknown edit/delete can positively supersede a protected
            // older version. Merely failing that write is never enough.
            let superseded = changes
                .active
                .iter()
                .filter(|c| {
                    c.uncertain
                        && matches!(c.target, MutationTarget::Published(_))
                        && c.acknowledged(Some(discussion), None)
                })
                .any(|c| {
                    let proof = match &c.body {
                        Some(body) => MutationReceipt::Edited {
                            target: c.target.clone(),
                            body: body.clone(),
                        },
                        None => MutationReceipt::Deleted {
                            target: c.target.clone(),
                        },
                    };
                    supersedes(&proof, old)
                });
            if superseded {
                return true;
            }
            // A created reply remains in the original thread after its parent
            // is deleted. Require that exact retained thread identity as well
            // as the receipt's exact comment/review identities, not a body match.
            let MutationReceipt::PublishedCreated(created) = old else {
                return false;
            };
            let Some(parent) = created.in_reply_to_id else {
                return false;
            };
            let Some(thread) = changes.members.get(&parent) else {
                return false;
            };
            if created.thread_id.as_ref().is_some_and(|id| id != thread)
                || discussion
                    .threads
                    .iter()
                    .flat_map(|t| &t.comments)
                    .any(|c| c.id == parent)
            {
                return false;
            }
            discussion
                .threads
                .iter()
                .any(|t| &t.id == thread && t.comments.iter().any(|c| created_identity(created, c)))
        }) {
            return false;
        }
        self.awaiting.clear();
        true
    }
}

fn published_effect(receipt: &MutationReceipt) -> bool {
    matches!(
        receipt,
        MutationReceipt::PublishedCreated(_)
            | MutationReceipt::Edited {
                target: MutationTarget::Published(_),
                ..
            }
            | MutationReceipt::Deleted {
                target: MutationTarget::Published(_),
                ..
            }
    )
}

fn target(receipt: &MutationReceipt) -> Option<CommentTarget> {
    match receipt {
        MutationReceipt::PublishedCreated(c) => {
            Some(if c.anchor.is_some() || c.in_reply_to_id.is_some() {
                CommentTarget::ReviewComment(c.node_id.clone())
            } else {
                CommentTarget::Issue(c.node_id.clone())
            })
        }
        MutationReceipt::Edited {
            target: MutationTarget::Published(target),
            ..
        }
        | MutationReceipt::Deleted {
            target: MutationTarget::Published(target),
        } => Some(target.clone()),
        _ => None,
    }
}

fn supersedes(new: &MutationReceipt, old: &MutationReceipt) -> bool {
    // A later accepted edit/delete replaces the earlier accepted version. Two
    // successful creates (even with identical text) remain independent.
    !matches!(new, MutationReceipt::PublishedCreated(_)) && target(new) == target(old)
}

fn created_identity(
    created: &github::PublishedComment,
    comment: &github::DiscussionComment,
) -> bool {
    comment.id == created.id
        && comment.permissions.node_id == created.node_id
        && created
            .review_node_id
            .as_ref()
            .is_none_or(|id| comment.review_id.as_ref() == Some(id))
}

fn acknowledged(receipt: &MutationReceipt, discussion: &PrDiscussion) -> bool {
    match receipt {
        MutationReceipt::PublishedCreated(created) => {
            let identity = |c: &github::DiscussionComment| created_identity(created, c);
            if created.anchor.is_some() || created.in_reply_to_id.is_some() {
                discussion.threads.iter().any(|thread| {
                    created.thread_id.as_ref().is_none_or(|id| &thread.id == id)
                        && created
                            .in_reply_to_id
                            .is_none_or(|parent| thread.comments.iter().any(|c| c.id == parent))
                        && thread.comments.iter().any(identity)
                })
            } else {
                discussion
                    .comments
                    .iter()
                    .any(|c| c.review_state.is_none() && identity(c))
            }
        }
        MutationReceipt::Edited {
            target: MutationTarget::Published(target),
            body,
        } => super::published(discussion, target).is_some_and(|c| &c.body == body),
        MutationReceipt::Deleted {
            target: MutationTarget::Published(target),
        } => super::published(discussion, target).is_none(),
        _ => true, // Pending data is confirmed by the resulting ReviewSession.
    }
}
