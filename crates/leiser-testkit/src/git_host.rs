//! A fake git host: its login signs a host account in by writing a file
//! into the host account's config directory, and a push signs in as the
//! account written there. Pushes go to the test repository's local remote.

use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};

use leiser_service::{
    GitHost, GitHostError, GitHostFuture, HostAccountAccess, LoginOutcome, LoginProcess,
};
use tokio::sync::{mpsc, oneshot};

/// The file the fake git host's login writes into the host account's
/// config directory: the account it signed in as.
pub const FAKE_GIT_HOST_LOGIN_FILE: &str = "fake-git-host-login";
/// The credential helper the fake git host gives bound repositories. A
/// local remote never asks for credentials.
pub const FAKE_CREDENTIAL_HELPER: &str = "!leiser-fake-git-host-credential";

/// A git host for tests and for trying the desktop app without a git host
/// account. Clones share what the next login signs in as.
#[derive(Debug, Clone, Default)]
pub struct FakeGitHost {
    next_login: Arc<Mutex<Option<String>>>,
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
}
