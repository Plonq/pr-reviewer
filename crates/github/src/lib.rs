//! The GitHub client facade: the domain-shaped interface every GitHub
//! operation in the app goes through, independent of how it's backed.
//!
//! Callers hold a [`GitHubClient`] (usually `Arc<dyn GitHubClient>`) and
//! work in domain terms — [`Viewer`], [`PrLocator`], [`PrMeta`],
//! [`ClientError`] — never subprocesses, exit codes, or HTTP. Every GitHub
//! operation the app runs goes through this one trait: the queue's viewer
//! and search, and the review's resolve/fetch/comment/post. v0.1's backend
//! ([`gh_cli::GhCliClient`]) shells out to the `gh` CLI, JSON crossing the
//! process boundary; a v0.2 backend doing direct REST/GraphQL with a PAT
//! implements the same trait and slots in without touching a single caller.
//!
//! Error kinds are stable vocabulary for callers to match on; the `Display`
//! text is the user-facing message and moves with the backend (it carries
//! v0.1's `gh`-CLI wording today, including the `gh auth login` fix).
pub mod commits;
pub use commits::{ActionsRun, PrCommit, PrCommits};
pub mod discussion;
pub mod gh_cli;
pub use discussion::{
    CommentPermissions, CommentTarget, DiscussionComment, PrDiscussion, Reaction,
    ReviewCodeContext, ReviewThread,
};
pub mod review_session;
pub use review_session::{CommentAnchor, CommentPosition, DiffSide, PendingComment, PendingReview};

/// Per-viewer file review state. DISMISSED means a previously viewed file
/// changed again; it is not currently viewed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum FileViewedState {
    Viewed,
    Unviewed,
    Dismissed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewedFiles {
    /// Opaque GraphQL identity used by mark/unmark mutations.
    pub pull_request_id: String,
    pub files: std::collections::HashMap<String, FileViewedState>,
}

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
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct QueueLabel {
    pub name: String,
    pub color: String,
}

/// Where a PR's reviews stand — the triage signal. GitHub's `REVIEW_REQUIRED`
/// (policy unmet) and a null decision (no policy) both fold into
/// [`Self::Awaiting`]: the queue row's question is "has anyone already
/// approved or blocked this?", and both answers are "no".
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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
    /// When the PR was opened, ISO 8601 (GitHub's format).
    /// Empty when reading an older serialized queue without this field.
    #[serde(default)]
    pub created_at: String,
    /// When the viewer's outstanding direct review request was made.
    /// Unknown for team-only requests or unavailable timeline data.
    #[serde(default)]
    pub review_requested_at: Option<String>,
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

/// Which dimension of the queue a filter chip narrows: the organization a
/// PR belongs to, the repository it lives in, or who wrote it. The order
/// fixes the order chips compile in — org, then repo, then author — so the
/// compiled search string depends on what's active, never on how it got
/// there.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum QueueFilterKind {
    Org,
    Repo,
    Author,
}

impl QueueFilterKind {
    /// The search qualifier's name — also the chip's prefix in the UI, so
    /// the queue shows exactly the search it compiles to.
    pub fn prefix(self) -> &'static str {
        match self {
            Self::Org => "org",
            Self::Repo => "repo",
            Self::Author => "author",
        }
    }
}

/// One quick filter over the queue: a chip in the UI, a search qualifier
/// server-side. Values compile verbatim — GitHub's orgs, repositories, and
/// logins can't contain spaces or colons, so no quoting is needed and none
/// is invented; the queue derives chips from well-formed queue rows, so
/// verbatim is honest.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum QueueFilter {
    /// Every PR under one organization: `org:<owner>`.
    Org(String),
    /// Every PR in one repository: `repo:<owner>/<name>`.
    Repo(String),
    /// Every PR by one author: `author:<login>`.
    Author(String),
}

impl QueueFilter {
    /// An organization filter: PRs under `owner`'s org.
    pub fn org(owner: impl Into<String>) -> Self {
        Self::Org(owner.into())
    }

