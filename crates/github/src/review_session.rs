//! Review lifecycle independent of UI. Cached drafts are display-only until
//! reconciled with GitHub; every mutation re-reads the viewer's pending review.

use crate::{ClientError, GitHubClient, PrLocator, ReviewVerdict};
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
    Edit {
        id: u64,
        body: String,
    },
    Delete {
        id: u64,
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
        // Immediate comments never start, resume, modify, or submit a draft.
        if let Command::Immediate { anchor, body } = command {
            anchor.validate()?;
            nonempty(&body)?;
            return client.post_anchored_comment(loc, &anchor, body.trim());
        }
        match &command {
            Command::Add { anchor, body } => {
                anchor.validate()?;
                nonempty(body)?;
            }
            Command::Edit { body, .. } => nonempty(body)?,
            _ => {}
        }
        self.verified = false;
        self.pending = client.pending_review(loc)?;
        self.verified = true;
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
                let comment = client.add_pending_comment(loc, review, &anchor, body.trim())?;
                review.comments.push(comment);
            }
            Command::Edit { id, body } => {
                let comment = self.comment_mut(id)?;
                client.edit_pending_comment(loc, comment, body.trim())?;
                comment.body = body.trim().into();
            }
            Command::Delete { id } => {
                let comment = self.comment_mut(id)?;
                client.delete_pending_comment(loc, comment)?;
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
            Command::Immediate { .. } => unreachable!(),
        }
        Ok(())
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
