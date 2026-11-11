//! Handoffs: real entries of a chat's transcript, selected within a token
//! budget, for an agent session that has not seen them. Never a summary.
//!
//! Also renders what the read-chat tool returns, under the same budget.
//!
//! Handoffs and the read-chat tool number the transcript's messages, tool
//! calls and results from 1 in chat order; entries such as the end of a
//! turn are not quoted and get no number.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use crate::{AgentCli, AgentEvent, HARNESS_MCP_SERVER, TranscriptEntry};

/// The token budget of a handoff, and of one read-chat answer.
pub(crate) const BUDGET_TOKENS: usize = 16_000;
/// Tokens are estimated as four bytes each. Bytes are never fewer than
/// characters, so the estimate errs towards shorter handoffs.
const BYTES_PER_TOKEN: usize = 4;
const BUDGET_BYTES: usize = BUDGET_TOKENS * BYTES_PER_TOKEN;
/// The first request takes at most this share of the budget.
const FIRST_REQUEST_BYTES: usize = BUDGET_BYTES / 4;
/// Room kept for one note about omitted entries.
const NOTE_BYTES: usize = 160;
/// An entry is truncated to fit only if at least this much of it fits.
const MIN_TRUNCATED_BYTES: usize = 400;
/// How much of an older outcome's output a handoff keeps.
const OUTCOME_TAIL_BYTES: usize = 400;
const TRUNCATED: &str = "\n[Truncated. Read the whole entry with read_chat.]\n";

/// What a handoff is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Handoff {
    /// A new agent session that has seen nothing of the chat.
    NewSession,
    /// An earlier agent session that has seen the first `known` transcript
    /// entries.
    Resume { known: usize },
}

/// A transcript entry that handoffs quote.
struct Quoted<'a> {
    /// Its number among the quoted entries, from 1.
    number: usize,
    /// Its position in the transcript, from 0.
    index: usize,
    entry: Entry<'a>,
}

enum Entry<'a> {
    Developer(&'a str),
    Agent(AgentCli, &'a str),
    ToolCall(AgentCli, &'a str, &'a str),
    ToolResult(AgentCli, &'a str, &'a str),
    Command(AgentCli, &'a str, Option<i32>, &'a str),
}

fn quoted(transcript: &[TranscriptEntry]) -> Vec<Quoted<'_>> {
    transcript
        .iter()
        .enumerate()
        .filter_map(|(index, entry)| {
            let entry = match entry {
                TranscriptEntry::UserMessage { text } => Entry::Developer(text),
                TranscriptEntry::Agent { agent_cli, event } => match event {
                    AgentEvent::Message { text } => Entry::Agent(*agent_cli, text),
                    AgentEvent::ToolCall { name, detail } => {
                        Entry::ToolCall(*agent_cli, name, detail)
                    }
                    AgentEvent::ToolResult { name, output } => {
                        Entry::ToolResult(*agent_cli, name, output)
                    }
                    AgentEvent::CommandFinished {
                        command,
                        exit_code,
                        output,
                    } => Entry::Command(*agent_cli, command, *exit_code, output),
                    AgentEvent::ApprovalRequested { .. }
                    | AgentEvent::UsageLimitReached { .. }
                    | AgentEvent::TurnEnded
                    | AgentEvent::TurnInterrupted
                    | AgentEvent::SessionEnded => return None,
                },
            };
            Some((index, entry))
        })
        .enumerate()
        .map(|(position, (index, entry))| Quoted {
            number: position + 1,
            index,
            entry,
        })
        .collect()
}

impl Quoted<'_> {
    /// The entry in full, followed by a blank line.
    fn render(&self) -> String {
        let number = self.number;
        match self.entry {
            Entry::Developer(text) => format!("## {number}. Developer\n{text}\n\n"),
            Entry::Agent(cli, text) => format!("## {number}. Agent ({cli})\n{text}\n\n"),
            Entry::ToolCall(cli, name, detail) => {
                format!("## {number}. Tool call ({cli}): {name} {detail}\n\n")
            }
            Entry::ToolResult(cli, name, output) => {
                format!("## {number}. Tool result ({cli}): {name}\n{output}\n\n")
            }
            Entry::Command(cli, command, exit_code, output) => format!(
                "{}{output}\n\n",
                command_heading(number, cli, command, exit_code)
            ),
        }
    }

    /// A tool result or command outcome with only the end of its output,
    /// where errors usually are; `None` for other entries.
    fn render_outcome(&self) -> Option<String> {
        let (heading, output) = match self.entry {
            Entry::ToolResult(cli, name, output) => (
                format!("## {}. Tool result ({cli}): {name}\n", self.number),
                output,
            ),
            Entry::Command(cli, command, exit_code, output) => (
                command_heading(self.number, cli, command, exit_code),
                output,
            ),
            Entry::Developer(_) | Entry::Agent(..) | Entry::ToolCall(..) => return None,
        };
        Some(format!("{heading}{}\n\n", tail(output, OUTCOME_TAIL_BYTES)))
    }
}

fn command_heading(number: usize, cli: AgentCli, command: &str, exit_code: Option<i32>) -> String {
    let status = match exit_code {
        Some(code) => format!("exited with {code}"),
        None => "was stopped".to_owned(),
    };
    format!("## {number}. Command ({cli}): `{command}` {status}\n")
}

/// The last `max` bytes of `text`, at a character boundary, marking the cut.
fn tail(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let mut start = text.len() - max;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    format!("[…]{}", &text[start..])
}

