//! PR commits in GitHub's connection order, with authoritative CI rollups.

use crate::CiState;

/// A successful fetch, including a genuinely empty commit list. Absence of this
/// payload means unavailable (or an older offline snapshot), never zero commits.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PrCommits {
    pub commits: Vec<PrCommit>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PrCommit {
    pub sha: String,
    /// Full commit message, not just its subject.
    pub message: String,
    /// GitHub login when associated, otherwise the git author's name.
    pub author: String,
    /// Includes checks AND legacy status contexts, as aggregated by GitHub.
    pub ci: CiState,
    pub actions: Vec<ActionsRun>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ActionsRun {
    pub name: String,
    /// GitHub's WorkflowRun.url, never constructed from a SHA or check-suite id.
    pub url: String,
}
