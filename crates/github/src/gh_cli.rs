//! The v0.1 [`GitHubClient`](crate::GitHubClient) backend: `gh` CLI
//! subprocesses, JSON over the process boundary.
//!
//! Subprocess mechanics stop at the [`GhProcess`] trait. The domain logic —
//! which `gh` invocation answers which facade operation, how its JSON is
//! parsed, how failures map to [`ClientError`] — is plain code, tested by
//! replaying recorded `gh` output through a fake `GhProcess` (fixtures in
//! `fixtures/`, each byte-real captured output, so tests fail when the CLI's
//! reality drifts).

use std::collections::HashMap;
use std::io;
use std::path::PathBuf;
use std::process::Command;

use sha2::{Digest, Sha256};

use crate::{
    CiState, ClientError, GitHubClient, Mention, PrLocator, PrMeta, PrSummary, QueueItem,
    QueueLabel, QueueSearch, ReviewComment, ReviewState, ReviewVerdict, Viewer,
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
            apply_enrichment(&mut items, &plan, stdout);
        }
        Ok(items)
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

    fn list_prs(&self, owner: &str, repo: &str) -> Result<Vec<PrSummary>, ClientError> {
        let slug = format!("{owner}/{repo}");
        let out = self
            .process
            .run(&[
                "pr",
                "list",
                "--repo",
                &slug,
                "--state",
                "open",
                "--limit",
                "200",
                "--json",
                "number,title,author,state,isDraft,headRefName,updatedAt",
            ])
            .map_err(spawn_failure)?;
        if out.code != Some(0) {
            return Err(classify_failure(&out));
        }
        serde_json::from_str(out.text()?)
            .map_err(|err| ClientError::Failed(format!("unexpected GitHub response: {err}")))
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
        Ok(pages.into_iter().flatten().collect())
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

/// gh's search caps one query at 1000 results — GitHub's own search
/// ceiling — and pages behind the scenes, so the cap costs nothing on a
/// short queue while a long one is never silently truncated.
const SEARCH_LIMIT: &str = "1000";

/// The queue's `--json` field list: exactly what a [`QueueItem`] is built
/// from, no more. The check-sized fields (review decision, CI, diff size,
/// avatar) ride the enrichment pass instead — a search can't carry them.
const QUEUE_JSON_FIELDS: &str =
    "number,title,author,repository,isDraft,updatedAt,commentsCount,labels";

/// How many queued PRs one enrichment query carries. gh's search caps at
/// 1000 results, so batches of 100 mean at most ten extra round trips for
/// the fullest queue — a "dozens of PRs" queue stays exactly one — while
/// no single query grows unbounded.
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
/// and the author's avatar.
fn enrich_query(plan: &EnrichPlan) -> String {
    let mut query = String::from("query {");
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
                     author {{ avatarUrl(size: 64) }} }}",
                    alias = pr.pr_alias,
                    number = pr.number
                ));
            }
        }
        query.push_str(" }");
    }
    query.push_str(" }");
    query
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
struct EnrichDataJson(HashMap<String, Option<EnrichRepoJson>>);

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
fn apply_enrichment(items: &mut [QueueItem], plan: &EnrichPlan, stdout: &str) {
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
            .0
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
    }
}

