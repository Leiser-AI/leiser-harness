//! A repository's own harness settings: the tracked `.leiser/config.toml`.
//!
//! Only the `[repo_check]` table is read here. Other tables belong to
//! other parts of the harness and are left alone.
//!
//! ```toml
//! [repo_check.off]
//! D3 = "No pre-commit hook: the full check runs in CI in under a minute."
//! ```
//!
//! Every switched-off check needs a reason, so the next reader knows why.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::{CheckId, SwitchedOffCheck};

/// Where the settings live, relative to the repository's root.
pub(crate) const CONFIG_FILE: &str = ".leiser/config.toml";

/// The settings of one repository. A repository without the file has
/// the defaults: every check on.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct RepoConfig {
    /// The `repo-check` rules switched off, each with its reason.
    pub repo_check_off: BTreeMap<CheckId, String>,
}

/// Why `.leiser/config.toml` could not be used.
#[derive(Debug, thiserror::Error)]
pub(crate) enum RepoConfigError {
    #[error("could not read {}: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("{} is not valid: {message}", path.display())]
    Invalid { path: PathBuf, message: String },
}

#[derive(Debug, Default, Deserialize)]
struct ConfigFile {
    #[serde(default)]
    repo_check: RepoCheckTable,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RepoCheckTable {
    #[serde(default)]
    off: BTreeMap<CheckId, String>,
}

impl RepoConfig {
    /// Reads the settings of the repository at `repository`.
    ///
    /// # Errors
    ///
    /// Returns a [`RepoConfigError`] when the file exists but cannot be
    /// read or is not valid.
    pub fn load(repository: &Path) -> Result<Self, RepoConfigError> {
        let path = repository.join(CONFIG_FILE);
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(source) => return Err(RepoConfigError::Io { path, source }),
        };
        let file: ConfigFile = toml::from_str(&text).map_err(|error| {
            let message = match error.span() {
                Some(span) => {
                    let line = text
                        .get(..span.start)
                        .map_or(0, |before| before.matches('\n').count())
                        + 1;
                    format!("line {line}: {}", error.message())
                }
                None => error.message().to_owned(),
            };
            RepoConfigError::Invalid {
                path: path.clone(),
                message,
            }
        })?;
        if let Some((check, _)) = file
            .repo_check
            .off
            .iter()
            .find(|(_, reason)| reason.trim().is_empty())
        {
            return Err(RepoConfigError::Invalid {
                path,
                message: format!("`repo_check.off.{check}` needs a reason"),
            });
        }
        Ok(Self {
            repo_check_off: file.repo_check.off,
        })
    }

    /// The switched-off checks, in order, as the desktop app lists them.
    pub fn switched_off(&self) -> Vec<SwitchedOffCheck> {
        self.repo_check_off
            .iter()
            .map(|(check, reason)| SwitchedOffCheck {
                check: check.clone(),
                reason: reason.trim().to_owned(),
            })
            .collect()
    }
}
