//! The GitHub client facade: the domain-shaped interface every GitHub
//! operation in the app goes through, independent of how it's backed.
//!
//! Callers hold a [`GitHubClient`] (usually `Arc<dyn GitHubClient>`) and work
//! in domain terms — [`Viewer`], [`ClientError`] — never subprocesses, exit
//! codes, or HTTP. v0.1's backend ([`gh_cli::GhCliClient`]) shells out to the
//! `gh` CLI, JSON crossing the process boundary; a v0.2 backend doing direct
//! REST/GraphQL with a PAT implements the same trait and slots in without
//! touching a single caller.
//!
//! Error kinds are stable vocabulary for callers to match on; the `Display`
//! text is the user-facing message and moves with the backend (it carries
//! v0.1's `gh`-CLI wording today, including the `gh auth login` fix).

pub mod gh_cli;

/// The signed-in GitHub user, as far as the app needs to know her: who she
/// is, and how to draw her.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Viewer {
    /// The `@handle` the user goes by.
    pub login: String,
    /// Where her avatar image lives.
    pub avatar_url: String,
}

/// A GitHub label as the queue shows it: its name and the six-hex-digit
/// color GitHub's own UI paints it (`"1D76DB"`, no `#`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueLabel {
    pub name: String,
    pub color: String,
}

/// Where a PR's reviews stand — the triage signal. GitHub's `REVIEW_REQUIRED`
/// (policy unmet) and a null decision (no policy) both fold into
/// [`Self::Awaiting`]: the queue row's question is "has anyone already
/// approved or blocked this?", and both answers are "no".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewState {
    /// No approving or blocking review recorded — the queue's default
    /// posture.
    Awaiting,
    /// Someone approved; the next look at it can be skim-level.
    Approved,
    /// Someone requested changes — worth knowing before opening.
    ChangesRequested,
}

impl ReviewState {
    /// GitHub's `reviewDecision` vocabulary → triage state. An unexpected
    /// value degrades to `Awaiting` rather than inventing a state.
    pub fn from_decision(decision: Option<&str>) -> Self {
        match decision {
            Some("APPROVED") => Self::Approved,
            Some("CHANGES_REQUESTED") => Self::ChangesRequested,
            _ => Self::Awaiting,
        }
    }
}

/// The summarized verdict of a PR's checks — the queue's CI dot. The whole
/// check rollup reduced to one look, so the queue stays fast.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CiState {
    /// Every check green — CI won't get in the way of this review.
    Pass,
    /// Some check failed or errored — expect breakage before opening.
    Fail,
    /// Checks running, or expected but not started — worth a moment.
    Pending,
    /// No signal: the PR has no checks, or the rollup couldn't be fetched.
    /// The queue degrades to gray rather than guessing.
    NoChecks,
}

impl CiState {
    /// GitHub's aggregated check-rollup state vocabulary → verdict.
    /// `EXPECTED` — a status awaited but never reported — reads as pending;
    /// a null rollup or state means no signal at all.
    pub fn from_rollup_state(state: Option<&str>) -> Self {
        match state {
            Some("SUCCESS") => Self::Pass,
            Some("FAILURE") | Some("ERROR") => Self::Fail,
            Some("PENDING") | Some("EXPECTED") => Self::Pending,
            _ => Self::NoChecks,
        }
    }
}

