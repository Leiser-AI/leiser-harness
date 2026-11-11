//! The registered subscriptions and the config directories their agent
//! CLIs run in.
//!
//! - Claude Code: every subscription gets its own `CLAUDE_CONFIG_DIR`,
//!   `subscriptions/<id>/claude` in the data directory. Its login, history
//!   and agent sessions stay in there, so a chat that changes Claude
//!   subscription continues in a new agent session with a handoff.
//! - Codex: one shared Codex home (the developer's `CODEX_HOME`, usually
//!   `~/.codex`) keeps the default login, and every subscription gets a
//!   shadow home, `subscriptions/<id>/codex-home`: links to everything in
//!   the shared home except the login (`auth.json`) and a few files that
//!   are each home's own. Threads live in the shared home, so a chat that
//!   changes Codex subscription continues its agent session.
//!
//! API keys are kept in the OS keychain only, under the subscription's
//! account (see `keychain.rs`), and read again for every agent session.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::keychain::{self, Keychain, KeychainError};
use crate::store::SubscriptionV3;
use crate::{
    ApiKey, Provider, SignIn, SubscriptionAccess, SubscriptionId, SubscriptionKind,
    SubscriptionOverview,
};

/// Directories t3code creates in the shared Codex home before it links
/// them, so a shadow home shares them even when Codex has not created
/// them yet.
const SHARED_CODEX_DIRS: [&str; 11] = [
    "sessions",
    "archived_sessions",
    "sqlite",
    "shell_snapshots",
    "worktrees",
    "skills",
    "plugins",
    "cache",
    "logs",
    "mcp-oauth-locks",
    "thread-writer-locks",
];
/// Entries of the shared Codex home that a shadow home never links: the
/// login and what belongs to it, and files each home keeps for itself.
const OWN_CODEX_ENTRIES: [&str; 7] = [
    "auth.json",
    "auth.lock",
    "models_cache.json",
    "log",
    "memories",
    "tmp",
    ".DS_Store",
];
/// Codex's login file.
const CODEX_AUTH: &str = "auth.json";

/// Why a subscription could not be added, prepared or removed.
#[derive(Debug, thiserror::Error)]
pub(crate) enum SubscriptionError {
    #[error("could not set up {}: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error(
        "the shared Codex home is unknown: set CODEX_HOME or HOME, or give the \
         harness service a Codex home"
    )]
    NoCodexHome,
    #[error("{} links to the shared login; remove it and log in again", path.display())]
    SharedLogin { path: PathBuf },
    #[error(transparent)]
    Keychain(#[from] KeychainError),
}

struct Registered {
    provider: Provider,
    name: String,
    kind: SubscriptionKind,
}

/// The registered subscriptions.
pub(crate) struct Subscriptions {
    registered: BTreeMap<SubscriptionId, Registered>,
    /// Holds one directory per subscription.
    root: PathBuf,
    codex_home: Option<PathBuf>,
    keychain: Arc<dyn Keychain>,
}

impl Subscriptions {
    pub fn load(
        stored: Vec<SubscriptionV3>,
        root: PathBuf,
        codex_home: Option<PathBuf>,
        keychain: Arc<dyn Keychain>,
    ) -> Self {
        let registered = stored
            .into_iter()
            .map(|subscription| {
                (
                    SubscriptionId(subscription.id),
                    Registered {
                        provider: subscription.provider,
                        name: subscription.name,
                        kind: subscription.kind,
                    },
                )
            })
            .collect();
        Self {
            registered,
            root,
            codex_home,
            keychain,
        }
    }

    pub fn snapshot(&self) -> Vec<SubscriptionV3> {
        self.registered
            .iter()
            .map(|(id, registered)| SubscriptionV3 {
                id: id.0,
                provider: registered.provider,
                name: registered.name.clone(),
                kind: registered.kind,
            })
            .collect()
    }

    pub fn contains(&self, id: SubscriptionId) -> bool {
        self.registered.contains_key(&id)
    }

    pub fn provider(&self, id: SubscriptionId) -> Option<Provider> {
        self.registered
            .get(&id)
            .map(|registered| registered.provider)
    }

    pub fn kind(&self, id: SubscriptionId) -> Option<SubscriptionKind> {
        self.registered.get(&id).map(|registered| registered.kind)
    }

    /// The provider's subscription called `name`.
    pub fn named(&self, provider: Provider, name: &str) -> Option<SubscriptionId> {
        self.registered
            .iter()
            .find(|(_, registered)| registered.provider == provider && registered.name == name)
            .map(|(id, _)| *id)
    }

    /// Registers a subscription under `id`: creates its config directory
    /// and stores an API key in the keychain. Nothing is left behind when
    /// it fails.
    pub fn add(
        &mut self,
        id: SubscriptionId,
        provider: Provider,
        name: String,
        sign_in: &SignIn,
    ) -> Result<(), SubscriptionError> {
        let home = self.home(id, provider);
        create_dir(&home)?;
        let kind = match sign_in {
            SignIn::Login => SubscriptionKind::Login,
            SignIn::ApiKey { api_key } => {
                if let Err(error) = self.keychain.store(&keychain::account(id.0), api_key) {
                    self.remove_dir(id);
                    return Err(error.into());
                }
                SubscriptionKind::ApiKey
            }
        };
        self.registered.insert(
            id,
            Registered {
                provider,
                name,
                kind,
            },
        );
        Ok(())
    }

    /// Takes a subscription back out of the registry, for when storing the
    /// registry failed right after [`Self::add`].
    pub fn undo_add(&mut self, id: SubscriptionId) {
        if self.registered.remove(&id).is_some() {
            if let Err(error) = self.keychain.delete(&keychain::account(id.0)) {
                tracing::warn!(?id, %error, "could not delete an API key");
            }
            self.remove_dir(id);
        }
    }

