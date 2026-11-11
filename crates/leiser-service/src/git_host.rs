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
//! [`GhGitHost`] is the GitHub CLI. Tests use the testkit's fake git host.

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;

use tokio::process::Command;

use crate::LoginProcess;
use crate::installed::{self, Environment, Program};

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
    use super::{https_host, shell_quote};

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
    fn quotes_for_the_shell() {
        assert_eq!(shell_quote("/a b/it's"), r"'/a b/it'\''s'");
    }
}
