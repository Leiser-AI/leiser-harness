//! The CLI extensions each agent CLI loads for a repository: instruction
//! files, skills, MCP servers and hooks (ADR 0003).
//!
//! They belong to the agent CLI. The harness service finds them where the
//! agent CLI itself looks, reads and saves the agent CLI's own files, and
//! never converts one agent CLI's extensions into another's. Symlinks are
//! followed: a skill that `.claude/skills` links into `.agents/skills` is
//! one file, listed under both agent CLIs with the same [`CliExtension::file`].
//!
//! Only files inside the repository are found here. User-level extensions
//! live in the agent CLI's config directory, which depends on the
//! subscription a chat runs on.
//!
//! Where the agent CLIs look is documented in
//! `docs/cli-extension-locations.md`.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use serde::Deserialize;

use crate::{AgentCli, CliExtension, CliExtensionKind, UnreadableCliExtensionFile};

/// How many `@` imports deep Claude Code follows from an instruction file.
const MAX_IMPORT_DEPTH: usize = 4;

/// Claude Code's instruction files at the repository root.
const CLAUDE_ROOT_INSTRUCTIONS: [&str; 3] = ["CLAUDE.md", ".claude/CLAUDE.md", "CLAUDE.local.md"];
/// What Claude Code reads at the root instead when none of
/// [`CLAUDE_ROOT_INSTRUCTIONS`] exists.
const CLAUDE_AGENTS_FALLBACK: [&str; 2] = ["AGENTS.md", ".claude/AGENTS.md"];
/// Claude Code's instruction files in any directory of the repository.
const CLAUDE_NESTED_INSTRUCTIONS: [&str; 2] = ["CLAUDE.md", "CLAUDE.local.md"];
/// Claude Code's topic rules, every Markdown file below it.
const CLAUDE_RULES: &str = ".claude/rules";
const CLAUDE_SKILLS: &str = ".claude/skills";
const CLAUDE_MCP: &str = ".mcp.json";
const CLAUDE_SETTINGS: [&str; 2] = [".claude/settings.json", ".claude/settings.local.json"];

/// Codex reads the override instead of `AGENTS.md` in the same directory.
const CODEX_OVERRIDE: &str = "AGENTS.override.md";
const CODEX_INSTRUCTIONS: &str = "AGENTS.md";
const CODEX_SKILLS: [&str; 2] = [".agents/skills", ".codex/skills"];
const CODEX_CONFIG: &str = ".codex/config.toml";
const CODEX_HOOKS: &str = ".codex/hooks.json";

const SKILL_FILE: &str = "SKILL.md";

/// What the agent CLIs load for one repository.
#[derive(Debug, Default)]
pub(crate) struct Listing {
    pub(crate) extensions: Vec<CliExtension>,
    pub(crate) unreadable: Vec<UnreadableCliExtensionFile>,
}

impl Listing {
    /// Whether `file`, as listed, holds CLI extensions of the repository.
    fn holds(&self, file: &Path) -> bool {
        self.extensions
            .iter()
            .any(|extension| extension.file == file)
            || self
                .unreadable
                .iter()
                .any(|unreadable| unreadable.file == file)
    }
}

/// Why a CLI extension file could not be read or saved.
#[derive(Debug, thiserror::Error)]
pub(crate) enum CliExtensionError {
    #[error("{} holds no CLI extension of this repository", file.display())]
    NotACliExtensionFile { file: PathBuf },
    #[error("{} changed since it was read", file.display())]
    Changed { file: PathBuf },
    #[error("{} would no longer load: {reason}", file.display())]
    Invalid { file: PathBuf, reason: String },
    #[error("could not access {}: {source}", file.display())]
    Io {
        file: PathBuf,
        #[source]
        source: io::Error,
    },
}

/// Finds every CLI extension Claude Code and Codex load for the repository
/// at `root`.
pub(crate) fn list(root: &Path) -> Listing {
    let finder = Finder::new(root);
    let mut listing = Listing::default();
    finder.claude_code(&mut listing);
    finder.codex(&mut listing);
    listing
}

