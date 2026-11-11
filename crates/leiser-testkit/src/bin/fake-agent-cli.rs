//! A scripted stand-in for an agent CLI, used by the harness service's tests.
//!
//! Usage: `fake-agent-cli <script.json> <harness-mcp-url>`. Reads one
//! [`FakeInput`] per line on standard input and answers each with the next
//! scripted turn, written as one [`AgentEvent`] per line on standard output.
//! Scripted calls to the harness MCP server go to `harness-mcp-url`.

use std::io::{self, BufRead, Write};
use std::process::ExitCode;

use leiser_service::AgentEvent;
use leiser_testkit::{FakeInput, HarnessMcpClient, McpClientError, Script, Step};

#[derive(Debug, thiserror::Error)]
enum FakeError {
    #[error("usage: fake-agent-cli <script.json> <harness-mcp-url>")]
    Usage,
    #[error("harness MCP server: {0}")]
    HarnessMcp(#[from] McpClientError),
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("invalid JSON: {0}")]
    Json(#[from] serde_json::Error),
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            let _ = writeln!(io::stderr(), "fake-agent-cli: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), FakeError> {
    let mut args = std::env::args().skip(1);
    let (Some(script_file), Some(harness_mcp_url)) = (args.next(), args.next()) else {
        return Err(FakeError::Usage);
    };
    let script: Script = serde_json::from_slice(&std::fs::read(script_file)?)?;
    let mut turns = script.turns.into_iter();
    let mut out = io::stdout().lock();
    for line in io::stdin().lock().lines() {
        let FakeInput::UserMessage { text } = serde_json::from_str(&line?)?;
        let mut end_session = false;
        for step in turns.next().unwrap_or_default() {
            let event = match step {
                Step::Emit { event } => event,
                Step::EchoUserMessage => AgentEvent::Message { text: text.clone() },
                Step::ReportWorkingDirectory => AgentEvent::Message {
                    text: std::env::current_dir()?.display().to_string(),
                },
                Step::ListHarnessTools => AgentEvent::Message {
                    text: HarnessMcpClient::connect(&harness_mcp_url)?
                        .list_tools()?
                        .join(","),
                },
                Step::CallHarnessTool { name, arguments } => {
                    let output = HarnessMcpClient::connect(&harness_mcp_url)?
                        .call_tool(&name, &arguments)?;
                    let prefix = if output.is_error { "error: " } else { "" };
                    AgentEvent::Message {
                        text: format!("{prefix}{}", output.text),
                    }
                }
                Step::EndSession => {
                    end_session = true;
                    continue;
                }
            };
            emit(&mut out, &event)?;
        }
        emit(&mut out, &AgentEvent::TurnEnded)?;
        if end_session {
            break;
        }
    }
    Ok(())
}

fn emit(out: &mut impl Write, event: &AgentEvent) -> Result<(), FakeError> {
    serde_json::to_writer(&mut *out, event)?;
    out.write_all(b"\n")?;
    out.flush()?;
    Ok(())
}
