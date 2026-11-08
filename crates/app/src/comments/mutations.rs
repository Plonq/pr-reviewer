//! Transient edit/delete effects and recovery. Never installed into confirmed
//! discussion/session snapshots. Successful writes are protected by SyncState.
use super::CommentKey;
use github::{MutationReceipt, MutationTarget, PendingReview, PrDiscussion, ReviewThread};
use std::collections::{HashMap, HashSet};

#[derive(Clone)]
pub(crate) struct Change {
    pub token: u64,
    pub key: CommentKey,
    pub target: MutationTarget,
    pub body: Option<String>,
    pub recovery_body: String,
    pub placement: crate::ComposerPlacement,
    pub uncertain: bool,
    pub error: Option<String>,
}

impl Change {
    pub fn acknowledged(
        &self,
        discussion: Option<&PrDiscussion>,
        pending: Option<&PendingReview>,
    ) -> bool {
        let actual = match &self.target {
            MutationTarget::Published(target) => {
                let Some(discussion) = discussion else {
                    return false;
                };
                super::published(discussion, target).map(|c| c.body.as_str())
            }
            MutationTarget::Pending {
                review_id,
                id,
                node_id,
            } => {
                // Absence in a different/no review is not a deletion acknowledgement.
                let Some(review) = pending.filter(|r| r.id == *review_id) else {
                    return false;
                };
                review
                    .comments
                    .iter()
                    .find(|c| c.id == *id && c.node_id == *node_id)
                    .map(|c| c.body.as_str())
            }
        };
        match &self.body {
            Some(body) => actual == Some(body.as_str()),
            None => actual.is_none(),
        }
    }
}

#[derive(Default)]
pub(crate) struct Changes {
    pub active: Vec<Change>,
    pub failed: Vec<Change>,
    successful: Vec<Change>,
    deleted_pending: HashSet<u64>,
    // Retain only original thread placement/anchor metadata after deleting its
    // root. These are view shells, never a source of visible confirmed bodies.
    pub threads: HashMap<String, ReviewThread>,
    pub members: HashMap<u64, String>,
    pub pending_roots: HashMap<u64, github::PendingComment>,
}

impl Changes {
    pub fn begin(
        &mut self,
        change: Change,
        discussion: Option<&PrDiscussion>,
        pending: Option<&PendingReview>,
    ) {
        if change.body.is_none() {
            if let MutationTarget::Pending { id, .. } = &change.target {
                if let Some(c) = pending
                    .into_iter()
                    .flat_map(|r| &r.comments)
                    .find(|c| c.id == *id)
                {
                    self.pending_roots.entry(*id).or_insert_with(|| c.clone());
                }
            }
            if let MutationTarget::Published(target) = &change.target {
                if let Some(thread) = discussion.into_iter().flat_map(|d| &d.threads).find(|t| {
                    t.comments
                        .iter()
                        .any(|c| super::published_node(target) == c.permissions.node_id)
                }) {
                    self.members.insert(
                        match change.key {
                            CommentKey::Inline(id) => id,
                            _ => unreachable!(),
                        },
                        thread.id.clone(),
                    );
                    self.threads.entry(thread.id.clone()).or_insert_with(|| {
                        let mut shell = thread.clone();
                        shell.comments.truncate(1);
                        shell
                    });
                }
            }
        }
        self.active.push(change);
    }

    pub fn finish(&mut self, token: u64, result: &Result<MutationReceipt, github::ClientError>) {
        let Some(ix) = self.active.iter().position(|c| c.token == token) else {
            return;
        };
        match result {
            Ok(receipt) => {
                let mut change = self.active.remove(ix);
                if let MutationReceipt::Edited { target, body } = receipt {
                    change.target = target.clone();
                    change.body = Some(body.clone());
                }
                self.successful.retain(|old| old.key != change.key);
                if let MutationTarget::Pending { id, .. } = change.target {
                    if change.body.is_none() {
                        self.deleted_pending.insert(id);
                    }
                    self.successful.push(change);
                }
            }
            Err(error) if error.mutation_is_uncertain() => {
                self.active[ix].uncertain = true;
                self.active[ix].error = Some(error.to_string());
            }
            Err(error) => {
                let mut change = self.active.remove(ix);
                change.error = Some(error.to_string());
                self.failed.push(change);
            }
        }
    }

