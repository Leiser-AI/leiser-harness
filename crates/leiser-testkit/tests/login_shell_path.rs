//! Agent sessions get the developer's `PATH` from their login shell, even
//! when the harness service inherited a minimal one (a desktop app started
//! from the Finder), and a slow or broken shell profile does not stop them.
//!
//! Each test gives the drivers an [`Environment`] of its own with a stand-in
//! login shell, so the test machine's shell profile does not matter.
#![cfg(unix)]

use std::error::Error;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use leiser_service::{
    AgentCli, Checkout, ClaudeCodeDriver, CodexDriver, Command, CommandError, Environment, Response,
};
use leiser_testkit::{TestRepo, TestService};

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

/// Long enough for a stand-in shell on a busy test machine.
const TIMEOUT: Duration = Duration::from_secs(10);

/// The tests write scripts and run them. Running them one at a time keeps
/// a script that one test is still writing from being open in a process
/// another test starts ("text file busy").
static ONE_AT_A_TIME: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn write_script(path: &Path, body: &str) -> TestResult {
    if let Some(directory) = path.parent() {
        fs::create_dir_all(directory)?;
    }
    fs::write(path, format!("#!/bin/sh\n{body}\n"))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o755))?;
    Ok(())
}

/// A login shell whose profile greets the developer and sets `PATH` to
/// `path`, then runs the command it was given. Each time it runs it adds a
/// line to `runs`.
fn login_shell(program: &Path, path: &str, runs: &Path) -> TestResult {
    let runs = runs.display();
    write_script(
        program,
        &format!(
            "echo ran >> '{runs}'\n\
             echo 'Welcome back!'\n\
             PATH='{path}'\n\
             export PATH\n\
             for argument; do command=$argument; done\n\
             eval \"$command\""
        ),
    )
}

/// A Codex that answers `--version`.
fn codex(program: &Path) -> TestResult {
    write_script(program, "echo 'codex-cli 0.167.1'")
}

fn path_of(directories: &[&Path]) -> TestResult<String> {
    Ok(std::env::join_paths(directories)?
        .into_string()
        .map_err(|_| "not UTF-8")?)
}

/// The directories of a stand-in machine: an empty home directory, one
/// directory on the inherited `PATH` and one only on the login shell's.
struct Machine {
    _root: tempfile::TempDir,
    home: PathBuf,
    inherited: PathBuf,
    login: PathBuf,
    shell: PathBuf,
    shell_runs: PathBuf,
}

impl Machine {
    fn new() -> TestResult<Self> {
        let root = tempfile::tempdir()?;
        let home = root.path().join("home");
        let inherited = root.path().join("inherited");
        let login = root.path().join("login");
        for directory in [&home, &inherited, &login] {
            fs::create_dir_all(directory)?;
        }
        Ok(Self {
            shell: root.path().join("shell"),
            shell_runs: root.path().join("shell-runs"),
            home,
            inherited,
            login,
            _root: root,
        })
    }

    /// The login shell sets `PATH` to the login directory and the system's.
    fn install_login_shell(&self) -> TestResult {
        let path = path_of(&[&self.login, Path::new("/usr/bin"), Path::new("/bin")])?;
        login_shell(&self.shell, &path, &self.shell_runs)
    }

    /// The service's environment: the stand-in home, shell and inherited
    /// `PATH`, with the login shell asked for its `PATH`.
    fn environment(&self, timeout: Duration) -> TestResult<Environment> {
        Ok(Environment::default()
            .with("HOME", &self.home)
            .with("PATH", path_of(&[&self.inherited])?)
            .with("SHELL", &self.shell)
            .with_login_shell_path(timeout))
    }

    fn shell_runs(&self) -> TestResult<usize> {
        Ok(fs::read_to_string(&self.shell_runs)?.lines().count())
    }
}

/// Sends the first message of a new chat on `driver`.
async fn send_first_message(
    driver: ClaudeCodeDriver,
) -> TestResult<Result<Response, CommandError>> {
    let harness = TestService::with_driver(AgentCli::ClaudeCode, Arc::new(driver))?;
    let service = harness.connect();
    let repo = TestRepo::new()?;
    let Response::RepositoryAdded { repository } = service
        .execute(Command::AddRepository {
            path: repo.path().to_path_buf(),
        })
        .await?
    else {
        return Err("expected RepositoryAdded".into());
    };
    let Response::ChatStarted { chat } = service
        .execute(Command::StartChat {
            repository,
            checkout: Checkout::Main,
        })
        .await?
    else {
        return Err("expected ChatStarted".into());
    };
    Ok(service
        .execute(Command::SendMessage {
            chat,
            text: "hello".to_owned(),
        })
        .await)
}

