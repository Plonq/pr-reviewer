//! The v0.1 [`GitHubClient`](crate::GitHubClient) backend: `gh` CLI
//! subprocesses, JSON over the process boundary.
//!
//! Subprocess mechanics stop at the [`GhProcess`] trait. The domain logic —
//! which `gh` invocation answers which facade operation, how its JSON is
//! parsed, how failures map to [`ClientError`] — is plain code, tested by
//! replaying recorded `gh` output through a fake `GhProcess` (fixtures in
//! `fixtures/`, each byte-real captured output, so tests fail when the CLI's
//! reality drifts).

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::PathBuf;
use std::process::Command;

use sha2::{Digest, Sha256};

use crate::{
    CiState, ClientError, CommentAnchor, GitHubClient, Mention, PendingComment, PendingReview,
    PrLocator, PrMeta, QueueItem, QueueLabel, QueueSearch, ReviewComment, ReviewState,
    ReviewVerdict, Viewer,
};

/// One `gh` invocation exactly as the process boundary sees it.
#[derive(Debug, Clone)]
pub struct GhOutput {
    /// The exit code; `None` when the process died to a signal.
    pub code: Option<i32>,
    /// stdout as the process wrote it — raw bytes, because file contents can
    /// be anything. Text-shaped payloads decode via [`Self::text`], which
    /// reports non-UTF-8 rather than corrupting it into silence.
    pub stdout: Vec<u8>,
    /// stderr as a message for humans — diagnostic text, always.
    pub stderr: String,
}

impl GhOutput {
    /// stdout as text, or a `Failed` when it isn't — every JSON endpoint's
    /// first question.
    fn text(&self) -> Result<&str, ClientError> {
        std::str::from_utf8(&self.stdout)
            .map_err(|_| ClientError::Failed("gh output was not UTF-8".to_string()))
    }
}

/// The process boundary the backend shells across: run `gh <args>`, get its
/// recorded outcome. Production runs a real binary ([`GhSubprocess`]); tests
/// replay captured output. Nothing outside this trait knows how `gh` is
/// reached.
pub trait GhProcess: Send + Sync {
    fn run(&self, args: &[&str]) -> io::Result<GhOutput>;
}

/// Runs the `gh` found on PATH.
#[derive(Debug, Default)]
pub struct GhSubprocess;

impl GhProcess for GhSubprocess {
    fn run(&self, args: &[&str]) -> io::Result<GhOutput> {
        let out = Command::new("gh").args(args).output()?;
        Ok(GhOutput {
            code: out.status.code(),
            stdout: out.stdout,
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }
}

/// The v0.1 backend: each facade operation becomes one or more `gh`
/// invocations.
#[derive(Debug)]
pub struct GhCliClient<P: GhProcess = GhSubprocess> {
    process: P,
}

impl GhCliClient<GhSubprocess> {
    /// A backend driving the real `gh` on PATH.
    pub fn new() -> Self {
        Self::with_process(GhSubprocess)
    }
}

impl Default for GhCliClient<GhSubprocess> {
    fn default() -> Self {
        Self::new()
    }
}

impl<P: GhProcess> GhCliClient<P> {
    fn review_api(&self, args: &[String]) -> Result<serde_json::Value, ClientError> {
        let argv: Vec<&str> = args.iter().map(String::as_str).collect();
        let out = self.process.run(&argv).map_err(spawn_failure)?;
        if out.code != Some(0) {
            return Err(classify_failure(&out));
        }
        if out.stdout.is_empty() {
            return Ok(serde_json::Value::Null);
        }
        let value: serde_json::Value = decode(out.text()?)?;
        if let Some(errors) = value.get("errors") {
            return Err(ClientError::Failed(format!(
                "GitHub rejected the review operation: {errors}"
            )));
        }
        Ok(value)
    }

    fn review_graphql(&self, query: String) -> Result<serde_json::Value, ClientError> {
        let value = self.review_api(&[
            "api".into(),
            "graphql".into(),
            "-f".into(),
            format!("query={query}"),
            "-H".into(),
            "GraphQL-Features: graphql_pr_comment_positioning".into(),
        ])?;
        if !value["data"].is_object() {
            return Err(ClientError::Failed("GitHub returned no review data".into()));
        }
        Ok(value)
    }

    /// Review-specific REST comments are legacy payloads: even current drafts
    /// can omit line and side. Read the authoritative thread anchors instead.
    fn pending_comments(
        &self,
        loc: &PrLocator,
        review_id: &str,
    ) -> Result<Vec<PendingComment>, ClientError> {
        let mut comments = Vec::new();
        self.for_each_thread_comment(loc, |thread, comment, head, base| {
            if comment["pullRequestReview"]["id"].as_str() == Some(review_id) {
                comments.push(pending_thread_comment(thread, comment.clone(), head, base)?);
            }
            Ok(())
        })?;
        Ok(comments)
    }

    fn for_each_thread_comment(
        &self,
        loc: &PrLocator,
        mut visit: impl FnMut(
            &serde_json::Value,
            &serde_json::Value,
            Option<&str>,
            Option<&str>,
        ) -> Result<(), ClientError>,
    ) -> Result<(), ClientError> {
        let mut after = None;
        loop {
            let query = format!("query {{ repository(owner:{},name:{}) {{ pullRequest(number:{}) {{ headRefOid baseRefOid reviewThreads(first:100{}) {{ nodes {{ {} }} pageInfo {{ hasNextPage endCursor }} }} }} }} }}",
                json_string(&loc.owner), json_string(&loc.repo), loc.number, after_arg(after.as_deref()), review_thread_fields(None));
            let value = self.review_graphql(query)?;
            let pr = &value["data"]["repository"]["pullRequest"];
            let connection = &pr["reviewThreads"];
            for thread in connection_nodes(connection)? {
                let mut page = thread.clone();
                loop {
                    for comment in connection_nodes(&page["comments"])? {
                        visit(
                            &page,
                            comment,
                            pr["headRefOid"].as_str(),
                            pr["baseRefOid"].as_str(),
                        )?;
                    }
                    let Some(cursor) = next_cursor(&page["comments"])? else {
                        break;
                    };
                    let id: String = decode_value(page["id"].clone())?;
                    let value = self.review_graphql(format!(
                        "query {{ node(id:{}) {{ ... on PullRequestReviewThread {{ {} }} }} }}",
                        json_string(&id),
                        review_thread_fields(Some(&cursor))
                    ))?;
                    page = value["data"]["node"].clone();
                    if next_cursor(&page["comments"])?.as_deref() == Some(&cursor) {
                        return Err(ClientError::Failed(
                            "GitHub repeated a review comment cursor".into(),
                        ));
                    }
                }
            }
            let next = next_cursor(connection)?;
            if next.is_some() && next == after {
                return Err(ClientError::Failed(
                    "GitHub repeated a review thread cursor".into(),
                ));
            }
            after = next;
            if after.is_none() {
                break;
            }
        }
        Ok(())
    }

    /// Run against a specific process boundary — how tests replay recorded
    /// `gh` output.
    pub fn with_process(process: P) -> Self {
        Self { process }
    }

    /// The authenticated user via `gh api user`: one subprocess answers both
    /// halves of the facade's auth check — whether GitHub is usable, and as
    /// whom. A signed-out `gh` fails this with its documented auth exit code
    /// and a `gh auth login` hint (see the fixtures).
    fn api_user(&self) -> Result<Viewer, ClientError> {
        let out = self.process.run(&["api", "user"]).map_err(spawn_failure)?;
        if out.code != Some(0) {
            return Err(classify_failure(&out));
        }
        parse_viewer(out.text()?)
    }

    /// A queue search via `gh search prs`, then one batched GraphQL
    /// enrichment per chunk of results (see [`enrich_plans`]). Search
    /// failures classify like the auth check's: a signed-out or rejected
    /// `gh` is the queue's "you're not signed in" case, not a crash.
    /// Enrichment failures never fail the queue — the rows stay, on honest
    /// defaults — because a search that succeeded means the queue is
    /// usable, and check data can catch up on the next refresh.
    fn search_queue(&self, search: &QueueSearch) -> Result<Vec<QueueItem>, ClientError> {
        let args = search_prs_args(search);
        let argv: Vec<&str> = args.iter().map(String::as_str).collect();
        let out = self.process.run(&argv).map_err(spawn_failure)?;
        if out.code != Some(0) {
            return Err(classify_failure(&out));
        }
        let mut items = parse_queue(out.text()?)?;
        for plan in enrich_plans(&items) {
            let field = format!("query={}", enrich_query(&plan));
            let Ok(out) = self.process.run(&["api", "graphql", "-f", field.as_str()]) else {
                continue; // this batch degrades; the queue stays usable
            };
            if out.code != Some(0) {
                continue; // same: a failed batch is not a failed queue
            }
            let Ok(stdout) = out.text() else {
                continue; // same: an undecodable batch is not a failed queue
            };
            apply_enrichment(self, &mut items, &plan, stdout);
        }
        search.sort_items(&mut items);
        Ok(items)
    }

    /// Only an outstanding *direct* request to the authenticated viewer counts.
    /// Old requests on team-only queue entries must not look like pending ones.
    fn queue_review_requested_at(
        &self,
        item: &QueueItem,
        viewer: &str,
        mut requests: serde_json::Value,
        mut events: serde_json::Value,
    ) -> Result<Option<String>, ClientError> {
        let mut seen = HashSet::new();
        loop {
            if connection_nodes(&requests)?
                .iter()
                .any(|node| requested_from_viewer(node, viewer))
            {
                break;
            }
            let Some(cursor) = next_cursor(&requests)? else {
                return Ok(None);
            };
            if !seen.insert(cursor.clone()) {
                return Err(ClientError::Failed("repeated review request cursor".into()));
            }
            requests =
                self.queue_pr_field(item, "reviewRequests", queue_review_requests(Some(&cursor)))?;
        }

        // Walk backwards, stopping at the latest request for this viewer. A
        // re-request resets the age; requests for other reviewers never do.
        seen.clear();
        loop {
            if let Some(event) = connection_nodes(&events)?
                .iter()
                .rev()
                .find(|node| requested_from_viewer(node, viewer))
            {
                return Ok(event["createdAt"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned));
            }
            let has_previous: bool = decode_value(events["pageInfo"]["hasPreviousPage"].clone())?;
            if !has_previous {
                return Ok(None);
            }
            let cursor: String = decode_value(events["pageInfo"]["startCursor"].clone())?;
            if cursor.is_empty() || !seen.insert(cursor.clone()) {
                return Err(ClientError::Failed("invalid review timeline cursor".into()));
            }
            events =
                self.queue_pr_field(item, "timelineItems", queue_review_events(Some(&cursor)))?;
        }
    }

    fn queue_pr_field(
        &self,
        item: &QueueItem,
        field: &str,
        selection: String,
    ) -> Result<serde_json::Value, ClientError> {
        let (owner, repo) = item
            .repository
            .split_once('/')
            .ok_or_else(|| ClientError::Failed("invalid queue repository".into()))?;
        let value = self.review_graphql(format!(
            "query {{ repository(owner:{},name:{}) {{ pullRequest(number:{}) {{ {selection} }} }} }}",
            json_string(owner), json_string(repo), item.number,
        ))?;
        Ok(value["data"]["repository"]["pullRequest"][field].clone())
    }

    /// A bare PR number needs a repository: ask `gh` which repo the current
    /// directory belongs to. Whatever gh said, the actionable hint stays —
    /// the fix for a bare number is naming the repo explicitly.
    fn locator_in_cwd_repo(&self, number: u64) -> Result<PrLocator, ClientError> {
        let out = self
            .process
            .run(&[
                "repo",
                "view",
                "--json",
                "nameWithOwner",
                "--jq",
                ".nameWithOwner",
            ])
            .map_err(spawn_failure)?;
        if out.code != Some(0) {
            let mut err = classify_failure(&out);
            if let ClientError::Failed(cause) = &mut err {
                *cause = format!(
                    "couldn't infer the repo from the current directory ({cause}); \
                     use owner/repo#123"
                );
            }
            return Err(err);
        }
        let stdout = out.text()?;
        stdout
            .trim()
            .split_once('/')
            .map(|(owner, repo)| PrLocator {
                owner: owner.to_string(),
                repo: repo.to_string(),
                number,
            })
            .ok_or_else(|| {
                ClientError::Failed(format!("unexpected gh repo view output: {}", stdout.trim()))
            })
    }
}

impl<P: GhProcess> GitHubClient for GhCliClient<P> {
    fn viewed_files(&self, loc: &PrLocator) -> Result<crate::ViewedFiles, ClientError> {
        let mut files = HashMap::new();
        let mut after = None;
        let mut cursors = std::collections::HashSet::new();
        let mut pull_request_id = None;
        loop {
            let value = self.review_graphql(format!(
                "query {{ repository(owner:{},name:{}) {{ pullRequest(number:{}) {{ id files(first:100{}) {{ nodes {{ path viewerViewedState }} pageInfo {{ hasNextPage endCursor }} }} }} }} }}",
                json_string(&loc.owner), json_string(&loc.repo), loc.number, after_arg(after.as_deref())
            ))?;
            let pr = &value["data"]["repository"]["pullRequest"];
            let id: String = decode_value(pr["id"].clone())?;
            if pull_request_id
                .as_ref()
                .is_some_and(|previous| previous != &id)
            {
                return Err(ClientError::Failed(
                    "GitHub changed the PR identity during file fetch".into(),
                ));
            }
            pull_request_id = Some(id);
            let connection = &pr["files"];
            for file in connection_nodes(connection)? {
                files.insert(
                    decode_value(file["path"].clone())?,
                    decode_value(file["viewerViewedState"].clone())?,
                );
            }
            after = next_cursor(connection)?;
            let Some(cursor) = &after else { break };
            if !cursors.insert(cursor.clone()) {
                return Err(ClientError::Failed(
                    "GitHub repeated a viewed-file cursor".into(),
                ));
            }
        }
        Ok(crate::ViewedFiles {
            pull_request_id: pull_request_id.unwrap(),
            files,
        })
    }

    fn set_thread_resolved(&self, thread_id: &str, resolved: bool) -> Result<(), ClientError> {
        if thread_id.is_empty() {
            return Err(ClientError::Invalid(
                "missing review thread identity".into(),
            ));
        }
        let operation = if resolved {
            "resolveReviewThread"
        } else {
            "unresolveReviewThread"
        };
        let value = self.review_graphql(format!(
            "mutation {{ {operation}(input:{{threadId:{}}}) {{ thread {{ id isResolved }} }} }}",
            json_string(thread_id)
        ))?;
        let thread = &value["data"][operation]["thread"];
        if thread["id"].as_str() != Some(thread_id)
            || thread["isResolved"].as_bool() != Some(resolved)
        {
            return Err(ClientError::Failed(
                "GitHub returned no updated thread resolution".into(),
            ));
        }
        Ok(())
    }

    fn set_file_viewed(
        &self,
        pull_request_id: &str,
        path: &str,
        viewed: bool,
    ) -> Result<(), ClientError> {
        let operation = if viewed {
            "markFileAsViewed"
        } else {
            "unmarkFileAsViewed"
        };
        let value = self.review_graphql(format!(
            "mutation {{ {operation}(input:{{pullRequestId:{},path:{}}}) {{ pullRequest {{ id }} }} }}",
            json_string(pull_request_id), json_string(path)
        ))?;
        if value["data"][operation]["pullRequest"]["id"].as_str() != Some(pull_request_id) {
            return Err(ClientError::Failed(
                "GitHub returned no updated viewed-file state".into(),
            ));
        }
        Ok(())
    }

    fn pr_commits(&self, loc: &PrLocator) -> Result<crate::PrCommits, ClientError> {
        let mut result = crate::PrCommits::default();
        let mut after = None;
        let mut commit_cursors = std::collections::HashSet::new();
        loop {
            let value = self.review_graphql(format!(
                "query {{ repository(owner:{},name:{}) {{ pullRequest(number:{}) {{ commits(first:100{}) {{ nodes {{ commit {{ id oid message author {{ name user {{ login }} }} statusCheckRollup {{ state }} {} }} }} pageInfo {{ hasNextPage endCursor }} }} }} }} }}",
                json_string(&loc.owner), json_string(&loc.repo), loc.number,
                after_arg(after.as_deref()), commit_suites(None)
            ))?;
            let connection = &value["data"]["repository"]["pullRequest"]["commits"];
            for raw in connection_nodes(connection)? {
                let raw = &raw["commit"];
                let mut commit = crate::PrCommit {
                    sha: decode_value(raw["oid"].clone())?,
                    message: decode_value(raw["message"].clone())?,
                    author: raw["author"]["user"]["login"]
                        .as_str()
                        .or_else(|| raw["author"]["name"].as_str())
                        .unwrap_or("unknown")
                        .to_owned(),
                    ci: CiState::from_rollup_state(raw["statusCheckRollup"]["state"].as_str()),
                    actions: Vec::new(),
                };
                // Each commit has its own suite connection/cursor. A suite's
                // workflowRun is singular: no check-run connection is truncated.
                // GitHub's rollup already includes non-Actions/legacy statuses.
                let mut suites = raw["checkSuites"].clone();
                let mut suite_cursors = std::collections::HashSet::new();
                loop {
                    for suite in connection_nodes(&suites)? {
                        let run = &suite["workflowRun"];
                        if run.is_null() {
                            continue;
                        } // third-party check suite
                        let run = crate::ActionsRun {
                            name: decode_value(run["workflow"]["name"].clone())?,
                            url: decode_value(run["url"].clone())?,
                        };
                        if !commit
                            .actions
                            .iter()
                            .any(|existing| existing.url == run.url)
                        {
                            commit.actions.push(run);
                        }
                    }
                    let Some(cursor) = next_cursor(&suites)? else {
                        break;
                    };
                    if !suite_cursors.insert(cursor.clone()) {
                        return Err(ClientError::Failed(
                            "GitHub repeated a commit check-suite cursor".into(),
                        ));
                    }
                    let id: String = decode_value(raw["id"].clone())?;
                    let page = self.review_graphql(format!(
                        "query {{ node(id:{}) {{ ... on Commit {{ {} }} }} }}",
                        json_string(&id),
                        commit_suites(Some(&cursor))
                    ))?;
                    suites = page["data"]["node"]["checkSuites"].clone();
                }
                result.commits.push(commit);
            }
            after = next_cursor(connection)?;
            let Some(cursor) = &after else {
                break;
            };
            if !commit_cursors.insert(cursor.clone()) {
                return Err(ClientError::Failed(
                    "GitHub repeated a PR commit cursor".into(),
                ));
            }
        }
        Ok(result)
    }

    fn pr_discussion(&self, loc: &PrLocator) -> Result<crate::PrDiscussion, ClientError> {
        let mut discussion = crate::PrDiscussion::default();
        // Paginate each connection independently: a busy PR can have >100
        // comments, reviews, threads, AND replies within one thread.
        for (name, fields) in [
            ("comments", "id:fullDatabaseId node_id:id viewerCanUpdate viewerCanDelete author { login } body createdAt reactionGroups { content users { totalCount } }"),
            ("reviews", "id:fullDatabaseId node_id:id viewerCanUpdate viewerCanDelete author { login } body createdAt submittedAt state"),
        ] {
            let mut after = None;
            loop {
                let value = self.review_graphql(format!(
                    "query {{ repository(owner:{},name:{}) {{ pullRequest(number:{}) {{ {name}(first:100{}) {{ nodes {{ {fields} }} pageInfo {{ hasNextPage endCursor }} }} }} }} }}",
                    json_string(&loc.owner), json_string(&loc.repo), loc.number, after_arg(after.as_deref())
                ))?;
                let connection = &value["data"]["repository"]["pullRequest"][name];
                for raw in connection_nodes(connection)? {
                    if name == "reviews" && raw["state"].as_str() == Some("PENDING") {
                        continue;
                    }
                    let mut comment = discussion_comment(raw)?;
                    if name == "reviews" {
                        comment.review_state = Some(decode_value(raw["state"].clone())?);
                        comment.review_id = raw["node_id"].as_str().map(str::to_owned);
                        // Reviews created as drafts belong where they were
                        // submitted, not where the draft was first started.
                        comment.created_at = decode_value(raw["submittedAt"].clone())?;
                    }
                    discussion.comments.push(comment);
                }
                let next = next_cursor(connection)?;
                if next.is_some() && next == after {
                    return Err(ClientError::Failed("GitHub repeated a discussion cursor".into()));
                }
                after = next;
                if after.is_none() { break; }
            }
        }
        let mut thread_of = HashMap::new();
        let mut code_blobs = HashMap::new();
        self.for_each_thread_comment(loc, |raw_thread, raw, head, base| {
            if raw["pullRequestReview"]["state"].as_str() == Some("PENDING") {
                return Ok(());
            }
            let id: String = decode_value(raw_thread["id"].clone())?;
            let ix = match thread_of.get(&id) {
                Some(&ix) => ix,
                None => {
                    let (start_line, line) = thread_position(raw_thread, head, base)?;
                    let path: String = decode_value(raw_thread["path"].clone())?;
                    let diff_hunk = raw["diffHunk"].as_str().unwrap_or_default();
                    let code_context = if diff_hunk.is_empty() {
                        captured_code_position(raw_thread).and_then(|(commit, start, end)| {
                            // Best effort: a missing/inaccessible blob must not hide the
                            // discussion. Memoize failures too, for busy outside-diff reviews.
                            let text = code_blobs
                                .entry((commit.to_owned(), path.clone()))
                                .or_insert_with(|| self.file_at(loc, commit, &path).ok().flatten());
                            crate::ReviewCodeContext::from_blob(
                                commit,
                                start,
                                end,
                                text.as_deref()?,
                            )
                        })
                    } else {
                        None
                    };
                    let ix = discussion.threads.len();
                    discussion.threads.push(crate::ReviewThread {
                        id: id.clone(),
                        is_resolved: decode_value(raw_thread["isResolved"].clone())?,
                        viewer_can_resolve: raw_thread["viewerCanResolve"]
                            .as_bool()
                            .unwrap_or(false),
                        viewer_can_unresolve: raw_thread["viewerCanUnresolve"]
                            .as_bool()
                            .unwrap_or(false),
                        is_outdated: decode_value(raw_thread["isOutdated"].clone())?,
                        path,
                        side: decode_value(raw_thread["side"].clone())?,
                        line,
                        start_line,
                        original_line: raw_thread["originalLine"].as_u64(),
                        original_start_line: raw_thread["originalStartLine"].as_u64(),
                        diff_hunk: diff_hunk.into(),
                        code_context,
                        comments: Vec::new(),
                    });
                    thread_of.insert(id, ix);
                    ix
                }
            };
            discussion.threads[ix]
                .comments
                .push(discussion_comment(raw)?);
            Ok(())
        })?;
        for thread in &mut discussion.threads {
            thread
                .comments
                .sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.id.cmp(&b.id)));
        }
        Ok(discussion)
    }

    fn edit_comment(&self, target: &crate::CommentTarget, body: &str) -> Result<(), ClientError> {
        use crate::CommentTarget::*;
        let (mutation, input, output, id) = match target {
            Issue(id) => ("updateIssueComment", "id", "issueComment", id),
            Review(id) => (
                "updatePullRequestReview",
                "pullRequestReviewId",
                "pullRequestReview",
                id,
            ),
            ReviewComment(id) => (
                "updatePullRequestReviewComment",
                "pullRequestReviewCommentId",
                "pullRequestReviewComment",
                id,
            ),
        };
        if id.is_empty() || body.trim().is_empty() {
            return Err(ClientError::Invalid(
                "missing comment identity or body".into(),
            ));
        }
        let value = self.review_graphql(format!(
            "mutation {{ {mutation}(input:{{{input}:{},body:{}}}) {{ {output} {{ id }} }} }}",
            json_string(id),
            json_string(body)
        ))?;
        if value["data"][mutation][output]["id"].as_str() != Some(id) {
            return Err(ClientError::Failed(
                "GitHub returned no updated comment".into(),
            ));
        }
        Ok(())
    }

    fn delete_comment(&self, target: &crate::CommentTarget) -> Result<(), ClientError> {
        use crate::CommentTarget::*;
        let (mutation, input, id) = match target {
            Issue(id) => ("deleteIssueComment", "id", id),
            Review(id) => ("deletePullRequestReview", "pullRequestReviewId", id),
            ReviewComment(id) => ("deletePullRequestReviewComment", "id", id),
        };
        if id.is_empty() {
            return Err(ClientError::Invalid("missing comment identity".into()));
        }
        let value = self.review_graphql(format!(
            "mutation {{ {mutation}(input:{{{input}:{},clientMutationId:{}}}) {{ clientMutationId }} }}",
            json_string(id), json_string(id)
        ))?;
        if value["data"][mutation]["clientMutationId"].as_str() != Some(id) {
            return Err(ClientError::Failed(
                "GitHub returned no deleted comment".into(),
            ));
        }
        Ok(())
    }

