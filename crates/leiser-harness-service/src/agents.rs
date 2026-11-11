//! The agent CLIs chats run on: the developer's installed Claude Code and
//! Codex, or the testkit's scripted fake agent CLI to try the app without a
//! subscription.

use std::io;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use leiser_service::{
    AgentCli, AgentEvent, ClaudeCodeDriver, CodexDriver, Question, QuestionOption, ServiceConfig,
    ServiceProgram,
};
use leiser_testkit::{FakeDriver, FakeGitHost, SAMPLE_REPO_CHECK_REPORT, Script, Step};

/// How often auto-fix polls open pull requests on the fake git host, so
/// trying it in the app takes seconds, not half a minute.
const FAKE_POLL_INTERVAL: Duration = Duration::from_secs(5);
/// Turns the fake agent CLI plays back before it only ends turns.
const FAKE_TURNS: usize = 1000;
/// Where the fake agent CLI keeps its agent sessions, in the data directory.
const FAKE_STATE_DIR: &str = "fake-agent-cli-state";
/// The shared Codex home of fake Codex subscriptions, in the data directory.
const FAKE_CODEX_HOME: &str = "codex-home";
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
/// `repo-check` runs on Claude Code, which has the skill, or on the fake
/// agent CLI. Worktrees go where the program says, by default into the
/// data directory. API keys go to the OS keychain and Codex subscriptions
/// share the developer's Codex home; on the fake agent CLI that shared
/// Codex home lives in the data directory, apart from the real one.
pub fn service_config(program: &ServiceProgram) -> Result<ServiceConfig, FakeAgentCliError> {
    let data_dir = program.data_dir().to_path_buf();
    let config = if let Some(fake) = program.fake_agent_cli() {
        let driver = fake_driver(fake, &data_dir.join(FAKE_STATE_DIR))?;
        let codex_home = data_dir.join(FAKE_CODEX_HOME);
        ServiceConfig::new(AgentCli::ClaudeCode, Arc::new(driver), data_dir)
            .with_codex_home(codex_home)
            .with_repo_check(Arc::new(fake_repo_check(fake)?))
            .with_git_host(Arc::new(FakeGitHost::new()))
            .with_pull_request_poll_interval(FAKE_POLL_INTERVAL)
    } else {
        let claude_code = Arc::new(ClaudeCodeDriver::installed());
        ServiceConfig::new(AgentCli::ClaudeCode, claude_code.clone(), data_dir)
            .with_driver(AgentCli::Codex, Arc::new(CodexDriver::installed()))
            .with_repo_check(claude_code)
    };
    Ok(match program.worktrees_dir() {
        Some(worktrees) => config.with_worktrees_dir(worktrees.to_path_buf()),
        None => config,
    })
}

/// A `repo-check` that answers with a made-up sample report.
fn fake_repo_check(program: &Path) -> io::Result<FakeDriver> {
    let script = Script::new().turn([Step::message(SAMPLE_REPO_CHECK_REPORT)]);
    FakeDriver::new(program, &script)
}

/// A driver that answers every message with a tool call, its result, the
/// message itself and a todo it adds through the harness MCP server. In a
/// repository without agent access to the todo list, the harness MCP
/// server refuses the todo, and the answer says so. Of every three turns,
/// the second first asks for an approval and the third asks a question,
/// to try the cards. Its agent sessions survive a service update.
fn fake_driver(program: &Path, state: &Path) -> io::Result<FakeDriver> {
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
        Step::CallHarnessTool {
            name: "add_todo".to_owned(),
            arguments: serde_json::json!({ "text": "Added by the fake agent CLI" }),
        },
    ];
    let script = (0..FAKE_TURNS).fold(Script::new(), |script, number| {
        let first = match number % 3 {
            1 => Some(approval.clone()),
            2 => Some(question.clone()),
            _ => None,
        };
        script.turn(first.into_iter().chain(turn.iter().cloned()))
    });
    FakeDriver::persistent(program, &script, state)
}
