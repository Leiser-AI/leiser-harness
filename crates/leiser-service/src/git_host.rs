//! The git host interface: how the harness service signs a host account
//! in and finds out which account a push signs in as.
//!
//! Every host account gets a `gh` config directory of its own
//! (`GH_CONFIG_DIR`), in which the git host's login runs. A bound
//! repository's local git config sends credentials for the git host to
//! that directory only ([`GitHost::credential_helper`]), so a push from the
//! repository, by the harness or in a terminal, signs in as that account.
//! Before it pushes, the harness service asks the git host who the push
//! signs in as ([`GitHost::account_for_push`]) and blocks a mismatch.
//!
//! While auto-fix watches a chat's open pull request, the harness service
//! polls it ([`GitHost::poll_pull_request`]) with the entity tags of the
//! last answer, so an unchanged pull request costs only `304 Not
//! Modified` answers, which do not count against the git host's rate limit.
//!
//! [`GhGitHost`] is the GitHub CLI. Tests use the testkit's fake git host.

use std::collections::BTreeMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;

use tokio::process::Command;

use crate::installed::{self, Environment, Program};
use crate::{LoginProcess, PullRequestState};

mod github_poll;

/// The variable that names the `gh` config directory.
pub const GH_CONFIG_DIR: &str = "GH_CONFIG_DIR";

/// A boxed future, so the git host interface can be used as `dyn GitHost`.
pub type GitHostFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// What the git host needs to act as one host account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostAccountAccess {
    /// The git host's name, such as `github.com`.
    pub git_host: String,
    /// The account the host account stands for.
    pub login: String,
    /// The host account's own `gh` config directory.
    pub config_dir: PathBuf,
}

/// A pull request the harness service asks the git host to open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewPullRequest {
    /// The branch on the remote to merge.
    pub head: String,
    /// The branch to merge it into.
    pub base: String,
    pub title: String,
    pub body: String,
}

/// A pull request the git host has opened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenedPullRequest {
    pub number: u64,
    pub url: String,
}

/// An open pull request as one poll of the git host found it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullRequestActivity {
    pub state: PullRequestState,
    /// The commit the pull request's head branch points to.
    pub head_commit: String,
    /// The commit its base branch points to.
    pub base_commit: String,
    pub mergeable: Mergeable,
    /// The checks on `head_commit`.
    pub checks: Vec<Check>,
    /// Review comments, reviews with text and comments on the pull
    /// request, by people, not bots.
    pub comments: Vec<ReviewComment>,
}

/// Whether a pull request can be merged into its base branch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mergeable {
    /// The git host has not worked it out yet.
    Unknown,
    Clean,
    /// The branch conflicts with the base branch.
    Conflicting,
}

/// A check, such as a CI job, on a pull request's head commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    /// The git host's id of the check, unique in the repository.
    pub id: String,
    pub name: String,
    pub outcome: CheckOutcome,
}

/// Where a check stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckOutcome {
    /// Queued or running.
    Pending,
    /// Succeeded, or ended in a way that is not a failure (skipped,
    /// neutral, cancelled).
    Passed,
    Failed {
        /// Where the git host shows the check.
        url: String,
        /// The end of the check's log, or the git host's summary of it.
        log: String,
    },
}

/// A comment on a pull request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewComment {
    /// The git host's id of the comment, unique in the repository.
    pub id: String,
    pub author: String,
    pub body: String,
    /// The file and line it is on, such as `src/lib.rs:12`; `None` for a
    /// comment on the whole pull request.
    pub location: Option<String>,
}

/// The entity tags (`ETag`) of the git host's answers to one poll, by
/// request. Only the git host reads them; the harness service passes them
/// back on the next poll.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EntityTags(BTreeMap<String, String>);

impl EntityTags {
    /// The tag of the answer to `request`, if it had one.
    #[must_use]
    pub fn get(&self, request: &str) -> Option<&str> {
        self.0.get(request).map(String::as_str)
    }

    /// Keeps `tag` as the tag of the answer to `request`.
    pub fn insert(&mut self, request: &str, tag: &str) {
        self.0.insert(request.to_owned(), tag.to_owned());
    }
}