    fn pending_review(&self, loc: &PrLocator) -> Result<Option<PendingReview>, ClientError> {
        let viewer = self.viewer()?;
        let value = self.review_api(&[
            "api".into(),
            "--paginate".into(),
            "--slurp".into(),
            format!(
                "repos/{}/pulls/{}/reviews?per_page=100",
                loc.repo_slug(),
                loc.number
            ),
        ])?;
        #[derive(serde::Deserialize)]
        struct Review {
            id: u64,
            node_id: String,
            state: String,
            body: Option<String>,
            user: crate::Author,
        }
        let pages: Vec<Vec<Review>> = decode_value(value)?;
        let Some(review) = pages.into_iter().flatten().find(|review| {
            review.state == "PENDING" && review.user.login.eq_ignore_ascii_case(&viewer.login)
        }) else {
            return Ok(None);
        };
        let comments = self.pending_comments(loc, &review.node_id)?;
        Ok(Some(PendingReview {
            id: review.id,
            node_id: review.node_id,
            body: review.body.unwrap_or_default(),
            comments,
        }))
    }

    fn start_review(&self, loc: &PrLocator, commit: &str) -> Result<PendingReview, ClientError> {
        // Omitting `event` is essential: this creates PENDING, not COMMENT.
        let value = self.review_api(&[
            "api".into(),
            "-X".into(),
            "POST".into(),
            format!("repos/{}/pulls/{}/reviews", loc.repo_slug(), loc.number),
            "-f".into(),
            format!("commit_id={commit}"),
        ])?;
        Ok(PendingReview {
            id: decode_value(value["id"].clone())?,
            node_id: decode_value(value["node_id"].clone())?,
            body: value["body"].as_str().unwrap_or_default().into(),
            comments: Vec::new(),
        })
    }

    fn update_pending_body(
        &self,
        loc: &PrLocator,
        review: &PendingReview,
        body: &str,
    ) -> Result<(), ClientError> {
        let value = self.review_api(&[
            "api".into(),
            "-X".into(),
            "PUT".into(),
            format!(
                "repos/{}/pulls/{}/reviews/{}",
                loc.repo_slug(),
                loc.number,
                review.id
            ),
            "-f".into(),
            format!("body={body}"),
        ])?;
        if value["id"].as_u64() != Some(review.id) {
            return Err(ClientError::Failed(
                "GitHub returned no updated review summary".into(),
            ));
        }
        Ok(())
    }

    fn post_conversation_comment(&self, loc: &PrLocator, body: &str) -> Result<(), ClientError> {
        self.post_conversation_comment_id(loc, body).map(|_| ())
    }

    fn post_conversation_comment_id(
        &self,
        loc: &PrLocator,
        body: &str,
    ) -> Result<Option<u64>, ClientError> {
        let value = self.review_api(&[
            "api".into(),
            "-X".into(),
            "POST".into(),
            format!("repos/{}/issues/{}/comments", loc.repo_slug(), loc.number),
            "-f".into(),
            format!("body={body}"),
        ])?;
        Ok(value["id"].as_u64())
    }

    fn post_reply_id(
        &self,
        loc: &PrLocator,
        comment_id: u64,
        body: &str,
    ) -> Result<Option<u64>, ClientError> {
        let value = self.review_api(&[
            "api".into(),
            "-X".into(),
            "POST".into(),
            format!(
                "repos/{}/pulls/{}/comments/{comment_id}/replies",
                loc.repo_slug(),
                loc.number
            ),
            "-f".into(),
            format!("body={body}"),
        ])?;
        Ok(value["id"].as_u64())
    }

    fn add_pending_comment(
        &self,
        _loc: &PrLocator,
        review: &PendingReview,
        anchor: &CommentAnchor,
        body: &str,
    ) -> Result<PendingComment, ClientError> {
        anchor.validate()?;
        let range = anchor
            .start_line
            .map(|start| format!(",startLine:{start},startSide:{}", anchor.side.api_str()))
            .unwrap_or_default();
        let positioning = positioning_input(anchor);
        let query = format!("mutation {{ addPullRequestReviewThread(input:{{pullRequestReviewId:{},path:{},side:{},line:{},body:{}{range}{positioning}}}) {{ thread {{ side:diffSide line start_line:startLine isOutdated {POSITIONING_FIELDS} comments(first:1) {{ nodes {{ id:fullDatabaseId node_id:id path body }} }} }} }} }}",
            json_string(&review.node_id), json_string(&anchor.path), anchor.side.api_str(), anchor.line, json_string(body));
        let value = self
            .review_graphql(query)
            .map_err(|error| positioning_error(anchor, error))?;
        let thread = &value["data"]["addPullRequestReviewThread"]["thread"];
        pending_thread_comment(
            thread,
            thread["comments"]["nodes"][0].clone(),
            Some(&anchor.commit_id),
            anchor
                .position
                .as_ref()
                .and_then(|position| position.base_commit_id.as_deref()),
        )
    }

    fn add_pending_reply(
        &self,
        loc: &PrLocator,
        review: &PendingReview,
        comment_id: u64,
        body: &str,
    ) -> Result<PendingComment, ClientError> {
        // Use the real node identity and authoritative thread anchor, including
        // preview-only/outdated positions. Never synthesize an opaque node id.
        let mut target = None;
        self.for_each_thread_comment(loc, |thread, comment, head, base| {
            if database_id(&comment["id"])? == comment_id {
                let node_id: String = decode_value(comment["node_id"].clone())?;
                target = Some((
                    thread.clone(),
                    node_id,
                    head.map(str::to_owned),
                    base.map(str::to_owned),
                ));
            }
            Ok(())
        })?;
        let (thread, node_id, head, base) = target.ok_or_else(|| {
            ClientError::Invalid(
                "this comment is no longer available; refresh and try again".into(),
            )
        })?;
        let value = self.review_graphql(format!(
            "mutation {{ addPullRequestReviewComment(input:{{pullRequestReviewId:{},inReplyTo:{},body:{}}}) {{ comment {{ id:fullDatabaseId node_id:id path body replyTo {{ id:fullDatabaseId }} }} }} }}",
            json_string(&review.node_id), json_string(&node_id), json_string(body)
        ))?;
        let comment = pending_thread_comment(
            &thread,
            value["data"]["addPullRequestReviewComment"]["comment"].clone(),
            head.as_deref(),
            base.as_deref(),
        )?;
        if comment.in_reply_to_id != Some(comment_id) {
            return Err(ClientError::Failed(
                "GitHub returned no reply to the requested comment".into(),
            ));
        }
        Ok(comment)
    }

    fn edit_pending_comment(
        &self,
        _loc: &PrLocator,
        comment: &PendingComment,
        body: &str,
    ) -> Result<(), ClientError> {
        let id = pending_node_id(comment)?;
        let value = self.review_graphql(format!("mutation {{ updatePullRequestReviewComment(input:{{pullRequestReviewCommentId:{},body:{}}}) {{ pullRequestReviewComment {{ id }} }} }}", json_string(id), json_string(body)))?;
        if value["data"]["updatePullRequestReviewComment"]["pullRequestReviewComment"]["id"]
            .as_str()
            != Some(id)
        {
            return Err(ClientError::Failed(
                "GitHub returned no updated draft comment".into(),
            ));
        }
        Ok(())
    }

    fn delete_pending_comment(
        &self,
        _loc: &PrLocator,
        comment: &PendingComment,
    ) -> Result<(), ClientError> {
        let id = pending_node_id(comment)?;
        let value = self.review_graphql(format!("mutation {{ deletePullRequestReviewComment(input:{{id:{},clientMutationId:{}}}) {{ clientMutationId }} }}", json_string(id), json_string(id)))?;
        if value["data"]["deletePullRequestReviewComment"]["clientMutationId"].as_str() != Some(id)
        {
            return Err(ClientError::Failed(
                "GitHub returned no deleted draft comment".into(),
            ));
        }
        Ok(())
    }

    fn submit_pending_review(
        &self,
        _loc: &PrLocator,
        review: &PendingReview,
        verdict: ReviewVerdict,
        body: &str,
    ) -> Result<(), ClientError> {
        let event = match verdict {
            ReviewVerdict::Approve => "APPROVE",
            ReviewVerdict::RequestChanges => "REQUEST_CHANGES",
            ReviewVerdict::Comment => "COMMENT",
        };
        let value = self.review_graphql(format!("mutation {{ submitPullRequestReview(input:{{pullRequestReviewId:{},event:{event},body:{}}}) {{ pullRequestReview {{ id state }} }} }}", json_string(&review.node_id), json_string(body)))?;
        if !value["data"]["submitPullRequestReview"]["pullRequestReview"]["id"].is_string() {
            return Err(ClientError::Failed(
                "GitHub returned no submitted review".into(),
            ));
        }
        Ok(())
    }

    fn post_anchored_comment(
        &self,
        loc: &PrLocator,
        anchor: &CommentAnchor,
        body: &str,
    ) -> Result<(), ClientError> {
        anchor.validate()?;
        if anchor.position.is_some() {
            // One atomic, already-submitted review: never add to or submit the
            // viewer's existing pending review, including when this fails.
            let value =
                self.review_graphql(format!(
                "query {{ repository(owner:{},name:{}) {{ pullRequest(number:{}) {{ id }} }} }}",
                json_string(&loc.owner), json_string(&loc.repo), loc.number
            ))?;
            let id: String =
                decode_value(value["data"]["repository"]["pullRequest"]["id"].clone())?;
            let value = self.review_graphql(format!(
                "mutation {{ addPullRequestReview(input:{{pullRequestId:{},commitOID:{},event:COMMENT,threads:[{{path:{},side:{},body:{}{}}}]}}) {{ pullRequestReview {{ id state }} }} }}",
                json_string(&id), json_string(&anchor.commit_id), json_string(&anchor.path), anchor.side.api_str(), json_string(body), positioning_input(anchor)
            )).map_err(|error| positioning_error(anchor, error))?;
            let review = &value["data"]["addPullRequestReview"]["pullRequestReview"];
            if !review["id"].is_string() || review["state"].as_str() != Some("COMMENTED") {
                return Err(ClientError::Failed(
                    "GitHub returned no posted comment review".into(),
                ));
            }
            return Ok(());
        }
        if anchor.start_line.is_none() {
            return self.post_review_comment(
                loc,
                &anchor.commit_id,
                &anchor.path,
                anchor.side.api_str(),
                anchor.line,
                body,
            );
        }
        self.review_api(&[
            "api".into(),
            "-X".into(),
            "POST".into(),
            format!("repos/{}/pulls/{}/comments", loc.repo_slug(), loc.number),
            "-f".into(),
            format!("body={body}"),
            "-f".into(),
            format!("commit_id={}", anchor.commit_id),
            "-f".into(),
            format!("path={}", anchor.path),
            "-f".into(),
            format!("side={}", anchor.side.api_str()),
            "-F".into(),
            format!("line={}", anchor.line),
            "-F".into(),
            format!("start_line={}", anchor.start_line.unwrap()),
            "-f".into(),
            format!("start_side={}", anchor.side.api_str()),
        ])?;
        Ok(())
    }

    fn viewer(&self) -> Result<Viewer, ClientError> {
        self.api_user()
    }

    fn search_prs(&self, search: &QueueSearch) -> Result<Vec<QueueItem>, ClientError> {
        self.search_queue(search)
    }

    fn resolve_pr(&self, arg: &str) -> Result<PrLocator, ClientError> {
        if let Some(rest) = arg
            .strip_prefix("https://github.com/")
            .or_else(|| arg.strip_prefix("http://github.com/"))
            .or_else(|| arg.strip_prefix("github.com/"))
        {
            let parts: Vec<&str> = rest.split('/').collect();
            if parts.len() >= 4 && parts[2] == "pull" {
                let digits: String = parts[3].chars().take_while(char::is_ascii_digit).collect();
                let number = digits
                    .parse()
                    .map_err(|_| ClientError::Invalid(format!("no PR number in URL {arg}")))?;
                return Ok(PrLocator {
                    owner: parts[0].to_string(),
                    repo: parts[1].to_string(),
                    number,
                });
            }
            return Err(ClientError::Invalid(format!(
                "unrecognized GitHub URL: {arg}"
            )));
        }

        if let Some((repo_part, number)) = arg.split_once('#') {
            let number = number
                .parse()
                .map_err(|_| ClientError::Invalid(format!("invalid PR number in {arg}")))?;
            if repo_part.is_empty() {
                return self.locator_in_cwd_repo(number);
            }
            let (owner, repo) = repo_part.split_once('/').ok_or_else(|| {
                ClientError::Invalid(format!("expected owner/repo before '#' in {arg}"))
            })?;
            return Ok(PrLocator {
                owner: owner.to_string(),
                repo: repo.to_string(),
                number,
            });
        }

        if let Ok(number) = arg.parse() {
            return self.locator_in_cwd_repo(number);
        }

        Err(ClientError::Invalid(format!(
            "could not parse {arg:?}; expected owner/repo#123, a PR URL, or a PR number"
        )))
    }

    fn pr_meta(&self, loc: &PrLocator) -> Result<PrMeta, ClientError> {
        let number = loc.number.to_string();
        let slug = loc.repo_slug();
        let out = self
            .process
            .run(&[
                "pr",
                "view",
                &number,
                "--repo",
                &slug,
                "--json",
                "number,title,author,state,url,body,baseRefName,headRefName,baseRefOid,headRefOid,\
                 additions,deletions,changedFiles,reviewDecision",
            ])
            .map_err(spawn_failure)?;
        if out.code != Some(0) {
            return Err(classify_failure(&out));
        }
        serde_json::from_str(out.text()?)
            .map_err(|err| ClientError::Failed(format!("unexpected GitHub response: {err}")))
    }

    fn review_changes(
        &self,
        loc: &PrLocator,
        head: &str,
    ) -> Result<crate::ReviewChanges, ClientError> {
        use crate::review_changes::{latest_review, valid_oid, validate_comparison};
        if !valid_oid(head) {
            return Err(ClientError::Failed("the PR head is unavailable".into()));
        }
        let viewer = self.viewer()?;
        let pages = self.review_api(&[
            "api".into(),
            "--paginate".into(),
            "--slurp".into(),
            format!(
                "repos/{}/pulls/{}/reviews?per_page=100",
                loc.repo_slug(),
                loc.number
            ),
        ])?;
        let Some(review) = latest_review(pages, &viewer.login)? else {
            return Ok(crate::ReviewChanges::Unavailable(
                "Submit a GitHub review to use Changed since last review.".into(),
            ));
        };
        if review.commit == head {
            return Ok(crate::ReviewChanges::Available(review));
        }
        let comparison = match self.review_api(&[
            "api".into(),
            format!(
                "repos/{}/compare/{}...{head}?per_page=1",
                loc.repo_slug(),
                review.commit
            ),
        ]) {
            Ok(value) => value,
            Err(ClientError::Failed(cause)) if cause.contains("HTTP 404") => {
                return Ok(crate::ReviewChanges::Unavailable(
                    "Unavailable: the commit from your last review can no longer be compared with this PR. It may have been removed by a rebase or force-push. View all changes instead.".into(),
                ));
            }
            Err(error) => return Err(error),
        };
        validate_comparison(review, &comparison)
    }

    fn comparison_patch(
        &self,
        loc: &PrLocator,
        base: &str,
        head: &str,
        upstream: &str,
    ) -> Result<crate::ReviewComparison, ClientError> {
        use crate::review_changes::valid_oid;
        if ![base, head, upstream].into_iter().all(valid_oid) {
            return Err(ClientError::Invalid("invalid comparison revision".into()));
        }
        if base == head {
            return Ok(crate::ReviewComparison::default());
        }
        // Both merge bases come from immutable revisions captured for this
        // fetch, not mutable branch names. The PR may not include the target's
        // latest commits, so never merge its tip directly into the baseline.
        let merge_base = |revision: &str| -> Result<String, ClientError> {
            let comparison = self.review_api(&[
                "api".into(),
                format!(
                    "repos/{}/compare/{upstream}...{revision}?per_page=1",
                    loc.repo_slug()
                ),
            ])?;
            comparison["merge_base_commit"]["sha"]
                .as_str()
                .filter(|oid| valid_oid(oid))
                .map(str::to_owned)
                .ok_or_else(|| ClientError::Failed("GitHub returned no upstream merge base".into()))
        };
        let old_upstream = merge_base(base)?;
        let new_upstream = merge_base(head)?;
        if old_upstream != new_upstream {
            return crate::comparison::fetch_comparison(
                loc,
                base,
                head,
                &old_upstream,
                &new_upstream,
            );
        }
        let endpoint = format!("repos/{}/compare/{base}...{head}", loc.repo_slug());
        let out = self
            .process
            .run(&[
                "api",
                &endpoint,
                "-H",
                "Accept: application/vnd.github.diff",
            ])
            .map_err(spawn_failure)?;
        if out.code != Some(0) {
            return Err(classify_failure(&out));
        }
        Ok(out.text()?.to_string().into())
    }

    fn pr_patch(&self, loc: &PrLocator) -> Result<String, ClientError> {
        let number = loc.number.to_string();
        let slug = loc.repo_slug();
        let out = self
            .process
            .run(&["pr", "diff", &number, "--repo", &slug])
            .map_err(spawn_failure)?;
        if out.code != Some(0) {
            return Err(classify_failure(&out));
        }
        Ok(out.text()?.to_string())
    }

    fn review_comments(&self, loc: &PrLocator) -> Result<Vec<ReviewComment>, ClientError> {
        let endpoint = format!(
            "repos/{}/{}/pulls/{}/comments?per_page=100",
            loc.owner, loc.repo, loc.number
        );
        let out = self
            .process
            .run(&["api", "--paginate", "--slurp", &endpoint])
            .map_err(spawn_failure)?;
        if out.code != Some(0) {
            return Err(classify_failure(&out));
        }
        // --paginate --slurp (gh ≥ 2.66; we require it) wraps each page's
        // JSON array into one array-of-arrays — without --slurp, --paginate
        // concatenates the arrays back-to-back ("[…]"), which serde can't
        // parse.
        let pages: Vec<Vec<ReviewComment>> = serde_json::from_str(out.text()?)
            .map_err(|err| ClientError::Failed(format!("unexpected GitHub response: {err}")))?;
        let mut comments: Vec<_> = pages.into_iter().flatten().collect();
        // REST has no blob-positioning fields. Recover preview-only anchors
        // from threads, retaining REST's authors, timestamps, and reply ids.
        if comments.iter().any(|comment| comment.line.is_none()) {
            let by_id: HashMap<_, _> = comments
                .iter_mut()
                .map(|comment| (comment.id, comment))
                .collect();
            let mut by_id = by_id;
            self.for_each_thread_comment(loc, |thread, raw, head, base| {
                let id: u64 = decode_value(raw["id"].clone()).or_else(|_| {
                    raw["id"]
                        .as_str()
                        .and_then(|id| id.parse().ok())
                        .ok_or_else(|| ClientError::Failed("GitHub returned no comment id".into()))
                })?;
                if let Some(comment) = by_id.get_mut(&id) {
                    if comment.line.is_none() {
                        let (start, line) = thread_position(thread, head, base)?;
                        comment.line = line;
                        comment.start_line = start;
                        comment.side = Some(
                            decode_value::<crate::DiffSide>(thread["side"].clone())?
                                .api_str()
                                .into(),
                        );
                    }
                }
                Ok(())
            })?;
        }
        Ok(comments)
    }

