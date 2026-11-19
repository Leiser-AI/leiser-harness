//! The Claude Code driver never leaves a Claude Code process behind, and
//! never waits forever on one.
//!
//! These tests stand in for Claude Code with shell scripts, so they need no
//! subscription and run in CI on Unix. The contract tests against the real
//! Claude Code are in `claude_code_contract.rs`.
#![cfg(unix)]

use std::error::Error;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use leiser_service::{
    Access, AgentEvent, ClaudeCodeDriver, Driver, DriverError, PermissionMode, SessionInput,
    SessionSpec,
};
use leiser_testkit::TestRepo;
use tokio::sync::mpsc;

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

/// Longer than the driver may take to end a process that does not exit.
const DEADLINE: Duration = Duration::from_secs(30);

/// The stand-ins for Claude Code, written before any test starts a process,
/// so none is still open for writing when one runs ("text file busy").
struct Scripts {
    _dir: tempfile::TempDir,
    /// Sends an approval request for each message and keeps running after
    /// its standard input closes, as Claude Code waiting on its prompt.
    waits_on_approval: PathBuf,
    /// Never answers `--version`.
    hangs_on_version: PathBuf,
}

static SCRIPTS: LazyLock<Result<Scripts, String>> =
    LazyLock::new(|| write_scripts().map_err(|error| error.to_string()));

fn write_scripts() -> TestResult<Scripts> {
    let dir = tempfile::tempdir()?;
    let version = ClaudeCodeDriver::MINIMUM_VERSION;
    let prompt = r#"{"type":"control_request","request_id":"r1","request":{"subtype":"can_use_tool","tool_name":"Bash","input":{"command":"ls"}}}"#;
    let waits_on_approval = script(
        dir.path(),
        "waits-on-approval",
        &format!(
            "if [ \"$1\" = --version ]; then echo '{version} (Claude Code)'; exit 0; fi\n\
             echo '{{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"s1\"}}'\n\
             while IFS= read -r line; do\n\
             case \"$line\" in *'\"type\":\"user\"'*) echo '{prompt}' ;; esac\n\
             done\n\
             exec sleep 120\n"
        ),
    )?;
    let hangs_on_version = script(dir.path(), "hangs-on-version", "exec sleep 120\n")?;
    Ok(Scripts {
        _dir: dir,
        waits_on_approval,
        hangs_on_version,
    })
}

fn script(dir: &Path, name: &str, body: &str) -> TestResult<PathBuf> {
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}"))?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
    Ok(path)
}

fn scripts() -> TestResult<&'static Scripts> {
    SCRIPTS.as_ref().map_err(|error| error.clone().into())
}

#[tokio::test]
async fn login_output_stays_open_until_the_cancelled_child_is_reaped() -> TestResult {
    let mut command = tokio::process::Command::new("sh");
    command.args(["-c", "echo $$; exec 1>&- 2>&-; exec sleep 30"]);
    let mut login = leiser_service::LoginProcess::spawn(command)?;
    let pid: u32 = login
        .output
        .recv()
        .await
        .ok_or("missing child PID")?
        .parse()?;
    assert!(
        tokio::time::timeout(Duration::from_millis(250), login.output.recv())
            .await
            .is_err(),
        "the login reported closure while its child still ran"
    );
    login.outcome.close();
    assert_eq!(
        tokio::time::timeout(DEADLINE, login.output.recv()).await?,
        None
    );
    assert!(
        !std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .output()?
            .status
            .success()
    );
    Ok(())
}

#[tokio::test]
async fn a_dropped_agent_session_ends_claude_code_even_while_it_waits_for_approval() -> TestResult {
    let scripts = scripts()?;
    let repo = TestRepo::new()?;
    let mut session = ClaudeCodeDriver::new(&scripts.waits_on_approval).start(SessionSpec {
        working_directory: repo.path().to_path_buf(),
        harness_mcp_url: None,
        access: Access::Chat,
        permission_mode: PermissionMode::Supervised,
        subscription: None,
    })?;
    session
        .input
        .send(SessionInput::UserMessage {
            text: "hello".to_owned(),
        })
        .await?;
    let asked = wait_for(&mut session.events, |event| {
        matches!(event, AgentEvent::ApprovalRequested { .. })
    })
    .await;
    assert!(asked, "no approval request");

    drop(session.input);
    let ended = wait_for(&mut session.events, |event| {
        *event == AgentEvent::SessionEnded
    })
    .await;

    assert!(ended, "Claude Code was left running");
    Ok(())
}

/// Whether an event that `wanted` accepts arrives within [`DEADLINE`].
async fn wait_for(
    events: &mut mpsc::Receiver<AgentEvent>,
    wanted: impl Fn(&AgentEvent) -> bool,
) -> bool {
    tokio::time::timeout(DEADLINE, async {
        while let Some(event) = events.recv().await {
            if wanted(&event) {
                return true;
            }
        }
        false
    })
    .await
    .unwrap_or(false)
}

#[test]
fn claude_code_that_never_answers_its_version_is_refused_in_time() -> TestResult {
    let scripts = scripts()?;
    let started = Instant::now();

    let result = ClaudeCodeDriver::new(&scripts.hangs_on_version).version();

    assert!(
        matches!(result, Err(DriverError::VersionTimeout { .. })),
        "expected VersionTimeout, got {result:?}"
    );
    assert!(started.elapsed() < DEADLINE, "{:?}", started.elapsed());
    Ok(())
}