/// A pull request as one poll found it, with the tags its next poll
/// sends back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolledPullRequest {
    pub activity: PullRequestActivity,
    pub tags: EntityTags,
}

/// Why the git host could not do what the harness service asked.
#[derive(Debug, thiserror::Error)]
pub enum GitHostError {
    #[error("{program} is not installed: {reason}")]
    NotInstalled { program: String, reason: String },
    #[error("could not run {program}: {source}")]
    Spawn {
        program: String,
        #[source]
        source: std::io::Error,
    },
    #[error("could not start the login: {reason}")]
    Login { reason: String },
    #[error("the host account is not signed in: {reason}")]
    NotSignedIn { reason: String },
    #[error(
        "`origin` is {remote}, which the harness cannot check: it pushes only \
         over HTTPS to the host account's git host {git_host}"
    )]
    UnsupportedRemote { remote: String, git_host: String },
    #[error("the git host refused: {reason}")]
    Refused { reason: String },
    #[error("the git host's answer is not what the harness expects: {answer:?}")]
    UnexpectedAnswer { answer: String },
}

/// A git host as the harness service reaches it.
pub trait GitHost: Send + Sync + std::fmt::Debug {
    /// Runs the git host's login for the host account in its config
    /// directory. Must be called inside a Tokio runtime.
    ///
    /// # Errors
    ///
    /// Returns a [`GitHostError`] when the login cannot start.
    fn login(&self, access: &HostAccountAccess) -> Result<LoginProcess, GitHostError>;

    /// The `credential.helper` a bound repository's local git config uses
    /// for the git host: it hands git the host account's credentials.
    ///
    /// # Errors
    ///
    /// Returns a [`GitHostError`] when the git host's program is missing.
    fn credential_helper(&self, access: &HostAccountAccess) -> Result<String, GitHostError>;

    /// The account a push to `remote` with the host account's credentials
    /// signs in as, by the git host's own answer.
    fn account_for_push<'a>(
        &'a self,
        access: &'a HostAccountAccess,
        remote: &'a str,
    ) -> GitHostFuture<'a, Result<String, GitHostError>>;

    /// Opens a pull request in the repository at `remote` with the host
    /// account's credentials. The harness service has pushed its head and
    /// checked the account first.
    fn open_pull_request<'a>(
        &'a self,
        access: &'a HostAccountAccess,
        remote: &'a str,
        request: &'a NewPullRequest,
    ) -> GitHostFuture<'a, Result<OpenedPullRequest, GitHostError>>;

    /// The state of pull request `number` in the repository at `remote`.
    fn pull_request_state<'a>(
        &'a self,
        access: &'a HostAccountAccess,
        remote: &'a str,
        number: u64,
    ) -> GitHostFuture<'a, Result<PullRequestState, GitHostError>>;

    /// Polls pull request `number` in the repository at `remote`: its
    /// state, head and base commits, whether it conflicts, its checks and
    /// its comments. `last` is the previous poll's answer; what has not
    /// changed since is taken from it, after a conditional request with its
    /// entity tags.
    fn poll_pull_request<'a>(
        &'a self,
        access: &'a HostAccountAccess,
        remote: &'a str,
        number: u64,
        last: Option<&'a PolledPullRequest>,
    ) -> GitHostFuture<'a, Result<PolledPullRequest, GitHostError>>;
}

/// The GitHub CLI `gh`, found as an installed agent CLI is (see
/// `installed.rs`), or the program `LEISER_GH` names.
#[derive(Debug, Clone)]
pub struct GhGitHost {
    program: Program,
}

impl GhGitHost {
    /// The developer's installed `gh`.
    #[must_use]
    pub fn installed() -> Self {
        Self {
            program: Program::Installed {
                install: &installed::GH,
                environment: Environment::current(),
            },
        }
    }

    fn command(&self, access: &HostAccountAccess) -> Result<Command, GitHostError> {
        let found = self
            .program
            .resolve()
            .map_err(|error| GitHostError::NotInstalled {
                program: "gh".to_owned(),
                reason: error.to_string(),
            })?;
        let mut command = Command::from(found.command());
        command
            .env(GH_CONFIG_DIR, &access.config_dir)
            // A token in the environment would win over the config
            // directory's login.
            .env_remove("GH_TOKEN")
            .env_remove("GITHUB_TOKEN")
            .env_remove("GH_ENTERPRISE_TOKEN")
            .env_remove("GITHUB_ENTERPRISE_TOKEN")
            .env("GH_PROMPT_DISABLED", "1");
        Ok(command)
    }

