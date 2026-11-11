//! The registered host accounts, and how a repository is bound to one.
//!
//! Every host account gets its own `gh` config directory,
//! `host-accounts/<id>/gh` in the data directory, in which the git host's
//! login runs. A repository names its host account in its
//! `.leiser/config.toml`; the harness service then sets the repository's
//! local git config, never the global one (see [`bind`]):
//!
//! - `user.name` and `user.email`: the host account's commit author, so
//!   every commit in the repository and its worktrees carries it;
//! - `credential.<https://git host>.helper`: first empty, which drops the
//!   helpers of the global config for the git host, then the git host's
//!   helper that hands git this host account's credentials;
//! - `credential.<https://git host>.username`: the host account's login.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

use crate::git::{self, GitError};
use crate::git_host::{GitHost, GitHostError, HostAccountAccess};
use crate::store::HostAccountV3;
use crate::{CommitAuthor, HostAccountId, HostAccountOverview};

/// Why a host account could not be added, removed or bound.
#[derive(Debug, thiserror::Error)]
pub(crate) enum HostAccountError {
    #[error("could not set up {}: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error(transparent)]
    GitHost(#[from] GitHostError),
    #[error("could not set the repository's local git config: {0}")]
    Git(#[from] GitError),
}

#[derive(Debug, Clone)]
struct Registered {
    name: String,
    git_host: String,
    login: String,
    author: CommitAuthor,
}

/// The registered host accounts.
pub(crate) struct HostAccounts {
    registered: BTreeMap<HostAccountId, Registered>,
    /// Holds one directory per host account.
    root: PathBuf,
}

impl HostAccounts {
    pub fn load(stored: Vec<HostAccountV3>, root: PathBuf) -> Self {
        let registered = stored
            .into_iter()
            .map(|account| {
                (
                    HostAccountId(account.id),
                    Registered {
                        name: account.name,
                        git_host: account.git_host,
                        login: account.login,
                        author: CommitAuthor {
                            name: account.author_name,
                            email: account.author_email,
                        },
                    },
                )
            })
            .collect();
        Self { registered, root }
    }

    pub fn snapshot(&self) -> Vec<HostAccountV3> {
        self.registered
            .iter()
            .map(|(id, account)| HostAccountV3 {
                id: id.0,
                name: account.name.clone(),
                git_host: account.git_host.clone(),
                login: account.login.clone(),
                author_name: account.author.name.clone(),
                author_email: account.author.email.clone(),
            })
            .collect()
    }

    pub fn contains(&self, id: HostAccountId) -> bool {
        self.registered.contains_key(&id)
    }

    /// The host account called `name`.
    pub fn named(&self, name: &str) -> Option<HostAccountId> {
        self.registered
            .iter()
            .find(|(_, account)| account.name == name)
            .map(|(id, _)| *id)
    }

    pub fn name(&self, id: HostAccountId) -> Option<&str> {
        self.registered
            .get(&id)
            .map(|account| account.name.as_str())
    }

    pub fn git_host(&self, id: HostAccountId) -> Option<&str> {
        self.registered
            .get(&id)
            .map(|account| account.git_host.as_str())
    }

    pub fn login(&self, id: HostAccountId) -> Option<&str> {
        self.registered
            .get(&id)
            .map(|account| account.login.as_str())
    }

    /// Registers a host account under `id` and creates its config
    /// directory.
    pub fn add(
        &mut self,
        id: HostAccountId,
        name: String,
        git_host: String,
        login: String,
        author: CommitAuthor,
    ) -> Result<(), HostAccountError> {
        let dir = self.config_dir(id);
        std::fs::create_dir_all(&dir)
            .map_err(|source| HostAccountError::Io { path: dir, source })?;
        self.registered.insert(
            id,
            Registered {
                name,
                git_host,
                login,
                author,
            },
        );
        Ok(())
    }

    /// Takes a host account back out, for when storing the registry failed
    /// right after [`Self::add`], or to remove it.
    pub fn remove(&mut self, id: HostAccountId) {
        if self.registered.remove(&id).is_some() {
            let dir = self.root.join(id.0.to_string());
            match std::fs::remove_dir_all(&dir) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => {
                    tracing::warn!(dir = %dir.display(), %error, "could not delete a host account's directory");
                }
            }
        }
    }

    /// What the git host needs to act as the host account.
    pub fn access(&self, id: HostAccountId) -> Option<HostAccountAccess> {
        let account = self.registered.get(&id)?;
        Some(HostAccountAccess {
            git_host: account.git_host.clone(),
            login: account.login.clone(),
            config_dir: self.config_dir(id),
        })
    }

    pub fn author(&self, id: HostAccountId) -> Option<&CommitAuthor> {
        self.registered.get(&id).map(|account| &account.author)
    }

    pub fn overview(&self, logging_in: impl Fn(HostAccountId) -> bool) -> Vec<HostAccountOverview> {
        self.registered
            .iter()
            .map(|(id, account)| HostAccountOverview {
                host_account: *id,
                name: account.name.clone(),
                git_host: account.git_host.clone(),
                login: account.login.clone(),
                author: account.author.clone(),
                config_dir: self.config_dir(*id),
                logging_in: logging_in(*id),
            })
            .collect()
    }

    fn config_dir(&self, id: HostAccountId) -> PathBuf {
        self.root.join(id.0.to_string()).join("gh")
    }
}

/// What [`bind`] writes into a repository's local git config.
#[derive(Debug, Clone)]
pub(crate) struct Binding {
    pub author: CommitAuthor,
    pub access: HostAccountAccess,
    pub credential_helper: String,
}

impl Binding {
    pub fn new(
        author: CommitAuthor,
        access: HostAccountAccess,
        git_host: &dyn GitHost,
    ) -> Result<Self, HostAccountError> {
        let credential_helper = git_host.credential_helper(&access)?;
        Ok(Self {
            author,
            access,
            credential_helper,
        })
    }
}

/// Binds the repository whose main checkout is `repository` to a host
/// account with its local git config only. Its worktrees share that
/// config. Settings already in place are written again, so a binding that
/// was changed by hand is restored.
pub(crate) async fn bind(repository: &Path, binding: &Binding) -> Result<(), HostAccountError> {
    let credential = format!("credential.https://{}", binding.access.git_host);
    let helper = format!("{credential}.helper");
    let username = format!("{credential}.username");
    set(repository, "user.name", &binding.author.name).await?;
    set(repository, "user.email", &binding.author.email).await?;
    // Exit code 5: there was nothing to unset.
    match git::run(repository, &["config", "--local", "--unset-all", &helper]).await {
        Ok(_) | Err(GitError::Failed { .. }) => {}
        Err(error) => return Err(error.into()),
    }
    git::run(repository, &["config", "--local", "--add", &helper, ""]).await?;
    git::run(
        repository,
        &[
            "config",
            "--local",
            "--add",
            &helper,
            &binding.credential_helper,
        ],
    )
    .await?;
    set(repository, &username, &binding.access.login).await?;
    Ok(())
}

async fn set(repository: &Path, key: &str, value: &str) -> Result<(), GitError> {
    git::run(repository, &["config", "--local", key, value])
        .await
        .map(|_| ())
}
