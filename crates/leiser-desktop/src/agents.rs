//! The agent CLIs chats run on: the developer's installed Claude Code and
//! Codex, or the testkit's scripted fake agent CLI to try the app without a
//! subscription.

use std::path::PathBuf;
use std::sync::Arc;
use std::{env, io};

use leiser_service::{AgentCli, AgentEvent, ClaudeCodeDriver, CodexDriver, ServiceConfig};
use leiser_testkit::{FakeDriver, SAMPLE_REPO_CHECK_REPORT, Script, Step};

/// When set, chats run on the `fake-agent-cli` program at this path.
const FAKE_AGENT_CLI_VARIABLE: &str = "LEISER_FAKE_AGENT_CLI";
/// Turns the fake agent CLI plays back before it only ends turns.
const FAKE_TURNS: usize = 1000;

/// Why the fake agent CLI cannot be used.
#[derive(Debug, thiserror::Error)]
#[error("could not set up the fake agent CLI: {0}")]
pub struct FakeAgentCliError(#[from] io::Error);

/// The harness service's configuration. Chats start on Claude Code and can
/// change to Codex; their worktrees go to `worktrees_dir`. `repo-check`
/// runs on Claude Code, which has the skill. With `LEISER_FAKE_AGENT_CLI`
/// set, both run on the fake agent CLI instead and keep their data and
/// worktrees apart from real chats.
pub fn service_config(
    data_dir: PathBuf,
    worktrees_dir: PathBuf,
) -> Result<ServiceConfig, FakeAgentCliError> {
    if let Some(program) = env::var_os(FAKE_AGENT_CLI_VARIABLE) {
        let program = PathBuf::from(program);
        return Ok(ServiceConfig::new(
            AgentCli::ClaudeCode,
            Arc::new(fake_driver(&program)?),
            data_dir.join(FAKE_DIR),
        )
        .with_worktrees_dir(worktrees_dir.join(FAKE_DIR))
        .with_repo_check(Arc::new(fake_repo_check(&program)?)));
    }
    let claude_code = Arc::new(ClaudeCodeDriver::installed());
    Ok(
        ServiceConfig::new(AgentCli::ClaudeCode, claude_code.clone(), data_dir)
            .with_driver(AgentCli::Codex, Arc::new(CodexDriver::installed()))
            .with_worktrees_dir(worktrees_dir)
            .with_repo_check(claude_code),
    )
}

/// A `repo-check` that answers with a made-up sample report.
fn fake_repo_check(program: &std::path::Path) -> io::Result<FakeDriver> {
    let script = Script::new().turn([Step::message(SAMPLE_REPO_CHECK_REPORT)]);
    FakeDriver::new(program, &script)
}

/// Where the fake agent CLI's chats keep their data and worktrees.
const FAKE_DIR: &str = "fake-agent-cli";

/// A driver that answers every message with a tool call, its result and
/// the message itself.
fn fake_driver(program: &std::path::Path) -> io::Result<FakeDriver> {
    let turn = [
        Step::Emit {
            event: AgentEvent::ToolCall {
                name: "read_file".to_owned(),
                detail: "README.md".to_owned(),
            },
        },
        Step::Emit {
            event: AgentEvent::ToolResult {
                name: "read_file".to_owned(),
                output: "(the fake agent CLI reads no files)".to_owned(),
            },
        },
        Step::message("Fake agent CLI here. You wrote:"),
        Step::EchoUserMessage,
    ];
    let script = (0..FAKE_TURNS).fold(Script::new(), |script, _| script.turn(turn.clone()));
    FakeDriver::new(program, &script)
}