/// One PR in the review queue: the summary a queue row is drawn from —
/// everything needed to triage without opening anything. The search
/// itself carries title/author/repo/draft/comments/labels/age; the
/// check-sized fields (review verdict, CI, diff size, avatar) arrive in a
/// batched enrichment pass, and a PR the enrichment missed keeps honest
/// defaults — gray CI, no sizes, initials instead of an avatar — rather
/// than fabricated values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueItem {
    pub number: u64,
    pub title: String,
    /// The author's `@handle`.
    pub author: String,
    /// The author's avatar, ready to load. `None` when the enrichment
    /// couldn't see her (a vanished account) — the row falls back to
    /// initials.
    pub author_avatar_url: Option<String>,
    /// `owner/name` of the repository the PR belongs to.
    pub repository: String,
    /// Draft PRs wait in the queue like any other, unready to merge.
    pub is_draft: bool,
    /// Where the PR's reviews stand.
    pub review_state: ReviewState,
    /// The summarized verdict of the PR's checks.
    pub ci: CiState,
    /// Lines the PR adds, when the enrichment could see the diff.
    pub additions: Option<u64>,
    /// Lines the PR removes, when the enrichment could see the diff.
    pub deletions: Option<u64>,
    /// Every conversation comment on the PR.
    pub comments: u64,
    /// The PR's labels, as GitHub paints them.
    pub labels: Vec<QueueLabel>,
    /// When the PR last saw activity, ISO 8601 (GitHub's format).
    pub updated_at: String,
}

/// Which set of PRs a queue search asks GitHub for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueuePreset {
    /// Every open PR where the viewer's review is requested — directly or
    /// via a team she belongs to; the one search qualifier covers both.
    ReviewRequested,
}

/// How a queue search orders its results.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueSort {
    /// Most recently updated first — the default, so fresh activity
    /// surfaces first.
    RecentlyUpdated,
    /// Oldest first, for clearing backlog.
    OldestUpdated,
}

/// A queue search — the reviewer-facing query the review queue runs.
/// Compiles to exact GitHub search syntax ([`Self::query`]) plus the search
/// API's sort spec ([`Self::sort`]); backends never see a preset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueueSearch {
    preset: QueuePreset,
    sort: QueueSort,
}

impl QueueSearch {
    /// The default queue: every open PR where the viewer's review is
    /// requested, most recently updated first.
    pub fn review_requested() -> Self {
        Self {
            preset: QueuePreset::ReviewRequested,
            sort: QueueSort::RecentlyUpdated,
        }
    }

    /// The same search, ordered differently.
    pub fn sorted_by(self, sort: QueueSort) -> Self {
        Self { sort, ..self }
    }

    /// The exact GitHub search string for this search. `@me` resolves to
    /// the signed-in viewer, and the `review-requested` qualifier includes
    /// both direct requests and ones via her teams.
    pub fn query(&self) -> &'static str {
        match self.preset {
            QueuePreset::ReviewRequested => "is:pr is:open review-requested:@me",
        }
    }

    /// The search API sort spec: `(field, order)`.
    pub fn sort(&self) -> (&'static str, &'static str) {
        match self.sort {
            QueueSort::RecentlyUpdated => ("updated", "desc"),
            QueueSort::OldestUpdated => ("updated", "asc"),
        }
    }
}

/// Why a client operation failed, in domain terms — never subprocess or
/// transport mechanics, which stay inside a backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientError {
    /// No credential source to draw on at all. v0.1 means the `gh` CLI is
    /// missing or can't run; a v0.2 token backend would mean no token is
    /// configured.
    NoCredentialSource,
    /// A credential source exists, but no usable sign-in: v0.1's `gh` is
    /// signed out or its token was rejected. The fix is `gh auth login`.
    Unauthenticated,
    /// The operation failed for any other reason — network, API, or a
    /// response we couldn't digest. The string is a short human cause.
    Failed(String),
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoCredentialSource => write!(
                f,
                "The GitHub CLI isn't installed. Install it from \
                 https://cli.github.com, then run `gh auth login` to sign in."
            ),
            Self::Unauthenticated => {
                write!(
                    f,
                    "You're not signed in to GitHub. Run `gh auth login` to sign in."
                )
            }
            Self::Failed(cause) => write!(
                f,
                "Couldn't reach GitHub: {cause}. Run `gh auth login` if you haven't signed in."
            ),
        }
    }
}

impl std::error::Error for ClientError {}

