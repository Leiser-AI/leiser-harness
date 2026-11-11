//! `repo-check`: the harness runs the developer's `repo-check` skill in a
//! read-only agent session and reads the report it writes.
//!
//! The skill lives with the agent CLI (`~/.claude/skills/repo-check/`),
//! not in the harness: its rules have one source. The harness only sends
//! `/repo-check` and parses the report at this boundary. The report is the
//! Markdown the skill's step 6 prescribes:
//!
//! ```markdown
//! # repo-check: <repo> @ <sha>
//!
//! <one paragraph: profile and verdict>
//!
//! ## Must
//! 1. **<title>** (<rule IDs>)
//!    Beleg: ...
//!    Warum: ...
//!    Fix: ...
//! ## Should
//! <same shape, numbering continues>
//! ## Later
//! ...
//! ```
//!
//! Only the Must and Should findings are kept. The agent session is ended
//! after its one turn, so the skill's closing question, which numbers to
//! implement, is never answered: nothing is fixed.

use std::collections::BTreeMap;
use std::time::Duration;

use crate::{AgentEvent, AgentSession, CheckId, Finding, RepoCheckReport, SessionInput};

/// The message that starts the skill.
pub(crate) const PROMPT: &str = "/repo-check";

/// How long `repo-check` may take before the harness gives up. The skill
/// reads the whole repository and the global agent configuration.
const TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// How a `repo-check` run ended.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum Outcome {
    /// The report, with every finding, before opt-outs.
    Done { report: RepoCheckReport },
    /// The answer was not a report the harness can read.
    Unreadable { reason: String, output: String },
    /// It could not run or did not finish.
    Failed { reason: String },
}

/// Runs `repo-check` in `session` and reads its report. Ends the agent
/// session when the turn has ended.
pub(crate) async fn run(session: AgentSession) -> Outcome {
    run_within(session, TIMEOUT).await
}

async fn run_within(session: AgentSession, timeout: Duration) -> Outcome {
    let AgentSession {
        input, mut events, ..
    } = session;
    let start = SessionInput::UserMessage {
        text: PROMPT.to_owned(),
    };
    if input.send(start).await.is_err() {
        return failed("the agent session ended before repo-check started");
    }
    let mut messages = Vec::new();
    let mut usage_limit = None;
    let turn = tokio::time::timeout(timeout, async {
        while let Some(event) = events.recv().await {
            match event {
                AgentEvent::Message { text } => messages.push(text),
                AgentEvent::UsageLimitReached { limit, .. } => usage_limit = Some(limit),
                AgentEvent::TurnEnded => return Ok(()),
                AgentEvent::TurnInterrupted => return Err("repo-check was interrupted"),
                AgentEvent::SessionEnded => {
                    return Err("the agent session ended before repo-check finished");
                }
                _ => {}
            }
        }
        Err("the agent session ended before repo-check finished")
    })
    .await;
    // Ends the agent session; the skill's closing question stays unanswered.
    drop(input);
    match turn {
        Err(_) => failed(&format!(
            "repo-check did not finish within {} minutes",
            timeout.as_secs() / 60
        )),
        Ok(Err(reason)) => failed(reason),
        Ok(Ok(())) => match parse_report(&messages) {
            Ok(report) => Outcome::Done { report },
            Err(error) => match usage_limit {
                Some(limit) => failed(&format!("the usage limit was reached: {limit}")),
                None => Outcome::Unreadable {
                    reason: error.to_string(),
                    output: messages.join("\n\n"),
                },
            },
        },
    }
}

fn failed(reason: &str) -> Outcome {
    Outcome::Failed {
        reason: reason.to_owned(),
    }
}

