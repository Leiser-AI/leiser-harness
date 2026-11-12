//! The installed drivers find the developer's agent CLIs where the desktop
//! app cannot rely on `PATH`, and say how to fix it when there is none.
//!
//! Each test gives the drivers an [`Environment`] of its own, so what is
//! installed on the test machine does not matter. The stand-in agent CLIs
//! are copies of the fake agent CLI.
#![cfg(unix)]

use std::error::Error;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use leiser_service::{
    AgentCli, ChatSettings, Checkout, ClaudeCodeDriver, CodexDriver, Command, CommandError, Driver,
    DriverError, Environment, Response,
};
use leiser_testkit::{TestRepo, TestService};

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

/// Answers `--version` with `0.1.0 (fake-agent-cli)`.
const FAKE_AGENT_CLI: &str = env!("CARGO_BIN_EXE_fake-agent-cli");

/// Puts a program called `name` into `directory`.
fn install(directory: &Path, name: &str) -> TestResult<PathBuf> {
    fs::create_dir_all(directory)?;
    let program = directory.join(name);
    fs::copy(FAKE_AGENT_CLI, &program)?;
    Ok(program)
}

/// An environment with only these variables set and no system-wide
/// install directories.
fn environment(variables: &[(&str, &Path)]) -> Environment {
    variables
        .iter()
        .fold(Environment::default(), |environment, (name, value)| {
            environment.with(*name, *value)
        })
}

