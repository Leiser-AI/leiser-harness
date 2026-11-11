//! Finds the developer's installed agent CLIs.
//!
//! A macOS app started from the Finder does not get the login shell's
//! `PATH`, and the Claude desktop app bundles a Claude Code of its own that
//! is on no `PATH` at all. So an installed driver looks for its agent CLI,
//! in this order:
//!
//! 1. the program its variable names (`LEISER_CLAUDE_CODE`, `LEISER_CODEX`),
//!    used as given;
//! 2. `PATH`;
//! 3. common install directories: `~/.local/bin` (the Claude Code
//!    installer), `/opt/homebrew/bin` and `/usr/local/bin` (Homebrew, npm),
//!    and for Claude Code `~/.claude/local` (its older local install);
//! 4. for Claude Code on macOS, the newest copy the Claude desktop app
//!    bundles.
//!
//! It looks again whenever an agent session starts, so an agent CLI that
//! is installed or updated while the harness service runs is found, and a
//! bundled copy that an update removed is not used.
//!
//! The agent CLI runs with its own directory first on `PATH`: an agent CLI
//! installed with npm, such as Codex from Homebrew, is a Node.js script that
//! needs the `node` installed next to it.
//!
//! Not done: asking the login shell for its `PATH`. That runs the
//! developer's shell profile, which can be slow or wait for input. Add it
//! if agent CLIs installed by a version manager such as nvm are not found.

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Path, PathBuf};

use crate::{DriverError, Version};

/// The variables an installed driver reads.
const VARIABLES: [&str; 4] = ["PATH", "HOME", CLAUDE_CODE.variable, CODEX.variable];

/// System-wide install directories, searched after `PATH`.
const SYSTEM_DIRECTORIES: &[&str] = if cfg!(unix) {
    &["/opt/homebrew/bin", "/usr/local/bin"]
} else {
    &[]
};

/// Install directories under the home directory, searched after `PATH`.
const HOME_DIRECTORIES: &[&str] = if cfg!(unix) { &[".local/bin"] } else { &[] };

/// What an installed driver searches: a few environment variables and the
/// system-wide install directories.
#[derive(Debug, Clone, Default)]
pub struct Environment {
    variables: HashMap<OsString, OsString>,
    system_directories: Vec<PathBuf>,
}

impl Environment {
    /// The harness service's own environment and the system's install
    /// directories.
    #[must_use]
    pub fn current() -> Self {
        let variables = VARIABLES
            .into_iter()
            .filter_map(|name| Some((name.into(), std::env::var_os(name)?)))
            .collect();
        Self {
            variables,
            system_directories: SYSTEM_DIRECTORIES.iter().map(PathBuf::from).collect(),
        }
    }

    /// This environment with the variable `name` set to `value`.
    #[must_use]
    pub fn with(mut self, name: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        self.variables.insert(name.into(), value.into());
        self
    }

    fn variable(&self, name: &str) -> Option<&OsStr> {
        self.variables
            .get(OsStr::new(name))
            .map(OsString::as_os_str)
    }
}

/// How to find one agent CLI.
#[derive(Debug)]
pub(crate) struct Install {
    agent_cli: &'static str,
    /// The program's name, without the platform's executable suffix.
    program: &'static str,
    /// The variable that names the program to run.
    pub(crate) variable: &'static str,
    /// Install directories under the home directory, besides the common
    /// ones.
    home_directories: &'static [&'static str],
    bundled: Option<Bundled>,
}

/// Copies of an agent CLI that another app bundles, one per version, as
/// `<root>/<version>/<build>/<program>`.
#[derive(Debug)]
struct Bundled {
    /// Relative to the home directory.
    root: &'static str,
    /// Relative to a build's directory.
    program: &'static str,
}

pub(crate) const CLAUDE_CODE: Install = Install {
    agent_cli: "Claude Code",
    program: "claude",
    variable: "LEISER_CLAUDE_CODE",
    home_directories: &[".claude/local"],
    bundled: if cfg!(target_os = "macos") {
        Some(Bundled {
            root: "Library/Application Support/Claude/claude-code",
            program: "claude.app/Contents/MacOS/claude",
        })
    } else {
        None
    },
};

