//! The GitLab CLI (`glab`) git host implementation, supporting both
//! GitLab.com and self-hosted GitLab instances.
//!
//! Every GitLab host account gets a `glab` config directory of its own
//! (`GLAB_CONFIG_DIR`), isolated from token and host variables in the
//! ambient environment. Project paths may include nested subgroups
//! (`group/subgroup/repo`) and are percent-encoded for the GitLab REST
//! API v4 (`projects/<encoded>/...`).

use std::path::PathBuf;

use serde::Deserialize;
use tokio::process::Command;

use super::http::{ConditionalAnswer, parse_conditional_output};
use super::{
    GitHost, GitHostError, GitHostFuture, HostAccountAccess, NewPullRequest, OpenedPullRequest,
    PolledPullRequest, PolledWayfinderMaps, encode_path_segment, https_host_matches,
    https_project_path, refuse_credential_in_remote, refused, shell_quote, shown_remote,
};
use crate::installed::{self, Environment, Program};
use crate::{LoginProcess, PullRequestState};

/// The variable that names the `glab` config directory.
pub const GLAB_CONFIG_DIR: &str = "GLAB_CONFIG_DIR";

/// Ambient environment variables that would override `GLAB_CONFIG_DIR`'s
/// login or target another GitLab host.
const STRIPPED_ENV: [&str; 7] = [
    "GITLAB_TOKEN",
    "GITLAB_ACCESS_TOKEN",
    "OAUTH_TOKEN",
    "GITLAB_HOST",
    "GITLAB_URI",
    "GLAB_HOST",
    "CI_JOB_TOKEN",
];

#[derive(Deserialize)]
struct CurrentUserJson {
    username: String,
}

#[derive(Deserialize)]
struct OpenedMergeRequestJson {
    iid: u64,
    web_url: String,
}

#[derive(Deserialize)]
struct MergeRequestStateJson {
    state: MergeRequestStateValue,
}

/// The `state` field of a GitLab merge request.
#[derive(Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(super) enum MergeRequestStateValue {
    Opened,
    /// GitLab marks an MR `locked` transiently while a merge is running.
    Locked,
    Merged,
    Closed,
}

impl From<MergeRequestStateValue> for PullRequestState {
    fn from(state: MergeRequestStateValue) -> Self {
        match state {
            MergeRequestStateValue::Opened | MergeRequestStateValue::Locked => Self::Open,
            MergeRequestStateValue::Merged => Self::Merged,
            MergeRequestStateValue::Closed => Self::Closed,
        }
    }
}

/// The GitLab CLI `glab`, found as an installed agent CLI is (see
/// `installed.rs`), or the program `LEISER_GLAB` names.
#[derive(Debug, Clone)]
pub struct GlabGitHost {
    program: Program,
}

impl GlabGitHost {
    /// The variable that overrides where `glab` is found.
    pub const PROGRAM_VARIABLE: &'static str = installed::GLAB.variable;

    /// The `glab` at `program`, a path or a name looked up on `PATH`.
    #[must_use]
    pub fn new(program: impl Into<PathBuf>) -> Self {
        Self {
            program: Program::Given(program.into()),
        }
    }

    /// The developer's installed `glab`.
    #[must_use]
    pub fn installed() -> Self {
        Self::installed_in(Environment::current())
    }

    /// `glab` looked up in `environment`.
    #[must_use]
    pub fn installed_in(environment: Environment) -> Self {
        Self {
            program: Program::Installed {
                install: &installed::GLAB,
                environment,
            },
        }
    }

    fn command(&self, access: &HostAccountAccess) -> Result<Command, GitHostError> {
        let found = self
            .program
            .resolve()
            .map_err(|error| GitHostError::NotInstalled {
                program: "glab".to_owned(),
                reason: error.to_string(),
            })?;
        let mut command = Command::from(found.command());
        command.env(GLAB_CONFIG_DIR, &access.config_dir);
        for variable in STRIPPED_ENV {
            command.env_remove(variable);
        }
        command
            .env("GLAB_CHECK_UPDATE", "false")
            .env("NO_PROMPT", "1");
        Ok(command)
    }

    /// Runs `glab api` for `projects/<encoded_project><path>` with `args`
    /// before the endpoint, and returns its output whatever its exit status.
    pub(super) async fn run_project_api(
        &self,
        access: &HostAccountAccess,
        encoded_project: &str,
        path: &str,
        args: &[String],
    ) -> Result<std::process::Output, GitHostError> {
        let mut command = self.command(access)?;
        command
            .args(["api", "--hostname", &access.git_host])
            .args(args)
            .arg(format!("projects/{encoded_project}{path}"))
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true);
        command
            .output()
            .await
            .map_err(|source| GitHostError::Spawn {
                program: "glab".to_owned(),
                source,
            })
    }

    /// Runs `glab api` against the project at `remote` and returns its
    /// standard output.
    pub(super) async fn project_api(
        &self,
        access: &HostAccountAccess,
        remote: &str,
        path: &str,
        args: &[String],
    ) -> Result<String, GitHostError> {
        let project = https_project_path(remote, &access.git_host)?;
        let encoded = encode_path_segment(&project);
        let output = self.run_project_api(access, &encoded, path, args).await?;
        if !output.status.success() {
            return Err(refused(&output, ""));
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    }

    /// Runs `glab api --include` for `path` in `encoded_project`, with
    /// `If-None-Match` when there is a tag.
    pub(super) async fn conditional(
        &self,
        access: &HostAccountAccess,
        encoded_project: &str,
        path: &str,
        tag: Option<&str>,
    ) -> Result<ConditionalAnswer, GitHostError> {
        let mut args = vec!["--include".to_owned()];
        if let Some(tag) = tag {
            args.extend(["--header".to_owned(), format!("If-None-Match: {tag}")]);
        }
        let output = self
            .run_project_api(access, encoded_project, path, &args)
            .await?;
        parse_conditional_output(&output)
    }
}

