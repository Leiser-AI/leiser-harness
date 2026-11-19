//! The Gemini CLI policy file an agent session runs with: the hard blocks
//! as deny rules, the harness MCP server's tools allowed without asking,
//! and, for a read-only agent session, the editing tools denied.
//!
//! The Gemini CLI loads it with `--policy`, in the user tier, which ranks
//! above its approval modes' own rules, `yolo` included. Rule format:
//! `docs/reference/policy-engine.md` in the Gemini CLI repository. Command
//! patterns are regular expressions matched against the whole shell
//! command; they use no inline flags or lookarounds, so they mean the same
//! in RE2 and JavaScript.

use std::fmt::Write as _;
use std::fs::OpenOptions;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::HARNESS_MCP_SERVER;
use crate::hard_blocks::{AI_NAMES, PROTECTED_BRANCHES};

/// The Gemini CLI's shell tool.
const SHELL_TOOL: &str = "run_shell_command";
/// The Gemini CLI's tools that write files.
const EDIT_TOOLS: [&str; 2] = ["write_file", "replace"];
/// Email addresses AI agents commit as.
const AI_EMAILS: [&str; 2] = ["noreply@anthropic.com", "noreply@openai.com"];

/// The highest priority in a tier: the hard blocks win over everything
/// else in the user tier.
const DENY_PRIORITY: u32 = 999;
const ALLOW_PRIORITY: u32 = 500;

/// Matches `git` and later, any text, `push`; `[\s\S]` spans the lines of
/// a multi-line command.
const GIT_PUSH: &str = r"\bgit\b[\s\S]*\bpush\b";
const GIT_COMMIT: &str = r"\bgit\b[\s\S]*\bcommit\b";
/// What may follow a branch name or option in a command.
const END: &str = r"([\s;&|)]|$)";

/// The rules of an agent session's policy file.
pub(super) fn rules(read_only: bool) -> String {
    let mut text =
        String::from("# Written by leiser-harness for one agent session; removed when it ends.\n");
    let branches = PROTECTED_BRANCHES.join("|");
    deny_command(
        &mut text,
        &format!(r"{GIT_PUSH}[\s\S]*[\s:+]((refs/)?heads/)?({branches}){END}"),
        "leiser-harness never pushes to a protected branch.",
    );
    deny_command(
        &mut text,
        &format!(r"{GIT_PUSH}[\s\S]*(\s(--force\S*|--mirror|-[A-Za-z]*f[A-Za-z]*){END}|\s\+\S)"),
        "leiser-harness never force-pushes.",
    );
    let names = AI_NAMES
        .iter()
        .map(|name| any_case(name))
        .collect::<Vec<_>>()
        .join("|");
    let emails = AI_EMAILS
        .iter()
        .map(|email| any_case(email))
        .collect::<Vec<_>>()
        .join("|");
    let attribution = [
        format!(
            r"{GIT_COMMIT}[\s\S]*{}[^\n]*({names})",
            any_case("co-authored-by:")
        ),
        format!(
            r"{GIT_COMMIT}[\s\S]*{}\s+({}|{})[^\n]*({names})",
            any_case("generated"),
            any_case("with"),
            any_case("by")
        ),
        format!(r"{GIT_COMMIT}[\s\S]*--author[=\s][^\n]*({names})"),
        format!(r"{GIT_COMMIT}[\s\S]*({emails})"),
    ];
    for pattern in attribution {
        deny_command(
            &mut text,
            &pattern,
            "leiser-harness never commits with AI attribution.",
        );
    }
    let _ = write!(
        text,
        "\n[[rule]]\nmcpName = \"{HARNESS_MCP_SERVER}\"\ndecision = \"allow\"\npriority = {ALLOW_PRIORITY}\n"
    );
    if read_only {
        for tool in EDIT_TOOLS {
            let _ = write!(
                text,
                "\n[[rule]]\ntoolName = \"{tool}\"\ndecision = \"deny\"\npriority = {DENY_PRIORITY}\n\
                 denyMessage = \"This agent session is read-only.\"\n"
            );
        }
    }
    text
}

fn deny_command(text: &mut String, pattern: &str, message: &str) {
    // A TOML literal string keeps backslashes; it cannot hold `'`.
    debug_assert!(!pattern.contains('\''));
    let _ = write!(
        text,
        "\n[[rule]]\ntoolName = \"{SHELL_TOOL}\"\ncommandRegex = '{pattern}'\ndecision = \"deny\"\n\
         priority = {DENY_PRIORITY}\ndenyMessage = \"{message}\"\n"
    );
}