    /// Call only for eligible post-write reads, never the pre-write snapshot
    /// retained after an error. Exact target/body or target absence is proof.
    pub fn reconcile(
        &mut self,
        discussion: Option<&PrDiscussion>,
        pending: Option<&PendingReview>,
        published: bool,
    ) {
        let proven: HashSet<_> = self
            .active
            .iter()
            .filter(|c| {
                c.uncertain
                    && matches!(c.target, MutationTarget::Published(_)) == published
                    && c.acknowledged(discussion, pending)
            })
            .map(|c| c.key)
            .collect();
        // A positively verified later unknown write also supersedes an older
        // accepted version of that same target.
        self.successful.retain(|c| !proven.contains(&c.key));
        for c in &self.active {
            if !published && proven.contains(&c.key) && c.body.is_none() {
                if let MutationTarget::Pending { id, .. } = c.target {
                    self.deleted_pending.insert(id);
                }
            }
        }
        self.active.retain(|c| !proven.contains(&c.key));
        if !published {
            self.successful.retain(|c| !c.acknowledged(None, pending));
        }
    }

    pub fn hidden(&self, key: CommentKey) -> bool {
        matches!(key, CommentKey::Pending(id) | CommentKey::Inline(id) if self.deleted_pending.contains(&id))
            || self
                .active
                .iter()
                .chain(&self.successful)
                .any(|c| Self::affects(c, key) && c.body.is_none())
    }

    pub fn body<'a>(&'a self, key: CommentKey, confirmed: &'a str) -> &'a str {
        self.active
            .iter()
            .rev()
            .chain(self.successful.iter().rev())
            .find(|c| Self::affects(c, key))
            .and_then(|c| c.body.as_deref())
            .unwrap_or(confirmed)
    }

    fn affects(c: &Change, key: CommentKey) -> bool {
        c.key == key
            || matches!((c.key, key), (CommentKey::Pending(a), CommentKey::Inline(b)) if a == b)
    }

    /// Reapply only exact successful receipts to a stale session read, never
    /// uncertain/speculative payloads. Retain unrelated fresh session fields.
    pub fn protect_session(
        &mut self,
        state: &mut github::review_session::ReviewSession,
        previous: &github::review_session::ReviewSession,
    ) {
        if let Some(review) = &state.pending {
            self.successful.retain(|c| matches!(c.target, MutationTarget::Pending { review_id, .. } if review_id == review.id));
        }
        for change in &self.successful {
            let MutationTarget::Pending {
                review_id,
                id,
                node_id,
            } = &change.target
            else {
                continue;
            };
            if state.pending.is_none() {
                state.pending = previous.pending.clone();
            }
            let Some(review) = state.pending.as_mut().filter(|r| r.id == *review_id) else {
                continue;
            };
            if let Some(body) = &change.body {
                if let Some(c) = review
                    .comments
                    .iter_mut()
                    .find(|c| c.id == *id && c.node_id == *node_id)
                {
                    c.body = body.clone();
                } else if let Some(c) = previous
                    .pending
                    .as_ref()
                    .filter(|r| r.id == *review_id)
                    .and_then(|r| {
                        r.comments
                            .iter()
                            .find(|c| c.id == *id && c.node_id == *node_id)
                    })
                {
                    let mut c = c.clone();
                    c.body = body.clone();
                    review.comments.push(c);
                }
            } else {
                review
                    .comments
                    .retain(|c| c.id != *id || c.node_id != *node_id);
            }
        }
        if let Some(review) = &mut state.pending {
            review
                .comments
                .retain(|c| !self.deleted_pending.contains(&c.id));
        }
    }

    pub fn session_protection(&self) -> Self {
        // Only successful effects can repair a cache. Unknown pending targets
        // are verification expectations only: protect_session never applies
        // their bodies/tombstones. No in-flight overlay or view shell is copied.
        Self {
            active: self
                .active
                .iter()
                .filter(|c| c.uncertain && matches!(c.target, MutationTarget::Pending { .. }))
                .cloned()
                .collect(),
            successful: self.successful.clone(),
            deleted_pending: self.deleted_pending.clone(),
            ..Self::default()
        }
    }

    pub fn supersede_pending(&mut self, receipt: &MutationReceipt) {
        if let MutationReceipt::Edited { target, .. } | MutationReceipt::Deleted { target } =
            receipt
        {
            self.successful.retain(|c| &c.target != target);
        }
    }

    pub fn review_ended(&mut self) {
        self.successful.clear();
    }

    pub fn has_uncertain(&self) -> bool {
        self.active.iter().any(|c| c.uncertain)
    }
}