    /// Runs `gh api` against the repository at `remote` and returns its
    /// standard output.
    async fn api(
        &self,
        access: &HostAccountAccess,
        remote: &str,
        path: &str,
        args: &[String],
    ) -> Result<String, GitHostError> {
        let repository = https_repository(remote, &access.git_host)?;
        let mut command = self.command(access)?;
        command
            .args(["api", "--hostname", &access.git_host])
            .arg(format!("repos/{repository}{path}"))
            .args(args)
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true);
        let output = command
            .output()
            .await
            .map_err(|source| GitHostError::Spawn {
                program: "gh".to_owned(),
                source,
            })?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
            let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
            return Err(GitHostError::Refused {
                reason: if stderr.is_empty() { stdout } else { stderr },
            });
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    }
}

impl GitHost for GhGitHost {
    fn login(&self, access: &HostAccountAccess) -> Result<LoginProcess, GitHostError> {
        let mut command = self.command(access)?;
        command.args([
            "auth",
            "login",
            "--hostname",
            &access.git_host,
            "--git-protocol",
            "https",
            "--web",
            "--skip-ssh-key",
        ]);
        LoginProcess::spawn(command).map_err(|error| GitHostError::Login {
            reason: error.to_string(),
        })
    }

    fn credential_helper(&self, access: &HostAccountAccess) -> Result<String, GitHostError> {
        let found = self
            .program
            .resolve()
            .map_err(|error| GitHostError::NotInstalled {
                program: "gh".to_owned(),
                reason: error.to_string(),
            })?;
        // Git runs a helper that starts with `!` in a shell, on Windows in
        // the `sh` of Git for Windows.
        Ok(format!(
            "!{GH_CONFIG_DIR}={} {} auth git-credential",
            shell_quote(&access.config_dir.to_string_lossy()),
            shell_quote(&found.program.to_string_lossy().replace('\\', "/")),
        ))
    }

    fn account_for_push<'a>(
        &'a self,
        access: &'a HostAccountAccess,
        remote: &'a str,
    ) -> GitHostFuture<'a, Result<String, GitHostError>> {
        Box::pin(async move {
            if https_host(remote).as_deref() != Some(access.git_host.as_str()) {
                return Err(GitHostError::UnsupportedRemote {
                    remote: remote.to_owned(),
                    git_host: access.git_host.clone(),
                });
            }
            let mut command = self.command(access)?;
            command
                .args([
                    "api",
                    "--hostname",
                    &access.git_host,
                    "user",
                    "--jq",
                    ".login",
                ])
                .stdin(std::process::Stdio::null())
                .kill_on_drop(true);
            let output = command
                .output()
                .await
                .map_err(|source| GitHostError::Spawn {
                    program: "gh".to_owned(),
                    source,
                })?;
            if !output.status.success() {
                return Err(GitHostError::NotSignedIn {
                    reason: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
                });
            }
            Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
        })
    }

    fn open_pull_request<'a>(
        &'a self,
        access: &'a HostAccountAccess,
        remote: &'a str,
        request: &'a NewPullRequest,
    ) -> GitHostFuture<'a, Result<OpenedPullRequest, GitHostError>> {
        Box::pin(async move {
            // `-f` sends each value as a raw string, never as a file name.
            let args = [
                "--method".to_owned(),
                "POST".to_owned(),
                "-f".to_owned(),
                format!("title={}", request.title),
                "-f".to_owned(),
                format!("head={}", request.head),
                "-f".to_owned(),
                format!("base={}", request.base),
                "-f".to_owned(),
                format!("body={}", request.body),
                "--jq".to_owned(),
                r#""\(.number) \(.html_url)""#.to_owned(),
            ];
            let answer = self.api(access, remote, "/pulls", &args).await?;
            let (number, url) = answer
                .split_once(' ')
                .and_then(|(number, url)| Some((number.parse().ok()?, url.to_owned())))
                .ok_or_else(|| GitHostError::UnexpectedAnswer {
                    answer: answer.clone(),
                })?;
            Ok(OpenedPullRequest { number, url })
        })
    }

    fn pull_request_state<'a>(
        &'a self,
        access: &'a HostAccountAccess,
        remote: &'a str,
        number: u64,
    ) -> GitHostFuture<'a, Result<PullRequestState, GitHostError>> {
        Box::pin(async move {
            let args = ["--jq".to_owned(), r#""\(.state) \(.merged)""#.to_owned()];
            let answer = self
                .api(access, remote, &format!("/pulls/{number}"), &args)
                .await?;
            match answer.as_str() {
                "open false" => Ok(PullRequestState::Open),
                "closed true" => Ok(PullRequestState::Merged),
                "closed false" => Ok(PullRequestState::Closed),
                _ => Err(GitHostError::UnexpectedAnswer { answer }),
            }
        })
    }

    fn poll_pull_request<'a>(
        &'a self,
        access: &'a HostAccountAccess,
        remote: &'a str,
        number: u64,
        last: Option<&'a PolledPullRequest>,
    ) -> GitHostFuture<'a, Result<PolledPullRequest, GitHostError>> {
        Box::pin(self.poll(access, remote, number, last))
    }
}