    fn mentionable_users(&self, loc: &PrLocator) -> Result<Vec<Mention>, ClientError> {
        #[derive(serde::Deserialize)]
        struct Resp {
            data: Data,
        }
        #[derive(serde::Deserialize)]
        struct Data {
            repository: RepositoryField,
        }
        #[derive(serde::Deserialize)]
        struct RepositoryField {
            #[serde(rename = "mentionableUsers")]
            mentionable_users: Connection,
        }
        #[derive(serde::Deserialize)]
        struct Connection {
            nodes: Vec<Mention>,
            #[serde(rename = "pageInfo")]
            page_info: PageInfo,
        }
        #[derive(serde::Deserialize)]
        struct PageInfo {
            #[serde(rename = "hasNextPage")]
            has_next_page: bool,
            #[serde(rename = "endCursor")]
            end_cursor: Option<String>,
        }

        const QUERY: &str = "query($owner:String!,$repo:String!,$after:String){\
            repository(owner:$owner,name:$repo){\
            mentionableUsers(first:100,after:$after){\
            nodes{login name} pageInfo{hasNextPage endCursor}}}}";

        let mut users = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let mut args = vec![
                "api".to_string(),
                "graphql".to_string(),
                "-f".to_string(),
                format!("query={QUERY}"),
                "-f".to_string(),
                format!("owner={}", loc.owner),
                "-f".to_string(),
                format!("repo={}", loc.repo),
            ];
            if let Some(after) = &cursor {
                args.push("-f".to_string());
                args.push(format!("after={after}"));
            }
            let argv: Vec<&str> = args.iter().map(String::as_str).collect();
            let out = self.process.run(&argv).map_err(spawn_failure)?;
            if out.code != Some(0) {
                return Err(classify_failure(&out));
            }
            let resp: Resp = serde_json::from_str(out.text()?)
                .map_err(|err| ClientError::Failed(format!("unexpected GitHub response: {err}")))?;
            let conn = resp.data.repository.mentionable_users;
            users.extend(conn.nodes);
            if users.len() >= MAX_MENTIONABLE || !conn.page_info.has_next_page {
                break;
            }
            match conn.page_info.end_cursor {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
        Ok(users)
    }

    fn file_at(
        &self,
        loc: &PrLocator,
        commit_oid: &str,
        path: &str,
    ) -> Result<Option<String>, ClientError> {
        let cache = cache_path(&loc.repo_slug(), commit_oid, path);
        if let Some(cache) = &cache {
            if cache.with_extension("absent").exists() {
                return Ok(None);
            }
            if let Ok(bytes) = std::fs::read(cache) {
                return Ok(String::from_utf8(bytes).ok());
            }
        }

        let endpoint = format!(
            "repos/{}/{}/contents/{}?ref={}",
            loc.owner,
            loc.repo,
            encode_path(path),
            commit_oid
        );
        let out = self
            .process
            .run(&[
                "api",
                "-H",
                "Accept: application/vnd.github.raw+json",
                &endpoint,
            ])
            .map_err(spawn_failure)?;
        if out.code != Some(0) {
            if out.stderr.contains("404") {
                mark_absent(cache);
                return Ok(None);
            }
            return Err(classify_failure(&out));
        }
        if out.stdout.len() > MAX_BLOB_BYTES {
            mark_absent(cache);
            return Ok(None);
        }
        let Ok(text) = String::from_utf8(out.stdout) else {
            mark_absent(cache);
            return Ok(None);
        };
        if let Some(cache) = cache {
            let _ = std::fs::write(cache, &text);
        }
        Ok(Some(text))
    }

    fn post_review_comment(
        &self,
        loc: &PrLocator,
        commit_id: &str,
        path: &str,
        side: &str,
        line: u64,
        body: &str,
    ) -> Result<(), ClientError> {
        let endpoint = format!(
            "repos/{}/{}/pulls/{}/comments",
            loc.owner, loc.repo, loc.number
        );
        let args = vec![
            "api".to_string(),
            "-X".to_string(),
            "POST".to_string(),
            endpoint,
            "-f".to_string(),
            format!("body={body}"),
            "-f".to_string(),
            format!("commit_id={commit_id}"),
            "-f".to_string(),
            format!("path={path}"),
            "-f".to_string(),
            format!("side={side}"),
            // -F, not -f: line must be a JSON integer, not a string.
            "-F".to_string(),
            format!("line={line}"),
        ];
        let argv: Vec<&str> = args.iter().map(String::as_str).collect();
        let out = self.process.run(&argv).map_err(spawn_failure)?;
        if out.code != Some(0) {
            return Err(classify_failure(&out));
        }
        Ok(())
    }

    fn post_reply(&self, loc: &PrLocator, comment_id: u64, body: &str) -> Result<(), ClientError> {
        let endpoint = format!(
            "repos/{}/{}/pulls/{}/comments/{comment_id}/replies",
            loc.owner, loc.repo, loc.number
        );
        let args = vec![
            "api".to_string(),
            "-X".to_string(),
            "POST".to_string(),
            endpoint,
            "-f".to_string(),
            format!("body={body}"),
        ];
        let argv: Vec<&str> = args.iter().map(String::as_str).collect();
        let out = self.process.run(&argv).map_err(spawn_failure)?;
        if out.code != Some(0) {
            return Err(classify_failure(&out));
        }
        Ok(())
    }

    fn submit_review(
        &self,
        loc: &PrLocator,
        verdict: ReviewVerdict,
        body: &str,
    ) -> Result<(), ClientError> {
        let number = loc.number.to_string();
        let slug = loc.repo_slug();
        let flag = match verdict {
            ReviewVerdict::Approve => "--approve",
            ReviewVerdict::RequestChanges => "--request-changes",
            ReviewVerdict::Comment => "--comment",
        };
        let mut args = vec![
            "pr".to_string(),
            "review".to_string(),
            number,
            "--repo".to_string(),
            slug,
            flag.to_string(),
        ];
        if !body.is_empty() {
            args.push("--body".to_string());
            args.push(body.to_string());
        }
        let argv: Vec<&str> = args.iter().map(String::as_str).collect();
        let out = self.process.run(&argv).map_err(spawn_failure)?;
        if out.code != Some(0) {
            return Err(classify_failure(&out));
        }
        Ok(())
    }
}

fn after_arg(cursor: Option<&str>) -> String {
    cursor
        .map(|cursor| format!(",after:{}", json_string(cursor)))
        .unwrap_or_default()
}

fn review_thread_fields(cursor: Option<&str>) -> String {
    format!("id path isResolved viewerCanResolve viewerCanUnresolve originalLine originalStartLine side:diffSide line start_line:startLine isOutdated {POSITIONING_FIELDS} comments(first:100{}) {{ nodes {{ id:fullDatabaseId node_id:id viewerCanUpdate viewerCanDelete path body diffHunk createdAt author {{ login }} reactionGroups {{ content users {{ totalCount }} }} replyTo {{ id:fullDatabaseId }} pullRequestReview {{ id state }} }} pageInfo {{ hasNextPage endCursor }} }}", after_arg(cursor))
}

fn database_id(value: &serde_json::Value) -> Result<u64, ClientError> {
    value
        .as_u64()
        .or_else(|| value.as_str()?.parse().ok())
        .ok_or_else(|| ClientError::Failed("GitHub returned no comment id".into()))
}

fn discussion_comment(raw: &serde_json::Value) -> Result<crate::DiscussionComment, ClientError> {
    let id = raw["id"]
        .as_u64()
        .or_else(|| raw["id"].as_str()?.parse().ok())
        .ok_or_else(|| ClientError::Failed("GitHub returned no discussion comment id".into()))?;
    let mut reactions = Vec::new();
    if let Some(groups) = raw["reactionGroups"].as_array() {
        for group in groups {
            let count: u64 = decode_value(group["users"]["totalCount"].clone())?;
            if count > 0 {
                reactions.push(crate::Reaction {
                    content: decode_value(group["content"].clone())?,
                    count,
                });
            }
        }
    }
    Ok(crate::DiscussionComment {
        id,
        permissions: crate::CommentPermissions {
            node_id: raw["node_id"].as_str().unwrap_or_default().into(),
            can_edit: raw["viewerCanUpdate"].as_bool().unwrap_or(false),
            can_delete: raw["viewerCanDelete"].as_bool().unwrap_or(false),
        },
        author: raw["author"]["login"].as_str().unwrap_or("ghost").into(),
        body: decode_value(raw["body"].clone())?,
        created_at: decode_value(raw["createdAt"].clone())?,
        reactions,
        review_state: None,
        review_id: raw["pullRequestReview"]["id"].as_str().map(str::to_owned),
    })
}

const POSITIONING_FIELDS: &str = "positioning { __typename ... on LineComment { line commitOid headCommitOid baseCommitOid } ... on MultilineComment { startLine endLine startCommitOid endCommitOid headCommitOid baseCommitOid } }";

fn positioning_input(anchor: &CommentAnchor) -> String {
    let Some(position) = &anchor.position else {
        return String::new();
    };
    let base = position
        .base_commit_id
        .as_ref()
        .map(|base| format!(",baseCommitOid:{}", json_string(base)))
        .unwrap_or_default();
    let path = json_string(&position.path);
    let commit = json_string(&position.commit_id);
    let head = json_string(&anchor.commit_id);
    match anchor.start_line {
        Some(start) => format!(",multilinePositioning:{{startPath:{path},endPath:{path},startLine:{start},endLine:{},startCommitOid:{commit},endCommitOid:{commit},headCommitOid:{head}{base}}}", anchor.line),
        None => format!(",linePositioning:{{path:{path},line:{},commitOid:{commit},headCommitOid:{head}{base}}}", anchor.line),
    }
}

fn positioning_error(anchor: &CommentAnchor, error: ClientError) -> ClientError {
    if anchor.position.is_some() && matches!(error, ClientError::Failed(_)) {
        ClientError::Failed(format!("outside-diff comments use GitHub's experimental positioning API, which may not be enabled for this repository: {error}"))
    } else {
        error
    }
}

/// Legacy current positions win. Preview blob positions can look outdated in
/// the legacy API; use them only when their captured diff is still current.
fn thread_position(
    thread: &serde_json::Value,
    head: Option<&str>,
    base: Option<&str>,
) -> Result<(Option<u64>, Option<u64>), ClientError> {
    let outdated: bool = decode_value(thread["isOutdated"].clone())?;
    if !outdated && thread["line"].is_u64() {
        let line = thread["line"].as_u64();
        return Ok((
            thread["start_line"]
                .as_u64()
                .filter(|start| line.is_some_and(|line| *start < line)),
            line,
        ));
    }
    let position = &thread["positioning"];
    let right = thread["side"].as_str() == Some("RIGHT");
    let position_head = position["headCommitOid"].as_str().or_else(|| {
        right
            .then(|| {
                position["commitOid"]
                    .as_str()
                    .or(position["endCommitOid"].as_str())
            })
            .flatten()
    });
    if head.is_none() || head == Some("") || position_head != head {
        return Ok((None, None));
    }
    if !right && (base.is_none() || position["baseCommitOid"].as_str() != base) {
        return Ok((None, None));
    }
    let (start, line) = match position["__typename"].as_str() {
        Some("LineComment") => (None, position["line"].as_u64()),
        Some("MultilineComment") => (position["startLine"].as_u64(), position["endLine"].as_u64()),
        _ => (None, None),
    };
    let line = line.filter(|line| *line > 0 && *line <= i32::MAX as u64);
    Ok((
        start.filter(|start| *start > 0 && line.is_some_and(|line| *start < line)),
        line,
    ))
}

/// Unlike current diff-row anchors, historical previews must use the captured
/// blob even when the PR has moved on. Never substitute the PR's current base
/// or head, and don't flatten ranges spanning two different revisions.
fn captured_code_position(thread: &serde_json::Value) -> Option<(&str, Option<u64>, u64)> {
    let position = &thread["positioning"];
    let (commit, start, line) = match position["__typename"].as_str()? {
        "LineComment" => (
            position["commitOid"].as_str()?,
            None,
            position["line"].as_u64()?,
        ),
        "MultilineComment" => {
            let commit = position["endCommitOid"].as_str()?;
            if position["startCommitOid"].as_str()? != commit {
                return None;
            }
            (
                commit,
                Some(position["startLine"].as_u64()?),
                position["endLine"].as_u64()?,
            )
        }
        _ => return None,
    };
    if commit.is_empty()
        || line == 0
        || line > i32::MAX as u64
        || start.is_some_and(|start| start == 0 || start > line)
    {
        return None;
    }
    Some((commit, start, line))
}

// CheckSuite.workflowRun and WorkflowRun.url/workflow are documented GraphQL
// fields (Checks/Actions reference). A CheckSuite.url is NOT an Actions run URL.
fn commit_suites(after: Option<&str>) -> String {
    format!("checkSuites(first:100{}) {{ nodes {{ workflowRun {{ url workflow {{ name }} }} }} pageInfo {{ hasNextPage endCursor }} }}", after_arg(after))
}

fn connection_nodes(
    connection: &serde_json::Value,
) -> Result<&Vec<serde_json::Value>, ClientError> {
    connection["nodes"]
        .as_array()
        .ok_or_else(|| ClientError::Failed("GitHub returned no review connection nodes".into()))
}

fn next_cursor(connection: &serde_json::Value) -> Result<Option<String>, ClientError> {
    let has_next: bool = decode_value(connection["pageInfo"]["hasNextPage"].clone())?;
    if !has_next {
        return Ok(None);
    }
    let cursor: String = decode_value(connection["pageInfo"]["endCursor"].clone())?;
    if cursor.is_empty() {
        return Err(ClientError::Failed(
            "GitHub returned an empty review cursor".into(),
        ));
    }
    Ok(Some(cursor))
}

fn pending_thread_comment(
    thread: &serde_json::Value,
    mut comment: serde_json::Value,
    head: Option<&str>,
    base: Option<&str>,
) -> Result<PendingComment, ClientError> {
    let side: crate::DiffSide = decode_value(thread["side"].clone())?;
    let (start, line) = thread_position(thread, head, base)?;
    let reply_to = if comment["replyTo"].is_null() {
        None
    } else {
        Some(database_id(&comment["replyTo"]["id"])?)
    };
    if let Some(comment) = comment.as_object_mut() {
        comment.insert("side".into(), serde_json::json!(side));
        comment.insert("line".into(), serde_json::json!(line));
        comment.insert("start_line".into(), serde_json::json!(start));
        comment.insert("in_reply_to_id".into(), serde_json::json!(reply_to));
    }
    let mut comment: PendingComment = decode_value(comment)?;
    pending_node_id(&comment)?;
    // GitHub reports startLine == line for single-line threads.
    comment.start_line = comment
        .start_line
        .filter(|start| comment.line.is_some_and(|line| *start < line));
    Ok(comment)
}

fn pending_node_id(comment: &PendingComment) -> Result<&str, ClientError> {
    if comment.node_id.is_empty() {
        return Err(ClientError::Invalid(
            "this draft is missing its GitHub identity; refresh drafts and try again".into(),
        ));
    }
    Ok(&comment.node_id)
}

fn json_string(value: &str) -> String {
    serde_json::to_string(value).expect("strings always serialize")
}

fn decode<T: serde::de::DeserializeOwned>(text: &str) -> Result<T, ClientError> {
    serde_json::from_str(text)
        .map_err(|err| ClientError::Failed(format!("unexpected GitHub response: {err}")))
}

fn decode_value<T: serde::de::DeserializeOwned>(
    value: serde_json::Value,
) -> Result<T, ClientError> {
    serde_json::from_value(value)
        .map_err(|err| ClientError::Failed(format!("unexpected GitHub response: {err}")))
}

/// gh's search caps one query at 1000 results — GitHub's own search
/// ceiling — and pages behind the scenes, so the cap costs nothing on a
/// short queue while a long one is never silently truncated.
const SEARCH_LIMIT: &str = "1000";

/// The queue's `--json` field list: exactly what a [`QueueItem`] is built
/// from, no more. The check-sized fields (review decision, CI, diff size,
/// avatar) ride the enrichment pass instead — a search can't carry them.
const QUEUE_JSON_FIELDS: &str =
    "number,title,author,repository,isDraft,createdAt,updatedAt,commentsCount,labels";

/// How many queued PRs one enrichment query carries. gh's search caps at
/// 1000 results, so batches of 100 mean at most ten initial enrichment calls.
/// Review requests and their history are paginated further only when needed.
const ENRICH_BATCH: usize = 100;

/// One repository an enrichment query asks about, under the alias the
/// query gives it.
struct EnrichRepo {
    alias: String,
    owner: String,
    name: String,
}

/// One queued PR's slot in an enrichment query: the repo alias carrying
/// it, its own alias, and the search item its answer lands on.
struct EnrichPr {
    repo_alias: String,
    pr_alias: String,
    number: u64,
    item: usize,
}

/// One enrichment query's plan: which repos it asks about under which
/// aliases, and each queued PR's slot within them. The plan is the only
/// place aliases are assigned, so the query built from it and the response
/// read back through it can't drift apart.
struct EnrichPlan {
    /// First-seen order.
    repos: Vec<EnrichRepo>,
    /// Search order.
    prs: Vec<EnrichPr>,
}

/// The enrichment plans for a searched queue: its PRs chunked into batches
/// of [`ENRICH_BATCH`], each batch's repositories aliased `r0…` in
/// first-seen order and each PR aliased `p<number>` under its repo (PR
/// numbers are unique per repository, so aliases can't collide). PRs
/// whose repository isn't a clean `owner/name` sit out the enrichment —
/// their rows keep honest defaults rather than a malformed query killing
/// the batch.
fn enrich_plans(items: &[QueueItem]) -> Vec<EnrichPlan> {
    let mut plans = Vec::new();
    let mut plan = EnrichPlan {
        repos: Vec::new(),
        prs: Vec::new(),
    };
    for (item_ix, item) in items.iter().enumerate() {
        if plan.prs.len() >= ENRICH_BATCH {
            plans.push(plan);
            plan = EnrichPlan {
                repos: Vec::new(),
                prs: Vec::new(),
            };
        }
        let Some((owner, name)) = item.repository.split_once('/') else {
            continue;
        };
        if owner.is_empty() || name.is_empty() {
            continue;
        }
        let alias = match plan
            .repos
            .iter()
            .find(|repo| repo.owner == owner && repo.name == name)
        {
            Some(repo) => repo.alias.clone(),
            None => {
                let alias = format!("r{}", plan.repos.len());
                plan.repos.push(EnrichRepo {
                    alias: alias.clone(),
                    owner: owner.to_string(),
                    name: name.to_string(),
                });
                alias
            }
        };
        plan.prs.push(EnrichPr {
            repo_alias: alias,
            pr_alias: format!("p{}", item.number),
            number: item.number,
            item: item_ix,
        });
    }
    if !plan.prs.is_empty() {
        plans.push(plan);
    }
    plans
}

/// The enrichment query, exactly as it's sent: one aliased repository per
/// repo, one aliased pullRequest per queued PR, each asking for the fields
/// a search can't carry — the check rollup summarized by GitHub itself
/// (`statusCheckRollup { state }`), the review decision, the diff size,
/// the author's avatar, and the viewer's review-request history.
fn enrich_query(plan: &EnrichPlan) -> String {
    let mut query = String::from("query { viewer { login }");
    for repo in &plan.repos {
        query.push_str(&format!(
            " {alias}: repository(owner: \"{owner}\", name: \"{name}\") {{",
            alias = repo.alias,
            owner = repo.owner,
            name = repo.name
        ));
        for pr in &plan.prs {
            if pr.repo_alias == repo.alias {
                query.push_str(&format!(
                    " {alias}: pullRequest(number: {number}) \
                     {{ additions deletions reviewDecision \
                     statusCheckRollup {{ state }} \
                     author {{ avatarUrl(size: 64) }} {requests} {events} }}",
                    alias = pr.pr_alias,
                    number = pr.number,
                    requests = queue_review_requests(None),
                    events = queue_review_events(None),
                ));
            }
        }
        query.push_str(" }");
    }
    query.push_str(" }");
    query
}

fn queue_review_requests(after: Option<&str>) -> String {
    format!("reviewRequests(first:100{}) {{ nodes {{ requestedReviewer {{ __typename ... on User {{ login }} }} }} pageInfo {{ hasNextPage endCursor }} }}", after_arg(after))
}

fn queue_review_events(before: Option<&str>) -> String {
    let before = before
        .map(|cursor| format!(",before:{}", json_string(cursor)))
        .unwrap_or_default();
    format!("timelineItems(last:100,itemTypes:[REVIEW_REQUESTED_EVENT]{before}) {{ nodes {{ ... on ReviewRequestedEvent {{ createdAt requestedReviewer {{ __typename ... on User {{ login }} }} }} }} pageInfo {{ hasPreviousPage startCursor }} }}")
}

fn requested_from_viewer(node: &serde_json::Value, viewer: &str) -> bool {
    let reviewer = &node["requestedReviewer"];
    !viewer.is_empty()
        && reviewer["__typename"] == "User"
        && reviewer["login"]
            .as_str()
            .is_some_and(|login| login.eq_ignore_ascii_case(viewer))
}

/// The fields of the enrichment response the facade consumes. Repo and PR
/// aliases are dynamic keys, so both levels read back as maps: `r0` → its
/// PR nodes, `p4781` → its answer.
#[derive(serde::Deserialize)]
struct EnrichJson {
    /// Absent when the whole query failed; every slot then degrades.
    data: Option<EnrichDataJson>,
}

#[derive(serde::Deserialize)]
struct EnrichDataJson {
    viewer: Option<EnrichViewerJson>,
    #[serde(flatten)]
    repos: HashMap<String, Option<EnrichRepoJson>>,
}

#[derive(serde::Deserialize)]
struct EnrichViewerJson {
    login: String,
}

#[derive(serde::Deserialize)]
struct EnrichRepoJson(HashMap<String, Option<EnrichPrJson>>);

#[derive(serde::Deserialize)]
struct EnrichPrJson {
    additions: u64,
    deletions: u64,
    #[serde(rename = "reviewDecision")]
    review_decision: Option<String>,
    #[serde(rename = "statusCheckRollup")]
    status_check_rollup: Option<EnrichRollupJson>,
    author: Option<EnrichAuthorJson>,
    #[serde(rename = "reviewRequests", default)]
    review_requests: serde_json::Value,
    #[serde(rename = "timelineItems", default)]
    timeline_items: serde_json::Value,
}

#[derive(serde::Deserialize)]
struct EnrichRollupJson {
    state: Option<String>,
}

#[derive(serde::Deserialize)]
struct EnrichAuthorJson {
    #[serde(rename = "avatarUrl")]
    avatar_url: Option<String>,
}

/// Read one batch's answers back onto the items its plan was built from.
/// Every miss — a repo GitHub can't see answers `null` for its alias, a PR
/// that vanished between the search and this query answers `null`, drift
/// degrades the whole payload — leaves the item on its defaults: the
/// queue degrades per PR rather than failing wholesale.
fn apply_enrichment<P: GhProcess>(
    client: &GhCliClient<P>,
    items: &mut [QueueItem],
    plan: &EnrichPlan,
    stdout: &str,
) {
    let Ok(parsed) = serde_json::from_str::<EnrichJson>(stdout) else {
        return;
    };
    let Some(data) = parsed.data else {
        return;
    };
    for slot in &plan.prs {
        let Some(item) = items.get_mut(slot.item) else {
            continue;
        };
        let Some(answer) = data
            .repos
            .get(&slot.repo_alias)
            .and_then(|repo| repo.as_ref())
            .and_then(|repo| repo.0.get(&slot.pr_alias))
            .and_then(|pr| pr.as_ref())
        else {
            continue;
        };
        item.additions = Some(answer.additions);
        item.deletions = Some(answer.deletions);
        item.review_state = ReviewState::from_decision(answer.review_decision.as_deref());
        item.ci = CiState::from_rollup_state(
            answer
                .status_check_rollup
                .as_ref()
                .and_then(|rollup| rollup.state.as_deref()),
        );
        item.author_avatar_url = answer
            .author
            .as_ref()
            .and_then(|author| author.avatar_url.clone());
        if let Some(viewer) = &data.viewer {
            item.review_requested_at = client
                .queue_review_requested_at(
                    item,
                    &viewer.login,
                    answer.review_requests.clone(),
                    answer.timeline_items.clone(),
                )
                .ok()
                .flatten();
        }
    }
}

/// The exact `gh` invocation a queue search compiles to. The search string
/// becomes one argument *per qualifier* — a single quoted string is
/// keyword-searched, not qualifier-parsed — chip qualifiers included,
/// followed by the supported server sort; logical ordering runs after
/// enrichment. The query is owned (chips compile into it),
/// so the arguments are too.
fn search_prs_args(search: &QueueSearch) -> Vec<String> {
    // GitHub has no review-request-time sort. Fetch oldest-created first
    // for both modes, then apply the logical order after enrichment.
    let (sort, order) = ("created", "asc");
    let mut args: Vec<String> = vec!["search".into(), "prs".into()];
    args.extend(search.query().split_whitespace().map(str::to_string));
    args.extend(
        [
            "--sort",
            sort,
            "--order",
            order,
            "--limit",
            SEARCH_LIMIT,
            "--json",
            QUEUE_JSON_FIELDS,
        ]
        .map(str::to_string),
    );
    args
}

/// A `gh` that couldn't even start. Only a missing binary means "no
/// credential source"; anything else (permissions, exec format) is a
/// reportable failure of an operation the user believed was possible.
fn spawn_failure(err: io::Error) -> ClientError {
    match err.kind() {
        io::ErrorKind::NotFound => ClientError::NoCredentialSource,
        _ => ClientError::Failed(format!("couldn't run the GitHub CLI: {err}")),
    }
}

/// Why a failed `gh` invocation means GitHub isn't usable. "Signed out" is
/// recognizable three ways, any suffices: gh's documented auth exit code 4,
/// its `gh auth login` hint on stderr, or a rejected token — which reports
/// itself as HTTP 401, spelled one way by `gh api` (`gh: Bad credentials
/// (HTTP 401)`) and another by `gh search` (`… 401 Unauthorized body: …`).
/// Anything else keeps gh's own cause.
fn classify_failure(out: &GhOutput) -> ClientError {
    if out.code == Some(4)
        || out.stderr.contains("gh auth login")
        || out.stderr.contains("HTTP 401")
        || out.stderr.contains("401 Unauthorized")
    {
        return ClientError::Unauthenticated;
    }
    ClientError::Failed(cause(out))
}

/// gh's first stderr line as a short human cause: the `gh: ` brand stripped,
/// trailing punctuation dropped, so it drops cleanly into a sentence.
fn cause(out: &GhOutput) -> String {
    out.stderr
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(|line| {
            line.strip_prefix("gh: ")
                .unwrap_or(line)
                .trim_end_matches('.')
                .to_string()
        })
        .unwrap_or_else(|| match out.code {
            Some(code) => format!("exit code {code}"),
            None => "gh exited abnormally".to_string(),
        })
}

/// The fields of `GET /user`'s payload the facade consumes; every other field
/// gh emits is ignored, so the full recorded shape stays a valid fixture.
#[derive(serde::Deserialize)]
struct UserJson {
    login: String,
    avatar_url: String,
}

/// Recorded `gh` JSON → [`Viewer`]. A payload that stops looking like the
/// user endpoint degrades to a `Failed`, never a panic: a drifting API shape
/// must surface as a message, not a crash.
fn parse_viewer(stdout: &str) -> Result<Viewer, ClientError> {
    serde_json::from_str::<UserJson>(stdout)
        .map(|user| Viewer {
            login: user.login,
            avatar_url: user.avatar_url,
        })
        .map_err(|err| ClientError::Failed(format!("unexpected GitHub response: {err}")))
}

/// The fields of one `gh search prs` hit the queue consumes; every other
/// field gh emits (the author's account type, the repository's short
/// name, …) is ignored, so the full recorded shape stays a valid fixture.
#[derive(serde::Deserialize)]
struct QueueItemJson {
    number: u64,
    title: String,
    author: AuthorJson,
    repository: RepositoryJson,
    #[serde(rename = "isDraft")]
    is_draft: bool,
    #[serde(rename = "createdAt")]
    created_at: String,
    #[serde(rename = "updatedAt")]
    updated_at: String,
    /// `default`: a gh that stops emitting the count degrades to zero
    /// rather than failing the whole queue over one field.
    #[serde(rename = "commentsCount", default)]
    comments: u64,
    #[serde(default)]
    labels: Vec<LabelJson>,
}

#[derive(serde::Deserialize)]
struct LabelJson {
    name: String,
    color: String,
}

#[derive(serde::Deserialize)]
struct AuthorJson {
    login: String,
}

#[derive(serde::Deserialize)]
struct RepositoryJson {
    #[serde(rename = "nameWithOwner")]
    name_with_owner: String,
}

/// Recorded `gh search prs` JSON → queue rows, in gh's order (the compiled
/// sort already ran server-side). A payload that stops looking like search
/// output degrades to a `Failed`, never a panic. The check-sized fields
/// start on their honest defaults — awaiting review, no checks, no sizes,
/// no avatar — for the enrichment pass to overwrite, and to keep forever
/// for any PR it misses.
fn parse_queue(stdout: &str) -> Result<Vec<QueueItem>, ClientError> {
    serde_json::from_str::<Vec<QueueItemJson>>(stdout)
        .map(|hits| {
            hits.into_iter()
                .map(|hit| QueueItem {
                    number: hit.number,
                    title: hit.title,
                    author: hit.author.login,
                    author_avatar_url: None,
                    repository: hit.repository.name_with_owner,
                    is_draft: hit.is_draft,
                    review_state: ReviewState::Awaiting,
                    ci: CiState::NoChecks,
                    additions: None,
                    deletions: None,
                    comments: hit.comments,
                    labels: hit
                        .labels
                        .into_iter()
                        .map(|label| QueueLabel {
                            name: label.name,
                            color: label.color,
                        })
                        .collect(),
                    created_at: hit.created_at,
                    review_requested_at: None,
                    updated_at: hit.updated_at,
                })
                .collect()
        })
        .map_err(|err| ClientError::Failed(format!("unexpected GitHub response: {err}")))
}

/// Never page past this many mentionable users; big repos can have
/// thousands, and the autocomplete only needs a workable pool of the most
/// relevant.
const MAX_MENTIONABLE: usize = 500;

/// Blob-size cap: PR review never needs multi-megabyte files, and the raw
/// contents API happily serves up to 100 MB.
const MAX_BLOB_BYTES: usize = 1024 * 1024;

/// Mark a negative cache entry (absent / binary / oversize) with an
/// `.absent` sidecar, so the miss is remembered too.
fn mark_absent(cache: Option<PathBuf>) {
    if let Some(cache) = cache {
        let _ = std::fs::write(cache.with_extension("absent"), b"");
    }
}

/// `~/.cache/gander/blobs/<key>`, creating the directory; None when HOME is
/// unset or the directory can't be created (cache disabled, fetch still
/// works).
fn cache_path(repo: &str, oid: &str, path: &str) -> Option<PathBuf> {
    let dir = PathBuf::from(std::env::var_os("HOME")?)
        .join(".cache")
        .join("gander")
        .join("blobs");
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir.join(cache_key(repo, oid, path)))
}