/// Renders the handoff, or `None` when there is nothing to hand over.
pub(crate) fn render(transcript: &[TranscriptEntry], handoff: Handoff) -> Option<String> {
    let all = quoted(transcript);
    let (items, header, pin_first) = match handoff {
        Handoff::NewSession => (&all[..], NEW_SESSION_HEADER, true),
        Handoff::Resume { known } => {
            let from = all.partition_point(|item| item.index < known);
            (&all[from..], RESUME_HEADER, false)
        }
    };
    let (first, last) = (items.first()?.number, items.last()?.number);
    let header = header.replace("{server}", HARNESS_MCP_SERVER);
    let budget = BUDGET_BYTES - header.len() - FOOTER.len();
    let kept = select(items, pin_first, budget);
    Some(format!("{header}{}{FOOTER}", assemble(&kept, first, last)))
}

const NEW_SESSION_HEADER: &str = "<handoff>
This chat started before this agent session, in another agent session of the leiser harness. \
Below are entries of the chat transcript, quoted as they were written, not summarized: \
the first request, the most recent entries and the outcomes of commands. \
Entries are numbered from 1. Read omitted entries with the read_chat tool of the {server} MCP server. \
The files are in the working directory.

";

const RESUME_HEADER: &str = "<handoff>
Since your last turn, this chat continued in another agent session of the leiser harness. \
Below are the new entries of the chat transcript, quoted as they were written, not summarized. \
Read omitted entries with the read_chat tool of the {server} MCP server.

";

const FOOTER: &str = "</handoff>";

/// Picks the entries to quote, by number with their rendering, within
/// `budget` bytes including the notes about omitted entries: the first
/// entry if `pin_first`, then the most recent entries, then older tool
/// results and command outcomes with the end of their output.
fn select(items: &[Quoted<'_>], pin_first: bool, budget: usize) -> BTreeMap<usize, String> {
    let mut kept = BTreeMap::new();
    // One note may be needed before the recent entries.
    let mut remaining = budget.saturating_sub(NOTE_BYTES);
    let mut older = items;
    if pin_first && let Some((first, rest)) = items.split_first() {
        let quoted = truncate(first.render(), FIRST_REQUEST_BYTES.min(remaining));
        remaining -= quoted.len();
        kept.insert(first.number, quoted);
        older = rest;
    }

    let mut recent = older.len();
    for (position, item) in older.iter().enumerate().rev() {
        let quoted = item.render();
        if quoted.len() <= remaining {
            remaining -= quoted.len();
            kept.insert(item.number, quoted);
            recent = position;
            continue;
        }
        if position + 1 == older.len() && remaining >= MIN_TRUNCATED_BYTES {
            // The newest entry matters most: quote what fits of it.
            let quoted = truncate(quoted, remaining);
            remaining -= quoted.len();
            kept.insert(item.number, quoted);
            recent = position;
        }
        break;
    }

    // Every older outcome kept can split a gap in two: one more note.
    for item in older[..recent].iter().rev() {
        let Some(quoted) = item.render_outcome() else {
            continue;
        };
        if quoted.len() + NOTE_BYTES <= remaining {
            remaining -= quoted.len() + NOTE_BYTES;
            kept.insert(item.number, quoted);
        }
    }
    kept
}

/// Joins the kept entries in order, with a note for every gap between
/// entry numbers `first` and `last`.
fn assemble(kept: &BTreeMap<usize, String>, first: usize, last: usize) -> String {
    let mut out = String::new();
    let mut next = first;
    for (&number, quoted) in kept {
        if number > next {
            out.push_str(&omitted(next, number - 1));
        }
        out.push_str(quoted);
        next = number + 1;
    }
    if next <= last {
        out.push_str(&omitted(next, last));
    }
    out
}

fn omitted(from: usize, to: usize) -> String {
    if from == to {
        format!("[Entry {from} omitted: read_chat with from {from}.]\n\n")
    } else {
        format!("[Entries {from} to {to} omitted: read_chat with from {from} and to {to}.]\n\n")
    }
}

/// Cuts `text` to at most `max` bytes, at a character boundary, marking
/// the cut.
fn truncate(mut text: String, max: usize) -> String {
    if text.len() <= max {
        return text;
    }
    let mut cut = max.saturating_sub(TRUNCATED.len());
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    text.truncate(cut);
    text.push_str(TRUNCATED);
    text
}

/// Why the read-chat tool could not read the asked entries.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum ReadError {
    #[error("the chat has {len} entries, numbered from 1; there is no entry {from}")]
    NoSuchEntry { from: usize, len: usize },
    #[error("`to` ({to}) must not be smaller than `from` ({from})")]
    EmptyRange { from: usize, to: usize },
    #[error("the chat could not be read: {0}")]
    Unavailable(String),
}

/// Renders entries `from..=to` (`to` defaults to the newest entry) for the
/// read-chat tool. Stops at the budget and says where to continue.
pub(crate) fn read(
    transcript: &[TranscriptEntry],
    from: usize,
    to: Option<usize>,
) -> Result<String, ReadError> {
    let items = quoted(transcript);
    let len = items.len();
    if from == 0 || from > len {
        return Err(ReadError::NoSuchEntry { from, len });
    }
    let to = to.unwrap_or(len);
    if to < from {
        return Err(ReadError::EmptyRange { from, to });
    }
    let to = to.min(len);
    let mut out = String::new();
    let mut remaining = BUDGET_BYTES - NOTE_BYTES;
    for item in &items[from - 1..to] {
        let quoted = item.render();
        if quoted.len() > remaining {
            let rest = if item.number == from {
                // Always answer with something: the start of the entry.
                out.push_str(&truncate(quoted, remaining));
                item.number + 1
            } else {
                item.number
            };
            if rest <= to {
                let _ = writeln!(
                    out,
                    "[Entries {rest} to {to} not returned: over the size limit. \
                     Call read_chat again with from {rest}.]"
                );
            }
            break;
        }
        remaining -= quoted.len();
        out.push_str(&quoted);
    }
    Ok(out)
}
