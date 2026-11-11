//! What auto-fix and the wayfinder maps share when they poll the git host:
//! the repository's host account, read from its `.leiser/config.toml`, and
//! its `origin`. Unlike a push, a poll leaves the repository's git config
//! alone.

use std::path::PathBuf;

use super::State;
use crate::git_host::{GitHostError, HostAccountAccess};
use crate::{HostAccountBinding, RepositoryId};

/// Why a poll could not ask the git host.
#[derive(Debug, thiserror::Error)]
pub(super) enum PollError {
    #[error("the repository is bound to no host account the harness knows")]
    NoHostAccount,
    #[error("the repository has no remote `origin`")]
    NoOrigin,
    #[error(transparent)]
    GitHost(#[from] GitHostError),
}

/// A repository as a poll reaches it on the git host.
pub(super) struct PollTarget {
    path: PathBuf,
    pub(super) access: HostAccountAccess,
}

impl PollTarget {
    /// The repository's `origin` URL.
    pub(super) async fn remote(&self) -> Result<String, PollError> {
        crate::git::origin_url(&self.path)
            .await
            .ok_or(PollError::NoOrigin)
    }
}

impl State {
    /// The repository's path and its host account's access, for a poll.
    pub(super) fn poll_target(&self, repository: RepositoryId) -> Result<PollTarget, PollError> {
        let path = self
            .repositories
            .get(&repository)
            .ok_or(PollError::NoHostAccount)?
            .clone();
        let HostAccountBinding::Bound { host_account, .. } = self.host_account_binding(&path)
        else {
            return Err(PollError::NoHostAccount);
        };
        let access = self
            .host_accounts
            .access(host_account)
            .ok_or(PollError::NoHostAccount)?;
        Ok(PollTarget { path, access })
    }
}
