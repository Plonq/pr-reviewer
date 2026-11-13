//! Review lifecycle independent of UI. Cached drafts are display-only until
//! reconciled with GitHub; every mutation re-reads the viewer's pending review.

use crate::{ClientError, GitHubClient, MutationReceipt, MutationTarget, PrLocator, ReviewVerdict};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum DiffSide {
    Left,
    #[default]
    Right,
}

impl DiffSide {
    pub fn api_str(self) -> &'static str {
        match self {
            Self::Left => "LEFT",
            Self::Right => "RIGHT",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommentAnchor {
    pub path: String,
    pub commit_id: String,
    pub side: DiffSide,
    pub line: u64,
    pub start_line: Option<u64>,
    /// Explicit blob positioning for comments outside a single patch hunk.
    /// `commit_id` above remains the PR head; this source may be the old side.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position: Option<CommentPosition>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommentPosition {
    pub path: String,
    pub commit_id: String,
    pub base_commit_id: Option<String>,
}

impl CommentAnchor {
    pub fn validate(&self) -> Result<(), ClientError> {
        if self.path.is_empty()
            || self.commit_id.is_empty()
            || self.line == 0
            || self.line > i32::MAX as u64
            || self
                .start_line
                .is_some_and(|start| start == 0 || start >= self.line)
            || self.position.as_ref().is_some_and(|position| {
                position.path.is_empty()
                    || position.commit_id.is_empty()
                    || position
                        .base_commit_id
                        .as_ref()
                        .is_some_and(String::is_empty)
            })
        {
            return Err(ClientError::Invalid("invalid comment anchor".into()));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingComment {
    #[serde(deserialize_with = "deserialize_id")]
    pub id: u64,
    /// Opaque GraphQL identity, required for mutations on unpublished drafts.
    /// Older local mirrors did not store it; reconciliation fills it in.
    #[serde(default)]
    pub node_id: String,
    pub path: String,
    #[serde(default, deserialize_with = "deserialize_side")]
    pub side: DiffSide,
    pub line: Option<u64>,
    pub start_line: Option<u64>,
    pub body: String,
    /// Published (or draft) parent, retained so pending replies stay in-thread.
    #[serde(default)]
    pub in_reply_to_id: Option<u64>,
}

// GraphQL BigInt identifiers are strings; REST identifiers are JSON integers.
fn deserialize_id<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<u64, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Id {
        Number(u64),
        Text(String),
    }
    match Id::deserialize(deserializer)? {
        Id::Number(id) => Ok(id),
        Id::Text(id) => id.parse().map_err(serde::de::Error::custom),
    }
}

fn deserialize_side<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<DiffSide, D::Error> {
    Ok(Option::<DiffSide>::deserialize(deserializer)?.unwrap_or_default())
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingReview {
    pub id: u64,
    pub node_id: String,
    pub body: String,
    pub comments: Vec<PendingComment>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ReviewSession {
    pub pending: Option<PendingReview>,
    #[serde(skip)]
    pub verified: bool,
}

#[derive(Clone, Debug)]
pub enum Command {
    Resume,
    Start {
        commit_id: String,
    },
    Add {
        anchor: CommentAnchor,
        body: String,
    },
    Reply {
        comment_id: u64,
        body: String,
    },
    /// Reply immediately unless reconciliation finds an existing review.
    /// An explicit start-review choice also creates a draft when necessary.
    ConversationReply {
        comment_id: u64,
        commit_id: String,
        body: String,
        pending: bool,
    },
    Conversation {
        body: String,
    },
    Edit {
        id: u64,
        body: String,
    },
    Delete {
        id: u64,
    },
    EditPublished {
        target: crate::CommentTarget,
        body: String,
    },
    DeletePublished {
        target: crate::CommentTarget,
    },
    SetReaction {
        target: crate::CommentTarget,
        content: String,
        reacted: bool,
    },
    SetThreadResolved {
        thread_id: String,
        resolved: bool,
    },
    Submit {
        verdict: ReviewVerdict,
        body: String,
    },
    Immediate {
        anchor: CommentAnchor,
        body: String,
    },
}

pub fn validate_submission(
    verdict: ReviewVerdict,
    body: &str,
    comments: usize,
) -> Result<(), ClientError> {
    if verdict != ReviewVerdict::Approve && body.trim().is_empty() && comments == 0 {
        return Err(ClientError::Invalid(
            "this review type needs a comment".into(),
        ));
    }
    Ok(())
}

impl ReviewSession {
    pub fn count(&self) -> usize {
        self.pending
            .as_ref()
            .map_or(0, |review| review.comments.len())
    }

    pub fn execute(
        &mut self,
        client: &dyn GitHubClient,
        loc: &PrLocator,
        command: Command,
    ) -> Result<(), ClientError> {
        self.execute_with_receipt(client, loc, command).map(|_| ())
    }

    /// Exact outcome plus the resulting confirmed session (including partial
    /// lifecycle success on failure). Creation never discards its server identity.
    pub fn execute_with_receipt(
        &mut self,
        client: &dyn GitHubClient,
        loc: &PrLocator,
        command: Command,
    ) -> Result<MutationReceipt, ClientError> {
        // Immediate comments never start, resume, modify, or submit a draft.
        if let Command::Immediate { anchor, body } = command {
            anchor.validate()?;
            nonempty(&body)?;
            return client
                .post_anchored_comment(loc, &anchor, body.trim())
                .map(MutationReceipt::PublishedCreated);
        }
        if let Command::Conversation { body } = command {
            nonempty(&body)?;
            return client
                .post_conversation_comment(loc, body.trim())
                .map(MutationReceipt::PublishedCreated);
        }
        // Published mutations must never resume, create, or submit a draft review.
        if let Command::EditPublished { target, body } = command {
            nonempty(&body)?;
            client.edit_comment(&target, body.trim())?;
            return Ok(MutationReceipt::Edited {
                target: MutationTarget::Published(target),
                body: body.trim().into(),
            });
        }
        if let Command::DeletePublished { target } = command {
            client.delete_comment(&target)?;
            return Ok(MutationReceipt::Deleted {
                target: MutationTarget::Published(target),
            });
        }
        if let Command::SetReaction {
            target,
            content,
            reacted,
        } = command
        {
            return client
                .set_comment_reaction(&target, &content, reacted)
                .map(|_| MutationReceipt::NoCommentChange);
        }
        if let Command::SetThreadResolved {
            thread_id,
            resolved,
        } = command
        {
            return client
                .set_thread_resolved(&thread_id, resolved)
                .map(|_| MutationReceipt::NoCommentChange);
        }
        match &command {
            Command::Add { anchor, body } => {
                anchor.validate()?;
                nonempty(body)?;
            }
            Command::Edit { body, .. }
            | Command::Reply { body, .. }
            | Command::ConversationReply { body, .. } => nonempty(body)?,
            _ => {}
        }
        self.verified = false;
        self.pending = client.pending_review(loc)?;
        self.verified = true;
        let mut receipt = MutationReceipt::NoCommentChange;
        match command {
            Command::Resume => {}
            Command::Start { commit_id } => {
                if commit_id.is_empty() {
                    return Err(ClientError::Invalid("missing head commit".into()));
                }
                if self.pending.is_none() {
                    self.pending = Some(client.start_review(loc, &commit_id)?);
                }
            }
            Command::Add { anchor, body } => {
                if self.pending.is_none() {
                    self.pending = Some(client.start_review(loc, &anchor.commit_id)?);
                }
                let review = self.pending.as_mut().unwrap();
                let created = client.add_pending_comment(loc, review, &anchor, body.trim())?;
                review.comments.push(created.comment.clone());
                receipt = MutationReceipt::PendingCreated {
                    review_id: review.id,
                    review_node_id: review.node_id.clone(),
                    created,
                };
            }
            Command::Edit { id, body } => {
                let review_id = self.pending.as_ref().map(|r| r.id);
                let comment = self.comment_mut(id)?;
                client.edit_pending_comment(loc, comment, body.trim())?;
                comment.body = body.trim().into();
                receipt = MutationReceipt::Edited {
                    target: MutationTarget::Pending {
                        review_id: review_id.unwrap(),
                        id,
                        node_id: comment.node_id.clone(),
                    },
                    body: comment.body.clone(),
                };
            }
            Command::Reply { comment_id, body } => {
                if let Some(review) = &mut self.pending {
                    let created = client.add_pending_reply(loc, review, comment_id, body.trim())?;
                    review.comments.push(created.comment.clone());
                    receipt = MutationReceipt::PendingCreated {
                        review_id: review.id,
                        review_node_id: review.node_id.clone(),
                        created,
                    };
                } else {
                    receipt = MutationReceipt::PublishedCreated(client.post_reply(
                        loc,
                        comment_id,
                        body.trim(),
                    )?);
                }
            }
            Command::ConversationReply {
                comment_id,
                commit_id,
                body,
                pending,
            } => {
                if pending && self.pending.is_none() {
                    if commit_id.is_empty() {
                        return Err(ClientError::Invalid("missing head commit".into()));
                    }
                    self.pending = Some(client.start_review(loc, &commit_id)?);
                }
                if let Some(review) = &mut self.pending {
                    let created = client.add_pending_reply(loc, review, comment_id, body.trim())?;
                    review.comments.push(created.comment.clone());
                    receipt = MutationReceipt::PendingCreated {
                        review_id: review.id,
                        review_node_id: review.node_id.clone(),
                        created,
                    };
                } else {
                    receipt = MutationReceipt::PublishedCreated(client.post_reply(
                        loc,
                        comment_id,
                        body.trim(),
                    )?);
                }
            }
            Command::Delete { id } => {
                let review_id = self.pending.as_ref().map(|r| r.id);
                let comment = self.comment_mut(id)?;
                client.delete_pending_comment(loc, comment)?;
                receipt = MutationReceipt::Deleted {
                    target: MutationTarget::Pending {
                        review_id: review_id.unwrap(),
                        id,
                        node_id: comment.node_id.clone(),
                    },
                };
                self.pending
                    .as_mut()
                    .unwrap()
                    .comments
                    .retain(|comment| comment.id != id);
            }
            Command::Submit { verdict, body } => {
                validate_submission(verdict, &body, self.count())?;
                match &self.pending {
                    Some(review) => {
                        client.submit_pending_review(loc, review, verdict, body.trim())?
                    }
                    None => client.submit_review(loc, verdict, body.trim())?,
                }
                self.pending = None;
            }
            Command::Immediate { .. }
            | Command::Conversation { .. }
            | Command::EditPublished { .. }
            | Command::DeletePublished { .. }
            | Command::SetReaction { .. }
            | Command::SetThreadResolved { .. } => unreachable!(),
        }
        Ok(receipt)
    }

    fn comment_mut(&mut self, id: u64) -> Result<&mut PendingComment, ClientError> {
        self.pending
            .as_mut()
            .and_then(|review| review.comments.iter_mut().find(|comment| comment.id == id))
            .ok_or_else(|| {
                ClientError::Invalid(
                    "this comment is no longer in your pending review; refresh and try again"
                        .into(),
                )
            })
    }
}

fn nonempty(body: &str) -> Result<(), ClientError> {
    if body.trim().is_empty() {
        Err(ClientError::Invalid("a comment cannot be empty".into()))
    } else {
        Ok(())
    }
}
