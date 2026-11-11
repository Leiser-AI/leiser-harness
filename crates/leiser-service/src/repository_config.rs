//! A repository's own settings for the harness, in `.leiser/config.toml`
//! of its main checkout. A repository without the file uses the defaults.
//!
//! ```toml
//! [agent_access]
//! # Agent sessions may read and change the chat's todo list.
//! todo_list = true
//! ```
//!
//! Only the keys this module knows are read; other tables are left to the
//! features that own them.

use std::io;
use std::path::{Path, PathBuf};

use serde::Deserialize;

/// Where a repository keeps its config, relative to its main checkout.
pub(crate) const CONFIG_FILE: &str = ".leiser/config.toml";

/// The settings of one repository.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub(crate) struct RepositoryConfig {
    pub agent_access: AgentAccess,
}

/// Which harness features agent sessions may use. Off by default: only the
/// developer uses them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct AgentAccess {
    /// The chat's todo list, through the harness MCP server.
    pub todo_list: bool,
}

/// Why a repository's config could not be read.
#[derive(Debug, thiserror::Error)]
pub(crate) enum ConfigError {
    #[error("could not read {}: {source}", path.display())]
    Read {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("{} is invalid: {source}", path.display())]
    Invalid {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
}

impl ConfigError {
    pub(crate) fn path(&self) -> &Path {
        match self {
            Self::Read { path, .. } | Self::Invalid { path, .. } => path,
        }
    }
}

/// Reads the config of the repository whose main checkout is `repository`.
pub(crate) fn load(repository: &Path) -> Result<RepositoryConfig, ConfigError> {
    let path = repository.join(CONFIG_FILE);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(RepositoryConfig::default());
        }
        Err(source) => return Err(ConfigError::Read { path, source }),
    };
    toml::from_str(&text).map_err(|source| ConfigError::Invalid { path, source })
}
