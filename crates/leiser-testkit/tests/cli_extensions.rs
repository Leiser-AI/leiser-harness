//! The CLI extensions panel (#42): the developer sees the skills, MCP
//! servers, hooks and instruction files each agent CLI loads from the
//! repository, and edits them in the agent CLI's own files. Nothing is
//! converted between agent CLIs (ADR 0003).

use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};

use leiser_service::{
    AgentCli, CliExtension, CliExtensionKind, Command, CommandError, RepositoryId, Response,
    ServiceHandle, UnreadableCliExtensionFile,
};
use leiser_testkit::{Script, TestRepo, TestService};

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

const FAKE_AGENT_CLI: &str = env!("CARGO_BIN_EXE_fake-agent-cli");

const CLAUDE_MCP: &str = r#"{
  "mcpServers": {
    "issues": { "command": "issue-server", "args": ["--stdio"] }
  }
}
"#;

const CLAUDE_SETTINGS: &str = r#"{
  "permissions": { "allow": ["Bash(cargo test *)"] },
  "hooks": {
    "PreToolUse": [
      { "matcher": "Bash", "hooks": [{ "type": "command", "command": "./check-bash.sh" }] }
    ]
  }
}
"#;

const CODEX_CONFIG: &str = r#"model = "gpt-5"

[mcp_servers.docs]
url = "https://docs.example.test/mcp"

[[hooks.Stop]]
[[hooks.Stop.hooks]]
type = "command"
command = "./notify.sh"
"#;

const CODEX_HOOKS: &str = r#"{
  "hooks": {
    "SessionStart": [
      { "hooks": [{ "type": "command", "command": "./setup.sh" }] }
    ]
  }
}
"#;

struct Panel {
    _service: TestService,
    app: ServiceHandle,
    repository: RepositoryId,
}

impl Panel {
    async fn open(repo: &TestRepo) -> TestResult<Self> {
        let service = TestService::start(Path::new(FAKE_AGENT_CLI), &Script::new())?;
        let app = service.connect();
        let response = app
            .execute(Command::AddRepository {
                path: repo.path().to_path_buf(),
            })
            .await?;
        let Response::RepositoryAdded { repository } = response else {
            return Err(format!("expected RepositoryAdded, got {response:?}").into());
        };
        Ok(Self {
            _service: service,
            app,
            repository,
        })
    }

    async fn list(&self) -> TestResult<(Vec<CliExtension>, Vec<UnreadableCliExtensionFile>)> {
        let response = self
            .app
            .execute(Command::ListCliExtensions {
                repository: self.repository,
            })
            .await?;
        let Response::CliExtensions {
            extensions,
            unreadable,
        } = response
        else {
            return Err(format!("expected CliExtensions, got {response:?}").into());
        };
        Ok((extensions, unreadable))
    }

    async fn extensions(&self, agent_cli: AgentCli) -> TestResult<Vec<CliExtension>> {
        Ok(self
            .list()
            .await?
            .0
            .into_iter()
            .filter(|extension| extension.agent_cli == agent_cli)
            .collect())
    }

    async fn read(&self, file: &str) -> Result<String, Box<dyn Error>> {
        let response = self
            .app
            .execute(Command::ReadCliExtensionFile {
                repository: self.repository,
                file: PathBuf::from(file),
            })
            .await?;
        let Response::CliExtensionFile { text } = response else {
            return Err(format!("expected CliExtensionFile, got {response:?}").into());
        };
        Ok(text)
    }

    async fn save(&self, file: &str, text: &str, previous: &str) -> Result<(), CommandError> {
        self.app
            .execute(Command::SaveCliExtensionFile {
                repository: self.repository,
                file: PathBuf::from(file),
                text: text.to_owned(),
                previous: previous.to_owned(),
            })
            .await
            .map(drop)
    }
}

fn write(repo: &TestRepo, path: &str, text: &str) -> TestResult {
    let path = repo.path().join(path);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, text)?;
    Ok(())
}