/// Reads a file that [`list`] reports, by its [`CliExtension::file`].
pub(crate) fn read(root: &Path, file: &Path) -> Result<String, CliExtensionError> {
    let path = listed(root, file)?;
    read_text(&path).map_err(|source| CliExtensionError::Io {
        file: file.to_path_buf(),
        source,
    })
}

/// Saves `text` to a file that [`list`] reports, if it still holds
/// `previous`, the text the developer started from. JSON and TOML files are
/// saved only when the agent CLI can still parse them.
pub(crate) fn save(
    root: &Path,
    file: &Path,
    text: &str,
    previous: &str,
) -> Result<(), CliExtensionError> {
    let path = listed(root, file)?;
    let io_error = |source| CliExtensionError::Io {
        file: file.to_path_buf(),
        source,
    };
    if read_text(&path).map_err(io_error)? != previous {
        return Err(CliExtensionError::Changed {
            file: file.to_path_buf(),
        });
    }
    if let Err(reason) = check_syntax(&path, text) {
        return Err(CliExtensionError::Invalid {
            file: file.to_path_buf(),
            reason,
        });
    }
    // `path` has its symlinks resolved, so the link itself stays a link.
    fs::write(&path, text).map_err(io_error)
}

/// The real path of `file` if the repository's listing holds it.
fn listed(root: &Path, file: &Path) -> Result<PathBuf, CliExtensionError> {
    if !list(root).holds(file) {
        return Err(CliExtensionError::NotACliExtensionFile {
            file: file.to_path_buf(),
        });
    }
    let root = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    Ok(root.join(file))
}

fn read_text(path: &Path) -> io::Result<String> {
    let bytes = fs::read(path)?;
    String::from_utf8(bytes).map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "not UTF-8"))
}

/// Whether the agent CLI can still parse `text` as the file at `path`.
fn check_syntax(path: &Path, text: &str) -> Result<(), String> {
    match path.extension().and_then(|extension| extension.to_str()) {
        Some("json") => serde_json::from_str::<serde::de::IgnoredAny>(text)
            .map(drop)
            .map_err(|error| error.to_string()),
        Some("toml") => toml::from_str::<toml::Table>(text)
            .map(drop)
            .map_err(|error| error.to_string()),
        _ => Ok(()),
    }
}

/// Looks for CLI extensions in one repository.
struct Finder {
    root: PathBuf,
    /// `root` with its symlinks resolved, to tell files inside the
    /// repository from files a symlink points out of it.
    real_root: PathBuf,
}

