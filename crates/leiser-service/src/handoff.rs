//! Handoffs: real entries of a chat's transcript, selected within a token
//! budget, for an agent session that has not seen them. Never a summary.
//!
//! Also renders what the read-chat tool returns, under the same budget.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::ops::RangeInclusive;

use crate::HARNESS_MCP_SERVER;
use crate::transcript::Transcript;

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
const TRUNCATED: &str = "\n[Truncated. Read the whole entry with read_chat.]\n";

/// What a handoff is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Handoff {
    /// A new agent session that has seen nothing of the chat.
    NewSession,
    /// An earlier agent session that has seen the first `known` entries.
    Resume { known: usize },
}

/// Renders the handoff, or `None` when there is nothing to hand over.
pub(crate) fn render(transcript: &Transcript, handoff: Handoff) -> Option<String> {
    let (first, header) = match handoff {
        Handoff::NewSession => (1, NEW_SESSION_HEADER),
        Handoff::Resume { known } => (known + 1, RESUME_HEADER),
    };
    let entries = first..=transcript.len();
    if entries.is_empty() {
        return None;
    }
    let header = header.replace("{server}", HARNESS_MCP_SERVER);
    let budget = BUDGET_BYTES - header.len() - FOOTER.len();
    let pin_first = handoff == Handoff::NewSession;
    let kept = select(transcript, entries.clone(), pin_first, budget);
    Some(format!("{header}{}{FOOTER}", assemble(&kept, entries)))
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

/// Picks the entries to quote, each with its rendering, within `budget`
/// bytes including the notes about omitted entries: the first entry if
/// `pin_first`, then the most recent entries, then older command outcomes
/// with the end of their output.
fn select(
    transcript: &Transcript,
    entries: RangeInclusive<usize>,
    pin_first: bool,
    budget: usize,
) -> BTreeMap<usize, String> {
    let (lo, hi) = (*entries.start(), *entries.end());
    let mut kept = BTreeMap::new();
    // One note may be needed between the first entry and the recent ones.
    let mut remaining = budget.saturating_sub(NOTE_BYTES);
    let mut recent_from = lo;
    if pin_first && let Some(entry) = transcript.get(lo) {
        let quoted = truncate(quote(entry.render(lo)), FIRST_REQUEST_BYTES.min(remaining));
        remaining -= quoted.len();
        kept.insert(lo, quoted);
        recent_from = lo + 1;
    }

    let mut oldest_recent = hi + 1;
    for number in (recent_from..=hi).rev() {
        let Some(entry) = transcript.get(number) else {
            continue;
        };
        let quoted = quote(entry.render(number));
        if quoted.len() <= remaining {
            remaining -= quoted.len();
        } else if number == hi && remaining >= MIN_TRUNCATED_BYTES {
            // The newest entry matters most: quote what fits of it.
            let quoted = truncate(quoted, remaining);
            remaining -= quoted.len();
            kept.insert(number, quoted);
            oldest_recent = number;
            break;
        } else {
            break;
        }
        kept.insert(number, quoted);
        oldest_recent = number;
    }

    // Every older outcome kept can split a gap in two: one more note.
    for number in (recent_from..oldest_recent).rev() {
        let Some(quoted) = transcript
            .get(number)
            .and_then(|entry| entry.render_outcome(number))
            .map(quote)
        else {
            continue;
        };
        if quoted.len() + NOTE_BYTES <= remaining {
            remaining -= quoted.len() + NOTE_BYTES;
            kept.insert(number, quoted);
        }
    }
    kept
}

/// Joins the kept entries in order, with a note for every gap.
fn assemble(kept: &BTreeMap<usize, String>, entries: RangeInclusive<usize>) -> String {
    let mut out = String::new();
    let mut next = *entries.start();
    for (&number, quoted) in kept {
        if number > next {
            out.push_str(&omitted(next, number - 1));
        }
        out.push_str(quoted);
        next = number + 1;
    }
    if next <= *entries.end() {
        out.push_str(&omitted(next, *entries.end()));
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

/// An entry's rendering followed by a blank line.
fn quote(mut rendered: String) -> String {
    rendered.push('\n');
    rendered
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
}

/// Renders entries `from..=to` (`to` defaults to the newest entry) for the
/// read-chat tool. Stops at the budget and says where to continue.
pub(crate) fn read(
    transcript: &Transcript,
    from: usize,
    to: Option<usize>,
) -> Result<String, ReadError> {
    let len = transcript.len();
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
    for number in from..=to {
        let Some(entry) = transcript.get(number) else {
            break;
        };
        let quoted = quote(entry.render(number));
        if quoted.len() > remaining {
            let rest = if number == from {
                // Always answer with something: the start of the entry.
                out.push_str(&truncate(quoted, remaining));
                number + 1
            } else {
                number
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