    /// A repository filter: PRs in `owner/name`.
    pub fn repo(name_with_owner: impl Into<String>) -> Self {
        Self::Repo(name_with_owner.into())
    }

    /// An author filter: PRs written by `login`.
    pub fn author(login: impl Into<String>) -> Self {
        Self::Author(login.into())
    }

    /// Which dimension this narrows.
    pub fn kind(&self) -> QueueFilterKind {
        match self {
            Self::Org(_) => QueueFilterKind::Org,
            Self::Repo(_) => QueueFilterKind::Repo,
            Self::Author(_) => QueueFilterKind::Author,
        }
    }

    /// The value this filters to, without its kind's prefix: `acme` for
    /// `org:acme`.
    pub fn value(&self) -> &str {
        match self {
            Self::Org(value) | Self::Repo(value) | Self::Author(value) => value,
        }
    }

    /// The exact search qualifier this compiles to, one word: `org:acme`,
    /// `repo:acme/widgets`, `author:octocat`.
    pub fn qualifier(&self) -> String {
        format!("{}:{}", self.kind().prefix(), self.value())
    }
}

/// A queue search — the reviewer-facing query the review queue runs.
/// Compiles to exact GitHub search syntax ([`Self::query`]) plus the search
/// API's sort spec ([`Self::sort`]); backends never see a preset or a
/// chip. Filters are one chip per dimension at most — a second chip of a
/// kind replaces the first, the way the queue's chip bar works — kept in
/// kind order, so the compiled string is a function of the search, never
/// of the clicks that built it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueSearch {
    preset: QueuePreset,
    sort: QueueSort,
    filters: Vec<QueueFilter>,
}

impl QueueSearch {
    /// The default queue: every open PR where the viewer's review is
    /// requested, most recently updated first, nothing filtered.
    pub fn review_requested() -> Self {
        Self {
            preset: QueuePreset::ReviewRequested,
            sort: QueueSort::RecentlyUpdated,
            filters: Vec::new(),
        }
    }

    /// A search rebuilt from the parts it is made of: the filters that
    /// were active and the order they ran in.
    ///
    /// This is how a search survives a restart: the app remembers the
    /// chips and the sort, not the compiled string, so a rebuilt search is
    /// the same search — the preset is the only one there is, and the
    /// filters land in kind order however they were restored.
    pub fn from_parts(filters: Vec<QueueFilter>, sort: QueueSort) -> Self {
        let mut search = Self {
            preset: QueuePreset::ReviewRequested,
            sort,
            filters: Vec::new(),
        };
        for filter in filters {
            search = search.filtered_by(filter);
        }
        search
    }

    /// The same search, ordered differently.
    pub fn sorted_by(self, sort: QueueSort) -> Self {
        Self { sort, ..self }
    }

    /// The same search narrowed by `filter`, replacing whatever that
    /// dimension was already filtered to.
    pub fn filtered_by(mut self, filter: QueueFilter) -> Self {
        self.filters.retain(|active| active.kind() != filter.kind());
        // Kind order holds however the chips were applied: the new filter
        // slots in ahead of the first kind that outranks it.
        let slot = self
            .filters
            .partition_point(|active| active.kind() < filter.kind());
        self.filters.insert(slot, filter);
        self
    }

    /// The same search with its `kind` chip lifted — selecting the active
    /// chip again clears the dimension.
    pub fn without_filter(mut self, kind: QueueFilterKind) -> Self {
        self.filters.retain(|active| active.kind() != kind);
        self
    }

    /// The same search with every chip lifted — the preset queue again,
    /// whatever its order.
    pub fn unfiltered(mut self) -> Self {
        self.filters.clear();
        self
    }

    /// The chips currently narrowing the search, in compile order: org,
    /// then repo, then author.
    pub fn filters(&self) -> &[QueueFilter] {
        &self.filters
    }

    /// How the search currently orders its results — the sort toggle's
    /// state.
    pub fn ordering(&self) -> QueueSort {
        self.sort
    }