impl Finder {
    fn new(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
            real_root: fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf()),
        }
    }

    fn claude_code(&self, listing: &mut Listing) {
        let agent_cli = AgentCli::ClaudeCode;
        let mut found = Found::new(agent_cli);

        let has_claude_md = CLAUDE_ROOT_INSTRUCTIONS
            .iter()
            .any(|path| self.existing(Path::new(path)).is_some());
        let root_instructions: &[&str] = if has_claude_md {
            &CLAUDE_ROOT_INSTRUCTIONS
        } else {
            &CLAUDE_AGENTS_FALLBACK
        };
        // Root files first, so the files they import follow them.
        let instructions = root_instructions
            .iter()
            .map(PathBuf::from)
            .chain(self.tracked(&CLAUDE_NESTED_INSTRUCTIONS))
            .chain(self.markdown_below(Path::new(CLAUDE_RULES)));
        for path in instructions {
            self.instruction_file(&mut found, &path, None, 0);
        }

        for (path, skill) in self.skills(Path::new(CLAUDE_SKILLS)) {
            self.skill(&mut found, &path, skill);
        }

        if let Some(file) = self.existing(Path::new(CLAUDE_MCP)) {
            match parse_json::<ClaudeMcpFile>(&file.real) {
                Ok(mcp) => {
                    for (name, server) in mcp.mcp_servers {
                        found.push(
                            &file,
                            name,
                            CliExtensionKind::McpServer {
                                launch: server.launch(),
                            },
                        );
                    }
                }
                Err(reason) => found.unreadable(&file, reason),
            }
        }

        for settings in CLAUDE_SETTINGS {
            if let Some(file) = self.existing(Path::new(settings)) {
                match parse_json::<HooksFile>(&file.real) {
                    Ok(settings) => settings.push_hooks(&mut found, &file),
                    Err(reason) => found.unreadable(&file, reason),
                }
            }
        }

        found.into(listing);
    }

    fn codex(&self, listing: &mut Listing) {
        let agent_cli = AgentCli::Codex;
        let mut found = Found::new(agent_cli);

        // In each directory Codex reads the override if there is one, and
        // `AGENTS.md` otherwise.
        let mut instructions: BTreeMap<PathBuf, PathBuf> = BTreeMap::new();
        let candidates = self
            .tracked(&[CODEX_INSTRUCTIONS, CODEX_OVERRIDE])
            .into_iter()
            .chain([
                PathBuf::from(CODEX_INSTRUCTIONS),
                PathBuf::from(CODEX_OVERRIDE),
            ]);
        for path in candidates {
            if self.existing(&path).is_none() {
                continue;
            }
            let directory = path.parent().map(Path::to_path_buf).unwrap_or_default();
            let is_override = path.file_name().is_some_and(|name| name == CODEX_OVERRIDE);
            if is_override || !instructions.contains_key(&directory) {
                instructions.insert(directory, path);
            }
        }
        for path in instructions.into_values() {
            if let Some(file) = self.existing(&path) {
                found.push(
                    &file,
                    file_name(&path),
                    CliExtensionKind::InstructionFile { imported_by: None },
                );
            }
        }

        for directory in CODEX_SKILLS {
            for (path, skill) in self.skills(Path::new(directory)) {
                self.skill(&mut found, &path, skill);
            }
        }

        if let Some(file) = self.existing(Path::new(CODEX_CONFIG)) {
            match parse_toml::<CodexConfig>(&file.real) {
                Ok(config) => {
                    for (name, server) in config.mcp_servers {
                        found.push(
                            &file,
                            name,
                            CliExtensionKind::McpServer {
                                launch: server.launch(),
                            },
                        );
                    }
                    HooksFile {
                        hooks: config.hooks,
                    }
                    .push_hooks(&mut found, &file);
                }
                Err(reason) => found.unreadable(&file, reason),
            }
        }

        if let Some(file) = self.existing(Path::new(CODEX_HOOKS)) {
            match parse_json::<HooksFile>(&file.real) {
                Ok(hooks) => hooks.push_hooks(&mut found, &file),
                Err(reason) => found.unreadable(&file, reason),
            }
        }

        found.into(listing);
    }

    /// Lists an instruction file of Claude Code and, following its `@`
    /// imports, the files it imports.
    fn instruction_file(
        &self,
        found: &mut Found,
        path: &Path,
        imported_by: Option<&Path>,
        depth: usize,
    ) {
        let Some(file) = self.existing(path) else {
            return;
        };
        if found.has(&file.listed) {
            return;
        }
        found.push(
            &file,
            file_name(path),
            CliExtensionKind::InstructionFile {
                imported_by: imported_by.map(Path::to_path_buf),
            },
        );
        if depth >= MAX_IMPORT_DEPTH {
            return;
        }
        let Ok(text) = read_text(&file.real) else {
            return;
        };
        let directory = path.parent().unwrap_or(Path::new(""));
        for import in imports(&text) {
            // `~` imports are user-level files; absolute ones lie outside
            // the repository in all but contrived cases.
            if import.starts_with("~") || import.is_absolute() {
                continue;
            }
            if let Some(imported) = normalize(&directory.join(import)) {
                self.instruction_file(found, &imported, Some(path), depth + 1);
            }
        }
    }

    fn skill(&self, found: &mut Found, path: &Path, skill: Option<Skill>) {
        let Some(file) = self.existing(path) else {
            return;
        };
        let directory_name = path
            .parent()
            .and_then(Path::file_name)
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let (name, description) = match skill {
            Some(skill) => (skill.name.unwrap_or(directory_name), skill.description),
            None => (directory_name, None),
        };
        found.push(&file, name, CliExtensionKind::Skill { description });
    }

    /// The `SKILL.md` of every skill directory in `directory`, with its
    /// front matter where it has one.
    fn skills(&self, directory: &Path) -> Vec<(PathBuf, Option<Skill>)> {
        let Ok(entries) = fs::read_dir(self.root.join(directory)) else {
            return Vec::new();
        };
        let mut names: Vec<_> = entries
            .filter_map(Result::ok)
            .map(|entry| entry.file_name())
            .collect();
        names.sort();
        names
            .into_iter()
            .map(|name| directory.join(name).join(SKILL_FILE))
            .filter(|path| self.root.join(path).is_file())
            .map(|path| {
                let skill = read_text(&self.root.join(&path))
                    .ok()
                    .map(|text| Skill::parse(&text));
                (path, skill)
            })
            .collect()
    }

    /// Every Markdown file below `directory`, sorted.
    fn markdown_below(&self, directory: &Path) -> Vec<PathBuf> {
        let mut files = Vec::new();
        // Real paths of the directories read, so a symlink loop ends.
        let mut seen = Vec::new();
        let mut pending = vec![directory.to_path_buf()];
        while let Some(directory) = pending.pop() {
            let full = self.root.join(&directory);
            let Ok(real) = fs::canonicalize(&full) else {
                continue;
            };
            if seen.contains(&real) {
                continue;
            }
            seen.push(real);
            let Ok(entries) = fs::read_dir(full) else {
                continue;
            };
            for entry in entries.filter_map(Result::ok) {
                let path = directory.join(entry.file_name());
                if self.root.join(&path).is_dir() {
                    pending.push(path);
                } else if path.extension().is_some_and(|extension| extension == "md") {
                    files.push(path);
                }
            }
        }
        files.sort();
        files
    }

    /// Files with one of `names` in any directory of the repository that
    /// git does not ignore, relative to the root.
    fn tracked(&self, names: &[&str]) -> Vec<PathBuf> {
        let mut command = Command::new("git");
        command.arg("-C").arg(&self.root).args([
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
            "--",
        ]);
        for name in names {
            command.arg(format!(":(glob)**/{name}"));
        }
        let Ok(output) = command.output() else {
            return Vec::new();
        };
        if !output.status.success() {
            return Vec::new();
        }
        let mut files: Vec<PathBuf> = output
            .stdout
            .split(|byte| *byte == 0)
            .filter(|path| !path.is_empty())
            .filter_map(|path| std::str::from_utf8(path).ok())
            .map(PathBuf::from)
            .collect();
        files.sort();
        files.dedup();
        files
    }

    /// `path`, relative to the root, if it is a file.
    fn existing(&self, path: &Path) -> Option<ExistingFile> {
        let full = self.root.join(path);
        if !full.is_file() {
            return None;
        }
        let real = fs::canonicalize(&full).ok()?;
        let listed = real
            .strip_prefix(&self.real_root)
            .map_or_else(|_| real.clone(), Path::to_path_buf);
        Some(ExistingFile {
            path: path.to_path_buf(),
            real,
            listed,
        })
    }
}