/// The exact `gh` invocation a queue search compiles to. The search string
/// becomes one argument *per qualifier* — a single quoted string is
/// keyword-searched, not qualifier-parsed — chip qualifiers included,
/// followed by the sort spec. The query is owned (chips compile into it),
/// so the arguments are too.
fn search_prs_args(search: &QueueSearch) -> Vec<String> {
    let (sort, order) = search.sort();
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

    /// The fake `gh` backend: each invocation pops the next recorded outcome,
    /// and an invocation beyond what the test scripted fails — a stray
    /// subprocess would be a behavior change worth noticing. Every
    /// invocation's argv is recorded, so a test can pin the exact `gh`
    /// command an operation compiles to.
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
    const PR_LIST_SUCCESS: &str = include_str!("../fixtures/gh_pr_list_success.json");
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
        let first = &queue[0];
        assert_eq!(first.number, 4781);
        assert_eq!(
            first.title,
            "Add deactivation filters to the users manage list"
        );
        assert_eq!(first.author, "octocat");
        assert_eq!(first.repository, "octocat/Hello-World");
        assert!(first.is_draft);
        assert_eq!(first.updated_at, "2026-10-20T08:07:08Z");
        // Rich search fields: conversation count, labels as GitHub paints them.
        assert_eq!(first.comments, 2);
        assert_eq!(first.labels.len(), 1);
        assert_eq!(first.labels[0].name, "api");
        assert_eq!(first.labels[0].color, "1D76DB");
        let second = &queue[1];
        assert_eq!(second.repository, "octocat/Spoon-Knife");
        assert!(!second.is_draft);
        assert_eq!(second.comments, 1);
        assert_eq!(second.labels.len(), 3);
        assert_eq!(second.labels[2].name, "migrations");
        // Order preserved as recorded: the compiled sort (updated, desc)
        // already ran server-side.
        assert!(first.updated_at > second.updated_at);
        // Honest defaults until an enrichment answers for each PR.
        for item in &queue {
            assert_eq!(item.review_state, ReviewState::Awaiting);
            assert_eq!(item.ci, CiState::NoChecks);
            assert_eq!(item.additions, None);
            assert_eq!(item.deletions, None);
            assert_eq!(item.author_avatar_url, None);
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
             --sort updated --order desc --limit 1000 \
             --json number,title,author,repository,isDraft,updatedAt,commentsCount,labels"
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
                    .filtered_by(QueueFilter::author("octocat"))
                    .sorted_by(QueueSort::OldestUpdated),
            )
            .expect("the recorded empty queue must parse");
        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 1, "chips and sort never add a round trip");
        assert_eq!(
            calls[0].join(" "),
            "search prs is:pr is:open review-requested:@me org:acme repo:acme/widgets \
             author:octocat --sort updated --order asc --limit 1000 \
             --json number,title,author,repository,isDraft,updatedAt,commentsCount,labels"
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
        let first = &queue[0];
        assert_eq!(first.number, 4781);
        assert_eq!(first.additions, Some(151));
        assert_eq!(first.deletions, Some(30));
        assert_eq!(first.review_state, ReviewState::Awaiting); // REVIEW_REQUIRED
        assert_eq!(first.ci, CiState::Pass);
        assert_eq!(
            first.author_avatar_url.as_deref(),
            Some("https://avatars.githubusercontent.com/u/583231?s=64&v=4")
        );
        let second = &queue[1];
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
            "api graphql -f query=query { \
             r0: repository(owner: \"octocat\", name: \"Hello-World\") { \
             p4781: pullRequest(number: 4781) { additions deletions reviewDecision \
             statusCheckRollup { state } author { avatarUrl(size: 64) } } } \
             r1: repository(owner: \"octocat\", name: \"Spoon-Knife\") { \
             p4761: pullRequest(number: 4761) { additions deletions reviewDecision \
             statusCheckRollup { state } author { avatarUrl(size: 64) } } } }"
        );
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
            queue[0].title,
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
                     \"title\":\"pr\",\"updatedAt\":\"2026-10-20T00:00:00Z\"}}"
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
        // The compiled sort holds on real data: most recently updated first
        // (ISO 8601 timestamps in one format compare chronologically).
        for pair in queue.windows(2) {
            assert!(
                pair[0].updated_at >= pair[1].updated_at,
                "the queue must come back recently-updated first"
            );
        }
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
    fn the_real_backend_narrows_by_author() {
        let client = GhCliClient::new();
        let full = client
            .search_prs(&QueueSearch::review_requested())
            .expect("gh on this machine is installed and signed in");
        if full.len() < 2 {
            return; // nothing to narrow on this account today
        }
        let author = full[0].author.clone();
        let narrowed = client
            .search_prs(
                &QueueSearch::review_requested().filtered_by(QueueFilter::author(author.clone())),
            )
            .expect("the narrowed search must succeed like the full one");
        assert!(narrowed.iter().all(|pr| pr.author == author));
        assert!(!narrowed.is_empty());
        // Narrowing means fewer rows whenever the author isn't the whole
        // queue.
        if full.iter().any(|pr| pr.author != author) {
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

    #[test]
    fn list_prs_parses_the_recorded_payload() {
        let prs = client(ok(Some(0), PR_LIST_SUCCESS, ""))
            .list_prs("octocat", "Hello-World")
            .unwrap();
        assert_eq!(prs.len(), 2);
        assert_eq!(prs[0].number, 4781);
        assert_eq!(
            prs[0].title,
            "Add deactivation filters to the users manage list"
        );
        assert_eq!(prs[0].author.login, "octocat");
        assert_eq!(prs[0].state, "OPEN");
        assert!(!prs[0].is_draft);
        assert_eq!(prs[0].head_ref_name, "deactivation-filters");
        assert_eq!(prs[0].updated_at, "2026-10-20T08:07:08Z");
        assert!(prs[1].is_draft);
    }

    #[test]
    fn list_prs_invokes_gh_pr_list_verbatim() {
        let fake = FakeGh::one(ok(Some(0), PR_LIST_SUCCESS, ""));
        let calls = fake.calls_handle();
        GhCliClient::with_process(fake)
            .list_prs("octocat", "Hello-World")
            .unwrap();
        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0].join(" "),
            "pr list --repo octocat/Hello-World --state open --limit 200 \
             --json number,title,author,state,isDraft,headRefName,updatedAt"
        );
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
        let comments = client(ok(Some(0), PULLS_COMMENTS_SUCCESS, ""))
            .review_comments(&loc())
            .unwrap();
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
        let fake = FakeGh::one(ok(Some(0), PULLS_COMMENTS_SUCCESS, ""));
        let calls = fake.calls_handle();
        GhCliClient::with_process(fake)
            .review_comments(&loc())
            .unwrap();
        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
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