/// `owner/name` of the repository at an `https://` remote URL on
/// `git_host`.
fn https_repository(remote: &str, git_host: &str) -> Result<String, GitHostError> {
    let unsupported = || GitHostError::UnsupportedRemote {
        remote: remote.to_owned(),
        git_host: git_host.to_owned(),
    };
    if https_host(remote).as_deref() != Some(git_host) {
        return Err(unsupported());
    }
    let path = remote
        .strip_prefix("https://")
        .and_then(|rest| rest.split_once('/'))
        .map(|(_, path)| path.trim_end_matches('/'))
        .ok_or_else(unsupported)?;
    let path = path.strip_suffix(".git").unwrap_or(path);
    let mut parts = path.split('/');
    match (parts.next(), parts.next(), parts.next()) {
        (Some(owner), Some(name), None) if !owner.is_empty() && !name.is_empty() => {
            Ok(format!("{owner}/{name}"))
        }
        _ => Err(unsupported()),
    }
}

/// The host of an `https://` remote URL, without user and port.
fn https_host(remote: &str) -> Option<String> {
    let rest = remote.strip_prefix("https://")?;
    let authority = rest.split('/').next()?;
    let host = authority.rsplit('@').next()?;
    let host = host.split(':').next()?;
    (!host.is_empty()).then(|| host.to_ascii_lowercase())
}

/// `text` in single quotes for `sh`.
fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use super::{https_host, https_repository, shell_quote};

    #[test]
    fn finds_the_host_of_https_remotes_only() {
        assert_eq!(
            https_host("https://github.com/owner/repo.git").as_deref(),
            Some("github.com")
        );
        assert_eq!(
            https_host("https://someone@GitHub.com:443/owner/repo").as_deref(),
            Some("github.com")
        );
        assert_eq!(https_host("git@github.com:owner/repo.git"), None);
        assert_eq!(https_host("ssh://git@github.com/owner/repo.git"), None);
        assert_eq!(https_host("/tmp/origin.git"), None);
    }

    #[test]
    fn finds_owner_and_name_of_https_remotes_on_the_git_host() {
        assert_eq!(
            https_repository("https://github.com/owner/repo.git", "github.com").ok(),
            Some("owner/repo".to_owned())
        );
        assert_eq!(
            https_repository("https://someone@github.com/owner/repo/", "github.com").ok(),
            Some("owner/repo".to_owned())
        );
        assert!(https_repository("https://github.com/owner", "github.com").is_err());
        assert!(https_repository("https://gitlab.com/owner/repo", "github.com").is_err());
        assert!(https_repository("git@github.com:owner/repo.git", "github.com").is_err());
    }

    #[test]
    fn quotes_for_the_shell() {
        assert_eq!(shell_quote("/a b/it's"), r"'/a b/it'\''s'");
    }
}
