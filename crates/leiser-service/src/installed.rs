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
//! `PATH` here is the developer's: the harness service asks the login shell
//! (`SHELL`) once, as `$SHELL -l -i -c`, so the profile files that set up
//! Homebrew, `cargo` or a version manager such as nvm run. Then come the
//! directories of the service's inherited `PATH` that the login shell's
//! lacks. The agent CLI runs with this `PATH`, so its shell tool finds the
//! developer's tools, with its own directory first: an agent CLI installed
//! with npm, such as Codex from Homebrew, is a Node.js script that needs
//! the `node` installed next to it.
//!
//! Asking the login shell runs the developer's profile, which can be slow,
//! broken or wait for input. So the shell gets no input, its `PATH` is read
//! between markers that the profile's own output cannot be mistaken for,
//! and it gets [`LOGIN_SHELL_TIMEOUT`]; when it does not answer in time it
//! is stopped with everything it started. On any failure the service uses
//! its inherited `PATH`, as before. [`Environment::current`] starts asking
//! in the background, so the first agent session usually need not wait.
//! The answer is kept for the life of the service: a changed profile takes
//! effect after a restart of the harness service.

use std::collections::{HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use crate::{DriverError, Version};

/// The variables an installed driver reads.
const VARIABLES: [&str; 6] = [
    "PATH",
    "HOME",
    "SHELL",
    CLAUDE_CODE.variable,
    CODEX.variable,
    GH.variable,
];

/// How long the harness service waits for the login shell's `PATH`.
const LOGIN_SHELL_TIMEOUT: Duration = Duration::from_secs(5);

/// System-wide install directories, searched after `PATH`.
const SYSTEM_DIRECTORIES: &[&str] = if cfg!(unix) {
    &["/opt/homebrew/bin", "/usr/local/bin"]
} else {
    &[]
};

/// Install directories under the home directory, searched after `PATH`.
const HOME_DIRECTORIES: &[&str] = if cfg!(unix) { &[".local/bin"] } else { &[] };

/// What an installed driver searches: a few environment variables, the
/// system-wide install directories and, if asked for, the login shell's
/// `PATH`.
#[derive(Debug, Clone, Default)]
pub struct Environment {
    variables: HashMap<OsString, OsString>,
    system_directories: Vec<PathBuf>,
    /// Shared by the environment's clones, so the login shell is asked
    /// once.
    login_shell: Option<Arc<LoginShell>>,
}

impl Environment {
    /// The harness service's own environment, the system's install
    /// directories and the login shell's `PATH`, asked once per harness
    /// service and starting now.
    #[must_use]
    pub fn current() -> Self {
        static LOGIN_SHELL: OnceLock<Arc<LoginShell>> = OnceLock::new();
        let variables = VARIABLES
            .into_iter()
            .filter_map(|name| Some((name.into(), std::env::var_os(name)?)))
            .collect();
        let mut first = false;
        let login_shell = LOGIN_SHELL.get_or_init(|| {
            first = true;
            Arc::new(LoginShell::new(LOGIN_SHELL_TIMEOUT))
        });
        let environment = Self {
            variables,
            system_directories: SYSTEM_DIRECTORIES.iter().map(PathBuf::from).collect(),
            login_shell: Some(Arc::clone(login_shell)),
        };
        if first {
            let asking = environment.clone();
            // Without the thread the first agent session asks.
            let _ = std::thread::Builder::new()
                .name("login-shell-path".to_owned())
                .spawn(move || asking.path());
        }
        environment
    }

    /// This environment with the variable `name` set to `value`.
    #[must_use]
    pub fn with(mut self, name: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        self.variables.insert(name.into(), value.into());
        self
    }

    /// This environment with `PATH` from the login shell its `SHELL` names,
    /// asked the first time a driver needs it and given up after
    /// `timeout`. The directories of its own `PATH` follow the login
    /// shell's.
    #[must_use]
    pub fn with_login_shell_path(mut self, timeout: Duration) -> Self {
        self.login_shell = Some(Arc::new(LoginShell::new(timeout)));
        self
    }

    fn variable(&self, name: &str) -> Option<&OsStr> {
        self.variables
            .get(OsStr::new(name))
            .map(OsString::as_os_str)
    }

    /// The `PATH` agent CLIs are looked up on and run with.
    fn path(&self) -> Option<OsString> {
        let inherited = self.variable("PATH");
        let Some(login) = self
            .login_shell
            .as_ref()
            .and_then(|login_shell| login_shell.path(self.variable("SHELL")))
        else {
            return inherited.map(OsStr::to_os_string);
        };
        let inherited = inherited.map(std::env::split_paths).into_iter().flatten();
        join_unique(std::env::split_paths(&login).chain(inherited))
    }
}

/// The login shell's `PATH`, once asked.
#[derive(Debug)]
struct LoginShell {
    timeout: Duration,
    /// `None` when the login shell gave none.
    path: OnceLock<Option<OsString>>,
}

impl LoginShell {
    fn new(timeout: Duration) -> Self {
        Self {
            timeout,
            path: OnceLock::new(),
        }
    }

    /// Asks `shell` the first time; later calls, and calls while it is
    /// being asked, get the same answer.
    fn path(&self, shell: Option<&OsStr>) -> Option<OsString> {
        self.path
            .get_or_init(|| {
                let shell = Path::new(shell?);
                login_shell::ask(shell, self.timeout)
            })
            .clone()
    }
}

/// `directories` as a `PATH`, each once and without empty ones.
fn join_unique(directories: impl Iterator<Item = PathBuf>) -> Option<OsString> {
    let mut seen = HashSet::new();
    let directories = directories
        .filter(|directory| !directory.as_os_str().is_empty())
        .filter(|directory| seen.insert(directory.clone()));
    std::env::join_paths(directories).ok()
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

/// The GitHub CLI, which signs host accounts in on GitHub.
pub(crate) const GH: Install = Install {
    agent_cli: "the GitHub CLI",
    program: "gh",
    variable: "LEISER_GH",
    home_directories: &[],
    bundled: None,
};

/// The program a driver runs.
#[derive(Debug, Clone)]
pub(crate) enum Program {
    /// A path, or a name the operating system looks up on `PATH`. Runs with
    /// the harness service's own `PATH`.
    Given(PathBuf),
    /// Looked up whenever an agent session starts.
    Installed {
        install: &'static Install,
        environment: Environment,
    },
}

/// A program to run now, and the `PATH` to run it with.
#[derive(Debug)]
pub(crate) struct Found {
    pub(crate) program: PathBuf,
    path: Option<OsString>,
}

impl Found {
    /// A command that runs the program with its `PATH`.
    pub(crate) fn command(&self) -> std::process::Command {
        let mut command = std::process::Command::new(&self.program);
        if let Some(path) = &self.path {
            command.env("PATH", path);
        }
        command
    }
}

impl Program {
    /// The program to run now.
    pub(crate) fn resolve(&self) -> Result<Found, DriverError> {
        let (program, path) = match self {
            Self::Given(program) => (program.clone(), std::env::var_os("PATH")),
            Self::Installed {
                install,
                environment,
            } => {
                let path = environment.path();
                (install.find(environment, path.as_deref())?, path)
            }
        };
        let path = path_for(&program, path.as_deref());
        Ok(Found { program, path })
    }
}

impl Install {
    fn find(
        &self,
        environment: &Environment,
        path: Option<&OsStr>,
    ) -> Result<PathBuf, DriverError> {
        if let Some(program) = environment.variable(self.variable) {
            return Ok(program.into());
        }
        let file_name = format!("{}{}", self.program, std::env::consts::EXE_SUFFIX);
        let on_path = path
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

/// The `PATH` to run `program` with: `path`, after the program's
/// directory.
fn path_for(program: &Path, path: Option<&OsStr>) -> Option<OsString> {
    let directory = program
        .parent()
        .filter(|directory| !directory.as_os_str().is_empty());
    if directory.is_none() && path.is_none() {
        return None;
    }
    let path = path.map(std::env::split_paths).into_iter().flatten();
    join_unique(directory.map(Path::to_path_buf).into_iter().chain(path))
}

/// Asking the login shell for its `PATH`.
mod login_shell {
    use std::ffi::OsString;
    use std::path::Path;
    use std::time::Duration;

    /// Printed before and after the `PATH`, so the profile's own output is
    /// not taken for it.
    #[cfg(unix)]
    const MARKER: &str = "__LEISER_HARNESS_LOGIN_SHELL_PATH__";

    /// The `PATH` of `shell` started as an interactive login shell, or
    /// `None` when it gives none within `timeout`.
    #[cfg(unix)]
    pub(super) fn ask(shell: &Path, timeout: Duration) -> Option<OsString> {
        use std::io::BufReader;
        use std::os::unix::process::CommandExt;
        use std::process::{Command, Stdio};
        use std::sync::mpsc;

        // `printenv` prints the exported `PATH` joined with `:` in every
        // shell, also in fish, whose own `$PATH` is a list.
        let script = format!("echo {MARKER}; printenv PATH; echo {MARKER}");
        let spawned = Command::new(shell)
            .args(["-l", "-i", "-c", &script])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            // Its own process group, to stop what the profile started too,
            // and so the shell does not take over the terminal the harness
            // service may run in.
            .process_group(0)
            .spawn();
        let mut child = match spawned {
            Ok(child) => child,
            Err(error) => {
                tracing::warn!(%error, shell = %shell.display(), "could not start the login shell; agent CLIs get the harness service's PATH");
                return None;
            }
        };
        let stdout = child.stdout.take()?;
        let (sender, receiver) = mpsc::channel();
        // Reads until the second marker, not to the end: something the
        // profile started in the background may keep the output open.
        std::thread::spawn(move || {
            let _ = sender.send(read_path(BufReader::new(stdout)));
        });
        let path = match receiver.recv_timeout(timeout) {
            Ok(Some(path)) => Some(path),
            Ok(None) => {
                tracing::warn!(shell = %shell.display(), "the login shell printed no PATH; agent CLIs get the harness service's PATH");
                None
            }
            Err(_) => {
                tracing::warn!(shell = %shell.display(), ?timeout, "the login shell did not print its PATH in time; agent CLIs get the harness service's PATH");
                stop_group(child.id());
                let _ = child.kill();
                None
            }
        };
        // Reaps the shell without waiting for it.
        std::thread::spawn(move || child.wait());
        path
    }

    #[cfg(not(unix))]
    pub(super) fn ask(_shell: &Path, _timeout: Duration) -> Option<OsString> {
        None
    }

    /// The line between the first two marker lines of `output`.
    #[cfg(unix)]
    fn read_path(output: impl std::io::BufRead) -> Option<OsString> {
        use std::os::unix::ffi::OsStringExt;

        let mut lines = output.split(b'\n');
        lines
            .by_ref()
            .map_while(Result::ok)
            .find(|line| line == MARKER.as_bytes())?;
        let path = lines.next()?.ok()?;
        let end = lines.next()?.ok()?;
        (end == MARKER.as_bytes() && !path.is_empty()).then(|| OsString::from_vec(path))
    }

    /// Stops the process group `group` and everything in it.
    #[cfg(unix)]
    fn stop_group(group: u32) {
        let _ = std::process::Command::new("kill")
            .args(["-KILL", &format!("-{group}")])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
}