    /// The exact GitHub search string for this search: the preset's
    /// qualifiers, then one qualifier per active chip. `@me` resolves to
    /// the signed-in viewer, and the `review-requested` qualifier includes
    /// both direct requests and ones via her teams.
    pub fn query(&self) -> String {
        let mut query = match self.preset {
            QueuePreset::ReviewRequested => "is:pr is:open review-requested:@me".to_string(),
        };
        for filter in &self.filters {
            query.push(' ');
            query.push_str(&filter.qualifier());
        }
        query
    }

    /// The search API sort spec: `(field, order)`.
    pub fn sort(&self) -> (&'static str, &'static str) {
        match self.sort {
            QueueSort::RecentlyUpdated => ("updated", "desc"),
            QueueSort::OldestUpdated => ("updated", "asc"),
        }
    }
}

// --- PR review -------------------------------------------------------------

/// A pull request's coordinates: which repository it lives in and which
/// number it is — everything the review operations need to address one PR.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrLocator {
    pub owner: String,
    pub repo: String,
    pub number: u64,
}

impl PrLocator {
    /// `owner/repo` — how the CLI and API address the repository.
    pub fn repo_slug(&self) -> String {
        format!("{}/{}", self.owner, self.repo)
    }
}

/// The verdict of a top-level PR review.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewVerdict {
    Approve,
    RequestChanges,
    Comment,
}

/// A GitHub account as PR metadata names one: just the handle the UI shows.
#[derive(Debug, Clone, serde::Deserialize, PartialEq)]
pub struct Author {
    pub login: String,
}

/// One PR's metadata: the titlebar's content, the review's anchor oids, and
/// the stats the header shows — fetched whole by one call
/// ([`GitHubClient::pr_meta`]).
#[derive(Debug, Clone, serde::Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PrMeta {
    pub number: u64,
    pub title: String,
    pub author: Author,
    /// "OPEN", "MERGED", or "CLOSED".
    pub state: String,
    pub url: String,
    /// PR description (markdown); empty when the PR has none.
    #[serde(default)]
    pub body: String,
    pub base_ref_name: String,
    pub head_ref_name: String,
    /// The commit the PR's base side sits at — where whole-file upgrades
    /// fetch old sides from. Older `gh` builds may not report it.
    #[serde(default)]
    pub base_ref_oid: String,
    /// The commit the PR's head side sits at — also the `commit_id` new
    /// review comments anchor against.
    #[serde(default)]
    pub head_ref_oid: String,
    pub additions: u64,
    pub deletions: u64,
    pub changed_files: u64,
    /// "APPROVED", "CHANGES_REQUESTED", "REVIEW_REQUIRED", or "" (no
    /// required reviews and none given).
    #[serde(default)]
    pub review_decision: String,
}

/// One PR review comment, flat — the app groups them into threads by
/// `in_reply_to_id` and anchors them by (path, side, line). Field names
/// follow the pulls comments API's snake_case; every backend must hand
/// back this shape.
#[derive(Debug, Clone, serde::Deserialize, PartialEq)]
pub struct ReviewComment {
    pub id: u64,
    pub path: String,
    /// Anchor line on `side` against the *current* diff; None = outdated
    /// (the code the comment was written against has since changed).
    pub line: Option<u64>,
    /// "RIGHT" (new side) or "LEFT" (old side).
    pub side: Option<String>,
    /// Multi-line comments start here and anchor at `line` (the end), like
    /// GitHub's own UI.
    pub start_line: Option<u64>,
    pub body: String,
    pub user: Author,
    /// When the comment was posted, ISO 8601 (GitHub's format).
    pub created_at: String,
    pub in_reply_to_id: Option<u64>,
}