fn skill(name: &str, description: &str) -> String {
    format!("---\nname: {name}\ndescription: {description}\n---\n\n# {name}\n")
}

fn extension(
    agent_cli: AgentCli,
    kind: CliExtensionKind,
    name: &str,
    path: &str,
    file: &str,
) -> CliExtension {
    CliExtension {
        agent_cli,
        kind,
        name: name.to_owned(),
        path: PathBuf::from(path),
        file: PathBuf::from(file),
    }
}

fn instruction_file(agent_cli: AgentCli, path: &str, imported_by: Option<&str>) -> CliExtension {
    let name = Path::new(path)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    extension(
        agent_cli,
        CliExtensionKind::InstructionFile {
            imported_by: imported_by.map(PathBuf::from),
        },
        &name,
        path,
        path,
    )
}

/// A repository with CLI extensions of both agent CLIs.
fn repository_with_extensions() -> TestResult<TestRepo> {
    let repo = TestRepo::new()?;
    write(&repo, "AGENTS.md", "# Rules\n")?;
    write(
        &repo,
        "CLAUDE.md",
        "@AGENTS.md\n\nAlso see `@not-an-import.md`.\n",
    )?;
    write(&repo, "crates/app/AGENTS.md", "# App rules\n")?;
    write(
        &repo,
        ".claude/skills/review/SKILL.md",
        &skill("review", "Reviews a branch."),
    )?;
    write(
        &repo,
        ".agents/skills/tdd/SKILL.md",
        &skill("tdd", "Test first."),
    )?;
    write(&repo, ".mcp.json", CLAUDE_MCP)?;
    write(&repo, ".claude/settings.json", CLAUDE_SETTINGS)?;
    write(&repo, ".codex/config.toml", CODEX_CONFIG)?;
    write(&repo, ".codex/hooks.json", CODEX_HOOKS)?;
    Ok(repo)
}

#[tokio::test]
async fn claude_code_lists_the_extensions_it_loads_from_the_repository() -> TestResult {
    let repo = repository_with_extensions()?;
    let panel = Panel::open(&repo).await?;

    assert_eq!(
        panel.extensions(AgentCli::ClaudeCode).await?,
        vec![
            instruction_file(AgentCli::ClaudeCode, "CLAUDE.md", None),
            instruction_file(AgentCli::ClaudeCode, "AGENTS.md", Some("CLAUDE.md")),
            extension(
                AgentCli::ClaudeCode,
                CliExtensionKind::Skill {
                    description: Some("Reviews a branch.".to_owned()),
                },
                "review",
                ".claude/skills/review/SKILL.md",
                ".claude/skills/review/SKILL.md",
            ),
            extension(
                AgentCli::ClaudeCode,
                CliExtensionKind::McpServer {
                    launch: Some("issue-server --stdio".to_owned()),
                },
                "issues",
                ".mcp.json",
                ".mcp.json",
            ),
            extension(
                AgentCli::ClaudeCode,
                CliExtensionKind::Hook {
                    matcher: Some("Bash".to_owned()),
                    commands: vec!["./check-bash.sh".to_owned()],
                },
                "PreToolUse",
                ".claude/settings.json",
                ".claude/settings.json",
            ),
        ]
    );
    Ok(())
}

#[tokio::test]
async fn codex_lists_the_extensions_it_loads_from_the_repository() -> TestResult {
    let repo = repository_with_extensions()?;
    let panel = Panel::open(&repo).await?;

    assert_eq!(
        panel.extensions(AgentCli::Codex).await?,
        vec![
            instruction_file(AgentCli::Codex, "AGENTS.md", None),
            instruction_file(AgentCli::Codex, "crates/app/AGENTS.md", None),
            extension(
                AgentCli::Codex,
                CliExtensionKind::Skill {
                    description: Some("Test first.".to_owned()),
                },
                "tdd",
                ".agents/skills/tdd/SKILL.md",
                ".agents/skills/tdd/SKILL.md",
            ),
            extension(
                AgentCli::Codex,
                CliExtensionKind::McpServer {
                    launch: Some("https://docs.example.test/mcp".to_owned()),
                },
                "docs",
                ".codex/config.toml",
                ".codex/config.toml",
            ),
            extension(
                AgentCli::Codex,
                CliExtensionKind::Hook {
                    matcher: None,
                    commands: vec!["./notify.sh".to_owned()],
                },
                "Stop",
                ".codex/config.toml",
                ".codex/config.toml",
            ),
            extension(
                AgentCli::Codex,
                CliExtensionKind::Hook {
                    matcher: None,
                    commands: vec!["./setup.sh".to_owned()],
                },
                "SessionStart",
                ".codex/hooks.json",
                ".codex/hooks.json",
            ),
        ]
    );
    Ok(())
}

