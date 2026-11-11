//! The agent CLIs chats run on: the developer's installed Claude Code and
//! Codex, or the testkit's scripted fake agent CLI to try the app without a
//! subscription.

use std::io;
use std::path::Path;
use std::sync::Arc;

use leiser_service::{
    AgentCli, AgentEvent, ClaudeCodeDriver, CodexDriver, ServiceConfig, ServiceProgram,
};
use leiser_testkit::{FakeDriver, Script, Step};

/// Turns the fake agent CLI plays back before it only ends turns.
const FAKE_TURNS: usize = 1000;
/// Where the fake agent CLI keeps its agent sessions, in the data directory.
const FAKE_STATE_DIR: &str = "fake-agent-cli-state";
/// A fake turn waits after its tool call while this file, in
/// [`FAKE_STATE_DIR`], is missing: delete it to hold turns open by hand,
/// for example to try a service update during a turn. Created on start.
const FAKE_TURN_GATE: &str = "turn-gate";

/// Why the fake agent CLI cannot be used.
#[derive(Debug, thiserror::Error)]
#[error("could not set up the fake agent CLI: {0}")]
pub struct FakeAgentCliError(#[from] io::Error);

/// The harness service's configuration. Chats start on Claude Code and can
/// change to Codex, or run on the fake agent CLI when the program says so.
pub fn service_config(program: &ServiceProgram) -> Result<ServiceConfig, FakeAgentCliError> {
    let data_dir = program.data_dir().to_path_buf();
    if let Some(fake) = program.fake_agent_cli() {
        let driver = fake_driver(fake, &data_dir.join(FAKE_STATE_DIR))?;
        return Ok(ServiceConfig::new(
            AgentCli::ClaudeCode,
            Arc::new(driver),
            data_dir,
        ));
    }
    Ok(ServiceConfig::new(
        AgentCli::ClaudeCode,
        Arc::new(ClaudeCodeDriver::installed()),
        data_dir,
    )
    .with_driver(AgentCli::Codex, Arc::new(CodexDriver::installed())))
}

/// A driver that answers every message with a tool call, its result and
/// the message itself. Its agent sessions survive a service update.
fn fake_driver(program: &Path, state: &Path) -> io::Result<FakeDriver> {
    std::fs::create_dir_all(state)?;
    let gate = state.join(FAKE_TURN_GATE);
    if !gate.exists() {
        std::fs::write(&gate, b"")?;
    }
    let turn = [
        Step::Emit {
            event: AgentEvent::ToolCall {
                name: "read_file".to_owned(),
                detail: "README.md".to_owned(),
            },
        },
        Step::WaitForGate { gate },
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
    FakeDriver::persistent(program, &script, state)
}
