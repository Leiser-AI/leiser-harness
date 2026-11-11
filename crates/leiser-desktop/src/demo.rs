//! The agent CLI chats run on until the real drivers land (#26, #27): the
//! testkit's scripted fake agent CLI.

use std::path::PathBuf;
use std::{env, io};

use leiser_service::AgentEvent;
use leiser_testkit::{FakeDriver, Script, Step};

/// Overrides where the `fake-agent-cli` program is.
const PROGRAM_VARIABLE: &str = "LEISER_FAKE_AGENT_CLI";
/// Turns the fake agent CLI plays back before it only ends turns.
const TURNS: usize = 1000;

/// Why the fake agent CLI cannot be used.
#[derive(Debug, thiserror::Error)]
pub enum DemoError {
    #[error(
        "the fake agent CLI is missing at {}; build it with \
         `cargo build -p leiser-testkit --bin fake-agent-cli` or set {PROGRAM_VARIABLE}",
        .0.display()
    )]
    Missing(PathBuf),
    #[error("could not set up the fake agent CLI: {0}")]
    Io(#[from] io::Error),
}

/// A driver that answers every message with a tool call, its result and
/// the message itself.
pub fn driver() -> Result<FakeDriver, DemoError> {
    let program = program()?;
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
    let script = (0..TURNS).fold(Script::new(), |script, _| script.turn(turn.clone()));
    Ok(FakeDriver::new(&program, &script)?)
}

/// `$LEISER_FAKE_AGENT_CLI`, or `fake-agent-cli` next to the app, which is
/// where cargo builds it.
fn program() -> Result<PathBuf, DemoError> {
    if let Some(program) = env::var_os(PROGRAM_VARIABLE) {
        return Ok(program.into());
    }
    let program =
        env::current_exe()?.with_file_name(format!("fake-agent-cli{}", env::consts::EXE_SUFFIX));
    if program.exists() {
        Ok(program)
    } else {
        Err(DemoError::Missing(program))
    }
}
