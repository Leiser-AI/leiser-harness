//! A fake git host: its login signs a host account in by writing a file
//! into the host account's config directory, and a push signs in as the
//! account written there. Pushes go to the test repository's local remote;
//! pull requests are kept in memory, shared by the fake's clones.

use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};

use leiser_service::{
    GitHost, GitHostError, GitHostFuture, HostAccountAccess, LoginOutcome, LoginProcess,
    NewPullRequest, OpenedPullRequest, PullRequestState,
};
use tokio::sync::{mpsc, oneshot};

/// The file the fake git host's login writes into the host account's
/// config directory: the account it signed in as.
pub const FAKE_GIT_HOST_LOGIN_FILE: &str = "fake-git-host-login";
/// A file in the host account's config directory that lists pull request
/// numbers, one per line: the fake git host reports them merged. For
/// trying the desktop app, where the fake runs inside the service process.
pub const FAKE_GIT_HOST_MERGED_FILE: &str = "fake-git-host-merged";
/// The credential helper the fake git host gives bound repositories. A
/// local remote never asks for credentials.
pub const FAKE_CREDENTIAL_HELPER: &str = "!leiser-fake-git-host-credential";

/// A git host for tests and for trying the desktop app without a git host
/// account. Clones share what the next login signs in as.
#[derive(Debug, Clone, Default)]
pub struct FakeGitHost {
    next_login: Arc<Mutex<Option<String>>>,
    pull_requests: Arc<Mutex<Vec<FakePullRequest>>>,
}

/// A pull request opened on the fake git host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FakePullRequest {
    pub number: u64,
    pub url: String,
    /// The repository's remote URL.
    pub remote: String,
    pub head: String,
    pub base: String,
    pub title: String,
    pub body: String,
    /// The account the host account was signed in as when it opened it.
    pub opened_by: String,
    pub state: PullRequestState,
}

impl FakeGitHost {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Makes the next login sign in as `login` instead of the host
    /// account's own login, as a developer who picked the wrong account
    /// in the browser would.
    pub fn next_login_signs_in_as(&self, login: &str) {
        *self
            .next_login
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(login.to_owned());
    }

    /// Every pull request opened so far, oldest first.
    #[must_use]
    pub fn pull_requests(&self) -> Vec<FakePullRequest> {
        self.pull_requests
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Merges pull request `number`, as a reviewer on the git host would.
    pub fn merge(&self, number: u64) {
        self.set_state(number, PullRequestState::Merged);
    }

    /// Sets the state of pull request `number`.
    pub fn set_state(&self, number: u64, state: PullRequestState) {
        let mut pull_requests = self
            .pull_requests
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(pull_request) = pull_requests
            .iter_mut()
            .find(|pull_request| pull_request.number == number)
        {
            pull_request.state = state;
        }
    }

    /// The account the host account whose config directory is
    /// `config_dir` is signed in as, if any.
    #[must_use]
    pub fn signed_in_as(config_dir: &Path) -> Option<String> {
        std::fs::read_to_string(config_dir.join(FAKE_GIT_HOST_LOGIN_FILE))
            .ok()
            .map(|login| login.trim().to_owned())
    }
}

impl GitHost for FakeGitHost {
    fn login(&self, access: &HostAccountAccess) -> Result<LoginProcess, GitHostError> {
        let login = self
            .next_login
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
            .unwrap_or_else(|| access.login.clone());
        let (lines, output) = mpsc::channel(4);
        let (input, _answers) = mpsc::channel(1);
        let (finished, outcome) = oneshot::channel();
        let written = std::fs::create_dir_all(&access.config_dir).and_then(|()| {
            std::fs::write(access.config_dir.join(FAKE_GIT_HOST_LOGIN_FILE), &login)
        });
        let (line, result) = match written {
            Ok(()) => (
                format!(
                    "Logged in to {} as {login} (fake git host)",
                    access.git_host
                ),
                LoginOutcome::SignedIn,
            ),
            Err(error) => (
                format!("could not sign in: {error}"),
                LoginOutcome::Failed {
                    reason: error.to_string(),
                },
            ),
        };
        // Both channels have room; the receivers are returned below.
        let _ = lines.try_send(line);
        let _ = finished.send(result);
        Ok(LoginProcess {
            output,
            input,
            outcome,
        })
    }

    fn credential_helper(&self, _access: &HostAccountAccess) -> Result<String, GitHostError> {
        Ok(FAKE_CREDENTIAL_HELPER.to_owned())
    }

    fn account_for_push<'a>(
        &'a self,
        access: &'a HostAccountAccess,
        _remote: &'a str,
    ) -> GitHostFuture<'a, Result<String, GitHostError>> {
        Box::pin(async move {
            Self::signed_in_as(&access.config_dir).ok_or_else(|| GitHostError::NotSignedIn {
                reason: format!("no login in {}", access.config_dir.display()),
            })
        })
    }

    fn open_pull_request<'a>(
        &'a self,
        access: &'a HostAccountAccess,
        remote: &'a str,
        request: &'a NewPullRequest,
    ) -> GitHostFuture<'a, Result<OpenedPullRequest, GitHostError>> {
        Box::pin(async move {
            let opened_by = Self::signed_in_as(&access.config_dir).ok_or_else(|| {
                GitHostError::NotSignedIn {
                    reason: format!("no login in {}", access.config_dir.display()),
                }
            })?;
            let mut pull_requests = self
                .pull_requests
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            // As GitHub does: one open pull request per head branch.
            if pull_requests.iter().any(|pull_request| {
                pull_request.remote == remote
                    && pull_request.head == request.head
                    && pull_request.state == PullRequestState::Open
            }) {
                return Err(GitHostError::Refused {
                    reason: format!("a pull request for {} already exists", request.head),
                });
            }
            let number = pull_requests.len() as u64 + 1;
            let url = format!("https://{}/fake/pull/{number}", access.git_host);
            pull_requests.push(FakePullRequest {
                number,
                url: url.clone(),
                remote: remote.to_owned(),
                head: request.head.clone(),
                base: request.base.clone(),
                title: request.title.clone(),
                body: request.body.clone(),
                opened_by,
                state: PullRequestState::Open,
            });
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
            let merged_by_file =
                std::fs::read_to_string(access.config_dir.join(FAKE_GIT_HOST_MERGED_FILE))
                    .is_ok_and(|merged| {
                        merged.lines().any(|line| line.trim() == number.to_string())
                    });
            if merged_by_file {
                self.set_state(number, PullRequestState::Merged);
            }
            self.pull_requests
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .iter()
                .find(|pull_request| pull_request.number == number && pull_request.remote == remote)
                .map(|pull_request| pull_request.state)
                .ok_or_else(|| GitHostError::Refused {
                    reason: format!("no pull request #{number}"),
                })
        })
    }
}