/// A file the agent CLI loads.
struct ExistingFile {
    /// Where the agent CLI finds it, relative to the repository.
    path: PathBuf,
    /// The file itself, symlinks resolved.
    real: PathBuf,
    /// [`ExistingFile::real`] relative to the repository when inside it.
    listed: PathBuf,
}

/// The CLI extensions found for one agent CLI so far.
struct Found {
    agent_cli: AgentCli,
    extensions: Vec<CliExtension>,
    unreadable: Vec<UnreadableCliExtensionFile>,
}

impl Found {
    fn new(agent_cli: AgentCli) -> Self {
        Self {
            agent_cli,
            extensions: Vec::new(),
            unreadable: Vec::new(),
        }
    }

    /// Whether an instruction file or skill in `file` is listed already,
    /// such as when two symlinks lead to it.
    fn has(&self, file: &Path) -> bool {
        self.extensions.iter().any(|extension| {
            extension.file == file
                && matches!(
                    extension.kind,
                    CliExtensionKind::InstructionFile { .. } | CliExtensionKind::Skill { .. }
                )
        })
    }

    fn push(&mut self, file: &ExistingFile, name: String, kind: CliExtensionKind) {
        let whole_file = matches!(
            kind,
            CliExtensionKind::InstructionFile { .. } | CliExtensionKind::Skill { .. }
        );
        if whole_file && self.has(&file.listed) {
            return;
        }
        self.extensions.push(CliExtension {
            agent_cli: self.agent_cli,
            kind,
            name,
            path: file.path.clone(),
            file: file.listed.clone(),
        });
    }

