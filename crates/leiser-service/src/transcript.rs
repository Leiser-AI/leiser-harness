//! The chat transcript: the chat's messages in order, across all of its
//! agent sessions. Handoffs and the read-chat tool quote from it.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use crate::{AgentCli, AgentEvent};

/// One message of a chat. Entries are numbered from 1 in chat order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Entry {
    /// A message from the developer.
    Developer { text: String },
    /// A message from an agent.
    Agent { agent_cli: AgentCli, text: String },
    /// A tool call by an agent.
    ToolCall {
        agent_cli: AgentCli,
        name: String,
        detail: String,
    },
    /// The outcome of a command an agent ran.
    CommandFinished {
        agent_cli: AgentCli,
        command: String,
        exit_code: Option<i32>,
        output: String,
    },
}

impl Entry {
    /// The entry as it is quoted in handoffs and by the read-chat tool.
    pub(crate) fn render(&self, number: usize) -> String {
        match self {
            Self::Developer { text } => format!("## {number}. Developer\n{text}\n"),
            Self::Agent { agent_cli, text } => {
                format!("## {number}. Agent ({agent_cli})\n{text}\n")
            }
            Self::ToolCall {
                agent_cli,
                name,
                detail,
            } => format!("## {number}. Tool call ({agent_cli}): {name} {detail}\n"),
            Self::CommandFinished {
                agent_cli,
                command,
                exit_code,
                output,
            } => format!(
                "{}{output}\n",
                command_heading(number, *agent_cli, command, *exit_code)
            ),
        }
    }

    /// A command outcome with only the end of its output, where errors
    /// usually are; `None` for other entries.
    pub(crate) fn render_outcome(&self, number: usize) -> Option<String> {
        let Self::CommandFinished {
            agent_cli,
            command,
            exit_code,
            output,
        } = self
        else {
            return None;
        };
        let heading = command_heading(number, *agent_cli, command, *exit_code);
        if output.len() <= OUTCOME_TAIL_BYTES {
            return Some(format!("{heading}{output}\n"));
        }
        let mut start = output.len() - OUTCOME_TAIL_BYTES;
        while !output.is_char_boundary(start) {
            start += 1;
        }
        Some(format!("{heading}[…]{}\n", &output[start..]))
    }
}

/// How much of an older command's output a handoff keeps.
const OUTCOME_TAIL_BYTES: usize = 400;

fn command_heading(
    number: usize,
    agent_cli: AgentCli,
    command: &str,
    exit_code: Option<i32>,
) -> String {
    let status = match exit_code {
        Some(code) => format!("exited with {code}"),
        None => "was stopped".to_owned(),
    };
    format!("## {number}. Command ({agent_cli}): `{command}` {status}\n")
}

/// All entries of one chat.
#[derive(Debug, Default)]
pub(crate) struct Transcript {
    entries: Vec<Entry>,
}

impl Transcript {
    pub(crate) fn push(&mut self, entry: Entry) {
        self.entries.push(entry);
    }

    /// Records what an agent event adds to the chat. Events that are not
    /// messages, such as the end of a turn, add nothing.
    pub(crate) fn record(&mut self, agent_cli: AgentCli, event: &AgentEvent) {
        let entry = match event.clone() {
            AgentEvent::Message { text } => Entry::Agent { agent_cli, text },
            AgentEvent::ToolCall { name, detail } => Entry::ToolCall {
                agent_cli,
                name,
                detail,
            },
            AgentEvent::CommandFinished {
                command,
                exit_code,
                output,
            } => Entry::CommandFinished {
                agent_cli,
                command,
                exit_code,
                output,
            },
            AgentEvent::ApprovalRequested { .. }
            | AgentEvent::UsageLimitReached { .. }
            | AgentEvent::TurnEnded
            | AgentEvent::SessionEnded => return,
        };
        self.push(entry);
    }

    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    /// The entry with this number, counted from 1.
    pub(crate) fn get(&self, number: usize) -> Option<&Entry> {
        number
            .checked_sub(1)
            .and_then(|index| self.entries.get(index))
    }
}

/// A transcript shared by the command loop, the tasks that read agent
/// events and the harness MCP server.
pub(crate) type SharedTranscript = Arc<Mutex<Transcript>>;

/// Locks a transcript. A panic while it was locked cannot leave it
/// half-written (every change is one `push`), so poisoning is ignored.
pub(crate) fn lock(transcript: &SharedTranscript) -> MutexGuard<'_, Transcript> {
    transcript.lock().unwrap_or_else(PoisonError::into_inner)
}