/// The contents of `file`, once it exists.
async fn wait_for(file: &Path) -> TestResult<String> {
    let started = Instant::now();
    while started.elapsed() < TIMEOUT {
        if let Ok(contents) = fs::read_to_string(file) {
            return Ok(contents);
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    Err(format!("{} was not written", file.display()).into())
}

#[tokio::test]
async fn a_chat_runs_claude_code_with_the_login_shells_path() -> TestResult {
    let _guard = ONE_AT_A_TIME.lock().await;
    let machine = Machine::new()?;
    machine.install_login_shell()?;
    // Claude Code is only on the login shell's PATH. Started for an agent
    // session, it writes down its PATH and exits.
    let recorded = machine.home.join("recorded-path");
    write_script(
        &machine.login.join("claude"),
        &format!(
            "if [ \"$1\" = --version ]; then echo '9.9.9 (Claude Code)'; exit 0; fi\n\
             printf '%s' \"$PATH\" > '{recorded}.partial'\n\
             mv '{recorded}.partial' '{recorded}'",
            recorded = recorded.display()
        ),
    )?;
    let driver = ClaudeCodeDriver::installed_in(machine.environment(TIMEOUT)?);

    let result = send_first_message(driver).await?;

    assert!(matches!(result, Ok(Response::MessageSent)), "{result:?}");
    let path = wait_for(&recorded).await?;
    let directories: Vec<PathBuf> = std::env::split_paths(&path).collect();
    let position = |directory: &Path| directories.iter().position(|entry| entry == directory);
    assert_eq!(position(&machine.login), Some(0), "{path}");
    assert!(position(Path::new("/usr/bin")).is_some(), "{path}");
    assert!(
        position(&machine.inherited) > position(Path::new("/usr/bin")),
        "the inherited PATH is kept after the login shell's: {path}"
    );
    Ok(())
}

#[test]
fn codex_on_the_login_shells_path_is_found_and_the_shell_asked_once() -> TestResult {
    let _guard = ONE_AT_A_TIME.blocking_lock();
    let machine = Machine::new()?;
    machine.install_login_shell()?;
    let program = machine.login.join("codex");
    codex(&program)?;
    let driver = CodexDriver::installed_in(machine.environment(TIMEOUT)?);

    assert_eq!(driver.program()?, program);
    assert_eq!(driver.program()?, program);
    assert_eq!(machine.shell_runs()?, 1);
    Ok(())
}

#[test]
fn codex_runs_the_tools_on_the_login_shells_path() -> TestResult {
    let _guard = ONE_AT_A_TIME.blocking_lock();
    let machine = Machine::new()?;
    machine.install_login_shell()?;
    // Like an agent's shell tool running `cargo` from the login shell's PATH.
    write_script(
        &machine.login.join("leiser-login-tool"),
        "echo 'codex-cli 0.167.1'",
    )?;
    write_script(
        &machine.inherited.join("codex"),
        "exec leiser-login-tool \"$@\"",
    )?;
    let driver = CodexDriver::installed_in(machine.environment(TIMEOUT)?);

    let version = driver.version()?;

    assert_eq!(version, leiser_service::Version::new(0, 167, 1));
    Ok(())
}

#[test]
fn a_login_shell_that_does_not_answer_in_time_is_given_up() -> TestResult {
    let _guard = ONE_AT_A_TIME.blocking_lock();
    let machine = Machine::new()?;
    // A profile that waits, with a child that keeps its output open.
    write_script(&machine.shell, "sleep 30 & sleep 30")?;
    let program = machine.inherited.join("codex");
    codex(&program)?;
    let driver = CodexDriver::installed_in(machine.environment(Duration::from_millis(300))?);
    let started = Instant::now();

    let found = driver.program()?;

    assert_eq!(found, program, "falls back to the inherited PATH");
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "waited {:?}",
        started.elapsed()
    );
    Ok(())
}

#[test]
fn a_broken_login_shell_falls_back_to_the_inherited_path() -> TestResult {
    let _guard = ONE_AT_A_TIME.blocking_lock();
    let machine = Machine::new()?;
    let program = machine.inherited.join("codex");
    codex(&program)?;
    let inherited = path_of(&[&machine.inherited])?;
    let home = machine.home.as_path();
    let broken = machine.home.join("broken-shell");
    write_script(&broken, "echo 'syntax error in .zshrc'\nexit 1")?;
    let environments = [
        (
            "a failing profile",
            Environment::default().with("SHELL", &broken),
        ),
        (
            "a missing shell",
            Environment::default().with("SHELL", machine.home.join("no-such-shell")),
        ),
        ("no SHELL", Environment::default()),
    ];

    for (case, environment) in environments {
        let environment = environment
            .with("HOME", home)
            .with("PATH", &inherited)
            .with_login_shell_path(TIMEOUT);

        let found = CodexDriver::installed_in(environment).program();

        assert_eq!(found.ok(), Some(program.clone()), "{case}");
    }
    Ok(())
}
