//! Shared helpers for CLI-backed git hosts: parsing HTTP responses printed
//! with `--include`, trimming failed CI logs, and reading wayfinder labels.

use serde::Deserialize;
use serde::de::IntoDeserializer;
use serde::de::value::{Error as ValueError, StrDeserializer};

use super::{GitHostError, refused};
use crate::TicketType;

/// How much of a failed check's log the agent gets: its last lines, at
/// most this many and this many bytes.
pub(super) const LOG_LINES: usize = 80;
pub(super) const LOG_BYTES: usize = 8_000;

/// The label of a wayfinder map issue.
pub(super) const MAP_LABEL: &str = "wayfinder:map";
/// The start of a ticket's type label, such as `wayfinder:research`.
const TYPE_LABEL: &str = "wayfinder:";

/// An answer to a conditional (`If-None-Match`) API request.
pub(super) enum ConditionalAnswer {
    NotModified,
    Fresh { tag: Option<String>, body: String },
}

/// Parses the output of a CLI `--include` API request (`gh api --include`
/// or `glab api --include`). Both CLIs exit non-zero on `304 Not Modified`,
/// so the HTTP status line is checked before the exit code.
pub(super) fn parse_conditional_output(
    output: &std::process::Output,
) -> Result<ConditionalAnswer, GitHostError> {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let (head, body) = split_response(&stdout);
    match status(head) {
        Some(304) => Ok(ConditionalAnswer::NotModified),
        Some(200..=299) if output.status.success() => Ok(ConditionalAnswer::Fresh {
            tag: header(head, "etag").map(str::to_owned),
            body: body.to_owned(),
        }),
        _ => Err(refused(output, head.lines().next().unwrap_or_default())),
    }
}

/// The status line and headers, and the body, of an `--include` response.
pub(super) fn split_response(output: &str) -> (&str, &str) {
    for separator in ["\r\n\r\n", "\n\n"] {
        if let Some(split) = output.split_once(separator) {
            return split;
        }
    }
    (output, "")
}

/// The status code in the status line, such as `HTTP/2.0 304 Not Modified`.
pub(super) fn status(head: &str) -> Option<u16> {
    head.lines().next()?.split_whitespace().nth(1)?.parse().ok()
}

/// A header's value; names are compared without case.
pub(super) fn header<'h>(head: &'h str, name: &str) -> Option<&'h str> {
    head.lines().skip(1).find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.trim()
            .eq_ignore_ascii_case(name)
            .then(|| value.trim())
            .filter(|value| !value.is_empty())
    })
}

/// The last lines of a CI job log, without leading ISO-8601 UTC timestamps.
pub(super) fn log_tail(log: &str) -> String {
    let lines: Vec<&str> = log.lines().map(without_timestamp).collect();
    let mut tail = lines[lines.len().saturating_sub(LOG_LINES)..].join("\n");
    if tail.len() > LOG_BYTES {
        let mut start = tail.len() - LOG_BYTES;
        while !tail.is_char_boundary(start) {
            start += 1;
        }
        tail = tail.split_off(start);
    }
    tail
}

/// A log line without a leading UTC timestamp such as
/// `2026-11-11T10:00:00.1234567Z`.
fn without_timestamp(line: &str) -> &str {
    match line.split_once(' ') {
        Some((stamp, rest))
            if stamp.len() >= 20
                && stamp.ends_with('Z')
                && stamp.as_bytes().get(4) == Some(&b'-')
                && stamp.as_bytes().get(10) == Some(&b'T') =>
        {
            rest
        }
        _ => line,
    }
}

/// The ticket type a label such as `wayfinder:research` names, by the
/// type's own name in the protocol.
pub(super) fn ticket_type_of_label(label: &str) -> Option<TicketType> {
    let name: StrDeserializer<'_, ValueError> = label.strip_prefix(TYPE_LABEL)?.into_deserializer();
    TicketType::deserialize(name).ok()
}

#[cfg(test)]
mod tests {
    use super::{LOG_LINES, header, log_tail, split_response, status, ticket_type_of_label};
    use crate::TicketType;

    #[test]
    fn reads_status_tag_and_body_of_an_included_answer() {
        let output =
            "HTTP/2.0 200 OK\r\nContent-Type: application/json\r\nEtag: W/\"abc\"\r\n\r\n{\"a\":1}";
        let (head, body) = split_response(output);
        assert_eq!(status(head), Some(200));
        assert_eq!(header(head, "etag"), Some("W/\"abc\""));
        assert_eq!(body, "{\"a\":1}");
        let (head, body) = split_response("HTTP/2.0 304 Not Modified\r\nEtag: \"abc\"\r\n\r\n");
        assert_eq!(status(head), Some(304));
        assert_eq!(body, "");
    }

    #[test]
    fn keeps_the_end_of_a_log_without_timestamps() {
        let log = (0..200).fold(String::new(), |log, line| {
            log + &format!("2026-11-11T10:00:00.1234567Z line {line}\n")
        });
        let tail = log_tail(&log);
        assert_eq!(tail.lines().count(), LOG_LINES);
        assert_eq!(tail.lines().last(), Some("line 199"));
        assert_eq!(log_tail("plain line"), "plain line");
    }

    #[test]
    fn reads_wayfinder_ticket_type_labels() {
        assert_eq!(
            ticket_type_of_label("wayfinder:research"),
            Some(TicketType::Research)
        );
        assert_eq!(
            ticket_type_of_label("wayfinder:task"),
            Some(TicketType::Task)
        );
        assert_eq!(ticket_type_of_label("wayfinder:map"), None);
        assert_eq!(ticket_type_of_label("bug"), None);
    }
}