    fn unreadable(&mut self, file: &ExistingFile, reason: String) {
        self.unreadable.push(UnreadableCliExtensionFile {
            agent_cli: self.agent_cli,
            path: file.path.clone(),
            file: file.listed.clone(),
            reason,
        });
    }

    fn into(self, listing: &mut Listing) {
        listing.extensions.extend(self.extensions);
        listing.unreadable.extend(self.unreadable);
    }
}

fn file_name(path: &Path) -> String {
    path.file_name().map_or_else(
        || path.display().to_string(),
        |name| name.to_string_lossy().into_owned(),
    )
}

/// `path` without `.` and `..`, or `None` when it leaves the repository.
fn normalize(path: &Path) -> Option<PathBuf> {
    let mut normal = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => normal.push(part),
            Component::CurDir => {}
            Component::ParentDir => {
                if !normal.pop() {
                    return None;
                }
            }
            Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    Some(normal)
}

/// The paths a Claude Code instruction file imports with `@path`. Imports
/// in code spans and code blocks do not count.
fn imports(text: &str) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    let mut in_code_block = false;
    for line in text.lines() {
        if line.trim_start().starts_with("```") {
            in_code_block = !in_code_block;
            continue;
        }
        if in_code_block {
            continue;
        }
        // Every odd backtick-separated part is a code span.
        for (index, part) in line.split('`').enumerate() {
            if index % 2 == 1 {
                continue;
            }
            for word in part.split_whitespace() {
                if let Some(path) = word.strip_prefix('@')
                    && !path.is_empty()
                {
                    paths.push(PathBuf::from(path));
                }
            }
        }
    }
    paths
}

/// What a skill's front matter says about it.
#[derive(Debug, Default, PartialEq, Eq)]
struct Skill {
    name: Option<String>,
    description: Option<String>,
}

impl Skill {
    /// Reads `name` and `description` from the YAML front matter of a
    /// `SKILL.md`, as plain or quoted scalars or folded blocks.
    fn parse(text: &str) -> Self {
        let mut skill = Self::default();
        let mut lines = text.lines();
        if lines.next().map(str::trim_end) != Some("---") {
            return skill;
        }
        let front_matter: Vec<&str> = lines.take_while(|line| line.trim_end() != "---").collect();
        let mut index = 0;
        while index < front_matter.len() {
            let line = front_matter[index];
            index += 1;
            let Some((key, value)) = line.split_once(':') else {
                continue;
            };
            if line.starts_with(char::is_whitespace) {
                continue;
            }
            let value = value.trim();
            let value = if matches!(value, ">" | "|" | ">-" | "|-" | ">+" | "|+") {
                let mut block = Vec::new();
                while index < front_matter.len()
                    && (front_matter[index].starts_with(char::is_whitespace)
                        || front_matter[index].trim().is_empty())
                {
                    block.push(front_matter[index].trim());
                    index += 1;
                }
                block.join(" ").trim().to_owned()
            } else {
                unquote(value).to_owned()
            };
            if value.is_empty() {
                continue;
            }
            match key.trim() {
                "name" => skill.name = Some(value),
                "description" => skill.description = Some(value),
                _ => {}
            }
        }
        skill
    }
}

fn unquote(value: &str) -> &str {
    for quote in ['"', '\''] {
        if let Some(inner) = value
            .strip_prefix(quote)
            .and_then(|value| value.strip_suffix(quote))
        {
            return inner;
        }
    }
    value
}