#[tokio::test]
async fn a_repository_without_extensions_lists_none() -> TestResult {
    let repo = TestRepo::new()?;
    write(&repo, "README.md", "# Nothing for agents here\n")?;
    let panel = Panel::open(&repo).await?;

    let (extensions, unreadable) = panel.list().await?;

    assert_eq!(extensions, []);
    assert_eq!(unreadable, []);
    Ok(())
}

#[tokio::test]
async fn claude_code_reads_agents_md_only_without_a_claude_md() -> TestResult {
    let repo = TestRepo::new()?;
    write(&repo, "AGENTS.md", "# Rules\n")?;
    let panel = Panel::open(&repo).await?;

    assert_eq!(
        panel.extensions(AgentCli::ClaudeCode).await?,
        vec![instruction_file(AgentCli::ClaudeCode, "AGENTS.md", None)]
    );

    write(&repo, "CLAUDE.md", "# Claude only\n")?;
    assert_eq!(
        panel.extensions(AgentCli::ClaudeCode).await?,
        vec![instruction_file(AgentCli::ClaudeCode, "CLAUDE.md", None)]
    );
    Ok(())
}

#[tokio::test]
async fn codex_lists_the_override_instead_of_agents_md_in_the_same_directory() -> TestResult {
    let repo = TestRepo::new()?;
    write(&repo, "AGENTS.md", "# Shared rules\n")?;
    write(&repo, "AGENTS.override.md", "# Local rules\n")?;
    write(&repo, "docs/AGENTS.md", "# Docs rules\n")?;
    let panel = Panel::open(&repo).await?;

    let instruction_files: Vec<PathBuf> = panel
        .extensions(AgentCli::Codex)
        .await?
        .into_iter()
        .map(|extension| extension.path)
        .collect();

    assert_eq!(
        instruction_files,
        [
            PathBuf::from("AGENTS.override.md"),
            PathBuf::from("docs/AGENTS.md")
        ]
    );
    Ok(())
}