/// A user who can be @-mentioned on a repository, for the comment
/// composer's autocomplete.
#[derive(Debug, Clone, serde::Deserialize, PartialEq)]
pub struct Mention {
    pub login: String,
    /// The user's display name, if set (shown as a hint beside the login).
    #[serde(default)]
    pub name: Option<String>,
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
    /// The request itself names no PR it could name — an argument the
    /// resolver can't turn into a [`PrLocator`]. Nothing was asked of
    /// GitHub, so the message names the expected form, not the sign-in
    /// fix: the problem is in the input, not the credential.
    Invalid(String),
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
            Self::Invalid(cause) => write!(f, "{cause}"),
            Self::Failed(cause) => write!(f, "GitHub operation failed: {cause}"),
        }
    }
}

impl std::error::Error for ClientError {}

/// The backend-agnostic client contract. Implementations are cheap to
/// construct and synchronous — they run on background threads, never the UI
/// thread — and may spawn subprocesses or make network calls inside a call.
pub trait GitHubClient: Send + Sync {
    fn set_thread_resolved(&self, _thread_id: &str, _resolved: bool) -> Result<(), ClientError> {
        Err(ClientError::Failed(
            "thread resolution is not supported by this backend".into(),
        ))
    }

    fn edit_comment(&self, _target: &CommentTarget, _body: &str) -> Result<(), ClientError> {
        Err(ClientError::Failed(
            "comment editing is not supported by this backend".into(),
        ))
    }

    fn delete_comment(&self, _target: &CommentTarget) -> Result<(), ClientError> {
        Err(ClientError::Failed(
            "comment deletion is not supported by this backend".into(),
        ))
    }
    /// Detail-only, paginated state for the authenticated viewer, never queue data.
    fn viewed_files(&self, _loc: &PrLocator) -> Result<ViewedFiles, ClientError> {
        Err(ClientError::Failed(
            "viewed files are not supported by this backend".into(),
        ))
    }

    /// Set one file's state for the authenticated viewer using the opaque PR
    /// identity returned by `viewed_files`. Paths are GitHub's exact paths,
    /// including the new path for renames and the old path for deletions.
    fn set_file_viewed(
        &self,
        _pull_request_id: &str,
        _path: &str,
        _viewed: bool,
    ) -> Result<(), ClientError> {
        Err(ClientError::Failed(
            "viewed files are not supported by this backend".into(),
        ))
    }

    /// Ordered PR commits and per-commit CI. Detail-only: queue searches must
    /// not call this. Unsupported backends report unavailable, not empty.
    fn pr_commits(&self, _loc: &PrLocator) -> Result<PrCommits, ClientError> {
        Err(ClientError::Failed(
            "PR commits are not supported by this backend".into(),
        ))
    }

    /// Complete published conversation, with paginated replies and resolution
    /// state. Pending review comments belong to the separate review session.
    fn pr_discussion(&self, loc: &PrLocator) -> Result<PrDiscussion, ClientError> {
        self.review_comments(loc)
            .map(PrDiscussion::from_review_comments)
    }