fn parse_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T, String> {
    let text = read_text(path).map_err(|error| error.to_string())?;
    serde_json::from_str(&text).map_err(|error| error.to_string())
}

fn parse_toml<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T, String> {
    let text = read_text(path).map_err(|error| error.to_string())?;
    toml::from_str(&text).map_err(|error| error.to_string())
}

/// Claude Code's `.mcp.json`.
#[derive(Debug, Deserialize)]
struct ClaudeMcpFile {
    #[serde(rename = "mcpServers", default)]
    mcp_servers: BTreeMap<String, McpServer>,
}

/// Codex's `.codex/config.toml`, as far as CLI extensions go: MCP servers
/// and inline hooks, `[[hooks.<event>]]` in the shape of `hooks.json`.
#[derive(Debug, Deserialize)]
struct CodexConfig {
    #[serde(default)]
    mcp_servers: BTreeMap<String, McpServer>,
    #[serde(default)]
    hooks: BTreeMap<String, HookEvent>,
}

/// How an MCP server is started or reached; the same keys in Claude Code's
/// JSON and Codex's TOML.
#[derive(Debug, Deserialize)]
struct McpServer {
    command: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    url: Option<String>,
}

impl McpServer {
    fn launch(self) -> Option<String> {
        match (self.command, self.url) {
            (Some(command), _) => Some(
                std::iter::once(command)
                    .chain(self.args)
                    .collect::<Vec<_>>()
                    .join(" "),
            ),
            (None, url) => url,
        }
    }
}

/// A file with a `hooks` object in the shape both agent CLIs use: event
/// name to matcher groups, each with its hook handlers. Claude Code's
/// settings files and Codex's `hooks.json` have it, Codex's `config.toml`
/// the same in TOML.
#[derive(Debug, Deserialize)]
struct HooksFile {
    #[serde(default)]
    hooks: BTreeMap<String, HookEvent>,
}

/// The matcher groups of one event. Any other value under `hooks` is not
/// a hook and does not stop the others from being listed.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum HookEvent {
    Groups(Vec<HookGroup>),
    Other(serde::de::IgnoredAny),
}

#[derive(Debug, Deserialize)]
struct HookGroup {
    #[serde(default)]
    matcher: Option<String>,
    #[serde(default)]
    hooks: Vec<HookHandler>,
}

#[derive(Debug, Deserialize)]
struct HookHandler {
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    prompt: Option<String>,
}

impl HooksFile {
    fn push_hooks(self, found: &mut Found, file: &ExistingFile) {
        for (event, groups) in self.hooks {
            let HookEvent::Groups(groups) = groups else {
                continue;
            };
            for group in groups {
                let matcher = group
                    .matcher
                    .filter(|matcher| !matcher.is_empty() && matcher != "*");
                let commands = group
                    .hooks
                    .into_iter()
                    .filter_map(|handler| handler.command.or(handler.url).or(handler.prompt))
                    .collect();
                found.push(
                    file,
                    event.clone(),
                    CliExtensionKind::Hook { matcher, commands },
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn imports_skip_code_spans_and_blocks() {
        let text = "See @AGENTS.md and `@not-this.md`.\n```\n@nor-this.md\n```\n@docs/more.md";
        assert_eq!(
            imports(text),
            [PathBuf::from("AGENTS.md"), PathBuf::from("docs/more.md")]
        );
    }

    #[test]
    fn skill_front_matter_reads_quoted_and_folded_values() {
        let text = "---\nname: \"tdd\"\ndescription: >\n  Test-driven\n  development.\n---\n# TDD";
        assert_eq!(
            Skill::parse(text),
            Skill {
                name: Some("tdd".to_owned()),
                description: Some("Test-driven development.".to_owned()),
            }
        );
    }

    #[test]
    fn normalize_refuses_paths_that_leave_the_repository() {
        assert_eq!(
            normalize(Path::new("docs/../AGENTS.md")),
            Some(PathBuf::from("AGENTS.md"))
        );
        assert_eq!(normalize(Path::new("../outside.md")), None);
    }
}
