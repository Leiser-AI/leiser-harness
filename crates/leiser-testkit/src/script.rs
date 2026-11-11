//! What the fake agent CLI plays back, and the lines it reads.

use leiser_service::AgentEvent;
use serde::{Deserialize, Serialize};

/// The turns the fake agent CLI plays back, one per message it receives.
/// Every turn ends with [`AgentEvent::TurnEnded`]; a message after the last
/// turn gets an empty turn. A new agent session starts at the first turn.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Script {
    pub turns: Vec<Vec<Step>>,
}

impl Script {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
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
    /// Answers with the names of the harness MCP server's tools, joined
    /// by commas.
    ListHarnessTools,
    /// Calls a tool of the harness MCP server and answers with the text it
    /// returned, prefixed with `error: ` when the tool reported an error.
    CallHarnessTool {
        name: String,
        arguments: serde_json::Value,
    },
    /// Ends the agent session once this turn has ended.
    EndSession,
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
}