    /// The viewer's server-side draft, including drafts started on the web.
    fn pending_review(&self, _loc: &PrLocator) -> Result<Option<PendingReview>, ClientError> {
        Ok(None)
    }
    fn start_review(&self, _loc: &PrLocator, _commit: &str) -> Result<PendingReview, ClientError> {
        Err(ClientError::Failed(
            "pending reviews are not supported by this backend".into(),
        ))
    }
    /// Persist a pending review's summary without submitting the review.
    fn update_pending_body(
        &self,
        _loc: &PrLocator,
        _review: &PendingReview,
        _body: &str,
    ) -> Result<(), ClientError> {
        Err(ClientError::Failed(
            "pending review summaries are not supported by this backend".into(),
        ))
    }
    /// Publish a main PR conversation comment, not an anchored review comment.
    fn post_conversation_comment(&self, _loc: &PrLocator, _body: &str) -> Result<(), ClientError> {
        Err(ClientError::Failed(
            "conversation comments are not supported by this backend".into(),
        ))
    }
    /// As above, retaining the server identity for optimistic UI reconciliation.
    /// Backends without a receipt may return None after a successful post.
    fn post_conversation_comment_id(
        &self,
        loc: &PrLocator,
        body: &str,
    ) -> Result<Option<u64>, ClientError> {
        self.post_conversation_comment(loc, body)?;
        Ok(None)
    }
    fn post_reply_id(
        &self,
        loc: &PrLocator,
        comment_id: u64,
        body: &str,
    ) -> Result<Option<u64>, ClientError> {
        self.post_reply(loc, comment_id, body)?;
        Ok(None)
    }
    fn add_pending_comment(
        &self,
        _loc: &PrLocator,
        _review: &PendingReview,
        _anchor: &CommentAnchor,
        _body: &str,
    ) -> Result<PendingComment, ClientError> {
        Err(ClientError::Failed(
            "pending reviews are not supported by this backend".into(),
        ))
    }
    /// Add a reply to the existing draft review without publishing or submitting it.
    fn add_pending_reply(
        &self,
        _loc: &PrLocator,
        _review: &PendingReview,
        _comment_id: u64,
        _body: &str,
    ) -> Result<PendingComment, ClientError> {
        Err(ClientError::Failed(
            "pending review replies are not supported by this backend".into(),
        ))
    }
    fn edit_pending_comment(
        &self,
        _loc: &PrLocator,
        _comment: &PendingComment,
        _body: &str,
    ) -> Result<(), ClientError> {
        Err(ClientError::Failed(
            "pending reviews are not supported by this backend".into(),
        ))
    }
    fn delete_pending_comment(
        &self,
        _loc: &PrLocator,
        _comment: &PendingComment,
    ) -> Result<(), ClientError> {
        Err(ClientError::Failed(
            "pending reviews are not supported by this backend".into(),
        ))
    }
    fn submit_pending_review(
        &self,
        _loc: &PrLocator,
        _review: &PendingReview,
        _verdict: ReviewVerdict,
        _body: &str,
    ) -> Result<(), ClientError> {
        Err(ClientError::Failed(
            "pending reviews are not supported by this backend".into(),
        ))
    }
    /// Explicitly immediate, never an add-to-pending operation.
    fn post_anchored_comment(
        &self,
        loc: &PrLocator,
        anchor: &CommentAnchor,
        body: &str,
    ) -> Result<(), ClientError> {
        if anchor.start_line.is_some() {
            return Err(ClientError::Invalid(
                "range comments are not supported by this backend".into(),
            ));
        }
        self.post_review_comment(
            loc,
            &anchor.commit_id,
            &anchor.path,
            anchor.side.api_str(),
            anchor.line,
            body,
        )
    }

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

    /// Turn a user-typed PR argument into a locator. Accepts
    /// `owner/repo#123`, `#123`, `123`, or a GitHub PR URL; a bare number
    /// resolves the repository from the current directory. A malformed
    /// argument is [`ClientError::Invalid`] — the message names the
    /// expected form, not the sign-in fix.
    fn resolve_pr(&self, arg: &str) -> Result<PrLocator, ClientError>;

    /// One PR's metadata (see [`PrMeta`]).
    fn pr_meta(&self, loc: &PrLocator) -> Result<PrMeta, ClientError>;

    /// The PR's unified diff patch, for the diff engine to parse.
    fn pr_patch(&self, loc: &PrLocator) -> Result<String, ClientError>;

    /// Every review comment on the PR, flat; the app groups them into
    /// threads. Pagination (gh ≥ 2.66's `--paginate --slurp`) is the
    /// backend's concern, not the caller's.
    fn review_comments(&self, loc: &PrLocator) -> Result<Vec<ReviewComment>, ClientError>;

    /// Users who can be @-mentioned on the PR's repository, for comment
    /// autocomplete. Capped by the backend — the autocomplete only needs
    /// a workable pool of the most relevant, not every account.
    fn mentionable_users(&self, loc: &PrLocator) -> Result<Vec<Mention>, ClientError>;

    /// Full contents of `path` at `commit_oid`, for whole-file upgrades.
    /// `Ok(None)` means "leave this file un-upgraded": absent at that
    /// commit, binary, or over the backend's size cap. Backends may cache
    /// — a path at a commit is immutable, including negative answers.
    fn file_at(
        &self,
        loc: &PrLocator,
        commit_oid: &str,
        path: &str,
    ) -> Result<Option<String>, ClientError>;