/// The backend-agnostic client contract. Implementations are cheap to
/// construct and synchronous — they run on background threads, never the UI
/// thread — and may spawn subprocesses or make network calls inside a call.
pub trait GitHubClient: Send + Sync {
    /// The signed-in viewer — also the app's auth check in one round trip:
    /// `Ok` says GitHub is usable as her, `Err` says why it isn't, in words
    /// that name the fix.
    fn viewer(&self) -> Result<Viewer, ClientError>;

    /// PRs matching `search`, in the search's order — the review queue's
    /// data. One call carries the whole queue: the search itself plus a
    /// batched enrichment pass (check rollups, review decisions, diff
    /// sizes, avatars) — a backend may make several round trips under this
    /// one call, but never one per PR. A PR the enrichment misses keeps
    /// honest defaults rather than fabricated values (see [`QueueItem`]).
    fn search_prs(&self, search: &QueueSearch) -> Result<Vec<QueueItem>, ClientError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The contract every failure message must meet, whatever the backend:
    /// the app shows it as the fix, so it has to name `gh auth login`.
    #[test]
    fn every_error_message_names_the_gh_auth_login_fix() {
        for err in [
            ClientError::NoCredentialSource,
            ClientError::Unauthenticated,
            ClientError::Failed("solar flares".into()),
        ] {
            let message = err.to_string();
            assert!(
                message.contains("gh auth login"),
                "{err:?} doesn't name the fix: {message}"
            );
        }
    }

    /// Golden string: the review-requested preset must compile to exactly
    /// this search — open PRs, review requested of the viewer (teams
    /// included by the qualifier) — sorted recently-updated. Every backend
    /// runs this verbatim, so the queue's contents are pinned here.
    #[test]
    fn review_requested_compiles_to_the_exact_queue_search() {
        let search = QueueSearch::review_requested();
        assert_eq!(search.query(), "is:pr is:open review-requested:@me");
        assert_eq!(search.sort(), ("updated", "desc"));
    }

    /// Re-sorting changes the sort spec, never the set of PRs.
    #[test]
    fn sorting_flips_the_order_not_the_search() {
        let search = QueueSearch::review_requested().sorted_by(QueueSort::OldestUpdated);
        assert_eq!(search.query(), "is:pr is:open review-requested:@me");
        assert_eq!(search.sort(), ("updated", "asc"));
    }

    /// GitHub's reviewDecision vocabulary, exhaustively: the two decided
    /// states map, everything else — REVIEW_REQUIRED, null, drift — folds
    /// into Awaiting rather than inventing a state.
    #[test]
    fn review_states_map_from_every_decision() {
        use ReviewState::{Approved, Awaiting, ChangesRequested};
        assert_eq!(ReviewState::from_decision(Some("APPROVED")), Approved);
        assert_eq!(
            ReviewState::from_decision(Some("CHANGES_REQUESTED")),
            ChangesRequested
        );
        assert_eq!(
            ReviewState::from_decision(Some("REVIEW_REQUIRED")),
            Awaiting
        );
        assert_eq!(ReviewState::from_decision(None), Awaiting);
        assert_eq!(
            ReviewState::from_decision(Some("NEW_GITHUB_STATE")),
            Awaiting
        );
    }

    /// GitHub's aggregated rollup vocabulary, exhaustively: pass and the
    /// fail pair map, pending covers both still-running spellings, and no
    /// signal — null rollup, null state, drift — degrades to NoChecks.
    #[test]
    fn ci_states_map_from_every_rollup_state() {
        use CiState::{Fail, NoChecks, Pass, Pending};
        assert_eq!(CiState::from_rollup_state(Some("SUCCESS")), Pass);
        assert_eq!(CiState::from_rollup_state(Some("FAILURE")), Fail);
        assert_eq!(CiState::from_rollup_state(Some("ERROR")), Fail);
        assert_eq!(CiState::from_rollup_state(Some("PENDING")), Pending);
        assert_eq!(CiState::from_rollup_state(Some("EXPECTED")), Pending);
        assert_eq!(CiState::from_rollup_state(None), NoChecks);
        assert_eq!(
            CiState::from_rollup_state(Some("NEW_GITHUB_STATE")),
            NoChecks
        );
    }
}
