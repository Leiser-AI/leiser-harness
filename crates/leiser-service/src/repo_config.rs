//! A repository's own settings for the harness: `.leiser/config.toml` in
//! its main checkout, parsed once where it is read.
//!
//! ```toml
//! # The permission mode new chats start in: "supervised",
//! # "auto_accept_edits", "auto" (the default) or "full_access".
//! permission_mode = "supervised"
//!
//! [worktree]
//! # The branch new worktrees start from. Default: the remote `origin`'s
//! # default branch, else what the main checkout has checked out.
//! base_branch = "origin/development"
//! # The pattern of the branch name the agent proposes. Placeholders:
//! # `<type>` (such as `feat`) and `<short-name>` (such as `login-form`).
//! branch_pattern = "<type>/<short-name>"
//! # Runs in a new worktree before the agent starts.
//! setup_script = "pnpm install"
//! # Runs in a worktree when the last chat in it is done.
//! done_script = "cargo clean"
//!
//! # `repo-check` rules switched off, each with the reason why.
//! [repo_check.off]
//! D3 = "No pre-commit hook: the full check runs in CI in under a minute."
//!
//! [agent_access]
//! # Agent sessions may read and change the chat's todo list through the
//! # harness MCP server. Default: false, only the developer uses it.
//! todo_list = true
//! ```
//!
//! Scripts run with `sh -c` in the worktree, on Windows with the `sh` of
//! Git for Windows, so one script serves every platform.
//!
//! A repository without the file uses the defaults. Sections other
//! features read may be present; unknown keys inside `[worktree]` and
//! `[repo_check]` and `[agent_access]` are refused, so a typo does not go
//! unnoticed.

use std::collections::BTreeMap;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::{CheckId, PermissionMode, SwitchedOffCheck};

/// Where the file lives, relative to the main checkout.
const CONFIG_FILE: [&str; 2] = [".leiser", "config.toml"];

/// Why a repository's config could not be read.
#[derive(Debug, thiserror::Error)]
pub(crate) enum ConfigError {
    #[error("could not read {}: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("{}: {message}", path.display())]
    Invalid { path: PathBuf, message: String },
}

/// A repository's settings.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub(crate) struct RepositoryConfig {
    /// The permission mode new chats start in.
    #[serde(default)]
    pub permission_mode: PermissionMode,
    #[serde(default)]
    pub worktree: WorktreeConfig,
    #[serde(default)]
    pub repo_check: RepoCheckConfig,
    #[serde(default)]
    pub agent_access: AgentAccess,
}

/// Which `repo-check` rules the repository switches off.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RepoCheckConfig {
    /// Each switched-off rule with its reason.
    #[serde(default)]
    pub off: BTreeMap<CheckId, String>,
}

impl RepoCheckConfig {
    /// The switched-off checks, in order, as the desktop app lists them.
    pub fn switched_off(&self) -> Vec<SwitchedOffCheck> {
        self.off
            .iter()
            .map(|(check, reason)| SwitchedOffCheck {
                check: check.clone(),
                reason: reason.trim().to_owned(),
            })
            .collect()
    }
}

/// Which harness features agent sessions may use. Off by default: only the
/// developer uses them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct AgentAccess {
    /// The chat's todo list, through the harness MCP server.
    pub todo_list: bool,
}

/// How chats of the repository get and leave their worktrees.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WorktreeConfig {
    /// A remote-tracking branch such as `origin/development`, or a local
    /// branch.
    pub base_branch: Option<String>,
    #[serde(default)]
    pub branch_pattern: BranchPattern,
    pub setup_script: Option<String>,
    pub done_script: Option<String>,
}

impl RepositoryConfig {
    /// Reads the config of the repository whose main checkout is
    /// `repository`; the defaults when it has none.
    pub fn load(repository: &Path) -> Result<Self, ConfigError> {
        let path = CONFIG_FILE
            .iter()
            .fold(repository.to_path_buf(), |path, part| path.join(part));
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(source) => return Err(ConfigError::Io { path, source }),
        };
        let config: Self = toml::from_str(&text).map_err(|error| ConfigError::Invalid {
            path: path.clone(),
            message: error.message().to_owned(),
        })?;
        // Every switched-off check says why, for the next reader.
        if let Some((check, _)) = config
            .repo_check
            .off
            .iter()
            .find(|(_, reason)| reason.trim().is_empty())
        {
            return Err(ConfigError::Invalid {
                path,
                message: format!("`repo_check.off.{check}` needs a reason"),
            });
        }
        Ok(config)
    }
}