    /// Post a new top-level review comment anchored at (path, side, line)
    /// against `commit_id` (the PR's head oid). `side` is GitHub's
    /// LEFT/RIGHT vocabulary; `line` a JSON integer on the wire — the
    /// backend sees to that.
    fn post_review_comment(
        &self,
        loc: &PrLocator,
        commit_id: &str,
        path: &str,
        side: &str,
        line: u64,
        body: &str,
    ) -> Result<(), ClientError>;

    /// Publish an immediate reply to the review thread rooted at `comment_id`.
    /// With a pending review, use `add_pending_reply` instead.
    fn post_reply(&self, loc: &PrLocator, comment_id: u64, body: &str) -> Result<(), ClientError>;

    /// Submit a top-level review with `verdict`. GitHub requires a
    /// non-empty body for request-changes and comment reviews — the
    /// caller enforces this so the error surfaces before a round trip;
    /// approvals may be bodyless.
    fn submit_review(
        &self,
        loc: &PrLocator,
        verdict: ReviewVerdict,
        body: &str,
    ) -> Result<(), ClientError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Only credential failures should advise signing in again.
    #[test]
    fn credential_failure_messages_name_the_gh_auth_login_fix() {
        for err in [
            ClientError::NoCredentialSource,
            ClientError::Unauthenticated,
        ] {
            let message = err.to_string();
            assert!(
                message.contains("gh auth login"),
                "{err:?} doesn't name the fix: {message}"
            );
        }
    }

    #[test]
    fn api_failures_preserve_the_cause_without_misdiagnosing_authentication() {
        for cause in [
            "Not Found (HTTP 404)",
            "network unavailable",
            "invalid line (HTTP 422)",
        ] {
            let message = ClientError::Failed(cause.into()).to_string();
            assert!(message.contains(cause));
            assert!(!message.contains("gh auth login"));
        }
    }

