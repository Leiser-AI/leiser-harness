//! The agent CLIs chats run on: the developer's installed Claude Code and
//! Codex, or the testkit's scripted fake agent CLI to try the app without a
//! subscription.

use std::path::PathBuf;
use std::sync::Arc;
use std::{env, io};

use leiser_service::{
    AgentCli, AgentEvent, ClaudeCodeDriver, CodexDriver, Question, QuestionOption, ServiceConfig,
};
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

/// A driver that answers every message with a tool call, its result and
/// the message itself. Of every three turns, the second first asks for an
/// approval and the third asks a question, to try the cards.
fn fake_driver(program: &std::path::Path) -> io::Result<FakeDriver> {
    let approval = Step::RequestApproval {
        description: "Run `cargo test --workspace`".to_owned(),
    };
    let question = Step::AskQuestions {
        questions: vec![Question {
            id: "runner".to_owned(),
            header: "Test runner".to_owned(),
            question: "Which test runner should I use?".to_owned(),
            options: vec![
                QuestionOption {
                    label: "cargo test".to_owned(),
                    description: "The default runner".to_owned(),
                },
                QuestionOption {
                    label: "cargo nextest".to_owned(),
                    description: "Faster, needs an install".to_owned(),
                },
            ],
            multi_select: false,
        }],
    };
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
    let script = (0..FAKE_TURNS).fold(Script::new(), |script, number| {
        let first = match number % 3 {
            1 => Some(approval.clone()),
            2 => Some(question.clone()),
            _ => None,
        };
        script.turn(first.into_iter().chain(turn.iter().cloned()))
    });
    FakeDriver::new(program, &script)
}