/// Sends the first message of a new chat on `driver`.
async fn send_first_message(driver: Arc<dyn Driver>) -> TestResult<Result<Response, CommandError>> {
    let harness = TestService::with_driver(AgentCli::ClaudeCode, driver)?;
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
            settings: ChatSettings::default(),
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

#[tokio::test]
async fn a_chat_says_how_to_fix_a_missing_claude_code() -> TestResult {
    let home = tempfile::tempdir()?;
    let nothing_on_path = tempfile::tempdir()?;
    let driver = ClaudeCodeDriver::installed_in(environment(&[
        ("HOME", home.path()),
        ("PATH", nothing_on_path.path()),
    ]));

    let result = send_first_message(Arc::new(driver)).await?;

    let Err(CommandError::AgentCli { reason }) = result else {
        return Err(format!("expected AgentCli, got {result:?}").into());
    };
    assert!(reason.contains("Claude Code was not found"), "{reason}");
    assert!(reason.contains("`claude` on PATH"), "{reason}");
    let installer_directory = home.path().join(".local/bin");
    assert!(
        reason.contains(&installer_directory.display().to_string()),
        "{reason}"
    );
    assert!(
        reason.contains(ClaudeCodeDriver::PROGRAM_VARIABLE),
        "{reason}"
    );
    Ok(())
}

#[test]
fn codex_that_is_missing_names_its_own_variable() -> TestResult {
    let driver = CodexDriver::installed_in(Environment::default());

    let result = driver.program();

    let Err(DriverError::NotInstalled {
        agent_cli,
        variable,
        ..
    }) = result
    else {
        return Err(format!("expected NotInstalled, got {result:?}").into());
    };
    assert_eq!(agent_cli, "Codex");
    assert_eq!(variable, CodexDriver::PROGRAM_VARIABLE);
    Ok(())
}

#[test]
fn claude_code_on_path_is_found() -> TestResult {
    let directory = tempfile::tempdir()?;
    let program = install(directory.path(), "claude")?;

    let found =
        ClaudeCodeDriver::installed_in(environment(&[("PATH", directory.path())])).program()?;

    assert_eq!(found, program);
    Ok(())
}

#[test]
fn a_program_on_path_comes_before_the_install_directories() -> TestResult {
    let home = tempfile::tempdir()?;
    let on_path = tempfile::tempdir()?;
    install(&home.path().join(".local/bin"), "claude")?;
    let program = install(on_path.path(), "claude")?;

    let found = ClaudeCodeDriver::installed_in(environment(&[
        ("HOME", home.path()),
        ("PATH", on_path.path()),
    ]))
    .program()?;

    assert_eq!(found, program);
    Ok(())
}

#[test]
fn claude_code_in_its_install_directories_is_found_without_path() -> TestResult {
    for directory in [".local/bin", ".claude/local"] {
        let home = tempfile::tempdir()?;
        let program = install(&home.path().join(directory), "claude")?;

        let found =
            ClaudeCodeDriver::installed_in(environment(&[("HOME", home.path())])).program()?;

        assert_eq!(found, program, "installed in {directory}");
    }
    Ok(())
}

#[test]
fn codex_in_the_installer_directory_is_found_without_path() -> TestResult {
    let home = tempfile::tempdir()?;
    let program = install(&home.path().join(".local/bin"), "codex")?;

    let found = CodexDriver::installed_in(environment(&[("HOME", home.path())])).program()?;

    assert_eq!(found, program);
    Ok(())
}

#[test]
fn the_variable_names_the_program_even_when_path_has_one() -> TestResult {
    let on_path = tempfile::tempdir()?;
    install(on_path.path(), "claude")?;
    let chosen = PathBuf::from("/opt/chosen/claude");

    let found = ClaudeCodeDriver::installed_in(environment(&[
        ("PATH", on_path.path()),
        (ClaudeCodeDriver::PROGRAM_VARIABLE, &chosen),
    ]))
    .program()?;

    assert_eq!(found, chosen);
    Ok(())
}

#[test]
fn a_file_the_developer_may_not_run_is_not_a_program() -> TestResult {
    let directory = tempfile::tempdir()?;
    let file = install(directory.path(), "claude")?;
    fs::set_permissions(&file, fs::Permissions::from_mode(0o644))?;

    let result =
        ClaudeCodeDriver::installed_in(environment(&[("PATH", directory.path())])).program();

    assert!(
        matches!(result, Err(DriverError::NotInstalled { .. })),
        "expected NotInstalled, got {result:?}"
    );
    Ok(())
}

#[test]
fn an_agent_cli_installed_after_the_driver_was_made_is_found() -> TestResult {
    let home = tempfile::tempdir()?;
    let driver = ClaudeCodeDriver::installed_in(environment(&[("HOME", home.path())]));
    assert!(driver.program().is_err(), "nothing is installed yet");

    let program = install(&home.path().join(".local/bin"), "claude")?;

    assert_eq!(driver.program()?, program);
    Ok(())
}

#[cfg(target_os = "macos")]
mod claude_desktop_app {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    use leiser_service::{ClaudeCodeDriver, CommandError};

    use super::{TestResult, environment, install, send_first_message};

    /// Where the Claude desktop app keeps the Claude Code it bundles.
    const BUNDLED: &str = "Library/Application Support/Claude/claude-code";

    /// Bundles a Claude Code of `version` the way the Claude desktop app
    /// does.
    fn bundle(home: &Path, version: &str) -> TestResult<PathBuf> {
        install(
            &home
                .join(BUNDLED)
                .join(version)
                .join("e6736b2d954d/claude.app/Contents/MacOS"),
            "claude",
        )
    }

    #[test]
    fn the_newest_bundled_claude_code_is_found() -> TestResult {
        let home = tempfile::tempdir()?;
        bundle(home.path(), "2.1.9")?;
        let newest = bundle(home.path(), "2.1.10")?;
        fs::create_dir_all(home.path().join(BUNDLED).join("not-a-version"))?;

        let found =
            ClaudeCodeDriver::installed_in(environment(&[("HOME", home.path())])).program()?;

        assert_eq!(found, newest);
        Ok(())
    }

    #[test]
    fn claude_code_the_developer_installed_comes_before_the_bundled_one() -> TestResult {
        let home = tempfile::tempdir()?;
        bundle(home.path(), "2.1.10")?;
        let own = install(&home.path().join(".local/bin"), "claude")?;

        let found =
            ClaudeCodeDriver::installed_in(environment(&[("HOME", home.path())])).program()?;

        assert_eq!(found, own);
        Ok(())
    }

    #[tokio::test]
    async fn a_chat_runs_the_bundled_claude_code_when_none_is_on_path() -> TestResult {
        let home = tempfile::tempdir()?;
        bundle(home.path(), "2.1.10")?;
        let driver = ClaudeCodeDriver::installed_in(environment(&[("HOME", home.path())]));

        let result = send_first_message(Arc::new(driver)).await?;

        // The bundled stand-in ran: the driver refuses its version.
        let Err(CommandError::AgentCli { reason }) = result else {
            return Err(format!("expected AgentCli, got {result:?}").into());
        };
        assert!(reason.contains("Claude Code 0.1.0 is too old"), "{reason}");
        Ok(())
    }
}
