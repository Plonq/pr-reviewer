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
                write!(f, "You're not signed in to GitHub. Run `gh auth login` to sign in.")
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
}