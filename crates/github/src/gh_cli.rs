//! The v0.1 [`GitHubClient`](crate::GitHubClient) backend: `gh` CLI
//! subprocesses, JSON over the process boundary.
//!
//! Subprocess mechanics stop at the [`GhProcess`] trait. The domain logic —
//! which `gh` invocation answers which facade operation, how its JSON is
//! parsed, how failures map to [`ClientError`] — is plain code, tested by
//! replaying recorded `gh` output through a fake `GhProcess` (fixtures in
//! `fixtures/`, each byte-real captured output, so tests fail when the CLI's
//! reality drifts).

use std::io;
use std::process::Command;

use crate::{ClientError, GitHubClient, Viewer};

/// One `gh` invocation exactly as the process boundary sees it.
#[derive(Debug, Clone)]
pub struct GhOutput {
    /// The exit code; `None` when the process died to a signal.
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
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
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
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
        let out = self
            .process
            .run(&["api", "user"])
            .map_err(spawn_failure)?;
        if out.code != Some(0) {
            return Err(classify_failure(&out));
        }
        parse_viewer(&out.stdout)
    }
}

impl<P: GhProcess> GitHubClient for GhCliClient<P> {
    fn viewer(&self) -> Result<Viewer, ClientError> {
        self.api_user()
    }
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
/// recognizable two ways, either suffices: gh's documented auth exit code 4,
/// or its `gh auth login` hint on stderr; a rejected token reports itself as
/// HTTP 401. Anything else keeps gh's own cause.
fn classify_failure(out: &GhOutput) -> ClientError {
    if out.code == Some(4)
        || out.stderr.contains("gh auth login")
        || out.stderr.contains("HTTP 401")
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    /// The fake `gh` backend: each invocation pops the next recorded outcome,
    /// and an invocation beyond what the test scripted fails — a stray
    /// subprocess would be a behavior change worth noticing.
    struct FakeGh {
        results: Mutex<VecDeque<Result<GhOutput, io::Error>>>,
    }

    impl FakeGh {
        fn one(result: Result<GhOutput, io::Error>) -> Self {
            Self {
                results: Mutex::new(VecDeque::from([result])),
            }
        }
    }

    impl GhProcess for FakeGh {
        fn run(&self, _args: &[&str]) -> io::Result<GhOutput> {
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

    fn ok(code: Option<i32>, stdout: &str, stderr: &str) -> Result<GhOutput, io::Error> {
        Ok(GhOutput {
            code,
            stdout: stdout.to_string(),
            stderr: stderr.to_string(),
        })
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

    #[test]
    fn viewer_parses_the_recorded_user_payload() {
        let viewer = client(ok(Some(0), SUCCESS, ""))
            .viewer()
            .expect("the recorded success payload must parse");
        assert_eq!(viewer.login, "octocat");
        assert_eq!(viewer.avatar_url, "https://avatars.githubusercontent.com/u/583231?v=4");
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
        let err = client(ok(Some(1), "", NETWORK_DOWN))
            .viewer()
            .unwrap_err();
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
}