/// Why the agent's answer is not a report.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum ReportError {
    #[error("the answer has no `# repo-check` heading")]
    NoHeading,
    #[error("the report has no sections (`## Must`, `## Should`, ...)")]
    NoSections,
    #[error("finding {line:?} in the report has no title")]
    UntitledFinding { line: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Section {
    Summary,
    Must,
    Should,
    /// `Later`, `Nicht geprüft`, `In Ordnung` and anything new.
    Other,
}

/// Reads the report from the agent's messages of the turn: the last
/// message with a `# repo-check` heading and everything after it.
pub(crate) fn parse_report(messages: &[String]) -> Result<RepoCheckReport, ReportError> {
    let start = messages
        .iter()
        .rposition(|message| message.lines().any(is_heading))
        .ok_or(ReportError::NoHeading)?;
    let text = messages[start..].join("\n");
    let mut lines = text.lines().skip_while(|line| !is_heading(line));
    let heading = lines.next().ok_or(ReportError::NoHeading)?;
    let mut report = RepoCheckReport {
        commit: commit(heading),
        summary: String::new(),
        must: Vec::new(),
        should: Vec::new(),
    };
    let mut summary = Vec::new();
    let mut section = Section::Summary;
    let mut sections = 0;
    let mut finding: Option<(Finding, Vec<&str>)> = None;
    for line in lines {
        if let Some(title) = line.strip_prefix("## ") {
            finish(&mut report, section, finding.take());
            section = section_named(title);
            sections += 1;
            continue;
        }
        match section {
            Section::Summary => summary.push(line.trim()),
            Section::Must | Section::Should => {
                if let Some(item) = finding_start(line) {
                    finish(&mut report, section, finding.take());
                    let (title, checks) =
                        title_and_checks(item).ok_or_else(|| ReportError::UntitledFinding {
                            line: line.trim().to_owned(),
                        })?;
                    let finding_start = Finding {
                        title,
                        checks,
                        details: String::new(),
                    };
                    finding = Some((finding_start, Vec::new()));
                } else if let Some((_, details)) = &mut finding {
                    details.push(line.trim());
                }
            }
            Section::Other => {}
        }
    }
    finish(&mut report, section, finding.take());
    if sections == 0 {
        return Err(ReportError::NoSections);
    }
    report.summary = paragraphs(&summary);
    Ok(report)
}

fn is_heading(line: &str) -> bool {
    line.trim_end().starts_with("# repo-check")
}

/// The commit in `# repo-check: <repo> @ <sha>`.
fn commit(heading: &str) -> Option<String> {
    let (_, commit) = heading.rsplit_once(" @ ")?;
    let commit = commit.trim().trim_matches('`');
    (!commit.is_empty()).then(|| commit.to_owned())
}

fn section_named(title: &str) -> Section {
    match title.split_whitespace().next().map(str::to_lowercase) {
        Some(word) if word == "must" => Section::Must,
        Some(word) if word == "should" => Section::Should,
        _ => Section::Other,
    }
}

/// The rest of a line that starts a finding: `1. ...`, `- ...` or `* ...`
/// at the start of the line. Indented lines belong to the finding above.
fn finding_start(line: &str) -> Option<&str> {
    let indent = line.len() - line.trim_start().len();
    if indent > 1 {
        return None;
    }
    let line = line.trim_start();
    if let Some(rest) = line.strip_prefix("- ").or_else(|| line.strip_prefix("* ")) {
        return Some(rest);
    }
    let digits = line.len() - line.trim_start_matches(|c: char| c.is_ascii_digit()).len();
    if digits == 0 {
        return None;
    }
    let rest = &line[digits..];
    rest.strip_prefix(". ")
        .or_else(|| rest.strip_prefix(") "))
        .map(str::trim_start)
}

/// `**<title>** (<rule IDs>)` or, without bold, `<title> (<rule IDs>)`.
fn title_and_checks(item: &str) -> Option<(String, Vec<CheckId>)> {
    let item = item.trim();
    let (title, rest) = match item.strip_prefix("**") {
        Some(bold) => {
            let (title, rest) = bold.split_once("**")?;
            (title.trim(), rest.trim())
        }
        None => match item.rsplit_once(" (") {
            Some((title, ids)) if ids.ends_with(')') && !checks(ids).is_empty() => {
                (title.trim(), ids)
            }
            _ => (item, ""),
        },
    };
    if title.is_empty() {
        return None;
    }
    let checks = rest
        .strip_prefix('(')
        .and_then(|rest| rest.split_once(')'))
        .map_or_else(Vec::new, |(ids, _)| checks(ids));
    Some((title.to_owned(), checks))
}

/// The rule ids in `A1, A2, H1`, with ranges such as `I1–I4` spelt out.
/// Anything that is not a rule id, such as `global`, is skipped.
fn checks(ids: &str) -> Vec<CheckId> {
    let mut checks = Vec::new();
    for part in
        ids.split(|c: char| matches!(c, ',' | ';' | '/' | '+' | '(' | ')') || c.is_whitespace())
    {
        match part.split_once(['–', '-']) {
            Some((from, to)) => checks.extend(range(from, to)),
            None => checks.extend(CheckId::parse(part)),
        }
    }
    let mut seen = std::collections::BTreeSet::new();
    checks.retain(|check| seen.insert(check.clone()));
    checks
}

/// `I1`..=`I4`; empty unless both ends are plain ids of one letter.
fn range(from: &str, to: &str) -> Vec<CheckId> {
    let split = |id: &str| -> Option<(char, u32)> {
        let letter = id.chars().next()?;
        let number = id.get(1..)?.parse().ok()?;
        CheckId::parse(id).map(|_| (letter, number))
    };
    match (split(from), split(to)) {
        (Some((letter, first)), Some((to_letter, last)))
            if letter == to_letter && first <= last =>
        {
            (first..=last)
                .filter_map(|number| CheckId::parse(&format!("{letter}{number}")))
                .collect()
        }
        _ => Vec::new(),
    }
}

/// Adds the finding to its section, with its detail lines joined.
fn finish(report: &mut RepoCheckReport, section: Section, finding: Option<(Finding, Vec<&str>)>) {
    let Some((mut finding, details)) = finding else {
        return;
    };
    finding.details = paragraphs(&details);
    match section {
        Section::Must => report.must.push(finding),
        Section::Should => report.should.push(finding),
        Section::Summary | Section::Other => {}
    }
}

/// Trimmed lines joined with line breaks; runs of blank lines become one
/// empty line, and leading and trailing ones are dropped.
fn paragraphs(lines: &[&str]) -> String {
    let mut text = String::new();
    let mut blank = false;
    for line in lines {
        if line.is_empty() {
            blank = !text.is_empty();
            continue;
        }
        if !text.is_empty() {
            text.push_str(if blank { "\n\n" } else { "\n" });
        }
        text.push_str(line);
        blank = false;
    }
    text
}

/// The report without the switched-off checks. A finding loses the
/// switched-off ones among its rules; a finding left with none is dropped.
/// A finding that names no rule cannot be switched off and stays.
pub(crate) fn without(
    report: &RepoCheckReport,
    off: &BTreeMap<CheckId, String>,
) -> RepoCheckReport {
    let keep = |findings: &[Finding]| -> Vec<Finding> {
        findings
            .iter()
            .filter_map(|finding| {
                if finding.checks.is_empty() {
                    return Some(finding.clone());
                }
                let checks: Vec<CheckId> = finding
                    .checks
                    .iter()
                    .filter(|check| !off.contains_key(*check))
                    .cloned()
                    .collect();
                (!checks.is_empty()).then(|| Finding {
                    checks,
                    ..finding.clone()
                })
            })
            .collect()
    };
    RepoCheckReport {
        must: keep(&report.must),
        should: keep(&report.should),
        ..report.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn ids(text: &str) -> Vec<String> {
        checks(text).iter().map(ToString::to_string).collect()
    }

    #[test]
    fn rule_ids_are_read_from_lists_and_ranges() {
        assert_eq!(ids("A1, A2, A4, H1"), ["A1", "A2", "A4", "H1"]);
        assert_eq!(ids("I1–I4"), ["I1", "I2", "I3", "I4"]);
        assert_eq!(ids("B3a; C1/C2 + D10"), ["B3a", "C1", "C2", "D10"]);
        assert_eq!(ids("global, I2"), ["I2"]);
        assert_eq!(ids("crash 027"), Vec::<String>::new());
    }

    #[test]
    fn the_report_is_the_last_message_with_a_heading_and_what_follows() -> TestResult {
        let messages = [
            "Ich lese zuerst die Dateien.".to_owned(),
            "# repo-check: demo @ `abc1234`\n\nKlein.\n\n## Must\n1. **Eins** (A1)\n   Fix: weg"
                .to_owned(),
            "## Should\n2. **Zwei** (B1)\n   Fix: auch".to_owned(),
        ];

        let report = parse_report(&messages)?;

        assert_eq!(report.commit.as_deref(), Some("abc1234"));
        assert_eq!(report.summary, "Klein.");
        assert_eq!(report.must.len(), 1);
        assert_eq!(report.should.len(), 1);
        assert_eq!(report.should[0].title, "Zwei");
        Ok(())
    }

    #[test]
    fn an_answer_without_a_heading_is_no_report() {
        let messages = ["Ich konnte das Repository nicht lesen.".to_owned()];

        assert_eq!(parse_report(&messages), Err(ReportError::NoHeading));
    }

    #[test]
    fn a_heading_without_sections_is_no_report() {
        let messages = ["# repo-check: demo @ abc1234\n\nNur ein Satz.".to_owned()];

        assert_eq!(parse_report(&messages), Err(ReportError::NoSections));
    }

    #[test]
    fn a_clean_repository_has_no_must_and_no_should() -> TestResult {
        let messages = [
            "# repo-check: demo @ abc1234\n\nAlles gut.\n\n## In Ordnung\n- Alles (A1)".to_owned(),
        ];

        let report = parse_report(&messages)?;

        assert_eq!(report.must, []);
        assert_eq!(report.should, []);
        Ok(())
    }
}
