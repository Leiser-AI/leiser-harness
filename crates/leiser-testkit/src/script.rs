//! What the fake agent CLI plays back, and the lines it reads.

use std::path::PathBuf;

use leiser_service::AgentEvent;
use serde::{Deserialize, Serialize};

/// The turns the fake agent CLI plays back, one per message it receives.
/// Every turn ends with [`AgentEvent::TurnEnded`], or with
/// [`AgentEvent::TurnInterrupted`] when it is interrupted; a message after
/// the last turn gets an empty turn. A resumed agent session continues
/// with the turn after the last one it played.
///
/// The fake agent CLI exits with an error on input a real one would not
/// get from a correct harness service: a message while a turn runs, or a
/// steer while no turn runs or when steering is not supported.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Script {
    pub turns: Vec<Vec<Step>>,
    /// Whether the fake agent CLI takes messages mid-turn.
    #[serde(default)]
    pub supports_steer: bool,
}

impl Script {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Lets the fake agent CLI take messages mid-turn.
    #[must_use]
    pub fn supports_steer(mut self) -> Self {
        self.supports_steer = true;
        self
    }

    /// Adds the next turn.
    #[must_use]
    pub fn turn(mut self, steps: impl IntoIterator<Item = Step>) -> Self {
        self.turns.push(steps.into_iter().collect());
        self
    }
}

/// One step of a scripted turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Step {
    /// Emits this event.
    Emit { event: AgentEvent },
    /// Answers with the text of the message it received.
    EchoUserMessage,
    /// Answers with the directory the fake agent CLI runs in.
    ReportWorkingDirectory,
    /// Waits for a steered message, unless one already came this turn;
    /// [`Step::EchoSteer`] can answer it.
    WaitForSteer,
    /// Answers with the text of the last steered message of this turn.
    EchoSteer,
    /// Waits until the file at `gate` exists, as a long turn would; see
    /// [`Gate`](crate::Gate).
    WaitForGate { gate: PathBuf },
    /// Answers with the names of the harness MCP server's tools, joined
    /// by commas.
    ListHarnessTools,
    /// Calls a tool of the harness MCP server and answers with the text it
    /// returned, prefixed with `error: ` when the tool reported an error.
    CallHarnessTool {
        name: String,
        arguments: serde_json::Value,
    },
    /// Ends the agent session once this turn has ended and forgets it, as
    /// an agent CLI whose session expired would: resuming it fails.
    ForgetSession,
    /// Writes `contents` to `path`, relative to the working directory, and
    /// answers `wrote <path>`. A read-only agent session declines, as a
    /// real agent CLI would, and answers `declined: read-only`.
    WriteFile { path: PathBuf, contents: String },
}

impl Step {
    /// A step that answers with this message.
    #[must_use]
    pub fn message(text: &str) -> Self {
        Self::Emit {
            event: AgentEvent::Message {
                text: text.to_owned(),
            },
        }
    }
}

/// A line the fake agent CLI reads on standard input. It writes
/// [`AgentEvent`]s, one JSON object per line, on standard output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FakeInput {
    UserMessage { text: String },
    Steer { text: String },
    Interrupt,
}