/// Stable cache key: hex sha256 of `repo\0oid\0path`.
fn cache_key(repo: &str, oid: &str, path: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(repo.as_bytes());
    hasher.update([0]);
    hasher.update(oid.as_bytes());
    hasher.update([0]);
    hasher.update(path.as_bytes());
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Percent-encode a repo path for the contents API, keeping `/` separators:
/// every byte outside RFC 3986 unreserved is encoded, per segment.
fn encode_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for byte in path.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{QueueFilter, QueueSort};
    use std::collections::VecDeque;
    use std::sync::Mutex;

    #[test]
    fn complete_discussion_paginates_all_connections_and_keeps_published_metadata() {
        use serde_json::json;
        let connection = |nodes: Vec<serde_json::Value>, cursor: Option<&str>| {
            json!({
                "nodes": nodes, "pageInfo": {"hasNextPage": cursor.is_some(), "endCursor": cursor}
            })
        };
        let page = |name: &str, conn: serde_json::Value| {
            json!({"data": {"repository": {"pullRequest": {
                "headRefOid": "head", "baseRefOid": "base", name: conn
            }}}})
            .to_string()
        };
        let comment = |id: u64| {
            json!({"id": id.to_string(), "body": "**hello**", "createdAt": "2026-01-01T00:00:00Z",
                "author": null, "reactionGroups": [
                    {"content": "HEART", "users": {"totalCount": 4}},
                    {"content": "EYES", "users": {"totalCount": 0}}
                ], "pullRequestReview": {"id": "R9", "state": "COMMENTED"},
                "diffHunk": "@@ -40,3 +40,0 @@\n-old one\n-old two\n-old three"
            })
        };
        let mut review = comment(9);
        review["state"] = json!("APPROVED");
        review["node_id"] = json!("R9");
        review["body"] = json!("");
        review["submittedAt"] = json!("2026-02-01T00:00:00Z");
        let mut pending = review.clone();
        pending["state"] = json!("PENDING");
        pending["submittedAt"] = json!(null);
        let thread = |id: &str, comments: serde_json::Value| {
            json!({
                "id": id, "path": "src/main.rs", "side": "LEFT", "line": null, "start_line": null,
                "isResolved": true, "isOutdated": true, "originalLine": 42, "originalStartLine": 40,
                "viewerCanResolve": false, "viewerCanUnresolve": true,
                "comments": comments
            })
        };
        let mut draft = comment(8);
        draft["pullRequestReview"]["state"] = json!("PENDING");
        let responses = [
            page(
                "comments",
                connection(vec![comment(u64::MAX)], Some("main-next")),
            ),
            page("comments", connection(vec![comment(2)], None)),
            page("reviews", connection(vec![pending], Some("reviews-next"))),
            page("reviews", connection(vec![review], None)),
            page(
                "reviewThreads",
                connection(
                    vec![thread(
                        "T1",
                        connection(vec![comment(3)], Some("reply-next")),
                    )],
                    Some("threads-next"),
                ),
            ),
            json!({"data": {"node": thread("T1", connection(vec![comment(4)], None))}}).to_string(),
            page(
                "reviewThreads",
                connection(
                    vec![thread("T2", connection(vec![comment(5), draft], None))],
                    None,
                ),
            ),
        ];
        let refs: Vec<_> = responses.iter().map(String::as_str).collect();
        let (client, calls) = session_script(&refs);
        let discussion = client.pr_discussion(&loc()).unwrap();
        assert_eq!(discussion.comments.len(), 3);
        assert_eq!(discussion.comments[0].id, u64::MAX);
        assert_eq!(discussion.comments[0].author, "ghost");
        assert_eq!(
            discussion.comments[0].reactions,
            vec![crate::Reaction {
                content: "HEART".into(),
                count: 4
            }]
        );
        assert_eq!(discussion.comments[2].created_at, "2026-02-01T00:00:00Z");
        assert_eq!(
            discussion.comments[2].review_state.as_deref(),
            Some("APPROVED")
        );
        assert!(discussion.comments[2].body.is_empty());
        assert_eq!(discussion.comments[2].review_id.as_deref(), Some("R9"));
        assert_eq!(discussion.threads.len(), 2);
        assert_eq!(
            discussion.threads[0]
                .comments
                .iter()
                .map(|c| c.id)
                .collect::<Vec<_>>(),
            vec![3, 4]
        );
        assert_eq!(
            discussion.threads[1].comments.len(),
            1,
            "no pending comments in the published timeline"
        );
        assert!(discussion.threads[0].is_resolved);
        assert!(!discussion.threads[0].viewer_can_resolve);
        assert!(discussion.threads[0].viewer_can_unresolve);
        assert_eq!(discussion.threads[0].original_line, Some(42));
        assert_eq!(
            discussion.threads[0].comments[0].review_id.as_deref(),
            Some("R9")
        );
        assert_eq!(
            discussion.threads[0].diff_hunk,
            "@@ -40,3 +40,0 @@\n-old one\n-old two\n-old three"
        );
        let flat = discussion.review_comments();
        assert_eq!(flat.len(), 3);
        assert_eq!(
            flat[0].line, None,
            "historical anchors never reattach to current code"
        );
        assert_eq!(flat[1].in_reply_to_id, Some(3));
        let calls = calls.lock().unwrap();
        assert!(calls[2].join(" ").contains("node_id:id"));
        assert!(calls[4].join(" ").contains("diffHunk"));
        assert!(calls[4]
            .join(" ")
            .contains("viewerCanResolve viewerCanUnresolve"));
        for (ix, cursor) in [
            (1, "main-next"),
            (3, "reviews-next"),
            (5, "reply-next"),
            (6, "threads-next"),
        ] {
            assert!(calls[ix].join(" ").contains(cursor));
        }
        assert!(calls
            .iter()
            .all(|call| !call.join(" ").contains("mutation")));
    }

    #[test]
    fn outside_diff_discussion_previews_use_captured_blobs_and_survive_blob_errors() {
        use serde_json::json;
        let unique = tempfile::tempdir().unwrap();
        let path = format!(
            "outside-{}/main.rs",
            unique.path().file_name().unwrap().to_str().unwrap()
        );
        let connection =
            |nodes| json!({"nodes": nodes, "pageInfo": {"hasNextPage": false, "endCursor": null}});
        let page = |name: &str, nodes| {
            json!({"data": {"repository": {"pullRequest": {
                "headRefOid": "current-head", "baseRefOid": "current-base", name: connection(nodes)
            }}}})
            .to_string()
        };
        let thread = |id: u64, side, position| {
            json!({
                "id": format!("T{id}"), "path": path, "side": side,
                "line": null, "start_line": null, "originalLine": 6, "originalStartLine": null,
                "isResolved": false, "isOutdated": true, "positioning": position,
                "comments": connection(vec![json!({"id": id, "body": "outside diff", "createdAt": "2026-10-24T00:00:00Z",
                    "diffHunk": "", "pullRequestReview": {"id": "R1", "state": "COMMENTED"}})])
            })
        };
        let left = json!({"__typename": "LineComment", "line": 6, "commitOid": "captured-left", "headCommitOid": "old-head", "baseCommitOid": "old-base"});
        let right = json!({"__typename": "MultilineComment", "startLine": 4, "endLine": 6,
            "startCommitOid": "captured-right", "endCommitOid": "captured-right", "headCommitOid": "old-head"});
        let missing = json!({"__typename": "LineComment", "line": 6, "commitOid": "inaccessible"});
        let main = page("comments", vec![]);
        let reviews = page("reviews", vec![]);
        let threads = page(
            "reviewThreads",
            vec![
                thread(1, "LEFT", left.clone()),
                thread(2, "RIGHT", right),
                thread(3, "LEFT", left),
                thread(4, "LEFT", missing.clone()),
                thread(5, "LEFT", missing),
            ],
        );
        let fake = FakeGh::many(vec![
            ok(Some(0), &main, ""),
            ok(Some(0), &reviews, ""),
            ok(Some(0), &threads, ""),
            ok(
                Some(0),
                "old 1\nold 2\nold 3\nold 4\nold 5\nold 6\nold 7\n",
                "",
            ),
            ok(
                Some(0),
                "new 1\nnew 2\nnew 3\nnew 4\nnew 5\nnew 6\nnew 7\n",
                "",
            ),
            ok(Some(1), "", "gh: Forbidden (HTTP 403)"),
        ]);
        let calls = fake.calls_handle();
        let discussion = GhCliClient::with_process(fake)
            .pr_discussion(&loc())
            .unwrap();
        assert_eq!(discussion.threads.len(), 5);
        let left = discussion.threads[0].code_context.as_ref().unwrap();
        assert_eq!(left.commit_id, "captured-left");
        assert_eq!(left.first_line, 3);
        assert_eq!(left.lines, ["old 3", "old 4", "old 5", "old 6"]);
        assert!(
            discussion.threads[0].line.is_none(),
            "stale previews never become current diff anchors"
        );
        let right = discussion.threads[1].code_context.as_ref().unwrap();
        assert_eq!(right.commit_id, "captured-right");
        assert_eq!(right.first_line, 1);
        assert_eq!(right.start_line, Some(4));
        assert_eq!(right.lines.last().unwrap(), "new 6");
        assert_eq!(discussion.threads[2].code_context.as_ref(), Some(left));
        assert!(discussion.threads[3..]
            .iter()
            .all(|t| t.code_context.is_none()));
        assert_eq!(
            serde_json::from_value::<crate::PrDiscussion>(
                serde_json::to_value(&discussion).unwrap()
            )
            .unwrap(),
            discussion
        );
        let calls = calls.lock().unwrap();
        assert_eq!(
            calls.len(),
            6,
            "one request per captured blob, including failures"
        );
        assert!(calls[3].last().unwrap().ends_with("?ref=captured-left"));
        assert!(calls[4].last().unwrap().ends_with("?ref=captured-right"));
        assert!(calls[5].last().unwrap().ends_with("?ref=inaccessible"));
    }

    #[test]
    fn captured_code_positions_do_not_guess_missing_or_cross_commit_anchors() {
        use serde_json::json;
        for position in [
            json!(null),
            json!({"__typename": "LineComment", "line": 88}),
            json!({"__typename": "LineComment", "line": 0, "commitOid": "old"}),
            json!({"__typename": "MultilineComment", "startLine": 3, "endLine": 4, "startCommitOid": "old", "endCommitOid": "new"}),
        ] {
            assert!(captured_code_position(&json!({"positioning": position})).is_none());
        }
    }

    #[test]
    fn resolution_mutations_use_thread_identity_and_never_touch_pending_reviews() {
        let id = "T\"\\\n1";
        let replies: Vec<_> = [
            ("resolveReviewThread", true),
            ("unresolveReviewThread", false),
        ]
        .into_iter()
        .map(|(operation, resolved)| {
            serde_json::json!({
                "data": {operation: {"thread": {"id": id, "isResolved": resolved}}}
            })
            .to_string()
        })
        .collect();
        let (client, calls) =
            session_script(&replies.iter().map(String::as_str).collect::<Vec<_>>());
        let mut session = crate::review_session::ReviewSession::default();
        for resolved in [true, false] {
            session
                .execute(
                    &client,
                    &loc(),
                    crate::review_session::Command::SetThreadResolved {
                        thread_id: id.into(),
                        resolved,
                    },
                )
                .unwrap();
        }
        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        for (call, operation) in calls
            .iter()
            .zip(["resolveReviewThread", "unresolveReviewThread"])
        {
            assert_eq!(call[3], format!(
                "query=mutation {{ {operation}(input:{{threadId:{}}}) {{ thread {{ id isResolved }} }} }}", json_string(id)
            ));
        }
    }

    #[test]
    fn resolution_errors_and_missing_or_mismatched_results_are_not_success() {
        for response in [
            r#"{"errors":[{"message":"not permitted"}]}"#,
            r#"{"data":{"resolveReviewThread":{"thread":null}}}"#,
            r#"{"data":{"resolveReviewThread":{"thread":{"id":"other","isResolved":true}}}}"#,
            r#"{"data":{"resolveReviewThread":{"thread":{"id":"T1","isResolved":false}}}}"#,
        ] {
            let (client, _) = session_script(&[response]);
            assert!(client.set_thread_resolved("T1", true).is_err());
        }
        let (empty_client, calls) = session_script(&[]);
        assert!(matches!(
            empty_client.set_thread_resolved("", true),
            Err(ClientError::Invalid(_))
        ));
        assert!(calls.lock().unwrap().is_empty());
        assert!(client(ok(Some(1), "", "network unavailable"))
            .set_thread_resolved("T1", false)
            .is_err());
    }

    #[test]
    fn discussion_errors_do_not_silently_turn_into_empty_conversations() {
        let repeated = r#"{"data":{"repository":{"pullRequest":{"comments":{"nodes":[],"pageInfo":{"hasNextPage":true,"endCursor":"same"}}}}}}"#;
        let (client, _) = session_script(&[repeated, repeated]);
        assert!(client
            .pr_discussion(&loc())
            .unwrap_err()
            .to_string()
            .contains("repeated"));
        let (client, _) = session_script(&[r#"{"data":{"repository":{"pullRequest":null}}}"#]);
        assert!(client.pr_discussion(&loc()).is_err());
        let (client, _) = session_script(&[r#"{"errors":[{"message":"rate limited"}]}"#]);
        assert!(client
            .pr_discussion(&loc())
            .unwrap_err()
            .to_string()
            .contains("rate limited"));
    }

    #[test]
    fn review_changes_uses_paginated_reviews_and_raw_diff_despite_capped_json_files() {
        let base = "a".repeat(40);
        let head = "b".repeat(40);
        let reviews = serde_json::json!([[{"id":1,"user":{"login":"me"},"state":"APPROVED","submitted_at":"2026-03-01T00:00:00Z","commit_id":base}]]).to_string();
        // The metadata listing is capped, but is not our diff source. A
        // base-branch merge can make this much larger than the PR's own diff.
        let comparison = serde_json::json!({"status":"ahead","merge_base_commit":{"sha":base},"files":vec![serde_json::json!({});300]}).to_string();
        let patch: String = (0..326).map(|ix| format!(
            "diff --git a/file-{ix} b/file-{ix}\n--- a/file-{ix}\n+++ b/file-{ix}\n@@ -1 +1 @@\n-old\n+new\n"
        )).collect();
        let fake = FakeGh::many(vec![
            ok(Some(0), r#"{"login":"me","avatar_url":""}"#, ""),
            ok(Some(0), &reviews, ""),
            ok(Some(0), &comparison, ""),
            ok(Some(0), &comparison, ""),
            ok(Some(0), &comparison, ""),
            ok(Some(0), &patch, ""),
        ]);
        let calls = fake.calls_handle();
        let client = GhCliClient::with_process(fake);
        let loc = PrLocator {
            owner: "acme".into(),
            repo: "widgets".into(),
            number: 7,
        };
        let changes = client.review_changes(&loc, &head).unwrap();
        assert_eq!(changes.baseline().unwrap().commit, base);
        let upstream = "c".repeat(40);
        let received = client
            .comparison_patch(&loc, &base, &head, &upstream)
            .unwrap();
        assert_eq!(received.patch, patch);
        assert!(received.base_files.is_none());
        assert_eq!(
            received
                .patch
                .lines()
                .filter(|line| line.starts_with("diff --git "))
                .count(),
            326
        );
        let calls = calls.lock().unwrap();
        assert_eq!(
            calls[1],
            vec![
                "api",
                "--paginate",
                "--slurp",
                "repos/acme/widgets/pulls/7/reviews?per_page=100"
            ]
        );
        assert_eq!(
            calls[2],
            vec![
                "api".to_string(),
                format!("repos/acme/widgets/compare/{base}...{head}?per_page=1")
            ]
        );
        assert_eq!(
            calls[3],
            vec![
                "api".to_string(),
                format!("repos/acme/widgets/compare/{upstream}...{base}?per_page=1")
            ]
        );
        assert_eq!(
            calls[4],
            vec![
                "api".to_string(),
                format!("repos/acme/widgets/compare/{upstream}...{head}?per_page=1")
            ]
        );
        assert_eq!(
            calls[5],
            vec![
                "api".to_string(),
                format!("repos/acme/widgets/compare/{base}...{head}"),
                "-H".into(),
                "Accept: application/vnd.github.diff".into()
            ]
        );
    }

    #[test]
    fn review_changes_no_review_and_current_head_do_not_compare() {
        let sha = "a".repeat(40);
        let loc = PrLocator {
            owner: "acme".into(),
            repo: "widgets".into(),
            number: 7,
        };
        for reviewed in [false, true] {
            let reviews = if reviewed {
                serde_json::json!([[{"id":1,"user":{"login":"me"},"state":"COMMENTED","submitted_at":"2026-03-01T00:00:00Z","commit_id":sha}]]).to_string()
            } else {
                "[[]]".into()
            };
            let client = GhCliClient::with_process(FakeGh::many(vec![
                ok(Some(0), r#"{"login":"me","avatar_url":""}"#, ""),
                ok(Some(0), &reviews, ""),
            ]));
            assert_eq!(
                client
                    .review_changes(&loc, &sha)
                    .unwrap()
                    .baseline()
                    .is_some(),
                reviewed
            );
            assert_eq!(
                client
                    .comparison_patch(&loc, &sha, &sha, &sha)
                    .unwrap()
                    .patch,
                ""
            );
        }
    }

    #[test]
    fn review_comparison_errors_are_not_empty_or_fabricated_rebases() {
        let base = "a".repeat(40);
        let head = "b".repeat(40);
        let loc = PrLocator {
            owner: "acme".into(),
            repo: "widgets".into(),
            number: 7,
        };
        let reviews = serde_json::json!([[{"id":1,"user":{"login":"me"},"state":"APPROVED","submitted_at":"2026-03-01T00:00:00Z","commit_id":base}]]).to_string();
        let client = GhCliClient::with_process(FakeGh::many(vec![
            ok(Some(0), r#"{"login":"me","avatar_url":""}"#, ""),
            ok(Some(0), &reviews, ""),
            ok(Some(1), "", "network unavailable"),
        ]));
        assert!(client
            .review_changes(&loc, &head)
            .unwrap_err()
            .to_string()
            .contains("network unavailable"));
        let missing = GhCliClient::with_process(FakeGh::many(vec![
            ok(Some(0), r#"{"login":"me","avatar_url":""}"#, ""),
            ok(Some(0), &reviews, ""),
            ok(Some(1), "", "gh: Not Found (HTTP 404)"),
        ]));
        let crate::ReviewChanges::Unavailable(reason) =
            missing.review_changes(&loc, &head).unwrap()
        else {
            panic!()
        };
        assert!(reason.contains("rebase or force-push"));
        let failed = GhCliClient::with_process(FakeGh::one(ok(Some(1), "", "diff unavailable")));
        assert!(failed.comparison_patch(&loc, &base, &head, &base).is_err());
        assert!(failed
            .comparison_patch(&loc, "--invalid", &head, &base)
            .is_err());
        for response in [r#"{}"#, r#"{"merge_base_commit":{"sha":"--invalid"}}"#] {
            let client = GhCliClient::with_process(FakeGh::one(ok(Some(0), response, "")));
            assert!(client.comparison_patch(&loc, &base, &head, &base).is_err());
        }
    }

    /// Compare an authenticated web view's exported per-file counts against
    /// the production path. Inputs stay external (including private PR data).
    #[test]
    #[ignore = "requires gh authentication, network, and GANDER_COMPARE_* inputs"]
    fn real_upstream_aware_comparison_matches_web_file_counts() {
        let input = |name: &str| std::env::var(format!("GANDER_COMPARE_{name}")).expect(name);
        let client = GhCliClient::new();
        let loc = client.resolve_pr(&input("PR")).unwrap();
        let comparison = client
            .comparison_patch(&loc, &input("REVIEWED"), &input("HEAD"), &input("UPSTREAM"))
            .unwrap();
        let expected: std::collections::BTreeMap<String, (u32, u32)> =
            serde_json::from_slice(&std::fs::read(input("EXPECTED")).unwrap()).unwrap();
        let actual: std::collections::BTreeMap<_, _> = diff_core::parse_patch(&comparison.patch)
            .files
            .into_iter()
            .map(|file| {
                (
                    file.display_path().to_owned(),
                    (file.additions, file.deletions),
                )
            })
            .collect();
        assert_eq!(actual, expected);
        let files = comparison
            .base_files
            .expect("exercise an upstream-adjusted baseline");
        for base in files.values() {
            assert_eq!(base.text.lines().count(), base.reviewed_lines.len());
        }
    }

    /// Each invocation pops the next recorded outcome and records argv;
    /// an invocation beyond the script fails the test.
    struct FakeGh {
        results: Mutex<VecDeque<Result<GhOutput, io::Error>>>,
        calls: std::sync::Arc<Mutex<Vec<Vec<String>>>>,
    }

    impl FakeGh {
        fn one(result: Result<GhOutput, io::Error>) -> Self {
            Self::many(vec![result])
        }

        /// Every scripted invocation, in order: search, then its enrichment
        /// batches. As with `one`, an invocation beyond the script fails the
        /// test — a stray subprocess is a behavior change worth noticing.
        fn many(results: Vec<Result<GhOutput, io::Error>>) -> Self {
            Self {
                results: Mutex::new(VecDeque::from(results)),
                calls: std::sync::Arc::new(Mutex::new(Vec::new())),
            }
        }

        /// The argv of every invocation so far, in order.
        fn calls_handle(&self) -> std::sync::Arc<Mutex<Vec<Vec<String>>>> {
            self.calls.clone()
        }
    }

    impl GhProcess for FakeGh {
        fn run(&self, args: &[&str]) -> io::Result<GhOutput> {
            self.calls
                .lock()
                .unwrap()
                .push(args.iter().map(|arg| arg.to_string()).collect());
            self.results
                .lock()
                .unwrap()
                .pop_front()
                .expect("gh was invoked more often than the test scripted")
        }
    }

    fn client(result: Result<GhOutput, io::Error>) -> GhCliClient<FakeGh> {
        GhCliClient::with_process(FakeGh::one(result))
    }

    /// A client scripting a search followed by its enrichment batches.
    fn rich_client(
        search: Result<GhOutput, io::Error>,
        enrich: Vec<Result<GhOutput, io::Error>>,
    ) -> (GhCliClient<FakeGh>, std::sync::Arc<Mutex<Vec<Vec<String>>>>) {
        let fake = FakeGh::many([search].into_iter().chain(enrich).collect());
        let calls = fake.calls_handle();
        (GhCliClient::with_process(fake), calls)
    }

    fn ok(code: Option<i32>, stdout: &str, stderr: &str) -> Result<GhOutput, io::Error> {
        Ok(GhOutput {
            code,
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.to_string(),
        })
    }

    /// `ok`, for payloads that aren't text — file contents can be any bytes.
    fn raw(code: Option<i32>, stdout: &[u8], stderr: &str) -> Result<GhOutput, io::Error> {
        Ok(GhOutput {
            code,
            stdout: stdout.to_vec(),
            stderr: stderr.to_string(),
        })
    }

    /// The PR every PR-shaped fixture below addresses.
    fn loc() -> PrLocator {
        PrLocator {
            owner: "octocat".into(),
            repo: "Hello-World".into(),
            number: 4781,
        }
    }

    const VIEWED_PAGE1: &str = include_str!("../fixtures/gh_graphql_viewed_files_page1.json");
    const VIEWED_PAGE2: &str = include_str!("../fixtures/gh_graphql_viewed_files_page2.json");

    #[test]
    fn viewed_files_fetches_all_pages_and_preserves_per_viewer_states() {
        use crate::FileViewedState::*;
        let fake = FakeGh::many(vec![
            ok(Some(0), VIEWED_PAGE1, ""),
            ok(Some(0), VIEWED_PAGE2, ""),
        ]);
        let calls = fake.calls_handle();
        let files = GhCliClient::with_process(fake)
            .viewed_files(&loc())
            .unwrap();
        assert_eq!(files.pull_request_id, "PR_kwDOExample");
        assert_eq!(
            files.files,
            HashMap::from([
                ("src/viewed.rs".into(), Viewed),
                ("old-name.txt".into(), Unviewed),
                ("src/new name\"λ.rs".into(), Dismissed),
            ])
        );
        let calls = calls.lock().unwrap();
        assert_eq!(&calls[0][..3], ["api", "graphql", "-f"]);
        assert_eq!(calls[0][3], "query=query { repository(owner:\"octocat\",name:\"Hello-World\") { pullRequest(number:4781) { id files(first:100) { nodes { path viewerViewedState } pageInfo { hasNextPage endCursor } } } } }");
        assert!(calls[1][3].contains("files(first:100,after:\"files-next\")"));
        assert!(calls.iter().all(|call| !call[3].contains("mutation")));
    }

    #[test]
    fn viewed_mutations_mark_and_unmark_exact_paths_and_round_trip() {
        let fake = FakeGh::many(vec![
            ok(
                Some(0),
                include_str!("../fixtures/gh_graphql_mark_file_viewed.json"),
                "",
            ),
            ok(
                Some(0),
                include_str!("../fixtures/gh_graphql_unmark_file_viewed.json"),
                "",
            ),
            ok(Some(0), VIEWED_PAGE1, ""),
            ok(Some(0), VIEWED_PAGE2, ""),
        ]);
        let calls = fake.calls_handle();
        let client = GhCliClient::with_process(fake);
        let path = "src/new name\"λ.rs";
        client
            .set_file_viewed("PR_kwDOExample", path, true)
            .unwrap();
        client
            .set_file_viewed("PR_kwDOExample", path, false)
            .unwrap();
        assert_eq!(
            client.viewed_files(&loc()).unwrap().files[path],
            crate::FileViewedState::Dismissed
        );
        let calls = calls.lock().unwrap();
        for (ix, operation) in ["markFileAsViewed", "unmarkFileAsViewed"]
            .iter()
            .enumerate()
        {
            assert_eq!(&calls[ix][..3], ["api", "graphql", "-f"]);
            assert_eq!(calls[ix][3], format!("query=mutation {{ {operation}(input:{{pullRequestId:\"PR_kwDOExample\",path:\"src/new name\\\"λ.rs\"}}) {{ pullRequest {{ id }} }} }}"));
        }
    }

    #[test]
    fn viewed_state_errors_are_never_silently_empty_or_successful() {
        for response in [
            r#"{"errors":[{"message":"Forbidden"}],"data":{}}"#,
            r#"{"data":{"repository":{"pullRequest":null}}}"#,
            r#"{"data":{"repository":{"pullRequest":{"id":"PR1","files":{"nodes":[{"path":"a","viewerViewedState":"UNKNOWN"}],"pageInfo":{"hasNextPage":false}}}}}}"#,
        ] {
            assert!(client(ok(Some(0), response, ""))
                .viewed_files(&loc())
                .is_err());
        }
        for response in [
            "{}",
            r#"{"data":{"markFileAsViewed":null}}"#,
            r#"{"data":{"markFileAsViewed":{"pullRequest":{"id":"wrong"}}}}"#,
            r#"{"errors":[{"message":"Forbidden"}]}"#,
        ] {
            assert!(client(ok(Some(0), response, ""))
                .set_file_viewed("PR1", "a", true)
                .is_err());
        }
        assert_eq!(
            client(ok(Some(4), "", UNAUTHENTICATED)).set_file_viewed("PR1", "a", false),
            Err(ClientError::Unauthenticated)
        );
        let repeated = FakeGh::many(vec![
            ok(Some(0), VIEWED_PAGE1, ""),
            ok(Some(0), VIEWED_PAGE1, ""),
        ]);
        assert!(GhCliClient::with_process(repeated)
            .viewed_files(&loc())
            .is_err());
    }

    /// The real `gh api user` payload (captured from gh 2.102, values swapped
    /// for the octocat example account): every field the CLI emits is kept,
    /// so parsing has to tolerate them all.
    const SUCCESS: &str = include_str!("../fixtures/gh_api_user_success.json");
    /// gh's signed-out stderr (captured: empty config, exit code 4).
    const UNAUTHENTICATED: &str = include_str!("../fixtures/gh_api_user_unauthenticated.txt");
    /// A rejected token (captured: bogus GH_TOKEN, exit code 1).
    const BAD_TOKEN: &str = include_str!("../fixtures/gh_api_user_bad_token.txt");
    /// A connection failure (captured: unresolvable host, exit code 1) — the
    /// shape gh prints whenever it can't reach its API.
    const NETWORK_DOWN: &str = include_str!("../fixtures/gh_api_user_network_down.txt");

    /// The real `gh search prs` payload for the queue search (captured from
    /// gh 2.102, values swapped for GitHub's example accounts): every field
    /// the CLI emits is kept, so parsing has to tolerate them all.
    const QUEUE_SUCCESS: &str = include_str!("../fixtures/gh_search_prs_success.json");
    /// A queue with nothing awaiting review (captured: gh prints `[]`).
    const QUEUE_EMPTY: &str = include_str!("../fixtures/gh_search_prs_empty.json");
    /// gh's signed-out stderr for a search (captured: empty config, exit 4).
    const SEARCH_SIGNED_OUT: &str = include_str!("../fixtures/gh_search_prs_signed_out.txt");
    /// A rejected token (captured: bogus GH_TOKEN, exit 1). Note the 401
    /// wording differs from `gh api`'s (see `SEARCH_BAD_TOKEN` vs
    /// `BAD_TOKEN`).
    const SEARCH_BAD_TOKEN: &str = include_str!("../fixtures/gh_search_prs_bad_token.txt");
    /// The real `gh api graphql` enrichment response for the fixture queue
    /// (captured from gh 2.102, values swapped for the example accounts):
    /// one repo-alias per fixture repo, one PR-alias per queued PR, each
    /// carrying the check-sized fields a search can't.
    const ENRICH_SUCCESS: &str = include_str!("../fixtures/gh_api_graphql_enrich_success.json");
    /// The gone-PR shapes GitHub really answers with when a repo or PR
    /// vanishes between the search and the enrichment: `null` for the
    /// repo's alias, `null` for the PR's.
    const ENRICH_GONE: &str = include_str!("../fixtures/gh_api_graphql_enrich_gone.json");

    /// The `gh pr view --json` payload for the fixture PR, exactly as the
    /// CLI projects it: one key per requested field, camelCase — the shape
    /// the golden invocation below asks for, kept whole so parsing has to
    /// tolerate it all.
    const PR_VIEW_SUCCESS: &str = include_str!("../fixtures/gh_pr_view_success.json");
    /// The `gh pr list --json` payload for the PR picker, exactly as the
    /// CLI projects it — two open PRs, one a draft, newest first.
    /// The `gh api --paginate --slurp` pulls-comments payload: one array
    /// per page — a top-level comment, its reply, and an outdated comment
    /// with null anchors — snake_case, as the REST API returns it.
    const PULLS_COMMENTS_SUCCESS: &str =
        include_str!("../fixtures/gh_api_pulls_comments_success.json");

    #[test]
    fn viewer_parses_the_recorded_user_payload() {
        let viewer = client(ok(Some(0), SUCCESS, ""))
            .viewer()
            .expect("the recorded success payload must parse");
        assert_eq!(viewer.login, "octocat");
        assert_eq!(
            viewer.avatar_url,
            "https://avatars.githubusercontent.com/u/583231?v=4"
        );
    }

    // Scripted API payloads for the pending lifecycle (not live mutations).
    const PENDING: &str = r#"[[{"id":91,"node_id":"PRR_pending","state":"PENDING","body":"web summary","user":{"login":"octocat"}}]]"#;
    const DRAFT: &str = r#"{"id":12,"node_id":"PRRC_draft","path":"a.rs","side":"RIGHT","line":7,"start_line":5,"body":"draft"}"#;
    const EMPTY_THREADS: &str = r#"{"data":{"repository":{"pullRequest":{"reviewThreads":{"nodes":[],"pageInfo":{"hasNextPage":false,"endCursor":null}}}}}}"#;

    fn pending_threads(comments: &[&str]) -> String {
        let nodes: Vec<_> = comments
            .iter()
            .map(|raw| {
                let mut comment: serde_json::Value = serde_json::from_str(raw).unwrap();
                comment["pullRequestReview"] = serde_json::json!({"id":"PRR_pending"});
                serde_json::json!({"id":"thread", "side":comment["side"], "line":comment["line"],
                "start_line":comment["start_line"], "isOutdated":comment["line"].is_null(),
                "comments":{"nodes":[comment],"pageInfo":{"hasNextPage":false,"endCursor":null}}})
            })
            .collect();
        serde_json::json!({"data":{"repository":{"pullRequest":{"reviewThreads":{
            "nodes":nodes,"pageInfo":{"hasNextPage":false,"endCursor":null}
        }}}}})
        .to_string()
    }

    fn session_script(
        payloads: &[&str],
    ) -> (GhCliClient<FakeGh>, std::sync::Arc<Mutex<Vec<Vec<String>>>>) {
        let fake = FakeGh::many(
            payloads
                .iter()
                .map(|payload| ok(Some(0), payload, ""))
                .collect(),
        );
        let calls = fake.calls_handle();
        (GhCliClient::with_process(fake), calls)
    }

    fn commit_connection(nodes: Vec<serde_json::Value>, next: Option<&str>) -> serde_json::Value {
        serde_json::json!({"nodes": nodes, "pageInfo": {"hasNextPage": next.is_some(), "endCursor": next}})
    }

    fn commit_page(connection: serde_json::Value) -> String {
        serde_json::json!({"data":{"repository":{"pullRequest":{"commits":connection}}}})
            .to_string()
    }

    fn commit_node(ix: usize, suites: serde_json::Value) -> serde_json::Value {
        serde_json::json!({"commit": {
            "id": format!("commit-{ix}"), "oid": format!("sha-{ix}"),
            "message": format!("Subject {ix}\n\nFull message body"),
            "author": {"name":"Git Author", "user":{"login":"octocat"}},
            "statusCheckRollup":{"state":(["SUCCESS", "FAILURE", "PENDING", "UNKNOWN"][ix % 4])},
            "checkSuites":suites
        }})
    }

    fn actions_suite(ix: usize) -> serde_json::Value {
        serde_json::json!({"workflowRun": {
            "url":format!("https://github.com/octocat/Hello-World/actions/runs/{ix}"),
            "workflow":{"name":format!("Workflow {ix}")}
        }})
    }

    #[test]
    fn pr_commits_paginates_commits_and_each_nested_suite_connection_independently() {
        let empty = commit_connection(vec![], None);
        let mut nodes: Vec<_> = (0..100).map(|ix| commit_node(ix, empty.clone())).collect();
        nodes[0]["commit"]["checkSuites"] =
            commit_connection((0..100).map(actions_suite).collect(), Some("suites-next"));
        // A second commit's independent connection can use the same cursor.
        nodes[1]["commit"]["checkSuites"] = commit_connection(
            vec![serde_json::json!({"workflowRun":null})],
            Some("suites-next"),
        );
        nodes[2]["commit"]["author"]["user"] = serde_json::Value::Null;
        nodes[3]["commit"]["author"] = serde_json::Value::Null;
        nodes[3]["commit"]["statusCheckRollup"] = serde_json::Value::Null;
        let first = commit_page(commit_connection(nodes, Some("commits-next")));
        let suites_a = serde_json::json!({"data":{"node":{"checkSuites":commit_connection(vec![actions_suite(100), actions_suite(0)], None)}}}).to_string();
        let suites_b = serde_json::json!({"data":{"node":{"checkSuites":commit_connection(vec![actions_suite(101)], None)}}}).to_string();
        let last = commit_page(commit_connection(vec![commit_node(100, empty)], None));
        let (client, calls) = session_script(&[&first, &suites_a, &suites_b, &last]);
        let result = client.pr_commits(&loc()).unwrap();
        assert_eq!(result.commits.len(), 101);
        assert_eq!(
            result
                .commits
                .iter()
                .map(|c| c.sha.clone())
                .collect::<Vec<_>>(),
            (0..101).map(|ix| format!("sha-{ix}")).collect::<Vec<_>>()
        );
        assert_eq!(result.commits[0].message, "Subject 0\n\nFull message body");
        assert_eq!(result.commits[0].author, "octocat");
        assert_eq!(result.commits[2].author, "Git Author");
        assert_eq!(result.commits[3].author, "unknown");
        assert_eq!(
            result.commits[..4].iter().map(|c| c.ci).collect::<Vec<_>>(),
            vec![
                CiState::Pass,
                CiState::Fail,
                CiState::Pending,
                CiState::NoChecks
            ]
        );
        assert_eq!(
            result.commits[0].actions.len(),
            101,
            "duplicate run URLs are deduplicated"
        );
        assert_eq!(
            result.commits[0].actions[100].url,
            "https://github.com/octocat/Hello-World/actions/runs/100"
        );
        assert_eq!(
            result.commits[1].actions.len(),
            1,
            "third-party suites invent no Actions link"
        );
        assert!(result.commits[2].actions.is_empty());
        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 4);
        let query = |ix: usize| calls[ix].iter().find(|a| a.starts_with("query=")).unwrap();
        assert!(query(0).contains("commits(first:100)"));
        assert!(query(0).contains("workflowRun { url workflow { name } }"));
        assert!(query(1).contains("node(id:\"commit-0\")"));
        assert!(query(2).contains("node(id:\"commit-1\")"));
        assert!(query(1).contains("checkSuites(first:100,after:\"suites-next\")"));
        assert!(query(3).contains("commits(first:100,after:\"commits-next\")"));
        assert!(
            query(3).contains("checkSuites(first:100)"),
            "suite cursor must not leak across commits"
        );
    }

    #[test]
    fn pr_commits_empty_is_success_but_errors_and_broken_pagination_are_unavailable() {
        let empty = commit_page(commit_connection(vec![], None));
        let (client, _) = session_script(&[&empty]);
        assert_eq!(
            client.pr_commits(&loc()).unwrap(),
            crate::PrCommits::default()
        );
        for payload in [
            r#"{"data":{"repository":{"pullRequest":null}}}"#,
            r#"{"errors":[{"message":"rate limited"}]}"#,
            r#"{"data":{"repository":{"pullRequest":{"commits":{"nodes":[],"pageInfo":{"hasNextPage":true,"endCursor":null}}}}}}"#,
        ] {
            let (client, _) = session_script(&[payload]);
            assert!(client.pr_commits(&loc()).is_err());
        }
        let repeated = commit_page(commit_connection(vec![], Some("same")));
        let (client, _) = session_script(&[&repeated, &repeated]);
        assert!(client
            .pr_commits(&loc())
            .unwrap_err()
            .to_string()
            .contains("repeated"));
        let nested = commit_connection(vec![], Some("same"));
        let first = commit_page(commit_connection(
            vec![commit_node(0, nested.clone())],
            None,
        ));
        let repeat = serde_json::json!({"data":{"node":{"checkSuites":nested}}}).to_string();
        let (client, _) = session_script(&[&first, &repeat]);
        assert!(client
            .pr_commits(&loc())
            .unwrap_err()
            .to_string()
            .contains("repeated"));
        let client = GhCliClient::with_process(FakeGh::one(ok(Some(1), "", "network unavailable")));
        assert!(client.pr_commits(&loc()).is_err());
    }

    #[test]
    #[ignore = "requires authenticated gh and network; read-only public PR"]
    fn real_pr_commits_use_available_graphql_fields_and_direct_actions_urls() {
        let loc = PrLocator {
            owner: "zed-industries".into(),
            repo: "zed".into(),
            number: 66062,
        };
        let commits = GhCliClient::new().pr_commits(&loc).unwrap();
        assert!(!commits.commits.is_empty());
        assert!(
            commits
                .commits
                .iter()
                .any(|commit| !commit.actions.is_empty()),
            "use a recent PR with retained Actions runs for this live smoke test"
        );
        for commit in &commits.commits {
            assert_eq!(commit.sha.len(), 40);
            for run in &commit.actions {
                assert!(run
                    .url
                    .starts_with("https://github.com/zed-industries/zed/actions/runs/"));
            }
        }
    }

    fn range_anchor() -> CommentAnchor {
        CommentAnchor {
            path: "a.rs".into(),
            commit_id: "head".into(),
            side: crate::DiffSide::Right,
            line: 7,
            start_line: Some(5),
            position: None,
        }
    }

    #[test]
    fn published_mutations_route_each_comment_family_without_touching_pending_reviews() {
        use crate::{
            review_session::{Command, ReviewSession},
            CommentTarget,
        };
        for (target, update, delete, input, output) in [
            (
                CommentTarget::Issue("IC1".into()),
                "updateIssueComment",
                "deleteIssueComment",
                "id",
                "issueComment",
            ),
            (
                CommentTarget::Review("PRR1".into()),
                "updatePullRequestReview",
                "deletePullRequestReview",
                "pullRequestReviewId",
                "pullRequestReview",
            ),
            (
                CommentTarget::ReviewComment("PRRC1".into()),
                "updatePullRequestReviewComment",
                "deletePullRequestReviewComment",
                "pullRequestReviewCommentId",
                "pullRequestReviewComment",
            ),
        ] {
            let id = match &target {
                CommentTarget::Issue(id)
                | CommentTarget::Review(id)
                | CommentTarget::ReviewComment(id) => id,
            };
            let edited = serde_json::json!({"data": {update: {output: {"id": id}}}}).to_string();
            let deleted =
                serde_json::json!({"data": {delete: {"clientMutationId": id}}}).to_string();
            let (client, calls) = session_script(&[&edited, &deleted]);
            let mut session = ReviewSession {
                pending: Some(PendingReview {
                    id: 91,
                    node_id: "pending".into(),
                    body: "untouched draft".into(),
                    comments: vec![],
                }),
                verified: true,
            };
            session
                .execute(
                    &client,
                    &loc(),
                    Command::EditPublished {
                        target: target.clone(),
                        body: "  quoted \"body\"\nnext  ".into(),
                    },
                )
                .unwrap();
            session
                .execute(
                    &client,
                    &loc(),
                    Command::DeletePublished {
                        target: target.clone(),
                    },
                )
                .unwrap();
            assert_eq!(session.pending.unwrap().body, "untouched draft");
            assert!(session.verified);
            let calls = calls.lock().unwrap();
            assert_eq!(calls.len(), 2, "no draft lookup or submission");
            let query = calls[0].join(" ");
            assert!(query.contains(&format!("{input}:{}", json_string(id))));
            assert!(query.contains(r#"body:"quoted \"body\"\nnext""#));
            assert!(calls[1].join(" ").contains(delete));
        }
    }

    #[test]
    fn published_mutations_reject_empty_identity_body_and_failed_or_missing_results() {
        use crate::CommentTarget;
        let (client, calls) = session_script(&[]);
        assert!(client
            .edit_comment(&CommentTarget::Issue("".into()), "body")
            .is_err());
        assert!(client
            .edit_comment(&CommentTarget::Issue("IC1".into()), " \n ")
            .is_err());
        assert!(client
            .delete_comment(&CommentTarget::Issue("".into()))
            .is_err());
        assert!(calls.lock().unwrap().is_empty());
        for response in [
            r#"{"errors":[{"message":"not permitted"}]}"#,
            r#"{"data":{}}"#,
        ] {
            let (client, _) = session_script(&[response, response]);
            assert!(client
                .edit_comment(&CommentTarget::Issue("IC1".into()), "body")
                .is_err());
            assert!(client
                .delete_comment(&CommentTarget::Issue("IC1".into()))
                .is_err());
        }
    }

    #[test]
    fn discussion_permissions_preserve_independent_server_capabilities_not_authorship() {
        let mut raw = serde_json::json!({"id":"5000000001", "node_id":"IC1", "body":"body",
            "createdAt":"2026-10-26T00:00:00Z", "author":{"login":"someone-else"},
            "viewerCanUpdate":false, "viewerCanDelete":true});
        let c = discussion_comment(&raw).unwrap();
        assert_eq!(
            c.permissions,
            crate::CommentPermissions {
                node_id: "IC1".into(),
                can_edit: false,
                can_delete: true
            }
        );
        raw["viewerCanUpdate"] = serde_json::json!(true);
        raw["viewerCanDelete"] = serde_json::json!(false);
        let c = discussion_comment(&raw).unwrap();
        assert!(c.permissions.can_edit);
        assert!(!c.permissions.can_delete);
        let mut cached = serde_json::to_value(&c).unwrap();
        cached.as_object_mut().unwrap().remove("permissions");
        let cached: crate::DiscussionComment = serde_json::from_value(cached).unwrap();
        assert_eq!(cached.permissions, Default::default());
        assert!(review_thread_fields(None).contains("viewerCanUpdate viewerCanDelete"));
    }

    #[test]
    fn replies_join_the_reconciled_pending_review_and_resume_in_their_thread() {
        use crate::review_session::{Command, ReviewSession};
        let mut parent: serde_json::Value = serde_json::from_str(DRAFT).unwrap();
        parent["id"] = serde_json::json!("11");
        parent["node_id"] = serde_json::json!("PRRC_published");
        let mut threads: serde_json::Value =
            serde_json::from_str(&pending_threads(&[&parent.to_string()])).unwrap();
        let nodes = &mut threads["data"]["repository"]["pullRequest"]["reviewThreads"]["nodes"];
        nodes[0]["comments"]["nodes"][0]["pullRequestReview"]["id"] =
            serde_json::json!("PRR_published");
        let published = threads.to_string();
        let reply = serde_json::json!({"id":"5000000000","node_id":"PRRC_reply","path":"a.rs",
            "body":"quoted \"reply\"", "replyTo":{"id":"11"}, "pullRequestReview":{"id":"PRR_pending"}});
        let response =
            serde_json::json!({"data":{"addPullRequestReviewComment":{"comment":reply}}})
                .to_string();
        threads["data"]["repository"]["pullRequest"]["reviewThreads"]["nodes"][0]["comments"]
            ["nodes"]
            .as_array_mut()
            .unwrap()
            .push(reply);
        let resumed = threads.to_string();
        let (client, calls) = session_script(&[
            SUCCESS, PENDING, &published, &published, &response, SUCCESS, PENDING, &resumed,
        ]);
        let mut session = ReviewSession::default();
        session
            .execute(
                &client,
                &loc(),
                Command::Reply {
                    comment_id: 11,
                    body: "  quoted \"reply\"  ".into(),
                },
            )
            .unwrap();
        let draft = &session.pending.as_ref().unwrap().comments[0];
        assert_eq!(draft.id, 5_000_000_000);
        assert_eq!(draft.in_reply_to_id, Some(11));
        assert_eq!(draft.line, Some(7));
        assert_eq!(draft.start_line, Some(5));
        session.execute(&client, &loc(), Command::Resume).unwrap();
        assert_eq!(
            session.pending.as_ref().unwrap().comments[0].in_reply_to_id,
            Some(11)
        );
        let calls = calls.lock().unwrap();
        let mutation = calls[4].join(" ");
        assert!(mutation.contains("addPullRequestReviewComment"));
        assert!(mutation.contains("pullRequestReviewId:\"PRR_pending\""));
        assert!(mutation.contains("inReplyTo:\"PRRC_published\""));
        assert!(mutation.contains(r#"body:"quoted \"reply\"""#));
        assert!(!calls
            .iter()
            .any(|call| call.iter().any(|arg| arg == "POST")));
        assert!(!calls
            .iter()
            .any(|call| call.join(" ").contains("submitPullRequestReview")));
    }

    #[test]
    fn reply_after_external_review_submission_posts_immediately_instead_of_using_stale_draft() {
        use crate::review_session::{Command, ReviewSession};
        let (client, calls) = session_script(&[SUCCESS, "[]", "{}"]);
        let mut session = ReviewSession {
            pending: Some(PendingReview {
                id: 91,
                node_id: "stale".into(),
                body: String::new(),
                comments: vec![],
            }),
            verified: false,
        };
        session
            .execute(
                &client,
                &loc(),
                Command::Reply {
                    comment_id: 11,
                    body: "reply".into(),
                },
            )
            .unwrap();
        assert!(session.pending.is_none());
        assert!(calls.lock().unwrap()[2]
            .join(" ")
            .contains("comments/11/replies"));
    }

    #[test]
    fn failed_pending_reply_never_falls_back_to_posting_or_submitting() {
        use crate::review_session::{Command, ReviewSession};
        let target = pending_threads(&[DRAFT]);
        let (client, calls) = session_script(&[
            SUCCESS,
            PENDING,
            &target,
            &target,
            r#"{"errors":[{"message":"cannot reply"}]}"#,
        ]);
        let mut session = ReviewSession::default();
        assert!(session
            .execute(
                &client,
                &loc(),
                Command::Reply {
                    comment_id: 12,
                    body: "reply".into()
                }
            )
            .is_err());
        assert_eq!(session.count(), 1, "original pending comments are retained");
        assert_eq!(
            calls.lock().unwrap().len(),
            5,
            "no retry as an immediate post"
        );
        let (client, calls) = session_script(&[]);
        assert!(session
            .execute(
                &client,
                &loc(),
                Command::Reply {
                    comment_id: 12,
                    body: " \n".into()
                }
            )
            .is_err());
        assert!(calls.lock().unwrap().is_empty());
    }

    fn positioned_anchor(side: crate::DiffSide, range: bool) -> CommentAnchor {
        CommentAnchor {
            side,
            start_line: range.then_some(5),
            position: Some(crate::CommentPosition {
                path: if side == crate::DiffSide::Left {
                    "old-name.rs"
                } else {
                    "a.rs"
                }
                .into(),
                commit_id: if side == crate::DiffSide::Left {
                    "base"
                } else {
                    "head"
                }
                .into(),
                base_commit_id: (side == crate::DiffSide::Left).then(|| "base".into()),
            }),
            ..range_anchor()
        }
    }

    fn preview_thread(anchor: &CommentAnchor) -> serde_json::Value {
        let position = anchor.position.as_ref().unwrap();
        let positioning = match anchor.start_line {
            Some(start) => serde_json::json!({"__typename":"MultilineComment", "startLine":start,
                "endLine":anchor.line, "startCommitOid":position.commit_id,
                "endCommitOid":position.commit_id, "headCommitOid":"head", "baseCommitOid":"base"}),
            None => serde_json::json!({"__typename":"LineComment", "line":anchor.line,
                "commitOid":position.commit_id, "headCommitOid":"head", "baseCommitOid":"base"}),
        };
        serde_json::json!({"id":"thread", "side":anchor.side, "line":null, "start_line":null,
            "isOutdated":true, "positioning":positioning,
            "comments":{"nodes":[{"id":"12", "node_id":"PRRC_draft", "path":"a.rs", "body":"draft",
                "pullRequestReview":{"id":"PRR_pending"}}],
                "pageInfo":{"hasNextPage":false,"endCursor":null}}})
    }

    #[test]
    fn outside_diff_drafts_use_preview_blob_positions_on_both_sides_and_ranges() {
        for side in [crate::DiffSide::Left, crate::DiffSide::Right] {
            for range in [false, true] {
                let anchor = positioned_anchor(side, range);
                let response = serde_json::json!({"data":{"addPullRequestReviewThread":{
                    "thread":preview_thread(&anchor)}}})
                .to_string();
                let (client, calls) = session_script(&[&response]);
                let review = PendingReview {
                    id: 91,
                    node_id: "PRR_pending".into(),
                    body: String::new(),
                    comments: vec![],
                };
                let comment = client
                    .add_pending_comment(&loc(), &review, &anchor, "draft")
                    .unwrap();
                assert_eq!(
                    comment.line,
                    Some(7),
                    "preview-only positions are not outdated"
                );
                assert_eq!(comment.start_line, anchor.start_line);
                assert_eq!(comment.side, side);
                let calls = calls.lock().unwrap();
                assert_eq!(calls.len(), 1);
                assert_eq!(
                    calls[0][5],
                    "GraphQL-Features: graphql_pr_comment_positioning"
                );
                let query = &calls[0][3];
                assert!(query.contains("pullRequestReviewId:\"PRR_pending\""));
                assert!(query.contains(if range {
                    "multilinePositioning:"
                } else {
                    "linePositioning:"
                }));
                assert!(query.contains("headCommitOid:\"head\""));
                if side == crate::DiffSide::Left {
                    assert!(query.contains("baseCommitOid:\"base\""));
                    assert!(
                        query.contains("old-name.rs"),
                        "renames use the source blob's path"
                    );
                    assert!(query.contains(if range {
                        "startCommitOid:\"base\""
                    } else {
                        "commitOid:\"base\""
                    }));
                }
            }
        }
    }

    #[test]
    fn preview_only_comments_resume_and_refresh_without_resurrecting_stale_positions() {
        for side in [crate::DiffSide::Left, crate::DiffSide::Right] {
            for range in [false, true] {
                for current in [false, true] {
                    let anchor = positioned_anchor(side, range);
                    let thread = preview_thread(&anchor);
                    let page = serde_json::json!({"data":{"repository":{"pullRequest":{
                        "headRefOid":if current { "head" } else { "new-head" }, "baseRefOid":"base",
                        "reviewThreads":{"nodes":[thread],"pageInfo":{"hasNextPage":false,"endCursor":null}}
                    }}}}).to_string();
                    let rest = r#"[[{"id":12,"path":"a.rs","line":null,"side":null,"start_line":null,"body":"draft","user":{"login":"octocat"},"created_at":"2026-10-24T00:00:00Z","in_reply_to_id":null}]]"#;
                    let (client, _) = session_script(&[SUCCESS, PENDING, &page, rest, &page]);
                    let review = client.pending_review(&loc()).unwrap().unwrap();
                    let comments = client.review_comments(&loc()).unwrap();
                    assert_eq!(review.comments[0].line, current.then_some(7));
                    assert_eq!(comments[0].line, current.then_some(7));
                    assert_eq!(
                        comments[0].start_line,
                        if current { anchor.start_line } else { None }
                    );
                    assert_eq!(comments[0].side.as_deref(), Some(side.api_str()));
                }
            }
        }
    }

    #[test]
    fn preview_immediate_comments_are_atomic_and_never_touch_existing_drafts_or_retry() {
        use crate::review_session::{Command, ReviewSession};
        for side in [crate::DiffSide::Left, crate::DiffSide::Right] {
            for range in [false, true] {
                for succeeds in [false, true] {
                    let response = if succeeds {
                        r#"{"data":{"addPullRequestReview":{"pullRequestReview":{"id":"new-review","state":"COMMENTED"}}}}"#
                    } else {
                        r#"{"errors":[{"message":"positioning not enabled"}],"data":null}"#
                    };
                    let (client, calls) = session_script(&[
                        r#"{"data":{"repository":{"pullRequest":{"id":"PR_id"}}}}"#,
                        response,
                    ]);
                    let mut session = ReviewSession {
                        verified: true,
                        pending: Some(PendingReview {
                            id: 91,
                            node_id: "PRR_pending".into(),
                            body: "keep me".into(),
                            comments: vec![serde_json::from_str(DRAFT).unwrap()],
                        }),
                    };
                    let saved = session.pending.clone();
                    let result = session.execute(
                        &client,
                        &loc(),
                        Command::Immediate {
                            anchor: positioned_anchor(side, range),
                            body: "now".into(),
                        },
                    );
                    assert_eq!(result.is_ok(), succeeds);
                    if let Err(error) = result {
                        assert!(error.to_string().contains("experimental positioning API"));
                    }
                    assert_eq!(session.pending, saved);
                    assert!(session.verified);
                    let calls = calls.lock().unwrap();
                    assert_eq!(
                        calls.len(),
                        2,
                        "no fallback or retry can duplicate a comment"
                    );
                    assert!(calls[1][3].contains("addPullRequestReview(input:{pullRequestId:\"PR_id\",commitOID:\"head\",event:COMMENT,threads:["));
                    assert!(calls[1][3].contains(if range {
                        "multilinePositioning:"
                    } else {
                        "linePositioning:"
                    }));
                    assert!(!calls[1][3].contains("pullRequestReviewId"));
                    assert!(!calls[1][3].contains("submitPullRequestReview"));
                }
            }
        }
    }

    #[test]
    fn pending_session_start_resume_add_edit_delete_submit_is_one_server_draft() {
        use crate::review_session::{Command, ReviewSession};
        let comment_page = pending_threads(&[DRAFT]);
        let added = format!(
            r#"{{"data":{{"addPullRequestReviewThread":{{"thread":{{"side":"RIGHT","line":7,"start_line":5,"isOutdated":false,"comments":{{"nodes":[{DRAFT}]}}}}}}}}}}"#
        );
        let (client, calls) = session_script(&[
            SUCCESS,
            "[[]]",
            r#"{"id":91,"node_id":"PRR_pending","body":""}"#,
            SUCCESS,
            PENDING,
            EMPTY_THREADS,
            &added,
            SUCCESS,
            PENDING,
            &comment_page,
            r#"{"data":{"updatePullRequestReviewComment":{"pullRequestReviewComment":{"id":"PRRC_draft"}}}}"#,
            SUCCESS,
            PENDING,
            &comment_page,
            r#"{"data":{"deletePullRequestReviewComment":{"clientMutationId":"PRRC_draft"}}}"#,
            SUCCESS,
            PENDING,
            EMPTY_THREADS,
            r#"{"data":{"submitPullRequestReview":{"pullRequestReview":{"id":"PRR_pending","state":"APPROVED"}}}}"#,
        ]);
        let mut session = ReviewSession::default();
        session
            .execute(
                &client,
                &loc(),
                Command::Start {
                    commit_id: "head".into(),
                },
            )
            .unwrap();
        assert_eq!(session.count(), 0);
        assert_eq!(session.pending.as_ref().unwrap().id, 91);
        session
            .execute(
                &client,
                &loc(),
                Command::Add {
                    anchor: range_anchor(),
                    body: "draft".into(),
                },
            )
            .unwrap();
        assert_eq!(session.count(), 1);
        assert_eq!(
            session.pending.as_ref().unwrap().comments[0].start_line,
            Some(5)
        );
        session
            .execute(
                &client,
                &loc(),
                Command::Edit {
                    id: 12,
                    body: "edited".into(),
                },
            )
            .unwrap();
        assert_eq!(session.pending.as_ref().unwrap().comments[0].body, "edited");
        session
            .execute(&client, &loc(), Command::Delete { id: 12 })
            .unwrap();
        assert_eq!(session.count(), 0);
        session
            .execute(
                &client,
                &loc(),
                Command::Submit {
                    verdict: ReviewVerdict::Approve,
                    body: String::new(),
                },
            )
            .unwrap();
        assert!(session.pending.is_none());
        let calls = calls.lock().unwrap();
        assert_eq!(
            calls[2].join(" "),
            "api -X POST repos/octocat/Hello-World/pulls/4781/reviews -f commit_id=head"
        );
        assert!(calls[6][3].contains("pullRequestReviewId:\"PRR_pending\""));
        assert!(calls[6][3].contains("side:RIGHT,line:7"));
        assert!(calls[6][3].contains("startLine:5,startSide:RIGHT"));
        assert!(calls[10][3].contains("updatePullRequestReviewComment(input:{pullRequestReviewCommentId:\"PRRC_draft\",body:\"edited\"}"));
        assert!(calls[14][3].contains("deletePullRequestReviewComment(input:{id:\"PRRC_draft\""));
        assert!(calls[18][3].contains("event:APPROVE,body:\"\""));
        assert_eq!(
            calls
                .iter()
                .filter(|args| args.iter().any(|arg| arg == "commit_id=head"))
                .count(),
            1,
            "only one review is created"
        );
    }

    #[test]
    fn resumes_only_the_viewers_pending_review_across_pages_including_outdated_comments() {
        use crate::review_session::{Command, ReviewSession};
        let reviews = r#"[[{"id":1,"node_id":"other","state":"PENDING","body":"private","user":{"login":"someone-else"}},{"id":2,"node_id":"old","state":"APPROVED","body":"old","user":{"login":"octocat"}}],[{"id":91,"node_id":"PRR_pending","state":"PENDING","body":"web summary","user":{"login":"Octocat"}}]]"#;
        let comments = pending_threads(&[
            DRAFT,
            r#"{"id":13,"node_id":"PRRC_old","path":"old.rs","side":"LEFT","line":null,"start_line":null,"body":"outdated"}"#,
        ]);
        let (client, calls) = session_script(&[SUCCESS, reviews, &comments]);
        let mut session = ReviewSession::default();
        session.execute(&client, &loc(), Command::Resume).unwrap();
        let review = session.pending.as_ref().unwrap();
        assert_eq!(review.id, 91);
        assert_eq!(review.body, "web summary");
        assert_eq!(session.count(), 2);
        assert_eq!(review.comments[1].line, None);
        assert!(session.verified);
        assert!(calls.lock().unwrap()[2][3].contains("reviewThreads(first:100)"));
    }

    #[test]
    fn restart_repairs_legacy_cached_anchors_and_edits_deletes_by_graphql_node_id() {
        use crate::review_session::{Command, ReviewSession};
        // Captured read-only from the reported PR. The review-specific REST
        // response omitted both side and line; the cached mirror defaulted RIGHT.
        const THREADS: &str = include_str!("../fixtures/gh_graphql_pending_threads_success.json");
        const REVIEWS: &str = r#"[[{"id":5635356985,"node_id":"PRR_kwDOVTOJkc8AAAABT-S5OQ","state":"PENDING","body":"","user":{"login":"octocat"}}]]"#;
        let (client, calls) = session_script(&[
            SUCCESS,
            REVIEWS,
            THREADS,
            SUCCESS,
            REVIEWS,
            THREADS,
            r#"{"data":{"updatePullRequestReviewComment":{"pullRequestReviewComment":{"id":"PRRC_kwDOVTOJkc8AAAABBCvhGQ"}}}}"#,
            SUCCESS,
            REVIEWS,
            THREADS,
            r#"{"data":{"deletePullRequestReviewComment":{"clientMutationId":"PRRC_kwDOVTOJkc8AAAABBCvhGQ"}}}"#,
        ]);
        let mut session: ReviewSession = serde_json::from_str(r#"{"pending":{"id":5635356985,"node_id":"PRR_kwDOVTOJkc8AAAABT-S5OQ","body":"","comments":[{"id":4364951833,"path":"crates/app/src/main.rs","side":"RIGHT","line":null,"start_line":null,"body":"Test"}]}}"#).unwrap();
        session.execute(&client, &loc(), Command::Resume).unwrap();
        let comment = &session.pending.as_ref().unwrap().comments[0];
        assert_eq!(session.count(), 1, "submitted comments are not drafts");
        assert_eq!(comment.id, 4364951833);
        assert_eq!(comment.node_id, "PRRC_kwDOVTOJkc8AAAABBCvhGQ");
        assert_eq!(comment.side, crate::DiffSide::Left);
        assert_eq!(comment.line, Some(78));
        assert_eq!(
            comment.start_line, None,
            "equal thread start/end is a single line"
        );
        let saved = serde_json::to_string(&session).unwrap();
        let mut reopened: ReviewSession = serde_json::from_str(&saved).unwrap();
        assert!(!reopened.verified);
        reopened
            .execute(
                &client,
                &loc(),
                Command::Edit {
                    id: 4364951833,
                    body: "edited \"draft\"\nnext line".into(),
                },
            )
            .unwrap();
        assert_eq!(
            reopened.pending.as_ref().unwrap().comments[0].body,
            "edited \"draft\"\nnext line"
        );
        reopened
            .execute(&client, &loc(), Command::Delete { id: 4364951833 })
            .unwrap();
        assert_eq!(reopened.count(), 0);
        let calls = calls.lock().unwrap();
        assert!(calls[6][3].contains("pullRequestReviewCommentId:\"PRRC_kwDOVTOJkc8AAAABBCvhGQ\""));
        assert!(calls[6][3].contains(r#"body:"edited \"draft\"\nnext line""#));
        assert!(calls[10][3]
            .contains("deletePullRequestReviewComment(input:{id:\"PRRC_kwDOVTOJkc8AAAABBCvhGQ\""));
        assert!(
            !calls.iter().flatten().any(|arg| arg.contains("/comments")),
            "no legacy REST draft reads or writes"
        );
    }

    #[test]
    fn pending_threads_and_comments_both_paginate_and_outdated_threads_never_reanchor() {
        let mut first: serde_json::Value =
            serde_json::from_str(&pending_threads(&[DRAFT])).unwrap();
        let connection = &mut first["data"]["repository"]["pullRequest"]["reviewThreads"];
        connection["pageInfo"] =
            serde_json::json!({"hasNextPage":true,"endCursor":"thread-cursor"});
        connection["nodes"][0]["comments"]["pageInfo"] =
            serde_json::json!({"hasNextPage":true,"endCursor":"comment-cursor"});
        let mut comment_page: serde_json::Value =
            serde_json::from_str(&pending_threads(&[DRAFT])).unwrap();
        let thread =
            &mut comment_page["data"]["repository"]["pullRequest"]["reviewThreads"]["nodes"][0];
        thread["comments"]["nodes"][0]["id"] = serde_json::json!(13);
        thread["comments"]["nodes"][0]["node_id"] = serde_json::json!("PRRC_second");
        let second = serde_json::json!({"data":{"node":thread}}).to_string();
        let mut last: serde_json::Value = serde_json::from_str(&pending_threads(&[DRAFT])).unwrap();
        let thread = &mut last["data"]["repository"]["pullRequest"]["reviewThreads"]["nodes"][0];
        thread["isOutdated"] = serde_json::json!(true);
        thread["comments"]["nodes"][0]["id"] = serde_json::json!(14);
        thread["comments"]["nodes"][0]["node_id"] = serde_json::json!("PRRC_outdated");
        let (client, calls) = session_script(&[
            SUCCESS,
            PENDING,
            &first.to_string(),
            &second,
            &last.to_string(),
        ]);
        let review = client.pending_review(&loc()).unwrap().unwrap();
        assert_eq!(review.comments.len(), 3);
        assert_eq!(review.comments[2].line, None);
        assert_eq!(review.comments[2].start_line, None);
        let calls = calls.lock().unwrap();
        assert!(calls[3][3].contains("node(id:\"thread\")"));
        assert!(calls[3][3].contains("comments(first:100,after:\"comment-cursor\")"));
        assert!(calls[4][3].contains("reviewThreads(first:100,after:\"thread-cursor\")"));
    }

    #[test]
    fn failed_graphql_draft_mutations_preserve_local_drafts_and_missing_identity_never_writes() {
        use crate::review_session::{Command, ReviewSession};
        for (command, response) in [
            (
                Command::Edit {
                    id: 12,
                    body: "edit".into(),
                },
                r#"{"errors":[{"message":"denied"}],"data":null}"#,
            ),
            (
                Command::Delete { id: 12 },
                r#"{"data":{"deletePullRequestReviewComment":null}}"#,
            ),
        ] {
            let comments = pending_threads(&[DRAFT]);
            let (client, _) = session_script(&[SUCCESS, PENDING, &comments, response]);
            let mut session = ReviewSession::default();
            assert!(session.execute(&client, &loc(), command).is_err());
            assert_eq!(session.pending.as_ref().unwrap().comments[0].body, "draft");
        }
        let mut comment: PendingComment = serde_json::from_str(DRAFT).unwrap();
        comment.node_id.clear();
        let (client, calls) = session_script(&[]);
        assert!(client
            .edit_pending_comment(&loc(), &comment, "edit")
            .is_err());
        assert!(client.delete_pending_comment(&loc(), &comment).is_err());
        assert!(calls.lock().unwrap().is_empty());
    }

    /// Read-only: GANDER_TEST_PR=owner/repo#123 cargo test -p github pending_review_live_read_only -- --ignored --nocapture
    #[test]
    #[ignore = "requires gh authentication and a PR with a pending review"]
    fn pending_review_live_read_only() {
        let pr = std::env::var("GANDER_TEST_PR").expect("set GANDER_TEST_PR");
        let client = GhCliClient::new();
        let loc = client.resolve_pr(&pr).unwrap();
        let review = client
            .pending_review(&loc)
            .unwrap()
            .expect("a pending review");
        for comment in &review.comments {
            assert!(!comment.node_id.is_empty());
            println!(
                "draft {}: {} {} {:?}–{:?}",
                comment.id,
                comment.path,
                comment.side.api_str(),
                comment.start_line,
                comment.line
            );
        }
        assert!(!review.comments.is_empty());
    }

    #[test]
    fn immediate_comments_do_not_resume_start_or_submit_the_pending_review() {
        use crate::review_session::{Command, ReviewSession};
        let (client, calls) = session_script(&[SUCCESS, PENDING, EMPTY_THREADS, "{}", "{}"]);
        let mut session = ReviewSession::default();
        session.execute(&client, &loc(), Command::Resume).unwrap();
        let pending = session.pending.clone();
        let mut anchor = range_anchor();
        anchor.side = crate::DiffSide::Left;
        session
            .execute(
                &client,
                &loc(),
                Command::Immediate {
                    anchor: anchor.clone(),
                    body: "now".into(),
                },
            )
            .unwrap();
        anchor.start_line = None;
        session
            .execute(
                &client,
                &loc(),
                Command::Immediate {
                    anchor,
                    body: "single".into(),
                },
            )
            .unwrap();
        assert_eq!(session.pending, pending);
        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 5);
        assert_eq!(calls[3].join(" "), "api -X POST repos/octocat/Hello-World/pulls/4781/comments -f body=now -f commit_id=head -f path=a.rs -f side=LEFT -F line=7 -F start_line=5 -f start_side=LEFT");
        assert!(!calls[4].iter().any(|arg| arg.starts_with("start_")));
    }

    #[test]
    fn external_submission_clears_cached_drafts_and_stale_edits_never_post() {
        use crate::review_session::{Command, ReviewSession};
        let comments = pending_threads(&[DRAFT]);
        let (client, calls) = session_script(&[
            SUCCESS, PENDING, &comments, SUCCESS, "[[]]", SUCCESS, "[[]]",
        ]);
        let mut session = ReviewSession::default();
        session.execute(&client, &loc(), Command::Resume).unwrap();
        let cached = serde_json::to_string(&session).unwrap();
        let mut restored: ReviewSession = serde_json::from_str(&cached).unwrap();
        assert_eq!(restored.count(), 1);
        assert!(!restored.verified);
        assert!(restored
            .execute(
                &client,
                &loc(),
                Command::Edit {
                    id: 12,
                    body: "stale edit".into()
                }
            )
            .is_err());
        assert!(restored.pending.is_none());
        assert!(session
            .execute(&client, &loc(), Command::Delete { id: 12 })
            .is_err());
        assert!(session.pending.is_none());
        assert_eq!(
            calls.lock().unwrap().len(),
            7,
            "reads only, no stale mutations"
        );
    }

    #[test]
    fn pending_verdicts_allow_bodyless_reviews_with_comments_and_quote_summaries() {
        use crate::review_session::{Command, ReviewSession};
        for (verdict, event) in [
            (ReviewVerdict::Approve, "APPROVE"),
            (ReviewVerdict::RequestChanges, "REQUEST_CHANGES"),
            (ReviewVerdict::Comment, "COMMENT"),
        ] {
            for body in ["", "summary \"quoted\"\nnext line"] {
                let comments = pending_threads(&[DRAFT]);
                let (client, calls) = session_script(&[
                    SUCCESS,
                    PENDING,
                    &comments,
                    r#"{"data":{"submitPullRequestReview":{"pullRequestReview":{"id":"PRR_pending","state":"SUBMITTED"}}}}"#,
                ]);
                let mut session = ReviewSession::default();
                session
                    .execute(
                        &client,
                        &loc(),
                        Command::Submit {
                            verdict,
                            body: body.into(),
                        },
                    )
                    .unwrap();
                assert!(session.pending.is_none());
                let calls = calls.lock().unwrap();
                assert!(calls[3][3].contains(&format!("event:{event},body:{}", json_string(body))));
            }
        }
    }

    #[test]
    fn failed_add_or_submission_preserves_pending_state_and_never_falls_back_to_immediate() {
        use crate::review_session::{Command, ReviewSession};
        let (client, calls) = session_script(&[
            SUCCESS,
            "[[]]",
            r#"{"id":91,"node_id":"PRR_pending"}"#,
            r#"{"errors":[{"message":"invalid line"}],"data":null}"#,
            SUCCESS,
            PENDING,
            EMPTY_THREADS,
            r#"{"errors":[{"message":"cannot approve own PR"}],"data":null}"#,
        ]);
        let mut session = ReviewSession::default();
        assert!(session
            .execute(
                &client,
                &loc(),
                Command::Add {
                    anchor: range_anchor(),
                    body: "draft".into()
                }
            )
            .is_err());
        assert_eq!(session.pending.as_ref().unwrap().id, 91);
        assert_eq!(session.count(), 0);
        assert!(session
            .execute(
                &client,
                &loc(),
                Command::Submit {
                    verdict: ReviewVerdict::Approve,
                    body: String::new()
                }
            )
            .is_err());
        assert!(session.pending.is_some());
        assert_eq!(calls.lock().unwrap().len(), 8);
    }

    #[test]
    fn invalid_anchors_and_empty_comments_fail_before_io() {
        use crate::review_session::{Command, ReviewSession};
        let (client, calls) = session_script(&[]);
        let mut session = ReviewSession::default();
        let anchor = range_anchor();
        for command in [
            Command::Add {
                anchor: anchor.clone(),
                body: "  ".into(),
            },
            Command::Edit {
                id: 12,
                body: "".into(),
            },
            Command::Immediate {
                anchor: CommentAnchor {
                    line: 0,
                    ..anchor.clone()
                },
                body: "body".into(),
            },
            Command::Add {
                anchor: CommentAnchor {
                    start_line: Some(7),
                    ..anchor
                },
                body: "body".into(),
            },
        ] {
            assert!(session.execute(&client, &loc(), command).is_err());
        }
        assert!(calls.lock().unwrap().is_empty());
    }

    #[test]
    fn pending_thread_response_uses_thread_side_and_64_bit_comment_ids() {
        let (client, calls) = session_script(&[
            r#"{"data":{"addPullRequestReviewThread":{"thread":{"side":"LEFT","line":7,"start_line":null,"isOutdated":false,"comments":{"nodes":[{"id":"5000000000","node_id":"PRRC_draft","path":"a.rs","line":null,"start_line":null,"body":"quoted \"body\""}]}}}}}"#,
        ]);
        let review = PendingReview {
            id: 91,
            node_id: "pending".into(),
            body: String::new(),
            comments: vec![],
        };
        let mut anchor = range_anchor();
        anchor.side = crate::DiffSide::Left;
        anchor.start_line = None;
        let comment = client
            .add_pending_comment(&loc(), &review, &anchor, "quoted \"body\"")
            .unwrap();
        assert_eq!(comment.id, 5_000_000_000);
        assert_eq!(comment.side, crate::DiffSide::Left);
        assert_eq!(comment.line, Some(7));
        assert_eq!(comment.start_line, None);
        let calls = calls.lock().unwrap();
        assert!(calls[0][3]
            .contains("thread { side:diffSide line start_line:startLine isOutdated positioning"));
        assert!(calls[0][3].contains("comments(first:1) { nodes { id:fullDatabaseId"));
        assert!(!calls[0][3].contains("startSide"));
        assert!(calls[0][3].contains(r#"body:"quoted \"body\"""#));
    }

    #[test]
    fn pending_thread_anchors_use_current_thread_ranges_and_never_original_positions() {
        for (side, outdated, expected) in [
            (crate::DiffSide::Left, false, Some(7)),
            (crate::DiffSide::Right, false, Some(7)),
            (crate::DiffSide::Left, true, None),
        ] {
            let response = serde_json::json!({"data":{"addPullRequestReviewThread":{"thread":{
                "side":side, "line":7, "start_line":5, "isOutdated":outdated,
                "comments":{"nodes":[{"id":"12","node_id":"PRRC_draft","path":"a.rs","line":null,
                    "start_line":null,"originalLine":7,"body":"range"}]}
            }}}})
            .to_string();
            let (client, _) = session_script(&[&response]);
            let review = PendingReview {
                id: 91,
                node_id: "pending".into(),
                body: String::new(),
                comments: vec![],
            };
            let anchor = CommentAnchor {
                side,
                ..range_anchor()
            };
            let comment = client
                .add_pending_comment(&loc(), &review, &anchor, "range")
                .unwrap();
            assert_eq!(comment.side, side);
            assert_eq!(comment.line, expected);
            assert_eq!(comment.start_line, expected.map(|_| 5));
        }
    }

    #[test]
    fn failed_resume_keeps_the_cached_draft_unverified_and_malformed_submit_never_clears_it() {
        use crate::review_session::{Command, ReviewSession};
        let (client, _) = session_script(&[SUCCESS, PENDING, EMPTY_THREADS]);
        let mut session = ReviewSession::default();
        session.execute(&client, &loc(), Command::Resume).unwrap();
        let saved = session.pending.clone();
        let failed = super::tests::client(ok(Some(1), "", "network unavailable"));
        assert!(session.execute(&failed, &loc(), Command::Resume).is_err());
        assert_eq!(session.pending, saved);
        assert!(!session.verified);
        for response in [
            r#"{"data":null}"#,
            r#"{"data":{"submitPullRequestReview":null}}"#,
        ] {
            let (client, _) = session_script(&[SUCCESS, PENDING, EMPTY_THREADS, response]);
            assert!(session
                .execute(
                    &client,
                    &loc(),
                    Command::Submit {
                        verdict: ReviewVerdict::Approve,
                        body: String::new()
                    }
                )
                .is_err());
            assert_eq!(session.pending, saved);
        }
    }

    #[test]
    fn viewer_reports_a_missing_binary_as_no_credential_source() {
        let err = client(Err(io::Error::from(io::ErrorKind::NotFound)))
            .viewer()
            .unwrap_err();
        assert_eq!(err, ClientError::NoCredentialSource);
    }

    #[test]
    fn viewer_reports_gh_signed_out_as_unauthenticated() {
        let err = client(ok(Some(4), "", UNAUTHENTICATED))
            .viewer()
            .unwrap_err();
        assert_eq!(err, ClientError::Unauthenticated);
    }

    /// Older gh builds without the reserved exit code still classify by the
    /// `gh auth login` hint on stderr.
    #[test]
    fn viewer_classifies_signed_out_by_the_hint_even_at_other_exit_codes() {
        let err = client(ok(Some(1), "", UNAUTHENTICATED))
            .viewer()
            .unwrap_err();
        assert_eq!(err, ClientError::Unauthenticated);
    }

    #[test]
    fn viewer_reports_a_rejected_token_as_unauthenticated() {
        let err = client(ok(Some(1), "", BAD_TOKEN)).viewer().unwrap_err();
        assert_eq!(err, ClientError::Unauthenticated);
    }

    #[test]
    fn viewer_reports_other_failures_with_the_recorded_cause() {
        let err = client(ok(Some(1), "", NETWORK_DOWN)).viewer().unwrap_err();
        assert_eq!(
            err,
            ClientError::Failed("error connecting to api.github.com.invalid".to_string())
        );
    }

    #[test]
    fn viewer_degrades_a_shape_drift_to_a_failure() {
        // gh exited 0 but the payload stopped looking like GET /user —
        // missing the fields we consume here, unparseable text below.
        let err = client(ok(Some(0), r#"{"login": "octocat"}"#, ""))
            .viewer()
            .unwrap_err();
        assert!(
            matches!(err, ClientError::Failed(ref cause) if cause.starts_with("unexpected GitHub response")),
            "a drifted payload must degrade to Failed, got {err:?}"
        );

        let err = client(ok(Some(0), "not json at all", ""))
            .viewer()
            .unwrap_err();
        assert!(matches!(err, ClientError::Failed(_)));
    }

    #[test]
    fn a_silent_signal_death_keeps_a_sayable_cause() {
        // No exit code, no stderr: the cause line still has to say
        // something human, not panic or leak a bare Option.
        let err = client(ok(None, "", "")).viewer().unwrap_err();
        assert_eq!(err, ClientError::Failed("gh exited abnormally".to_string()));

        let err = client(ok(Some(70), "", "")).viewer().unwrap_err();
        assert_eq!(err, ClientError::Failed("exit code 70".to_string()));
    }

    #[test]
    fn search_prs_parses_the_recorded_queue_payload() {
        // The enrichment answered with nothing to say ("data": null), so
        // this pins the search fields plus the honest defaults every
        // un-enriched PR keeps.
        let (client, _) = rich_client(
            ok(Some(0), QUEUE_SUCCESS, ""),
            vec![ok(Some(0), "{\"data\":null}", "")],
        );
        let queue = client
            .search_prs(&QueueSearch::review_requested())
            .expect("the recorded queue payload must parse");
        assert_eq!(queue.len(), 2);
        let first = &queue[1];
        assert_eq!(first.number, 4781);
        assert_eq!(
            first.title,
            "Add deactivation filters to the users manage list"
        );
        assert_eq!(first.author, "octocat");
        assert_eq!(first.repository, "octocat/Hello-World");
        assert!(first.is_draft);
        assert_eq!(first.created_at, "2026-10-15T10:00:00Z");
        assert_eq!(first.updated_at, "2026-10-20T08:07:08Z");
        // Rich search fields: conversation count, labels as GitHub paints them.
        assert_eq!(first.comments, 2);
        assert_eq!(first.labels.len(), 1);
        assert_eq!(first.labels[0].name, "api");
        assert_eq!(first.labels[0].color, "1D76DB");
        let second = &queue[0];
        assert_eq!(second.repository, "octocat/Spoon-Knife");
        assert!(!second.is_draft);
        assert_eq!(second.created_at, "2026-10-10T09:00:00Z");
        assert_eq!(second.comments, 1);
        assert_eq!(second.labels.len(), 3);
        assert_eq!(second.labels[2].name, "migrations");
        // With both request times unknown, creation time orders the rows.
        assert!(queue[0].created_at < queue[1].created_at);
        // Honest defaults until an enrichment answers for each PR.
        for item in &queue {
            assert_eq!(item.review_state, ReviewState::Awaiting);
            assert_eq!(item.ci, CiState::NoChecks);
            assert_eq!(item.additions, None);
            assert_eq!(item.deletions, None);
            assert_eq!(item.author_avatar_url, None);
            assert_eq!(item.review_requested_at, None);
        }
    }

    #[test]
    fn search_prs_returns_an_empty_queue_as_no_items() {
        // FakeGh scripts exactly one invocation: an enrichment call for an
        // empty queue would fail this test — nothing to enrich means no
        // second subprocess.
        let queue = client(ok(Some(0), QUEUE_EMPTY, ""))
            .search_prs(&QueueSearch::review_requested())
            .expect("an empty result is a success, not an error");
        assert!(queue.is_empty());
    }

    #[test]
    fn search_prs_reports_gh_signed_out_as_unauthenticated() {
        let err = client(ok(Some(4), "", SEARCH_SIGNED_OUT))
            .search_prs(&QueueSearch::review_requested())
            .unwrap_err();
        assert_eq!(err, ClientError::Unauthenticated);
    }

    #[test]
    fn search_prs_reports_a_rejected_token_as_unauthenticated() {
        // gh search spells its 401 differently than gh api does; both
        // spellings must classify as the sign-in case.
        let err = client(ok(Some(1), "", SEARCH_BAD_TOKEN))
            .search_prs(&QueueSearch::review_requested())
            .unwrap_err();
        assert_eq!(err, ClientError::Unauthenticated);
    }

    #[test]
    fn search_prs_degrades_a_shape_drift_to_a_failure() {
        // gh exited 0 but the payload stopped looking like search output.
        for stdout in [r#"[{"number": 1}]"#, "not json at all"] {
            let err = client(ok(Some(0), stdout, ""))
                .search_prs(&QueueSearch::review_requested())
                .unwrap_err();
            assert!(
                matches!(err, ClientError::Failed(ref cause) if cause.starts_with("unexpected GitHub response")),
                "a drifted payload must degrade to Failed, got {err:?}"
            );
        }
    }

    #[test]
    fn search_prs_invokes_the_compiled_search_verbatim() {
        // The queue's golden invocation, pinned at the process boundary: the
        // compiled query must reach gh as one argument per qualifier (a
        // single quoted string is keyword-searched, not qualifier-parsed)
        // with the sort spec as flags.
        let fake = FakeGh::one(ok(Some(0), QUEUE_EMPTY, ""));
        let calls = fake.calls_handle();
        GhCliClient::with_process(fake)
            .search_prs(&QueueSearch::review_requested())
            .expect("the recorded empty queue must parse");
        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 1, "the queue search is one gh invocation");
        assert_eq!(
            calls[0].join(" "),
            "search prs is:pr is:open review-requested:@me \
             --sort created --order asc --limit 1000 \
             --json number,title,author,repository,isDraft,createdAt,updatedAt,commentsCount,labels"
        );
    }

    #[test]
    fn filtered_searches_invoke_the_compiled_chips_and_sort_verbatim() {
        // The golden invocation with chips and a flipped sort — the whole
        // pipeline from a chip click to gh argv: each chip's qualifier is
        // its own argument after the preset's, in kind order, and the
        // sort spec follows. Filtering never adds a round trip.
        let fake = FakeGh::one(ok(Some(0), QUEUE_EMPTY, ""));
        let calls = fake.calls_handle();
        GhCliClient::with_process(fake)
            .search_prs(
                &QueueSearch::review_requested()
                    .filtered_by(QueueFilter::org("acme"))
                    .filtered_by(QueueFilter::repo("acme/widgets"))
                    .sorted_by(QueueSort::CreatedAscending),
            )
            .expect("the recorded empty queue must parse");
        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 1, "chips and sort never add a round trip");
        assert_eq!(
            calls[0].join(" "),
            "search prs is:pr is:open review-requested:@me org:acme repo:acme/widgets \
             --sort created --order asc --limit 1000 \
             --json number,title,author,repository,isDraft,createdAt,updatedAt,commentsCount,labels"
        );
    }

    #[test]
    fn enrichment_lands_check_data_on_the_right_rows() {
        let (client, _) = rich_client(
            ok(Some(0), QUEUE_SUCCESS, ""),
            vec![ok(Some(0), ENRICH_SUCCESS, "")],
        );
        let queue = client
            .search_prs(&QueueSearch::review_requested())
            .expect("a search plus its enrichment both succeeded");
        // Each PR's answer landed on the PR that asked for it — alias
        // mapping is position-sensitive, and the whole point of the plan.
        let first = &queue[1];
        assert_eq!(first.number, 4781);
        assert_eq!(first.additions, Some(151));
        assert_eq!(first.deletions, Some(30));
        assert_eq!(first.review_state, ReviewState::Awaiting); // REVIEW_REQUIRED
        assert_eq!(first.ci, CiState::Pass);
        assert_eq!(
            first.author_avatar_url.as_deref(),
            Some("https://avatars.githubusercontent.com/u/583231?s=64&v=4")
        );
        let second = &queue[0];
        assert_eq!(second.number, 4761);
        assert_eq!(second.additions, Some(1459));
        assert_eq!(second.deletions, Some(17));
        assert_eq!(second.review_state, ReviewState::ChangesRequested);
        assert_eq!(second.ci, CiState::Pass);
        assert_eq!(
            second.author_avatar_url.as_deref(),
            Some("https://avatars.githubusercontent.com/u/480938?s=64&v=4")
        );
    }

    #[test]
    fn enrichment_invokes_the_compiled_graphql_verbatim() {
        // The enrichment's golden invocation: one `gh api graphql` for the
        // whole queue, repo aliases in first-seen order, PR aliases pinned
        // to PR numbers, asking for exactly the check-sized fields.
        let (client, calls) = rich_client(
            ok(Some(0), QUEUE_SUCCESS, ""),
            vec![ok(Some(0), ENRICH_SUCCESS, "")],
        );
        client
            .search_prs(&QueueSearch::review_requested())
            .expect("the recorded queue and enrichment must parse");
        let calls = calls.lock().unwrap();
        assert_eq!(
            calls.len(),
            2,
            "the rich queue search is two gh invocations"
        );
        assert_eq!(
            calls[1].join(" "),
             "api graphql -f query=query { viewer { login } \
             r0: repository(owner: \"octocat\", name: \"Hello-World\") { \
             p4781: pullRequest(number: 4781) { additions deletions reviewDecision \
             statusCheckRollup { state } author { avatarUrl(size: 64) } \
             reviewRequests(first:100) { nodes { requestedReviewer { __typename ... on User { login } } } pageInfo { hasNextPage endCursor } } \
             timelineItems(last:100,itemTypes:[REVIEW_REQUESTED_EVENT]) { nodes { ... on ReviewRequestedEvent { createdAt requestedReviewer { __typename ... on User { login } } } } pageInfo { hasPreviousPage startCursor } } } } \
             r1: repository(owner: \"octocat\", name: \"Spoon-Knife\") { \
             p4761: pullRequest(number: 4761) { additions deletions reviewDecision \
             statusCheckRollup { state } author { avatarUrl(size: 64) } \
             reviewRequests(first:100) { nodes { requestedReviewer { __typename ... on User { login } } } pageInfo { hasNextPage endCursor } } \
             timelineItems(last:100,itemTypes:[REVIEW_REQUESTED_EVENT]) { nodes { ... on ReviewRequestedEvent { createdAt requestedReviewer { __typename ... on User { login } } } } pageInfo { hasPreviousPage startCursor } } } } }"
        );
    }

    fn queue_request_node(login: &str, created_at: &str) -> serde_json::Value {
        serde_json::json!({
            "requestedReviewer": {"__typename": "User", "login": login},
            "createdAt": created_at,
        })
    }

    fn queue_request_enrichment(requests: serde_json::Value, events: serde_json::Value) -> String {
        let mut value: serde_json::Value = serde_json::from_str(ENRICH_SUCCESS).unwrap();
        value["data"]["viewer"] = serde_json::json!({"login": "octocat"});
        value["data"]["r0"]["p4781"]["reviewRequests"] = requests;
        value["data"]["r0"]["p4781"]["timelineItems"] = events;
        value.to_string()
    }

    fn queue_requests(nodes: Vec<serde_json::Value>, after: Option<&str>) -> serde_json::Value {
        serde_json::json!({"nodes": nodes, "pageInfo": {
            "hasNextPage": after.is_some(), "endCursor": after,
        }})
    }

    fn queue_request_events(
        nodes: Vec<serde_json::Value>,
        before: Option<&str>,
    ) -> serde_json::Value {
        serde_json::json!({"nodes": nodes, "pageInfo": {
            "hasPreviousPage": before.is_some(), "startCursor": before,
        }})
    }

    #[test]
    fn queue_request_age_is_the_latest_request_for_the_authenticated_viewer() {
        let old = queue_request_node("octocat", "2026-10-16T00:00:00Z");
        let mine = queue_request_node("OctoCat", "2026-10-18T00:00:00Z");
        let other = queue_request_node("hubot", "2026-10-19T00:00:00Z");
        let enrichment = queue_request_enrichment(
            queue_requests(vec![mine.clone(), other.clone()], None),
            queue_request_events(vec![old, mine, other], None),
        );
        let (client, calls) = rich_client(
            ok(Some(0), QUEUE_SUCCESS, ""),
            vec![ok(Some(0), &enrichment, "")],
        );
        let queue = client.search_prs(&QueueSearch::review_requested()).unwrap();
        assert_eq!(
            queue[0].review_requested_at.as_deref(),
            Some("2026-10-18T00:00:00Z")
        );
        assert_eq!(queue[1].review_requested_at, None);
        assert_eq!(calls.lock().unwrap().len(), 2);
    }

    #[test]
    fn queue_order_uses_enriched_request_times_not_creation_or_activity() {
        // The newer PR has the older request, despite the search returning
        // it second. Sorting must happen after enrichment, not before it.
        let mut hits: serde_json::Value = serde_json::from_str(QUEUE_SUCCESS).unwrap();
        hits.as_array_mut().unwrap().reverse();
        let mut enrichment: serde_json::Value = serde_json::from_str(ENRICH_SUCCESS).unwrap();
        enrichment["data"]["viewer"] = serde_json::json!({"login": "octocat"});
        for (repo, pr, time) in [
            ("r0", "p4761", "2026-10-19T00:00:00Z"),
            ("r1", "p4781", "2026-10-16T00:00:00Z"),
        ] {
            let source_repo = if pr == "p4781" { "r0" } else { "r1" };
            let original: serde_json::Value = serde_json::from_str(ENRICH_SUCCESS).unwrap();
            let mut answer = original["data"][source_repo][pr].clone();
            let node = queue_request_node("octocat", time);
            answer["reviewRequests"] = queue_requests(vec![node.clone()], None);
            answer["timelineItems"] = queue_request_events(vec![node], None);
            enrichment["data"][repo] = serde_json::json!({pr: answer});
        }
        for (sort, expected) in [
            (QueueSort::ReviewRequestedAscending, vec![4781, 4761]),
            (QueueSort::CreatedAscending, vec![4761, 4781]),
        ] {
            let (client, _) = rich_client(
                ok(Some(0), &hits.to_string(), ""),
                vec![ok(Some(0), &enrichment.to_string(), "")],
            );
            let queue = client
                .search_prs(&QueueSearch::review_requested().sorted_by(sort))
                .unwrap();
            assert_eq!(
                queue.iter().map(|pr| pr.number).collect::<Vec<_>>(),
                expected
            );
            assert!(queue.iter().all(|pr| pr.review_requested_at.is_some()));
        }
    }

    #[test]
    fn queue_request_age_does_not_reuse_removed_or_completed_direct_requests() {
        let old = queue_request_node("octocat", "2026-10-16T00:00:00Z");
        let other = queue_request_node("hubot", "2026-10-19T00:00:00Z");
        let team = serde_json::json!({"requestedReviewer": {"__typename": "Team"}});
        for nodes in [vec![], vec![other], vec![team]] {
            let enrichment = queue_request_enrichment(
                queue_requests(nodes, None),
                queue_request_events(vec![old.clone()], None),
            );
            let (client, _) = rich_client(
                ok(Some(0), QUEUE_SUCCESS, ""),
                vec![ok(Some(0), &enrichment, "")],
            );
            let queue = client.search_prs(&QueueSearch::review_requested()).unwrap();
            let pr = queue.iter().find(|pr| pr.number == 4781).unwrap();
            assert_eq!(pr.review_requested_at, None);
            assert_eq!(pr.additions, Some(151));
        }
    }

    #[test]
    fn queue_request_age_paginates_pending_requests_and_filtered_history() {
        let mine = queue_request_node("octocat", "2026-10-16T00:00:00Z");
        let other = queue_request_node("hubot", "2026-10-19T00:00:00Z");
        let enrichment = queue_request_enrichment(
            queue_requests(vec![other.clone()], Some("more-requests")),
            queue_request_events(vec![other], Some("older-events")),
        );
        let requests = serde_json::json!({"data":{"repository":{"pullRequest":{
            "reviewRequests": queue_requests(vec![mine.clone()], None),
        }}}})
        .to_string();
        let events = serde_json::json!({"data":{"repository":{"pullRequest":{
            "timelineItems": queue_request_events(vec![mine], None),
        }}}})
        .to_string();
        let (client, calls) = rich_client(
            ok(Some(0), QUEUE_SUCCESS, ""),
            vec![
                ok(Some(0), &enrichment, ""),
                ok(Some(0), &requests, ""),
                ok(Some(0), &events, ""),
            ],
        );
        let queue = client.search_prs(&QueueSearch::review_requested()).unwrap();
        assert_eq!(
            queue[0].review_requested_at.as_deref(),
            Some("2026-10-16T00:00:00Z")
        );
        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 4);
        assert!(calls[2][3].contains("reviewRequests(first:100,after:\"more-requests\")"));
        assert!(calls[3][3].contains("itemTypes:[REVIEW_REQUESTED_EVENT],before:\"older-events\""));
    }

    #[test]
    fn queue_request_age_stays_unknown_when_history_is_missing_or_unavailable() {
        let mine = queue_request_node("octocat", "2026-10-16T00:00:00Z");
        for events in [
            serde_json::Value::Null,
            queue_request_events(vec![], None),
            queue_request_events(vec![], Some("older")),
        ] {
            let enrichment =
                queue_request_enrichment(queue_requests(vec![mine.clone()], None), events);
            let (client, _) = rich_client(
                ok(Some(0), QUEUE_SUCCESS, ""),
                vec![ok(Some(0), &enrichment, ""), ok(Some(1), "", NETWORK_DOWN)],
            );
            let queue = client.search_prs(&QueueSearch::review_requested()).unwrap();
            let pr = queue.iter().find(|pr| pr.number == 4781).unwrap();
            assert_eq!(pr.review_requested_at, None);
            assert_eq!(pr.additions, Some(151));
        }
    }

    #[test]
    fn enrichment_degrades_gone_repos_and_prs_to_defaults() {
        // A PR that vanished between search and enrichment answers null;
        // a repo GitHub can no longer see answers null for its whole
        // alias. Both rows keep every default — the queue never fails
        // wholesale over one gone answer.
        let (client, _) = rich_client(
            ok(Some(0), QUEUE_SUCCESS, ""),
            vec![ok(Some(0), ENRICH_GONE, "")],
        );
        let queue = client
            .search_prs(&QueueSearch::review_requested())
            .expect("a queue with gone answers is still a queue");
        assert_eq!(queue.len(), 2);
        for item in &queue {
            assert_eq!(item.review_state, ReviewState::Awaiting);
            assert_eq!(item.ci, CiState::NoChecks);
            assert_eq!(item.additions, None);
            assert_eq!(item.deletions, None);
            assert_eq!(item.author_avatar_url, None);
        }
    }

    #[test]
    fn enrichment_failure_leaves_the_queue_usable() {
        // The enrichment's gh call itself failed (here: a rejected token,
        // mid-session). The rows stay, on defaults — a failed batch is
        // not a failed queue.
        let (client, _) = rich_client(
            ok(Some(0), QUEUE_SUCCESS, ""),
            vec![ok(Some(1), "", BAD_TOKEN)],
        );
        let queue = client
            .search_prs(&QueueSearch::review_requested())
            .expect("the search succeeded, so the queue must come back");
        assert_eq!(queue.len(), 2);
        assert_eq!(
            queue[1].title,
            "Add deactivation filters to the users manage list"
        );
        for item in &queue {
            assert_eq!(item.ci, CiState::NoChecks);
            assert_eq!(item.additions, None);
        }
    }

    /// The search payload for a synthetic queue of `count` PRs across two
    /// repos, interleaved — real recorded shape, generated values.
    fn synthetic_queue(count: usize) -> String {
        let hits: Vec<String> = (0..count as u64)
            .map(|number| {
                let repo = if number % 2 == 0 {
                    "octocat/Hello-World"
                } else {
                    "octocat/Spoon-Knife"
                };
                format!(
                    "{{\"author\":{{\"login\":\"octocat\"}},\"isDraft\":false,\
                     \"labels\":[],\"number\":{number},\
                     \"repository\":{{\"nameWithOwner\":\"{repo}\"}},\
                     \"title\":\"pr\",\"createdAt\":\"2026-10-15T00:00:00Z\",\
                     \"updatedAt\":\"2026-10-20T00:00:00Z\"}}"
                )
            })
            .collect();
        format!("[{}]", hits.join(","))
    }

    #[test]
    fn enrichment_batches_big_queues() {
        // 101 queued PRs: one search, then two enrichment queries — the
        // first carries ENRICH_BATCH slots, the second the remainder.
        let (client, calls) = rich_client(
            ok(Some(0), &synthetic_queue(101), ""),
            vec![
                ok(Some(0), "{\"data\":null}", ""),
                ok(Some(0), "{\"data\":null}", ""),
            ],
        );
        let queue = client
            .search_prs(&QueueSearch::review_requested())
            .expect("the generated queue must parse");
        assert_eq!(queue.len(), 101);
        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 3, "one search plus two enrichment batches");
        let batch_sizes: Vec<usize> = calls[1..]
            .iter()
            .map(|call| call[3].matches("pullRequest(number:").count())
            .collect();
        assert_eq!(batch_sizes, vec![100, 1]);
        // Every batch asks about only the repos it still needs.
        assert_eq!(calls[1][3].matches("repository(").count(), 2);
        assert_eq!(calls[2][3].matches("repository(").count(), 1);
    }

    #[test]
    fn enrich_plans_chunk_and_alias() {
        // The planner, pinned on its own: 250 PRs across two interleaved
        // repos become three batches of 100/100/50, each batch's repos
        // aliased from r0 in first-seen order and each PR aliased to its
        // number under its repo.
        let items: Vec<QueueItem> = (0..250)
            .map(|number| QueueItem {
                number,
                title: String::new(),
                author: String::new(),
                author_avatar_url: None,
                repository: if number % 2 == 0 {
                    "octocat/Hello-World".to_string()
                } else {
                    "octocat/Spoon-Knife".to_string()
                },
                is_draft: false,
                review_state: ReviewState::Awaiting,
                ci: CiState::NoChecks,
                additions: None,
                deletions: None,
                comments: 0,
                labels: Vec::new(),
                created_at: String::new(),
                review_requested_at: None,
                updated_at: String::new(),
            })
            .collect();
        let plans = enrich_plans(&items);
        assert_eq!(
            plans.iter().map(|plan| plan.prs.len()).collect::<Vec<_>>(),
            vec![100, 100, 50]
        );
        let first = &plans[0];
        assert_eq!(first.repos.len(), 2);
        assert_eq!(first.repos[0].alias, "r0");
        assert_eq!(first.repos[0].owner, "octocat");
        assert_eq!(first.repos[0].name, "Hello-World");
        assert_eq!(first.repos[1].alias, "r1");
        // Each batch re-aliases its repos from r0 — aliases never leak
        // across batches.
        assert_eq!(plans[2].repos[0].alias, "r0");
        // PR aliases are the PR's number, and the item index points back
        // at the search's order.
        let slot = &first.prs[0];
        assert_eq!(slot.pr_alias, "p0");
        assert_eq!(slot.item, 0);
        assert_eq!(slot.repo_alias, "r0");
        // A PR whose repository isn't owner/name sits the enrichment out.
        let malformed = QueueItem {
            repository: "not-a-slug".to_string(),
            ..items[0].clone()
        };
        assert!(enrich_plans(std::slice::from_ref(&malformed)).is_empty());
    }

    /// Live smoke check against the real `gh` on this machine — not part of
    /// the recorded set. Run explicitly with `cargo test -p github --
    /// --ignored` whenever the CLI contract is in question.
    #[test]
    #[ignore = "needs gh installed, authenticated, and network"]
    fn the_real_backend_sees_the_signed_in_user() {
        let viewer = GhCliClient::new()
            .viewer()
            .expect("gh on this machine is installed and signed in");
        assert!(!viewer.login.is_empty());
        assert!(viewer.avatar_url.starts_with("https://"));
    }

    /// Live smoke check of the queue search against the real `gh` on this
    /// machine — not part of the recorded set. Run explicitly with
    /// `cargo test -p github -- --ignored` whenever the search contract is
    /// in question.
    #[test]
    #[ignore = "needs gh installed, authenticated, and network"]
    fn the_real_backend_runs_the_real_queue_search() {
        let queue = GhCliClient::new()
            .search_prs(&QueueSearch::review_requested())
            .expect("gh on this machine is installed and signed in");
        let mut sorted = queue.clone();
        QueueSearch::review_requested().sort_items(&mut sorted);
        assert_eq!(
            queue, sorted,
            "the queue must come back oldest request first"
        );
        // The live enrichment answered: a non-empty queue comes back with
        // its check-sized fields on at least one row (all-null answers are
        // the degrade path, and would mean the live contract broke).
        if !queue.is_empty() {
            assert!(
                queue.iter().any(|pr| pr.additions.is_some()),
                "with PRs queued, at least one must carry its diff size"
            );
        }
    }

    /// Live smoke check that chips narrow on real data — not part of the
    /// recorded set. Run explicitly with `cargo test -p github --
    /// --ignored` whenever the search contract is in question.
    #[test]
    #[ignore = "needs gh installed, authenticated, and network"]
    fn the_real_backend_narrows_by_repo() {
        let client = GhCliClient::new();
        let full = client
            .search_prs(&QueueSearch::review_requested())
            .expect("gh on this machine is installed and signed in");
        if full.len() < 2 {
            return; // nothing to narrow on this account today
        }
        let repository = full[0].repository.clone();
        let narrowed = client
            .search_prs(
                &QueueSearch::review_requested().filtered_by(QueueFilter::repo(repository.clone())),
            )
            .expect("the narrowed search must succeed like the full one");
        assert!(narrowed.iter().all(|pr| pr.repository == repository));
        assert!(!narrowed.is_empty());
        // Narrowing means fewer rows whenever the repository isn't the whole queue.
        if full.iter().any(|pr| pr.repository != repository) {
            assert!(narrowed.len() < full.len());
        }
    }

    // --- resolve_pr --------------------------------------------------------

    #[test]
    fn resolve_pr_parses_the_slug_form() {
        // No invocation is scripted: a slug needs no round trip, and a
        // stray subprocess would fail the fake below.
        let client = GhCliClient::with_process(FakeGh::many(vec![]));
        assert_eq!(
            client.resolve_pr("zed-industries/zed#12345").unwrap(),
            PrLocator {
                owner: "zed-industries".into(),
                repo: "zed".into(),
                number: 12345,
            }
        );
    }

    #[test]
    fn resolve_pr_parses_the_url_form() {
        let client = GhCliClient::with_process(FakeGh::many(vec![]));
        let loc = client
            .resolve_pr("https://github.com/rust-lang/rust/pull/99999/files")
            .unwrap();
        assert_eq!(loc.owner, "rust-lang");
        assert_eq!(loc.repo, "rust");
        assert_eq!(loc.number, 99999);
    }

    #[test]
    fn resolve_pr_rejects_garbage_as_invalid() {
        // The one error that isn't a GitHub failure: the message names the
        // expected form, never the sign-in fix.
        let client = GhCliClient::with_process(FakeGh::many(vec![]));
        let err = client.resolve_pr("not-a-pr").unwrap_err();
        assert!(
            matches!(err, ClientError::Invalid(ref cause) if cause.contains("expected owner/repo#123")),
            "{err:?}"
        );
    }

    #[test]
    fn resolve_pr_resolves_a_bare_number_through_the_cwd_repo() {
        let fake = FakeGh::one(ok(Some(0), "octocat/Hello-World\n", ""));
        let calls = fake.calls_handle();
        assert_eq!(
            GhCliClient::with_process(fake).resolve_pr("#123").unwrap(),
            PrLocator {
                owner: "octocat".into(),
                repo: "Hello-World".into(),
                number: 123,
            }
        );
        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0].join(" "),
            "repo view --json nameWithOwner --jq .nameWithOwner"
        );
    }

    #[test]
    fn resolve_pr_keeps_the_explicit_form_hint_when_the_cwd_has_no_repo() {
        let err = client(ok(Some(1), "", "gh: not a git repository"))
            .resolve_pr("123")
            .unwrap_err();
        assert!(
            matches!(err, ClientError::Failed(ref cause) if cause.contains("use owner/repo#123")),
            "{err:?}"
        );
    }

    // --- PR metadata & listing ----------------------------------------------

    #[test]
    fn pr_meta_parses_the_recorded_payload() {
        let meta = client(ok(Some(0), PR_VIEW_SUCCESS, ""))
            .pr_meta(&loc())
            .unwrap();
        assert_eq!(meta.number, 4781);
        assert_eq!(
            meta.title,
            "Add deactivation filters to the users manage list"
        );
        assert_eq!(meta.author.login, "octocat");
        assert_eq!(meta.state, "OPEN");
        assert_eq!(meta.url, "https://github.com/octocat/Hello-World/pull/4781");
        assert_eq!(meta.body, "Adds the filters discussed in #4778.");
        assert_eq!(meta.base_ref_name, "main");
        assert_eq!(meta.head_ref_name, "deactivation-filters");
        assert_eq!(
            meta.base_ref_oid,
            "f16ea0a8cfd0fbb0328b8348972356d532b921d0"
        );
        assert_eq!(
            meta.head_ref_oid,
            "a66406e5daa6e8b7c15b05f3b8b1c9d2f04d0e11"
        );
        assert_eq!(
            (meta.additions, meta.deletions, meta.changed_files),
            (151, 30, 3)
        );
        assert_eq!(meta.review_decision, "CHANGES_REQUESTED");
    }

    #[test]
    fn pr_meta_tolerates_an_older_gh_without_review_decision() {
        // gh builds that predate the field must still deserialize — the
        // decision degrades to "", the same no-decision vocabulary the
        // titlebar treats as no tag.
        let json = r#"{
            "number": 1, "title": "t", "author": {"login": "a"}, "state": "OPEN",
            "url": "https://github.com/o/r/pull/1",
            "baseRefName": "main", "headRefName": "feat",
            "baseRefOid": "abc123", "headRefOid": "def456",
            "additions": 1, "deletions": 2, "changedFiles": 3
        }"#;
        let meta = client(ok(Some(0), json, "")).pr_meta(&loc()).unwrap();
        assert_eq!(meta.review_decision, "");
    }

    #[test]
    fn pr_meta_invokes_gh_pr_view_verbatim() {
        let fake = FakeGh::one(ok(Some(0), PR_VIEW_SUCCESS, ""));
        let calls = fake.calls_handle();
        GhCliClient::with_process(fake).pr_meta(&loc()).unwrap();
        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0].join(" "),
            "pr view 4781 --repo octocat/Hello-World --json \
             number,title,author,state,url,body,baseRefName,headRefName,baseRefOid,headRefOid,\
             additions,deletions,changedFiles,reviewDecision"
        );
    }

    #[test]
    fn pr_meta_reports_gh_signed_out_as_unauthenticated() {
        // The classification is shared with the auth check, so one replay
        // pins it for the PR operations too.
        let err = client(ok(Some(4), "", UNAUTHENTICATED))
            .pr_meta(&loc())
            .unwrap_err();
        assert_eq!(err, ClientError::Unauthenticated);
    }

    // --- patch, comments, mentions ------------------------------------------

    #[test]
    fn pr_patch_returns_the_recorded_diff() {
        let patch = client(ok(Some(0), "diff --git a/a.rs b/a.rs\n", ""))
            .pr_patch(&loc())
            .unwrap();
        assert!(patch.starts_with("diff --git"));
    }

    #[test]
    fn pr_patch_invokes_gh_pr_diff_verbatim() {
        let fake = FakeGh::one(ok(Some(0), "", ""));
        let calls = fake.calls_handle();
        GhCliClient::with_process(fake).pr_patch(&loc()).unwrap();
        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0].join(" "),
            "pr diff 4781 --repo octocat/Hello-World"
        );
    }

    #[test]
    fn review_comments_flattens_the_recorded_pages() {
        let (client, _) = session_script(&[PULLS_COMMENTS_SUCCESS, EMPTY_THREADS]);
        let comments = client.review_comments(&loc()).unwrap();
        assert_eq!(comments.len(), 3);
        let top = &comments[0];
        assert_eq!(top.id, 100);
        assert_eq!(top.path, "src/main.rs");
        assert_eq!(top.line, Some(42));
        assert_eq!(top.side.as_deref(), Some("RIGHT"));
        assert_eq!(top.start_line, Some(40));
        assert_eq!(top.user.login, "alice");
        assert_eq!(top.in_reply_to_id, None);
        let reply = &comments[1];
        assert_eq!(reply.in_reply_to_id, Some(100));
        assert_eq!(reply.start_line, None);
        // Outdated: null line/side, and a missing in_reply_to_id key.
        let outdated = &comments[2];
        assert_eq!(outdated.line, None);
        assert_eq!(outdated.side, None);
        assert_eq!(outdated.in_reply_to_id, None);
    }

    #[test]
    fn review_comments_invokes_the_paginated_slurp_verbatim() {
        let (client, calls) = session_script(&[PULLS_COMMENTS_SUCCESS, EMPTY_THREADS]);
        client.review_comments(&loc()).unwrap();
        let calls = calls.lock().unwrap();
        assert_eq!(
            calls.len(),
            2,
            "null REST positions are checked against preview threads"
        );
        assert_eq!(
            calls[0].join(" "),
            "api --paginate --slurp \
             repos/octocat/Hello-World/pulls/4781/comments?per_page=100"
        );
    }

    #[test]
    fn mentionable_users_follow_the_pagination_cursor() {
        // Two pages: the first ends with a cursor the second call passes
        // back as `after` — and the merged pool keeps each page's order.
        let page = |cursor: &str, logins: &str, has_next: bool| {
            format!(
                r#"{{"data":{{"repository":{{"mentionableUsers":{{"nodes":[{logins}],"pageInfo":{{"hasNextPage":{has_next},"endCursor":"{cursor}"}}}}}}}}}}"#
            )
        };
        let (client, calls) = rich_client(
            ok(
                Some(0),
                &page("c1", r#"{"login":"alice","name":"Alice"}"#, true),
                "",
            ),
            vec![ok(Some(0), &page("c2", r#"{"login":"bob"}"#, false), "")],
        );
        let users = client.mentionable_users(&loc()).unwrap();
        assert_eq!(users.len(), 2);
        assert_eq!(users[0].login, "alice");
        assert_eq!(users[0].name.as_deref(), Some("Alice"));
        assert_eq!(users[1].login, "bob");
        assert_eq!(users[1].name, None);
        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 2, "one gh api graphql per page");
        assert!(calls[0][3].starts_with("query=query($owner:String!"));
        assert_eq!(calls[0][5], "owner=octocat");
        assert_eq!(calls[0][7], "repo=Hello-World");
        assert_eq!(calls[1][9], "after=c1");
    }

    // --- posting -------------------------------------------------------------

    #[test]
    fn conversation_comment_uses_issue_comments_not_review_comments() {
        use crate::review_session::{Command, ReviewSession};
        let (client, calls) = session_script(&[r#"{"id":123}"#]);
        let mut session = ReviewSession::default();
        let receipt = session
            .execute_with_receipt(
                &client,
                &loc(),
                Command::Conversation {
                    body: "  hello\n@octocat  ".into(),
                },
            )
            .unwrap();
        assert_eq!(receipt, Some(123));
        assert!(session.pending.is_none());
        let calls = calls.lock().unwrap();
        assert_eq!(
            calls.len(),
            1,
            "immediate comments never start or submit a review"
        );
        assert_eq!(
            calls[0],
            [
                "api",
                "-X",
                "POST",
                "repos/octocat/Hello-World/issues/4781/comments",
                "-f",
                "body=hello\n@octocat"
            ]
        );
    }

    #[test]
    fn conversation_reply_returns_the_actual_server_comment_identity() {
        use crate::review_session::{Command, ReviewSession};
        let (client, calls) = session_script(&[SUCCESS, "[]", r#"{"id":5000000000}"#]);
        let receipt = ReviewSession::default()
            .execute_with_receipt(
                &client,
                &loc(),
                Command::ConversationReply {
                    comment_id: 11,
                    commit_id: "head".into(),
                    body: "reply".into(),
                    pending: false,
                },
            )
            .unwrap();
        assert_eq!(receipt, Some(5_000_000_000));
        assert_eq!(
            calls.lock().unwrap()[2],
            [
                "api",
                "-X",
                "POST",
                "repos/octocat/Hello-World/pulls/4781/comments/11/replies",
                "-f",
                "body=reply"
            ]
        );
    }

    #[test]
    fn conversation_summary_starts_a_draft_without_publishing() {
        use crate::review_session::{Command, ReviewSession};
        let (client, calls) = session_script(&[
            SUCCESS,
            "[]",
            r#"{"id":91,"node_id":"PRR_pending","body":""}"#,
            r#"{"id":91,"body":"summary"}"#,
        ]);
        let mut session = ReviewSession::default();
        session
            .execute(
                &client,
                &loc(),
                Command::AddSummary {
                    commit_id: "head".into(),
                    body: " summary ".into(),
                },
            )
            .unwrap();
        assert_eq!(session.pending.as_ref().unwrap().body, "summary");
        assert_eq!(
            session.count(),
            0,
            "summary text is not a fake inline comment"
        );
        let calls = calls.lock().unwrap();
        assert_eq!(
            calls[2],
            [
                "api",
                "-X",
                "POST",
                "repos/octocat/Hello-World/pulls/4781/reviews",
                "-f",
                "commit_id=head"
            ]
        );
        assert_eq!(
            calls[3],
            [
                "api",
                "-X",
                "PUT",
                "repos/octocat/Hello-World/pulls/4781/reviews/91",
                "-f",
                "body=summary"
            ]
        );
        assert!(!calls
            .iter()
            .any(|call| call.join(" ").contains("submitPullRequestReview")));
    }

    #[test]
    fn conversation_summary_reconciles_and_preserves_web_body_and_drafts() {
        use crate::review_session::{Command, ReviewSession};
        let mut pending: serde_json::Value = serde_json::from_str(PENDING).unwrap();
        pending[0][0]["body"] = serde_json::json!("web summary");
        let (client, calls) = session_script(&[
            SUCCESS,
            &pending.to_string(),
            &pending_threads(&[DRAFT]),
            r#"{"id":91}"#,
        ]);
        let mut session = ReviewSession::default();
        session
            .execute(
                &client,
                &loc(),
                Command::AddSummary {
                    commit_id: "head".into(),
                    body: "extra".into(),
                },
            )
            .unwrap();
        let review = session.pending.as_ref().unwrap();
        assert_eq!(review.body, "web summary\n\nextra");
        assert_eq!(review.comments.len(), 1);
        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 4);
        assert_eq!(calls[3].last().unwrap(), "body=web summary\n\nextra");
    }

    #[test]
    fn conversation_failed_summary_retains_reconciled_draft_without_fallback_post() {
        use crate::review_session::{Command, ReviewSession};
        let (client, calls) =
            session_script(&[SUCCESS, PENDING, &pending_threads(&[DRAFT]), r#"{}"#]);
        let mut session = ReviewSession::default();
        assert!(session
            .execute(
                &client,
                &loc(),
                Command::AddSummary {
                    commit_id: "head".into(),
                    body: "extra".into()
                }
            )
            .is_err());
        let review = session.pending.as_ref().unwrap();
        assert_eq!(review.body, "web summary");
        assert_eq!(review.comments.len(), 1);
        assert_eq!(calls.lock().unwrap().len(), 4);
    }

    #[test]
    fn empty_conversation_compositions_never_touch_github() {
        use crate::review_session::{Command, ReviewSession};
        let (client, calls) = session_script(&[]);
        for command in [
            Command::Conversation {
                body: " \n ".into(),
            },
            Command::AddSummary {
                commit_id: "head".into(),
                body: " ".into(),
            },
            Command::ConversationReply {
                comment_id: 11,
                commit_id: "head".into(),
                body: "".into(),
                pending: true,
            },
        ] {
            assert!(ReviewSession::default()
                .execute(&client, &loc(), command)
                .is_err());
        }
        assert!(calls.lock().unwrap().is_empty());
    }

    #[test]
    fn post_review_comment_invokes_the_api_verbatim() {
        let fake = FakeGh::one(ok(Some(0), "", ""));
        let calls = fake.calls_handle();
        GhCliClient::with_process(fake)
            .post_review_comment(&loc(), "a66406e", "src/main.rs", "RIGHT", 42, "nice")
            .unwrap();
        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0].join(" "),
            "api -X POST repos/octocat/Hello-World/pulls/4781/comments \
             -f body=nice -f commit_id=a66406e -f path=src/main.rs -f side=RIGHT -F line=42"
        );
    }

    #[test]
    fn post_review_comment_reports_ghs_cause() {
        // A 403 usually means a missing token scope, a 422 an unanchorable
        // line — either way the user sees gh's own first stderr line, and
        // never a sign-in hint for a permission problem.
        let err = client(ok(Some(1), "", "gh: Forbidden (HTTP 403)"))
            .post_review_comment(&loc(), "a66406e", "src/main.rs", "RIGHT", 42, "nice")
            .unwrap_err();
        assert_eq!(err, ClientError::Failed("Forbidden (HTTP 403)".to_string()));
    }

    #[test]
    fn post_reply_invokes_the_api_verbatim() {
        let fake = FakeGh::one(ok(Some(0), "", ""));
        let calls = fake.calls_handle();
        GhCliClient::with_process(fake)
            .post_reply(&loc(), 100, "+1")
            .unwrap();
        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0].join(" "),
            "api -X POST repos/octocat/Hello-World/pulls/4781/comments/100/replies \
             -f body=+1"
        );
    }

    #[test]
    fn submit_review_invokes_gh_pr_review_verbatim() {
        // Approvals may be bodyless; the other two verdicts carry --body —
        // exactly the flags the caller's own emptiness rule assumes.
        let (client, calls) = rich_client(
            ok(Some(0), "", ""),
            vec![ok(Some(0), "", ""), ok(Some(0), "", "")],
        );
        client
            .submit_review(&loc(), ReviewVerdict::Approve, "")
            .unwrap();
        client
            .submit_review(&loc(), ReviewVerdict::RequestChanges, "please no")
            .unwrap();
        client
            .submit_review(&loc(), ReviewVerdict::Comment, "fyi")
            .unwrap();
        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 3);
        assert_eq!(
            calls[0].join(" "),
            "pr review 4781 --repo octocat/Hello-World --approve"
        );
        assert_eq!(
            calls[1].join(" "),
            "pr review 4781 --repo octocat/Hello-World --request-changes --body please no"
        );
        assert_eq!(
            calls[2].join(" "),
            "pr review 4781 --repo octocat/Hello-World --comment --body fyi"
        );
    }

    // --- file_at and its cache -----------------------------------------------

    #[test]
    fn file_paths_encode_per_segment_keeping_slashes() {
        assert_eq!(encode_path("src/main.rs"), "src/main.rs");
        assert_eq!(
            encode_path("dir with space/naïve+file#1.rs"),
            "dir%20with%20space/na%C3%AFve%2Bfile%231.rs"
        );
        assert_eq!(encode_path("a?b&c=d/e%f"), "a%3Fb%26c%3Dd/e%25f");
        assert_eq!(encode_path("A-Z_a.z~0/9"), "A-Z_a.z~0/9");
    }

    #[test]
    fn cache_key_is_stable() {
        // Pinned: changing this constant silently invalidates every user's
        // on-disk cache. The components are NUL-separated so `("a/b", "c")`
        // and `("a", "b/c")` can't collide.
        assert_eq!(
            cache_key(
                "BurntSushi/ripgrep",
                "f16ea0a8cfd0fbb0328b8348972356d532b921d0",
                "crates/core/main.rs"
            ),
            "1b21e76f45d6d948cf7b44c696608c64bdaeee714ac61b21dce21750ad9cb6bc"
        );
        assert_ne!(
            cache_key("o/r", "oid", "a/b"),
            cache_key("o/r/a", "oid", "b")
        );
    }

    #[test]
    fn file_at_absent_binary_oversize_and_cached_answers_replay_the_contract() {
        // The blob cache lives under $HOME, and only this test reads or
        // writes it: swap HOME for a tempdir under a lock, and nothing
        // else in this suite notices.
        static HOME: Mutex<()> = Mutex::new(());
        let _guard = HOME.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let old_home = std::env::var_os("HOME");
        std::env::set_var("HOME", tmp.path());

        const OID: &str = "f16ea0a8cfd0fbb0328b8348972356d532b921d0";
        let fake = FakeGh::many(vec![
            // gone.rs: absent at that commit (404).
            ok(Some(1), "", "gh: Not Found (HTTP 404)"),
            // main.rs: present, plain text.
            ok(Some(0), "fn main() {}\n", ""),
            // blob.bin: binary bytes.
            raw(Some(0), &[0u8, 159, 146, 150], ""),
            // huge.txt: over the blob cap.
            raw(Some(0), &vec![b'x'; MAX_BLOB_BYTES + 1], ""),
            // denied.txt: a real failure, not an absence — twice, because
            // failures are never cached.
            ok(Some(1), "", "gh: Forbidden (HTTP 403)"),
            ok(Some(1), "", "gh: Forbidden (HTTP 403)"),
        ]);
        let calls = fake.calls_handle();
        let client = GhCliClient::with_process(fake);

        // Absent: Ok(None), and the invocation carries the raw-contents
        // Accept header with the path percent-encoded per segment.
        assert_eq!(client.file_at(&loc(), OID, "gone.rs").unwrap(), None);
        {
            let calls = calls.lock().unwrap();
            assert_eq!(
                calls[0].join(" "),
                format!(
                    "api -H Accept: application/vnd.github.raw+json \
                     repos/octocat/Hello-World/contents/gone.rs?ref={OID}"
                )
            );
        }
        // The negative answer is cached: no second subprocess.
        assert_eq!(client.file_at(&loc(), OID, "gone.rs").unwrap(), None);

        // Present: the text, cached on disk…
        assert_eq!(
            client.file_at(&loc(), OID, "main.rs").unwrap().as_deref(),
            Some("fn main() {}\n")
        );
        // …so a second call never leaves the process.
        assert_eq!(
            client.file_at(&loc(), OID, "main.rs").unwrap().as_deref(),
            Some("fn main() {}\n")
        );

        // Binary and oversize are absences too — the file stays
        // patch-derived rather than upgrading garbage.
        assert_eq!(client.file_at(&loc(), OID, "blob.bin").unwrap(), None);
        assert_eq!(client.file_at(&loc(), OID, "huge.txt").unwrap(), None);

        // Any other failure is an error, and never cached.
        let err = client.file_at(&loc(), OID, "denied.txt").unwrap_err();
        assert_eq!(err, ClientError::Failed("Forbidden (HTTP 403)".to_string()));
        assert_eq!(
            client.file_at(&loc(), OID, "denied.txt").unwrap_err(),
            ClientError::Failed("Forbidden (HTTP 403)".to_string())
        );

        let calls = calls.lock().unwrap();
        assert_eq!(
            calls.len(),
            6,
            "cache hits and negative cache never spawn gh"
        );

        match old_home {
            Some(home) => std::env::set_var("HOME", home),
            None => std::env::remove_var("HOME"),
        }
    }
}
