//! A scripted stand-in for an agent CLI, used by the harness service's tests.
//!
//! Usage: `fake-agent-cli <script.json>`. Reads one [`FakeInput`] per line on
//! standard input and answers each with the next scripted turn, written as
//! one [`AgentEvent`] per line on standard output.
//!
//! `fake-agent-cli --version` prints [`FAKE_VERSION`], older than any agent
//! CLI the harness supports, for tests of the minimum version check.

use std::io::{self, BufRead, Write};
use std::process::ExitCode;

use leiser_service::AgentEvent;
use leiser_testkit::{FakeInput, Script, Step};

#[derive(Debug, thiserror::Error)]
enum FakeError {
    #[error("usage: fake-agent-cli <script.json>")]
    Usage,
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

/// The answer to `--version`, in the form Claude Code uses.
const FAKE_VERSION: &str = "0.1.0 (fake-agent-cli)";

fn run() -> Result<(), FakeError> {
    let script_file = std::env::args_os().nth(1).ok_or(FakeError::Usage)?;
    if script_file == "--version" {
        writeln!(io::stdout(), "{FAKE_VERSION}")?;
        return Ok(());
    }
    let script: Script = serde_json::from_slice(&std::fs::read(script_file)?)?;
    let mut turns = script.turns.into_iter();
    let mut out = io::stdout().lock();
    for line in io::stdin().lock().lines() {
        let FakeInput::UserMessage { text } = serde_json::from_str(&line?)?;
        for step in turns.next().unwrap_or_default() {
            let event = match step {
                Step::Emit { event } => event,
                Step::EchoUserMessage => AgentEvent::Message { text: text.clone() },
                Step::ReportWorkingDirectory => AgentEvent::Message {
                    text: std::env::current_dir()?.display().to_string(),
                },
            };
            emit(&mut out, &event)?;
        }
        emit(&mut out, &AgentEvent::TurnEnded)?;
    }
    Ok(())
}

fn emit(out: &mut impl Write, event: &AgentEvent) -> Result<(), FakeError> {
    serde_json::to_writer(&mut *out, event)?;
    out.write_all(b"\n")?;
    out.flush()?;
    Ok(())
}