impl GitHost for GlabGitHost {
    fn login(&self, access: &HostAccountAccess) -> Result<LoginProcess, GitHostError> {
        let mut command = self.command(access)?;
        // Interactive login may prompt for an OAuth browser flow or token.
        command.env_remove("NO_PROMPT");
        command.args([
            "auth",
            "login",
            "--hostname",
            &access.git_host,
            "--git-protocol",
            "https",
            "--api-protocol",
            "https",
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
                program: "glab".to_owned(),
                reason: error.to_string(),
            })?;
        // Git runs a helper that starts with `!` in a shell, on Windows in
        // the `sh` of Git for Windows. Unset ambient token and host
        // variables so `glab auth git-credential` always reads from the
        // host account's `GLAB_CONFIG_DIR`.
        Ok(format!(
            "!unset {} && {GLAB_CONFIG_DIR}={} {} auth git-credential",
            STRIPPED_ENV.join(" "),
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
            refuse_credential_in_remote(remote)?;
            if !https_host_matches(remote, &access.git_host) {
                return Err(GitHostError::UnsupportedRemote {
                    remote: shown_remote(remote),
                    git_host: access.git_host.clone(),
                });
            }
            let mut command = self.command(access)?;
            command
                .args(["api", "--hostname", &access.git_host, "user"])
                .stdin(std::process::Stdio::null())
                .kill_on_drop(true);
            let output = command
                .output()
                .await
                .map_err(|source| GitHostError::Spawn {
                    program: "glab".to_owned(),
                    source,
                })?;
            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
                let reason = if stderr.is_empty() {
                    String::from_utf8_lossy(&output.stdout).trim().to_owned()
                } else {
                    stderr
                };
                return Err(GitHostError::NotSignedIn { reason });
            }
            let answer = String::from_utf8_lossy(&output.stdout).trim().to_owned();
            let user: CurrentUserJson =
                serde_json::from_str(&answer).map_err(|_| GitHostError::UnexpectedAnswer {
                    answer: answer.clone(),
                })?;
            let username = user.username.trim();
            if username.is_empty() {
                return Err(GitHostError::UnexpectedAnswer { answer });
            }
            Ok(username.to_owned())
        })
    }

    fn open_pull_request<'a>(
        &'a self,
        access: &'a HostAccountAccess,
        remote: &'a str,
        request: &'a NewPullRequest,
    ) -> GitHostFuture<'a, Result<OpenedPullRequest, GitHostError>> {
        Box::pin(async move {
            // `--raw-field` sends each value as a raw string, never as a
            // file path or coerced JSON literal.
            let args = [
                "--method".to_owned(),
                "POST".to_owned(),
                "--raw-field".to_owned(),
                format!("source_branch={}", request.head),
                "--raw-field".to_owned(),
                format!("target_branch={}", request.base),
                "--raw-field".to_owned(),
                format!("title={}", request.title),
                "--raw-field".to_owned(),
                format!("description={}", request.body),
            ];
            let answer = self
                .project_api(access, remote, "/merge_requests", &args)
                .await?;
            let opened: OpenedMergeRequestJson =
                serde_json::from_str(&answer).map_err(|_| GitHostError::UnexpectedAnswer {
                    answer: answer.clone(),
                })?;
            if opened.web_url.trim().is_empty() {
                return Err(GitHostError::UnexpectedAnswer { answer });
            }
            Ok(OpenedPullRequest {
                number: opened.iid,
                url: opened.web_url,
            })
        })
    }

    fn pull_request_state<'a>(
        &'a self,
        access: &'a HostAccountAccess,
        remote: &'a str,
        number: u64,
    ) -> GitHostFuture<'a, Result<PullRequestState, GitHostError>> {
        Box::pin(async move {
            let answer = self
                .project_api(access, remote, &format!("/merge_requests/{number}"), &[])
                .await?;
            let mr: MergeRequestStateJson = serde_json::from_str(&answer)
                .map_err(|_| GitHostError::UnexpectedAnswer { answer })?;
            Ok(PullRequestState::from(mr.state))
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

    fn poll_wayfinder_maps<'a>(
        &'a self,
        access: &'a HostAccountAccess,
        remote: &'a str,
        last: Option<&'a PolledWayfinderMaps>,
    ) -> GitHostFuture<'a, Result<PolledWayfinderMaps, GitHostError>> {
        Box::pin(self.poll_maps(access, remote, last))
    }
}