pub(crate) const CODEX: Install = Install {
    agent_cli: "Codex",
    program: "codex",
    variable: "LEISER_CODEX",
    home_directories: &[],
    bundled: None,
};

/// The program a driver runs.
#[derive(Debug, Clone)]
pub(crate) enum Program {
    /// A path, or a name the operating system looks up on `PATH`.
    Given(PathBuf),
    /// Looked up whenever an agent session starts.
    Installed {
        install: &'static Install,
        environment: Environment,
    },
}

impl Program {
    /// The program to run now.
    pub(crate) fn resolve(&self) -> Result<PathBuf, DriverError> {
        match self {
            Self::Given(program) => Ok(program.clone()),
            Self::Installed {
                install,
                environment,
            } => install.find(environment),
        }
    }
}

impl Install {
    fn find(&self, environment: &Environment) -> Result<PathBuf, DriverError> {
        if let Some(program) = environment.variable(self.variable) {
            return Ok(program.into());
        }
        let file_name = format!("{}{}", self.program, std::env::consts::EXE_SUFFIX);
        let on_path = environment
            .variable("PATH")
            .map(|path| std::env::split_paths(path).collect::<Vec<_>>())
            .unwrap_or_default();
        let home = environment.variable("HOME").map(Path::new);
        let directories = self.install_directories(environment, home);
        let found = on_path
            .iter()
            .chain(&directories)
            .map(|directory| directory.join(&file_name))
            .find(|program| is_program(program));
        if let Some(program) = found {
            return Ok(program);
        }
        let bundled_root = self
            .bundled
            .as_ref()
            .zip(home)
            .map(|(bundled, home)| (bundled, home.join(bundled.root)));
        if let Some(program) = bundled_root
            .as_ref()
            .and_then(|(bundled, root)| newest_bundled(root, bundled.program))
        {
            return Ok(program);
        }
        let mut searched = directories;
        searched.extend(bundled_root.map(|(_, root)| root));
        Err(DriverError::NotInstalled {
            agent_cli: self.agent_cli,
            program: self.program,
            variable: self.variable,
            searched,
        })
    }

    /// The install directories searched after `PATH`, in order.
    fn install_directories(&self, environment: &Environment, home: Option<&Path>) -> Vec<PathBuf> {
        let under_home = |names: &[&str]| -> Vec<PathBuf> {
            home.map(|home| names.iter().map(|name| home.join(name)).collect())
                .unwrap_or_default()
        };
        let mut directories = under_home(HOME_DIRECTORIES);
        directories.extend(environment.system_directories.iter().cloned());
        directories.extend(under_home(self.home_directories));
        directories
    }
}

/// The bundled program of the newest version under `root` that has one.
fn newest_bundled(root: &Path, program: &str) -> Option<PathBuf> {
    let mut versions: Vec<(Version, PathBuf)> = fs::read_dir(root)
        .ok()?
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let version = Version::parse(entry.file_name().to_str()?)?;
            Some((version, entry.path()))
        })
        .collect();
    versions.sort_by_key(|(version, _)| std::cmp::Reverse(*version));
    versions.into_iter().find_map(|(_, directory)| {
        let mut builds: Vec<PathBuf> = fs::read_dir(directory)
            .ok()?
            .filter_map(Result::ok)
            .map(|build| build.path().join(program))
            .filter(|path| is_program(path))
            .collect();
        builds.sort();
        builds.into_iter().next()
    })
}

/// Whether `path` is a file the developer may run.
fn is_program(path: &Path) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        metadata.is_file()
    }
}

/// The `PATH` to run `program` with: the harness service's own, after the
/// program's directory. `None` for a bare name, which is looked up on the
/// service's own `PATH`.
pub(crate) fn path_for(program: &Path) -> Option<OsString> {
    let directory = program
        .parent()
        .filter(|directory| !directory.as_os_str().is_empty())?;
    let inherited = std::env::var_os("PATH").unwrap_or_default();
    let directories =
        std::iter::once(directory.to_path_buf()).chain(std::env::split_paths(&inherited));
    std::env::join_paths(directories).ok()
}