    pub fn rename(&mut self, id: SubscriptionId, name: String) {
        if let Some(registered) = self.registered.get_mut(&id) {
            registered.name = name;
        }
    }

    /// Forgets a subscription and deletes its config directory and key.
    pub fn remove(&mut self, id: SubscriptionId) -> Result<(), SubscriptionError> {
        let Some(registered) = self.registered.get(&id) else {
            return Ok(());
        };
        if registered.kind == SubscriptionKind::ApiKey {
            self.keychain.delete(&keychain::account(id.0))?;
        }
        self.registered.remove(&id);
        self.remove_dir(id);
        Ok(())
    }

    /// What the agent CLI needs to run on the subscription, with its
    /// config directory set up and its API key read from the keychain.
    pub fn access(&self, id: SubscriptionId) -> Result<SubscriptionAccess, SubscriptionError> {
        let Some(registered) = self.registered.get(&id) else {
            // Callers check that the subscription exists.
            return Err(SubscriptionError::Io {
                path: self.root.clone(),
                source: io::Error::new(io::ErrorKind::NotFound, "unknown subscription"),
            });
        };
        let home = self.home(id, registered.provider);
        match registered.provider {
            Provider::Anthropic => create_dir(&home)?,
            Provider::OpenAi => {
                let shared = self
                    .codex_home
                    .as_ref()
                    .ok_or(SubscriptionError::NoCodexHome)?;
                shadow_codex_home(shared, &home)?;
            }
        }
        let api_key = match registered.kind {
            SubscriptionKind::Login => None,
            SubscriptionKind::ApiKey => Some(self.api_key(id)?),
        };
        Ok(SubscriptionAccess { home, api_key })
    }

    /// Whether an agent session of the provider's agent CLI that ran on
    /// `from` can continue on `to`; `None` is the default login. Codex
    /// keeps threads in the shared home, which every shadow home links to;
    /// a Claude config directory keeps its agent sessions to itself.
    pub fn shares_agent_sessions(
        provider: Provider,
        from: Option<SubscriptionId>,
        to: Option<SubscriptionId>,
    ) -> bool {
        match provider {
            Provider::OpenAi => true,
            Provider::Anthropic => from == to,
        }
    }

    pub fn overview(
        &self,
        logging_in: impl Fn(SubscriptionId) -> bool,
    ) -> Vec<SubscriptionOverview> {
        self.registered
            .iter()
            .map(|(id, registered)| SubscriptionOverview {
                subscription: *id,
                provider: registered.provider,
                name: registered.name.clone(),
                kind: registered.kind,
                home: self.home(*id, registered.provider),
                logging_in: logging_in(*id),
            })
            .collect()
    }

    fn api_key(&self, id: SubscriptionId) -> Result<ApiKey, KeychainError> {
        self.keychain.read(&keychain::account(id.0))
    }

    fn home(&self, id: SubscriptionId, provider: Provider) -> PathBuf {
        let dir = self.root.join(id.0.to_string());
        match provider {
            Provider::Anthropic => dir.join("claude"),
            Provider::OpenAi => dir.join("codex-home"),
        }
    }

    fn remove_dir(&self, id: SubscriptionId) {
        let dir = self.root.join(id.0.to_string());
        // Removes the shadow home's links, never what they point to.
        match std::fs::remove_dir_all(&dir) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                tracing::warn!(dir = %dir.display(), %error, "could not delete a subscription's directory");
            }
        }
    }
}

fn create_dir(path: &Path) -> Result<(), SubscriptionError> {
    std::fs::create_dir_all(path).map_err(|source| SubscriptionError::Io {
        path: path.to_path_buf(),
        source,
    })
}

/// Sets up `shadow` as a shadow home of the shared Codex home: every entry
/// of the shared home is linked, except the shadow home's own entries
/// ([`OWN_CODEX_ENTRIES`]). Entries the shadow home already has are kept,
/// so it can run again before every agent session and pick up new ones.
fn shadow_codex_home(shared: &Path, shadow: &Path) -> Result<(), SubscriptionError> {
    for dir in SHARED_CODEX_DIRS {
        create_dir(&shared.join(dir))?;
    }
    create_dir(shadow)?;
    let auth = shadow.join(CODEX_AUTH);
    if std::fs::symlink_metadata(&auth).is_ok_and(|metadata| metadata.is_symlink()) {
        return Err(SubscriptionError::SharedLogin { path: auth });
    }
    let io_error = |path: &Path| {
        let path = path.to_path_buf();
        move |source| SubscriptionError::Io { path, source }
    };
    for entry in std::fs::read_dir(shared).map_err(io_error(shared))? {
        let entry = entry.map_err(io_error(shared))?;
        let name = entry.file_name();
        if OWN_CODEX_ENTRIES.iter().any(|own| name == *own) {
            continue;
        }
        let link = shadow.join(&name);
        if std::fs::symlink_metadata(&link).is_ok() {
            continue;
        }
        let is_dir = entry.file_type().map_err(io_error(&entry.path()))?.is_dir();
        symlink(&entry.path(), &link, is_dir).map_err(io_error(&link))?;
    }
    Ok(())
}

#[cfg(unix)]
fn symlink(target: &Path, link: &Path, _is_dir: bool) -> io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

/// Needs Windows' developer mode or the right to create symbolic links.
#[cfg(windows)]
fn symlink(target: &Path, link: &Path, is_dir: bool) -> io::Result<()> {
    if is_dir {
        std::os::windows::fs::symlink_dir(target, link)
    } else {
        std::os::windows::fs::symlink_file(target, link)
    }
}