    /// A malformed request reports its own cause — the expected form — and
    /// never claims GitHub is unreachable or the user unsigned in.
    #[test]
    fn an_invalid_request_names_the_expected_form_not_the_sign_in_fix() {
        let err = ClientError::Invalid(
            "could not parse \"nope\"; expected owner/repo#123, a PR URL, or a PR number".into(),
        );
        let message = err.to_string();
        assert!(message.contains("expected owner/repo#123"));
        assert!(!message.contains("gh auth login"), "{message}");
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

    /// Golden strings for chips: each kind compiles to its one qualifier
    /// after the preset's, and a search carrying all three pins the exact
    /// order — org, then repo, then author — however the chips were
    /// applied.
    #[test]
    fn chips_compile_to_exact_search_qualifiers() {
        let preset = "is:pr is:open review-requested:@me";
        assert_eq!(
            QueueSearch::review_requested()
                .filtered_by(QueueFilter::org("acme"))
                .query(),
            format!("{preset} org:acme")
        );
        assert_eq!(
            QueueSearch::review_requested()
                .filtered_by(QueueFilter::repo("acme/widgets"))
                .query(),
            format!("{preset} repo:acme/widgets")
        );
        assert_eq!(
            QueueSearch::review_requested()
                .filtered_by(QueueFilter::author("octocat"))
                .query(),
            format!("{preset} author:octocat")
        );
        // All three, applied in reverse order: the compiled string still
        // runs org, repo, author.
        let search = QueueSearch::review_requested()
            .filtered_by(QueueFilter::author("octocat"))
            .filtered_by(QueueFilter::repo("acme/widgets"))
            .filtered_by(QueueFilter::org("acme"));
        assert_eq!(
            search.query(),
            format!("{preset} org:acme repo:acme/widgets author:octocat")
        );
    }

    /// Chips and sorts together — the full combinations the queue can
    /// compile to. A chip changes the query, a sort changes the spec, and
    /// neither touches the other; the sort reads back in domain terms for
    /// the queue's toggle.
    #[test]
    fn chips_and_sorts_compile_together() {
        let search = QueueSearch::review_requested()
            .filtered_by(QueueFilter::org("acme"))
            .filtered_by(QueueFilter::author("octocat"))
            .sorted_by(QueueSort::OldestUpdated);
        assert_eq!(
            search.query(),
            "is:pr is:open review-requested:@me org:acme author:octocat"
        );
        assert_eq!(search.sort(), ("updated", "asc"));
        assert_eq!(search.ordering(), QueueSort::OldestUpdated);
        // Re-sorting a filtered search flips the spec, never the chips.
        let search = search.sorted_by(QueueSort::RecentlyUpdated);
        assert_eq!(search.ordering(), QueueSort::RecentlyUpdated);
        assert_eq!(search.sort(), ("updated", "desc"));
        assert_eq!(search.filters().len(), 2);
    }

    /// One chip per dimension: a second chip of a kind replaces the first,
    /// and lifting one dimension leaves the others filtering.
    #[test]
    fn chips_replace_their_own_kind_and_leave_the_others() {
        let search = QueueSearch::review_requested()
            .filtered_by(QueueFilter::org("acme"))
            .filtered_by(QueueFilter::org("globex"));
        assert_eq!(search.filters().to_vec(), vec![QueueFilter::org("globex")]);
        assert_eq!(
            search.query(),
            "is:pr is:open review-requested:@me org:globex"
        );
        // Lifting the org leaves the author.
        let search = search
            .filtered_by(QueueFilter::author("octocat"))
            .without_filter(QueueFilterKind::Org);
        assert_eq!(
            search.filters().to_vec(),
            vec![QueueFilter::author("octocat")]
        );
        // Unfiltered lifts every chip at once, keeping the order — the
        // preset queue again, backlog-first or not.
        let search = search.sorted_by(QueueSort::OldestUpdated).unfiltered();
        assert!(search.filters().is_empty());
        assert_eq!(search.query(), "is:pr is:open review-requested:@me");
        assert_eq!(search.ordering(), QueueSort::OldestUpdated);
    }

    /// A filter's own vocabulary: kinds order as org < repo < author (the
    /// compile order), prefixes match the compiled qualifier, and values
    /// read back without them.
    #[test]
    fn filters_expose_kind_value_and_qualifier() {
        assert!(QueueFilterKind::Org < QueueFilterKind::Repo);
        assert!(QueueFilterKind::Repo < QueueFilterKind::Author);
        let org = QueueFilter::org("acme");
        assert_eq!(org.kind(), QueueFilterKind::Org);
        assert_eq!(org.value(), "acme");
        assert_eq!(org.qualifier(), "org:acme");
        assert_eq!(QueueFilterKind::Repo.prefix(), "repo");
        assert_eq!(
            QueueFilter::repo("acme/widgets").qualifier(),
            "repo:acme/widgets"
        );
        assert_eq!(QueueFilter::author("octocat").qualifier(), "author:octocat");
    }

    /// A search rebuilt from its parts is the same search: the chips come
    /// back in kind order however they were restored, and the sort with
    /// them. This is what lets a search survive a restart — the parts are
    /// remembered, not the compiled string.
    #[test]
    fn a_search_rebuilt_from_its_parts_is_the_same_search() {
        let search = QueueSearch::from_parts(
            vec![
                QueueFilter::author("octocat"),
                QueueFilter::org("acme"),
                QueueFilter::repo("acme/widgets"),
            ],
            QueueSort::OldestUpdated,
        );
        assert_eq!(
            search.query(),
            "is:pr is:open review-requested:@me org:acme repo:acme/widgets author:octocat"
        );
        assert_eq!(search.sort(), ("updated", "asc"));
        // No parts, no chips: the default queue.
        let empty = QueueSearch::from_parts(Vec::new(), QueueSort::RecentlyUpdated);
        assert_eq!(empty.query(), "is:pr is:open review-requested:@me");
        assert!(empty.filters().is_empty());
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