#[tokio::test]
async fn instruction_files_git_ignores_outside_the_root_are_not_listed() -> TestResult {
    let repo = TestRepo::new()?;
    write(&repo, ".gitignore", "target/\n")?;
    write(&repo, "target/vendored/AGENTS.md", "# Not ours\n")?;
    write(&repo, "src/AGENTS.md", "# Ours\n")?;
    let panel = Panel::open(&repo).await?;

    let paths: Vec<PathBuf> = panel
        .extensions(AgentCli::Codex)
        .await?
        .into_iter()
        .map(|extension| extension.path)
        .collect();

    assert_eq!(paths, [PathBuf::from("src/AGENTS.md")]);
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn a_symlinked_skill_is_one_file_that_both_agent_clis_load() -> TestResult {
    use std::os::unix::fs::symlink;

    let repo = TestRepo::new()?;
    write(
        &repo,
        ".agents/skills/tdd/SKILL.md",
        &skill("tdd", "Test first."),
    )?;
    fs::create_dir_all(repo.path().join(".claude"))?;
    // As in this repository: `.claude/skills` points at `.agents/skills`.
    symlink("../.agents/skills", repo.path().join(".claude/skills"))?;
    let panel = Panel::open(&repo).await?;

    let skill_kind = CliExtensionKind::Skill {
        description: Some("Test first.".to_owned()),
    };
    assert_eq!(
        panel.list().await?.0,
        vec![
            extension(
                AgentCli::ClaudeCode,
                skill_kind.clone(),
                "tdd",
                ".claude/skills/tdd/SKILL.md",
                ".agents/skills/tdd/SKILL.md",
            ),
            extension(
                AgentCli::Codex,
                skill_kind,
                "tdd",
                ".agents/skills/tdd/SKILL.md",
                ".agents/skills/tdd/SKILL.md",
            ),
        ]
    );

    let text = panel.read(".agents/skills/tdd/SKILL.md").await?;
    let edited = text.replace("Test first.", "Red, green, refactor.");
    panel
        .save(".agents/skills/tdd/SKILL.md", &edited, &text)
        .await?;

    assert!(
        fs::symlink_metadata(repo.path().join(".claude/skills"))?
            .file_type()
            .is_symlink(),
        "saving must not replace the symlink with a copy"
    );
    assert_eq!(
        fs::read_to_string(repo.path().join(".claude/skills/tdd/SKILL.md"))?,
        edited
    );
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn two_links_to_one_skill_list_it_once() -> TestResult {
    use std::os::unix::fs::symlink;

    let repo = TestRepo::new()?;
    write(&repo, "shared/tdd/SKILL.md", &skill("tdd", "Test first."))?;
    fs::create_dir_all(repo.path().join(".claude/skills"))?;
    symlink("../../shared/tdd", repo.path().join(".claude/skills/tdd"))?;
    symlink(
        "../../shared/tdd",
        repo.path().join(".claude/skills/test-first"),
    )?;
    let panel = Panel::open(&repo).await?;

    let files: Vec<PathBuf> = panel
        .extensions(AgentCli::ClaudeCode)
        .await?
        .into_iter()
        .map(|extension| extension.file)
        .collect();

    assert_eq!(files, [PathBuf::from("shared/tdd/SKILL.md")]);
    Ok(())
}

#[tokio::test]
async fn an_edit_is_saved_to_the_agent_clis_own_file_and_nowhere_else() -> TestResult {
    let repo = TestRepo::new()?;
    write(&repo, ".mcp.json", CLAUDE_MCP)?;
    write(&repo, ".codex/config.toml", CODEX_CONFIG)?;
    let panel = Panel::open(&repo).await?;

    let text = panel.read(".mcp.json").await?;
    assert_eq!(text, CLAUDE_MCP);
    let edited = text.replace("\"--stdio\"", "\"--stdio\", \"--verbose\"");
    panel.save(".mcp.json", &edited, &text).await?;

    assert_eq!(fs::read_to_string(repo.path().join(".mcp.json"))?, edited);
    assert_eq!(
        fs::read_to_string(repo.path().join(".codex/config.toml"))?,
        CODEX_CONFIG,
        "an edit for Claude Code must not reach Codex's files"
    );
    let servers: Vec<(AgentCli, String, CliExtensionKind)> = panel
        .list()
        .await?
        .0
        .into_iter()
        .filter(|extension| matches!(extension.kind, CliExtensionKind::McpServer { .. }))
        .map(|extension| (extension.agent_cli, extension.name, extension.kind))
        .collect();
    assert_eq!(
        servers,
        [
            (
                AgentCli::ClaudeCode,
                "issues".to_owned(),
                CliExtensionKind::McpServer {
                    launch: Some("issue-server --stdio --verbose".to_owned()),
                }
            ),
            (
                AgentCli::Codex,
                "docs".to_owned(),
                CliExtensionKind::McpServer {
                    launch: Some("https://docs.example.test/mcp".to_owned()),
                }
            ),
        ]
    );
    Ok(())
}

#[tokio::test]
async fn only_files_that_hold_cli_extensions_can_be_read_or_saved() -> TestResult {
    let repo = TestRepo::new()?;
    write(&repo, "AGENTS.md", "# Rules\n")?;
    write(&repo, "Cargo.toml", "[workspace]\n")?;
    let outside = tempfile::tempdir()?;
    fs::write(outside.path().join("secret.txt"), "keep out\n")?;
    let panel = Panel::open(&repo).await?;

    for file in [
        "Cargo.toml".to_owned(),
        "../AGENTS.md".to_owned(),
        outside.path().join("secret.txt").display().to_string(),
    ] {
        let read = panel.read(&file).await;
        assert!(read.is_err(), "{file} was read: {read:?}");
        let saved = panel.save(&file, "overwritten\n", "").await;
        assert_eq!(
            saved,
            Err(CommandError::NotACliExtensionFile {
                file: PathBuf::from(&file)
            })
        );
    }
    assert_eq!(
        fs::read_to_string(repo.path().join("Cargo.toml"))?,
        "[workspace]\n"
    );
    assert_eq!(
        fs::read_to_string(outside.path().join("secret.txt"))?,
        "keep out\n"
    );
    Ok(())
}

#[tokio::test]
async fn a_file_that_changed_since_it_was_read_is_not_overwritten() -> TestResult {
    let repo = TestRepo::new()?;
    write(&repo, "AGENTS.md", "# Rules\n")?;
    let panel = Panel::open(&repo).await?;

    let text = panel.read("AGENTS.md").await?;
    // An agent edits the file while the developer has it open.
    write(&repo, "AGENTS.md", "# Rules\n\n- Added by an agent.\n")?;
    let saved = panel.save("AGENTS.md", "# My rules\n", &text).await;

    assert_eq!(
        saved,
        Err(CommandError::CliExtensionFileChanged {
            file: PathBuf::from("AGENTS.md")
        })
    );
    assert_eq!(
        fs::read_to_string(repo.path().join("AGENTS.md"))?,
        "# Rules\n\n- Added by an agent.\n"
    );
    Ok(())
}

#[tokio::test]
async fn an_edit_the_agent_cli_could_not_parse_is_refused() -> TestResult {
    let repo = TestRepo::new()?;
    write(&repo, ".claude/settings.json", CLAUDE_SETTINGS)?;
    write(&repo, ".codex/config.toml", CODEX_CONFIG)?;
    let panel = Panel::open(&repo).await?;

    for (file, broken) in [
        (".claude/settings.json", "{ \"hooks\": "),
        (".codex/config.toml", "[mcp_servers.docs\n"),
    ] {
        let text = panel.read(file).await?;
        let saved = panel.save(file, broken, &text).await;
        assert!(
            matches!(saved, Err(CommandError::InvalidCliExtensionFile { .. })),
            "{file}: {saved:?}"
        );
        assert_eq!(fs::read_to_string(repo.path().join(file))?, text);
    }
    Ok(())
}

#[tokio::test]
async fn a_file_that_does_not_parse_is_reported_and_can_be_fixed() -> TestResult {
    let repo = TestRepo::new()?;
    write(&repo, ".mcp.json", "{ \"mcpServers\": { \"issues\": ")?;
    let panel = Panel::open(&repo).await?;

    let (extensions, unreadable) = panel.list().await?;
    assert_eq!(extensions, []);
    let [broken] = unreadable.as_slice() else {
        return Err(format!("expected one unreadable file, got {unreadable:?}").into());
    };
    assert_eq!(
        (broken.agent_cli, &broken.path, &broken.file),
        (
            AgentCli::ClaudeCode,
            &PathBuf::from(".mcp.json"),
            &PathBuf::from(".mcp.json")
        )
    );

    let text = panel.read(".mcp.json").await?;
    panel.save(".mcp.json", CLAUDE_MCP, &text).await?;

    let (extensions, unreadable) = panel.list().await?;
    assert_eq!(unreadable, []);
    assert_eq!(
        extensions,
        vec![extension(
            AgentCli::ClaudeCode,
            CliExtensionKind::McpServer {
                launch: Some("issue-server --stdio".to_owned()),
            },
            "issues",
            ".mcp.json",
            ".mcp.json",
        )]
    );
    Ok(())
}