/// The form of a branch name, such as `<type>/<short-name>`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(try_from = "String")]
pub(crate) struct BranchPattern {
    text: String,
    parts: Vec<Part>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Part {
    Literal(String),
    /// Lowercase letters, such as `feat` or `fix`.
    Type,
    /// Lowercase words of letters and digits joined by `-`.
    ShortName,
}

const DEFAULT_PATTERN: &str = "<type>/<short-name>";

impl Default for BranchPattern {
    /// [`DEFAULT_PATTERN`], parsed; a test proves both agree.
    fn default() -> Self {
        Self {
            text: DEFAULT_PATTERN.to_owned(),
            parts: vec![Part::Type, Part::Literal("/".to_owned()), Part::ShortName],
        }
    }
}

impl TryFrom<String> for BranchPattern {
    type Error = String;

    fn try_from(text: String) -> Result<Self, String> {
        let mut parts = Vec::new();
        let mut rest = text.as_str();
        while !rest.is_empty() {
            let Some(start) = rest.find('<') else {
                parts.push(Part::Literal(rest.to_owned()));
                break;
            };
            if start > 0 {
                parts.push(Part::Literal(rest[..start].to_owned()));
            }
            let end = rest[start..]
                .find('>')
                .map(|end| start + end)
                .ok_or_else(|| format!("unclosed placeholder in branch pattern {text:?}"))?;
            parts.push(match &rest[start..=end] {
                "<type>" => Part::Type,
                "<short-name>" => Part::ShortName,
                other => {
                    return Err(format!(
                        "unknown placeholder {other} in branch pattern {text:?}; \
                         use <type> and <short-name>"
                    ));
                }
            });
            rest = &rest[end + 1..];
        }
        if !parts.contains(&Part::ShortName) {
            return Err(format!(
                "branch pattern {text:?} needs a <short-name>, or every chat gets the same branch"
            ));
        }
        Ok(Self { text, parts })
    }
}

impl fmt::Display for BranchPattern {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.text)
    }
}

impl BranchPattern {
    /// Whether `name` has the pattern's form.
    pub fn matches(&self, name: &str) -> bool {
        matches_from(&self.parts, name)
    }

    /// A name of the pattern's form, to show agents.
    pub fn example(&self) -> String {
        self.parts
            .iter()
            .map(|part| match part {
                Part::Literal(text) => text.as_str(),
                Part::Type => "feat",
                Part::ShortName => "login-form",
            })
            .collect()
    }
}

fn matches_from(parts: &[Part], name: &str) -> bool {
    let Some((part, rest)) = parts.split_first() else {
        return name.is_empty();
    };
    match part {
        Part::Literal(text) => name
            .strip_prefix(text.as_str())
            .is_some_and(|name| matches_from(rest, name)),
        // Tries every length, so a placeholder ends where the rest fits.
        Part::Type | Part::ShortName => (1..=name.len())
            .filter(|&end| name.is_char_boundary(end))
            .any(|end| fits(part, &name[..end]) && matches_from(rest, &name[end..])),
    }
}

fn fits(part: &Part, text: &str) -> bool {
    match part {
        Part::Literal(literal) => text == literal,
        Part::Type => text.bytes().all(|byte| byte.is_ascii_lowercase()),
        Part::ShortName => text.split('-').all(|word| {
            !word.is_empty()
                && word
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pattern(text: &str) -> Result<BranchPattern, String> {
        BranchPattern::try_from(text.to_owned())
    }

    #[test]
    fn the_default_pattern_is_type_slash_short_name() -> Result<(), String> {
        assert_eq!(BranchPattern::default(), pattern(DEFAULT_PATTERN)?);
        assert_eq!(BranchPattern::default().example(), "feat/login-form");
        Ok(())
    }

    #[test]
    fn names_of_the_patterns_form_match() -> Result<(), String> {
        let default = BranchPattern::default();
        assert!(default.matches("feat/tool-registry"));
        assert!(default.matches("fix/v2-login"));
        for name in [
            "feat",
            "feat/",
            "Feat/x",
            "feat/a--b",
            "feat/a/b",
            "feat/a_b",
        ] {
            assert!(!default.matches(name), "{name}");
        }
        let prefixed = pattern("timo/<type>/<short-name>")?;
        assert!(prefixed.matches("timo/docs/readme"));
        assert!(!prefixed.matches("docs/readme"));
        Ok(())
    }

    #[test]
    fn a_pattern_needs_known_placeholders_and_a_short_name() {
        assert!(pattern("<type>/<nonsense>").is_err());
        assert!(pattern("<type>/<short-name").is_err());
        assert!(pattern("<type>/fixed").is_err());
    }

    #[test]
    fn unknown_worktree_keys_are_refused() {
        let config: Result<RepositoryConfig, _> = toml::from_str("[worktree]\nsetup = \"x\"\n");
        assert!(config.is_err());
        let other: Result<RepositoryConfig, _> = toml::from_str("[other]\nkey = 1\n");
        assert_eq!(other.ok(), Some(RepositoryConfig::default()));
    }
}