/// `text` as a pattern that matches it in any case, with `.` literal.
fn any_case(text: &str) -> String {
    text.chars()
        .map(|c| {
            if c.is_ascii_alphabetic() {
                format!("[{}{}]", c.to_ascii_lowercase(), c.to_ascii_uppercase())
            } else if c == '.' {
                r"\.".to_owned()
            } else {
                c.to_string()
            }
        })
        .collect()
}

/// A policy file on disk, removed when dropped.
#[derive(Debug)]
pub(super) struct PolicyFile {
    path: PathBuf,
}

impl PolicyFile {
    /// Writes the rules to a new file in the temporary directory, which
    /// only the developer can write to on macOS and Windows.
    pub fn write(read_only: bool) -> io::Result<Self> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| since.subsec_nanos());
        let path = std::env::temp_dir().join(format!(
            "leiser-harness-gemini-policy-{}-{}-{nanos}.toml",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        // A new file: never one someone else placed there.
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)?;
        file.write_all(rules(read_only).as_bytes())?;
        Ok(Self { path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for PolicyFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::rules;

    /// The commands a rule's pattern denies, by a small regular expression
    /// matcher for the subset the patterns use.
    fn denied(command: &str) -> bool {
        patterns(false)
            .iter()
            .any(|pattern| regex_lite::matches(pattern, command))
    }

    fn patterns(read_only: bool) -> Vec<String> {
        rules(read_only)
            .lines()
            .filter_map(|line| line.strip_prefix("commandRegex = '"))
            .filter_map(|line| line.strip_suffix('\''))
            .map(str::to_owned)
            .collect()
    }

    #[test]
    fn the_policy_parses_as_toml_with_a_rule_per_block() -> Result<(), toml::de::Error> {
        let parsed: toml::Table = toml::from_str(&rules(true))?;

        let rules = parsed
            .get("rule")
            .and_then(toml::Value::as_array)
            .map_or(0, Vec::len);
        assert_eq!(rules, 2 + 4 + 1 + 2);
        Ok(())
    }

    #[test]
    fn the_hard_blocks_are_denied() {
        for command in [
            "git push origin main",
            "git push origin HEAD:refs/heads/development",
            "git -C repo push --force origin feat/x",
            "git push -f origin feat/x",
            "git push --force-with-lease",
            "git push origin +feat/x",
            "git commit -m 'Fix\n\nCo-authored-by: Claude <noreply@anthropic.com>'",
            "git commit -m \"x\n\nco-authored-by: GEMINI\"",
            "git commit -m 'x\n\nGenerated with Codex'",
            "git commit --author='Gemini <a@b.c>' -m x",
        ] {
            assert!(denied(command), "not denied: {command}");
        }
    }

    #[test]
    fn ordinary_commands_are_not_denied() {
        for command in [
            "git push origin feat/main-menu",
            "git push -u origin feat/x",
            "git push --follow-tags origin feat/x",
            "git commit -m 'Add the main menu'",
            "git status",
        ] {
            assert!(!denied(command), "denied: {command}");
        }
    }

    /// A backtracking matcher for the patterns above: literals, `.`
    /// escapes, `\s`, `\S`, `\b`, classes, groups with `|`, `?`, `*`, `$`.
    mod regex_lite {
        #[derive(Debug, Clone)]
        enum Node {
            Char(char),
            Class(Vec<(char, char)>, bool),
            Space(bool),
            Boundary,
            End,
            Group(Vec<Vec<(Node, Rep)>>),
        }

        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        enum Rep {
            One,
            Optional,
            Many,
            Some,
        }

        pub fn matches(pattern: &str, text: &str) -> bool {
            let chars: Vec<char> = pattern.chars().collect();
            let mut at = 0;
            let alternatives = parse_alternatives(&chars, &mut at);
            let text: Vec<char> = text.chars().collect();
            (0..=text.len()).any(|start| {
                alternatives
                    .iter()
                    .any(|sequence| !run(sequence, &text, start).is_empty())
            })
        }

        fn parse_alternatives(chars: &[char], at: &mut usize) -> Vec<Vec<(Node, Rep)>> {
            let mut alternatives = vec![Vec::new()];
            while *at < chars.len() {
                let c = chars[*at];
                *at += 1;
                let node = match c {
                    '|' => {
                        alternatives.push(Vec::new());
                        continue;
                    }
                    ')' => break,
                    '(' => Node::Group(parse_alternatives(chars, at)),
                    '$' => Node::End,
                    '[' => {
                        let negated = chars[*at] == '^';
                        if negated {
                            *at += 1;
                        }
                        let mut ranges = Vec::new();
                        while chars[*at] != ']' {
                            let mut first = chars[*at];
                            *at += 1;
                            if first == '\\' {
                                first = match chars[*at] {
                                    'n' => '\n',
                                    'r' => '\r',
                                    other => other,
                                };
                                *at += 1;
                                if chars[*at - 1] == 's' || chars[*at - 1] == 'S' {
                                    let space = chars[*at - 1] == 's';
                                    for c in [' ', '\t', '\n', '\r'] {
                                        if space {
                                            ranges.push((c, c));
                                        }
                                    }
                                    if !space {
                                        ranges.push(('!', '~'));
                                    }
                                    continue;
                                }
                            }
                            if chars[*at] == '-' && chars[*at + 1] != ']' {
                                ranges.push((first, chars[*at + 1]));
                                *at += 2;
                            } else {
                                ranges.push((first, first));
                            }
                        }
                        *at += 1;
                        Node::Class(ranges, negated)
                    }
                    '\\' => {
                        let escaped = chars[*at];
                        *at += 1;
                        match escaped {
                            's' => Node::Space(true),
                            'S' => Node::Space(false),
                            'b' => Node::Boundary,
                            'n' => Node::Char('\n'),
                            other => Node::Char(other),
                        }
                    }
                    other => Node::Char(other),
                };
                let rep = match chars.get(*at) {
                    Some('?') => Rep::Optional,
                    Some('*') => Rep::Many,
                    Some('+') => Rep::Some,
                    _ => Rep::One,
                };
                if rep != Rep::One {
                    *at += 1;
                }
                if let Some(sequence) = alternatives.last_mut() {
                    sequence.push((node, rep));
                }
            }
            alternatives
        }

        /// Every position the sequence can end at from `at`.
        fn run(sequence: &[(Node, Rep)], text: &[char], at: usize) -> Vec<usize> {
            let Some(((node, rep), rest)) = sequence.split_first() else {
                return vec![at];
            };
            let mut ends = Vec::new();
            let mut positions = match rep {
                Rep::One | Rep::Some => Vec::new(),
                Rep::Optional | Rep::Many => vec![at],
            };
            let mut frontier = vec![at];
            loop {
                let next: Vec<usize> = frontier
                    .iter()
                    .flat_map(|&position| one(node, text, position))
                    .filter(|position| !positions.contains(position))
                    .collect();
                if next.is_empty() {
                    break;
                }
                positions.extend(next.iter().copied());
                if matches!(rep, Rep::One | Rep::Optional) {
                    break;
                }
                frontier = next;
            }
            for position in positions {
                for end in run(rest, text, position) {
                    if !ends.contains(&end) {
                        ends.push(end);
                    }
                }
            }
            ends
        }

        fn one(node: &Node, text: &[char], at: usize) -> Vec<usize> {
            let word = |c: Option<&char>| c.is_some_and(|c| c.is_alphanumeric() || *c == '_');
            match node {
                Node::Boundary => {
                    let before = at.checked_sub(1).and_then(|before| text.get(before));
                    if word(before) == word(text.get(at)) {
                        Vec::new()
                    } else {
                        vec![at]
                    }
                }
                Node::End => {
                    if at == text.len() {
                        vec![at]
                    } else {
                        Vec::new()
                    }
                }
                Node::Group(alternatives) => alternatives
                    .iter()
                    .flat_map(|sequence| run(sequence, text, at))
                    .collect(),
                Node::Char(expected) => match text.get(at) {
                    Some(c) if c == expected => vec![at + 1],
                    _ => Vec::new(),
                },
                Node::Space(space) => match text.get(at) {
                    Some(c) if c.is_whitespace() == *space => vec![at + 1],
                    _ => Vec::new(),
                },
                Node::Class(ranges, negated) => match text.get(at) {
                    Some(c)
                        if ranges.iter().any(|(low, high)| low <= c && c <= high) != *negated =>
                    {
                        vec![at + 1]
                    }
                    _ => Vec::new(),
                },
            }
        }
    }
}
