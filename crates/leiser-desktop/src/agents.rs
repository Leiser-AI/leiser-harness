//! The agent CLIs chats run on: the developer's installed Claude Code and
//! Codex, or the testkit's scripted fake agent CLI to try the app without a
//! subscription.

use std::path::PathBuf;
use std::sync::Arc;
use std::{env, io};

use leiser_service::{AgentCli, AgentEvent, ClaudeCodeDriver, CodexDriver, ServiceConfig};
use leiser_testkit::{FakeDriver, Script, Step};

/// When set, chats run on the `fake-agent-cli` program at this path.
const FAKE_AGENT_CLI_VARIABLE: &str = "LEISER_FAKE_AGENT_CLI";
/// Turns the fake agent CLI plays back before it only ends turns.
const FAKE_TURNS: usize = 1000;

/// Why the fake agent CLI cannot be used.
#[derive(Debug, thiserror::Error)]
#[error("could not set up the fake agent CLI: {0}")]
pub struct FakeAgentCliError(#[from] io::Error);

/// The harness service's configuration. Chats start on Claude Code and can
/// change to Codex. With `LEISER_FAKE_AGENT_CLI` set, they run on the fake
/// agent CLI instead and keep their data apart from real chats.
pub fn service_config(data_dir: PathBuf) -> Result<ServiceConfig, FakeAgentCliError> {
    if let Some(program) = env::var_os(FAKE_AGENT_CLI_VARIABLE) {
        let driver = fake_driver(&PathBuf::from(program))?;
        return Ok(ServiceConfig::new(
            AgentCli::ClaudeCode,
            Arc::new(driver),
            data_dir.join("fake-agent-cli"),
        ));
    }
    Ok(ServiceConfig::new(
        AgentCli::ClaudeCode,
        Arc::new(ClaudeCodeDriver::installed()),
        data_dir,
    )
    .with_driver(AgentCli::Codex, Arc::new(CodexDriver::installed())))
}

/// A driver that answers every message with a tool call, its result, the
/// message itself and a todo it adds through the harness MCP server. In a
/// repository without agent access to the todo list, the harness MCP
/// server refuses the todo, and the answer says so.
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
        Step::CallHarnessTool {
            name: "add_todo".to_owned(),
            arguments: serde_json::json!({ "text": "Added by the fake agent CLI" }),
        },
    ];
    let script = (0..FAKE_TURNS).fold(Script::new(), |script, _| script.turn(turn.clone()));
    FakeDriver::new(program, &script)
}
