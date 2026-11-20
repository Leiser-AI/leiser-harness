//! Lightweight `leiser` CLI client for inspecting repositories/queues and
//! enqueueing tasks over [`leiser_service`]'s local interface.

use std::ffi::OsString;
use std::fmt::Write as _;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use leiser_service::{
    AgentCli, ChatOverview, Command, CommandError, ConnectError, ENDPOINT_FILE, LocalClient,
    PermissionMode, QueueAnswer, QueueCommand, QueueItem, QueueItemId, QueueItemState, QueueStatus,
    QueueTask, RepositoryOverview, RepositoryQueue, Response,
};
use serde::Serialize;

/// Usage text printed for `--help`.
pub const HELP_TEXT: &str = "\
Usage:
  leiser [--data-dir <dir>] [--json] status
  leiser [--data-dir <dir>] [--json] queue list [--repo <id-or-path>]
  leiser [--data-dir <dir>] [--json] queue add (<prompt> | --issue <number>) [--repo <id-or-path>] [--cli <agent>] [--mode <permission>]

Global options:
  --data-dir <dir>   Override the leiser-harness service data directory (env: LEISER_DATA_DIR)
  --json             Emit JSON instead of human-readable text
  -h, --help         Show this help text

Agent CLIs (--cli):
  claude | claude-code, codex, gemini

Permission modes (--mode):
  supervised, auto-accept-edits, auto, full-access";

const APP_IDENTIFIER: &str = "ai.leiser.harness";
const ENV_DATA_DIR: &str = "LEISER_DATA_DIR";

/// Parsed invocation ready to execute against the harness service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cli {
    pub data_dir: Option<PathBuf>,
    pub json: bool,
    pub action: Action,
}

/// Top-level subcommand.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Status,
    Queue(QueueAction),
}

/// Queue subcommand.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueueAction {
    List {
        repo: Option<String>,
    },
    Add {
        repo: Option<String>,
        task: QueueTask,
        agent_cli: Option<AgentCli>,
        permission_mode: Option<PermissionMode>,
    },
}

/// Result of parsing CLI arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseOutcome {
    Run(Cli),
    Help,
}

/// Failures surfaced by the `leiser` CLI.
#[derive(Debug, thiserror::Error)]
pub enum CliError {
    #[error("{0}\n\n{HELP_TEXT}")]
    Usage(String),
    #[error("could not determine default data directory ({0}); pass --data-dir <dir>")]
    MissingDataDir(&'static str),
    #[error(
        "no leiser-harness service is running for {} ({} was not found or its socket is closed); start leiser-harness or pass --data-dir <dir>",
        data_dir.display(),
        endpoint_file.display()
    )]
    NotRunning {
        data_dir: PathBuf,
        endpoint_file: PathBuf,
    },
    #[error("could not connect to harness service at {}: {source}", data_dir.display())]
    Connect {
        data_dir: PathBuf,
        #[source]
        source: ConnectError,
    },
    #[error("harness command failed: {0}")]
    Command(#[from] CommandError),
    #[error("no repository matches `{selector}`; available: {available}")]
    UnknownRepository { selector: String, available: String },
    #[error("repository selector `{selector}` is ambiguous: {matches}")]
    AmbiguousRepository { selector: String, matches: String },
    #[error(
        "could not infer repository from current directory; pass --repo <id-or-path> (available: {available})"
    )]
    RepositoryRequired { available: String },
    #[error("unexpected response from harness service: expected {expected}")]
    UnexpectedResponse { expected: &'static str },
    #[error("failed to format JSON output: {0}")]
    Json(#[from] serde_json::Error),
    #[error("failed to write output: {0}")]
    Io(#[from] io::Error),
}

/// JSON payload emitted for `leiser status --json`.
#[derive(Debug, Serialize)]
pub struct StatusOutput {
    pub data_dir: PathBuf,
    pub service_version: String,
    pub service_pid: u32,
    pub update_waiting: bool,
    pub repositories: Vec<RepositoryOverview>,
    pub chats: Vec<ChatOverview>,
}

/// One repository entry inside `leiser queue list --json`.
#[derive(Debug, Serialize)]
pub struct QueueListEntry {
    pub repository: RepositoryOverview,
    pub queue: RepositoryQueue,
}

/// JSON payload emitted for `leiser queue list --json`.
#[derive(Debug, Serialize)]
pub struct QueueListOutput {
    pub queues: Vec<QueueListEntry>,
}

/// JSON payload emitted for `leiser queue add --json`.
#[derive(Debug, Serialize)]
pub struct QueueAddOutput {
    pub repository: RepositoryOverview,
    pub item: QueueItemId,
    pub task: QueueTask,
    pub queue: RepositoryQueue,
}

#[derive(Debug, Default)]
struct ParsedFlags {
    data_dir: Option<PathBuf>,
    json: bool,
    help: bool,
    positionals: Vec<String>,
    repo: Option<String>,
    issue: Option<u64>,
    agent_cli: Option<AgentCli>,
    permission_mode: Option<PermissionMode>,
}

/// Parses command-line arguments after the executable name.
///
/// # Errors
///
/// Returns [`CliError::Usage`] when flags, subcommands or task parameters are
/// missing, unknown or contradictory.
pub fn parse_args<I, S>(args: I) -> Result<ParseOutcome, CliError>
where
    I: IntoIterator<Item = S>,
    S: Into<OsString>,
{
    let raw: Vec<OsString> = args.into_iter().map(Into::into).collect();
    if raw.is_empty() {
        return Err(CliError::Usage("missing command".to_owned()));
    }

    let mut parsed = ParsedFlags::default();
    let mut index = 0;
    let mut end_of_options = false;

    while index < raw.len() {
        let token = os_to_str(&raw[index])?;
        if !end_of_options {
            if token == "--" {
                end_of_options = true;
                index += 1;
                continue;
            }
            if token == "-h" || token == "--help" {
                parsed.help = true;
                index += 1;
                continue;
            }
            if token == "--json" {
                parsed.json = true;
                index += 1;
                continue;
            }
            if let Some(advance) = parse_value_flag(&raw, index, token, &mut parsed)? {
                index += advance;
                continue;
            }
            if token.starts_with('-') {
                return Err(CliError::Usage(format!("unknown option `{token}`")));
            }
        }
        parsed.positionals.push(token.to_owned());
        index += 1;
    }

    if parsed.help {
        return Ok(ParseOutcome::Help);
    }

    let action = build_action(&parsed)?;
    Ok(ParseOutcome::Run(Cli {
        data_dir: parsed.data_dir,
        json: parsed.json,
        action,
    }))
}

fn os_to_str(value: &OsString) -> Result<&str, CliError> {
    value
        .to_str()
        .ok_or_else(|| CliError::Usage("command argument is not valid UTF-8".to_owned()))
}

fn next_flag_value<'a>(
    raw: &'a [OsString],
    index: usize,
    flag: &str,
) -> Result<(usize, &'a OsString), CliError> {
    let value = raw
        .get(index + 1)
        .ok_or_else(|| CliError::Usage(format!("option `{flag}` requires a value")))?;
    Ok((2, value))
}

fn parse_value_flag(
    raw: &[OsString],
    index: usize,
    token: &str,
    parsed: &mut ParsedFlags,
) -> Result<Option<usize>, CliError> {
    if token == "--data-dir" {
        let (advance, value) = next_flag_value(raw, index, "--data-dir")?;
        set_data_dir(parsed, PathBuf::from(value))?;
        return Ok(Some(advance));
    }
    if let Some(value) = token.strip_prefix("--data-dir=") {
        set_data_dir(parsed, PathBuf::from(value))?;
        return Ok(Some(1));
    }

    if token == "--repo" {
        let (advance, value) = next_flag_value(raw, index, "--repo")?;
        parsed.repo = Some(non_empty_flag("--repo", os_to_str(value)?)?.to_owned());
        return Ok(Some(advance));
    }
    if let Some(value) = token.strip_prefix("--repo=") {
        parsed.repo = Some(non_empty_flag("--repo", value)?.to_owned());
        return Ok(Some(1));
    }

    if token == "--issue" {
        let (advance, value) = next_flag_value(raw, index, "--issue")?;
        parsed.issue = Some(parse_issue_number(os_to_str(value)?)?);
        return Ok(Some(advance));
    }
    if let Some(value) = token.strip_prefix("--issue=") {
        parsed.issue = Some(parse_issue_number(value)?);
        return Ok(Some(1));
    }

    if token == "--cli" {
        let (advance, value) = next_flag_value(raw, index, "--cli")?;
        parsed.agent_cli = Some(parse_agent_cli(os_to_str(value)?)?);
        return Ok(Some(advance));
    }
    if let Some(value) = token.strip_prefix("--cli=") {
        parsed.agent_cli = Some(parse_agent_cli(value)?);
        return Ok(Some(1));
    }

    if token == "--mode" {
        let (advance, value) = next_flag_value(raw, index, "--mode")?;
        parsed.permission_mode = Some(parse_permission_mode(os_to_str(value)?)?);
        return Ok(Some(advance));
    }
    if let Some(value) = token.strip_prefix("--mode=") {
        parsed.permission_mode = Some(parse_permission_mode(value)?);
        return Ok(Some(1));
    }

    Ok(None)
}

fn set_data_dir(parsed: &mut ParsedFlags, dir: PathBuf) -> Result<(), CliError> {
    if dir.as_os_str().is_empty() {
        return Err(CliError::Usage("`--data-dir` cannot be empty".to_owned()));
    }
    parsed.data_dir = Some(dir);
    Ok(())
}

fn non_empty_flag<'a>(flag: &str, value: &'a str) -> Result<&'a str, CliError> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(CliError::Usage(format!("`{flag}` cannot be empty")));
    }
    Ok(trimmed)
}

fn parse_issue_number(raw: &str) -> Result<u64, CliError> {
    let parsed: u64 = raw
        .parse()
        .map_err(|_| CliError::Usage(format!("invalid issue number `{raw}`")))?;
    if parsed == 0 {
        return Err(CliError::Usage("issue numbers start at 1".to_owned()));
    }
    Ok(parsed)
}

fn parse_agent_cli(raw: &str) -> Result<AgentCli, CliError> {
    match raw.to_ascii_lowercase().as_str() {
        "claude" | "claude-code" | "claude_code" => Ok(AgentCli::ClaudeCode),
        "codex" => Ok(AgentCli::Codex),
        "gemini" => Ok(AgentCli::Gemini),
        _ => Err(CliError::Usage(format!(
            "unknown agent CLI `{raw}`; expected one of: claude, codex, gemini"
        ))),
    }
}

fn parse_permission_mode(raw: &str) -> Result<PermissionMode, CliError> {
    match raw.to_ascii_lowercase().as_str() {
        "supervised" => Ok(PermissionMode::Supervised),
        "auto-accept-edits" | "auto_accept_edits" => Ok(PermissionMode::AutoAcceptEdits),
        "auto" => Ok(PermissionMode::Auto),
        "full-access" | "full_access" => Ok(PermissionMode::FullAccess),
        _ => Err(CliError::Usage(format!(
            "unknown permission mode `{raw}`; expected one of: supervised, auto-accept-edits, auto, full-access"
        ))),
    }
}

fn build_action(parsed: &ParsedFlags) -> Result<Action, CliError> {
    match parsed.positionals.first().map(String::as_str) {
        Some("status") => {
            if parsed.positionals.len() > 1 {
                return Err(CliError::Usage(format!(
                    "unexpected argument `{}` after `status`",
                    parsed.positionals[1]
                )));
            }
            if parsed.repo.is_some()
                || parsed.issue.is_some()
                || parsed.agent_cli.is_some()
                || parsed.permission_mode.is_some()
            {
                return Err(CliError::Usage(
                    "`status` only accepts global flags (`--data-dir`, `--json`)".to_owned(),
                ));
            }
            Ok(Action::Status)
        }
        Some("queue") => build_queue_action(parsed),
        Some(other) => Err(CliError::Usage(format!("unknown command `{other}`"))),
        None => Err(CliError::Usage("missing command".to_owned())),
    }
}

fn build_queue_action(parsed: &ParsedFlags) -> Result<Action, CliError> {
    match parsed.positionals.get(1).map(String::as_str) {
        Some("list") => {
            if parsed.positionals.len() > 2 {
                return Err(CliError::Usage(format!(
                    "unexpected argument `{}` after `queue list`",
                    parsed.positionals[2]
                )));
            }
            if parsed.issue.is_some()
                || parsed.agent_cli.is_some()
                || parsed.permission_mode.is_some()
            {
                return Err(CliError::Usage(
                    "`queue list` does not accept `--issue`, `--cli` or `--mode`".to_owned(),
                ));
            }
            Ok(Action::Queue(QueueAction::List {
                repo: parsed.repo.clone(),
            }))
        }
        Some("add") => {
            if parsed.positionals.len() > 3 {
                return Err(CliError::Usage(format!(
                    "unexpected extra argument `{}` (quote multi-word prompts)",
                    parsed.positionals[3]
                )));
            }
            let prompt = parsed.positionals.get(2).cloned();
            let task = match (prompt, parsed.issue) {
                (Some(_), Some(_)) => {
                    return Err(CliError::Usage(
                        "pass either a prompt or `--issue <number>`, not both".to_owned(),
                    ));
                }
                (None, None) => {
                    return Err(CliError::Usage(
                        "`queue add` requires a `<prompt>` or `--issue <number>`".to_owned(),
                    ));
                }
                (Some(text), None) => {
                    if text.trim().is_empty() {
                        return Err(CliError::Usage("prompt text cannot be empty".to_owned()));
                    }
                    QueueTask::Prompt { text }
                }
                (None, Some(number)) => QueueTask::Issue { number },
            };
            Ok(Action::Queue(QueueAction::Add {
                repo: parsed.repo.clone(),
                task,
                agent_cli: parsed.agent_cli,
                permission_mode: parsed.permission_mode,
            }))
        }
        Some(other) => Err(CliError::Usage(format!("unknown queue command `{other}`"))),
        None => Err(CliError::Usage(
            "missing queue subcommand (`list` or `add`)".to_owned(),
        )),
    }
}

/// Resolves the `leiser-harness` service data directory.
///
/// Order: `explicit` (`--data-dir`), `LEISER_DATA_DIR`, then the platform
/// application data directory for `ai.leiser.harness`.
///
/// # Errors
///
/// Returns [`CliError::MissingDataDir`] if no override is set and the required
/// platform environment variable is missing or empty, or [`CliError::Usage`] if
/// an explicit override is empty.
pub fn resolve_data_dir(explicit: Option<&Path>) -> Result<PathBuf, CliError> {
    resolve_data_dir_with(explicit, |name| std::env::var_os(name))
}

fn resolve_data_dir_with(
    explicit: Option<&Path>,
    env_var: impl Fn(&str) -> Option<OsString>,
) -> Result<PathBuf, CliError> {
    if let Some(dir) = explicit {
        if dir.as_os_str().is_empty() {
            return Err(CliError::Usage("`--data-dir` cannot be empty".to_owned()));
        }
        return Ok(dir.to_path_buf());
    }
    if let Some(dir) = non_empty_env(&env_var, ENV_DATA_DIR) {
        return Ok(PathBuf::from(dir));
    }
    platform_app_data_dir(&env_var)
}

fn non_empty_env(env_var: &impl Fn(&str) -> Option<OsString>, key: &str) -> Option<OsString> {
    env_var(key).filter(|value| !value.is_empty())
}

#[cfg(target_os = "windows")]
fn platform_app_data_dir(env_var: &impl Fn(&str) -> Option<OsString>) -> Result<PathBuf, CliError> {
    let base = non_empty_env(env_var, "APPDATA").ok_or(CliError::MissingDataDir("APPDATA"))?;
    Ok(PathBuf::from(base).join(APP_IDENTIFIER))
}

#[cfg(target_os = "macos")]
fn platform_app_data_dir(env_var: &impl Fn(&str) -> Option<OsString>) -> Result<PathBuf, CliError> {
    let home = non_empty_env(env_var, "HOME").ok_or(CliError::MissingDataDir("HOME"))?;
    Ok(PathBuf::from(home)
        .join("Library")
        .join("Application Support")
        .join(APP_IDENTIFIER))
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
fn platform_app_data_dir(env_var: &impl Fn(&str) -> Option<OsString>) -> Result<PathBuf, CliError> {
    if let Some(xdg) = non_empty_env(env_var, "XDG_DATA_HOME") {
        return Ok(PathBuf::from(xdg).join(APP_IDENTIFIER));
    }
    let home =
        non_empty_env(env_var, "HOME").ok_or(CliError::MissingDataDir("XDG_DATA_HOME or HOME"))?;
    Ok(PathBuf::from(home)
        .join(".local")
        .join("share")
        .join(APP_IDENTIFIER))
}

/// Resolves a repository from an explicit `--repo` selector (`id`, path or
/// unique name), or infers it from `cwd`.
///
/// # Errors
///
/// Returns [`CliError::UnknownRepository`], [`CliError::AmbiguousRepository`]
/// or [`CliError::RepositoryRequired`] if the selector does not uniquely pick
/// one registered repository.
pub fn resolve_repository<'a>(
    repositories: &'a [RepositoryOverview],
    selector: Option<&str>,
    cwd: &Path,
) -> Result<&'a RepositoryOverview, CliError> {
    if let Some(selector) = selector {
        return resolve_explicit_repository(repositories, selector, cwd);
    }

    let normalized_cwd = normalize_path(cwd);
    let mut best: Option<(&RepositoryOverview, usize)> = None;
    let mut tied = false;

    for repo in repositories {
        let normalized_repo = normalize_path(&repo.path);
        if normalized_cwd.starts_with(&normalized_repo) {
            let depth = normalized_repo.as_os_str().len();
            match best {
                Some((_, best_depth)) if depth > best_depth => {
                    best = Some((repo, depth));
                    tied = false;
                }
                Some((_, best_depth)) if depth == best_depth => tied = true,
                None => best = Some((repo, depth)),
                Some(_) => {}
            }
        }
    }

    if !tied && let Some((repo, _)) = best {
        return Ok(repo);
    }
    if repositories.len() == 1 {
        return Ok(&repositories[0]);
    }
    Err(CliError::RepositoryRequired {
        available: format_repository_choices(repositories),
    })
}

fn resolve_explicit_repository<'a>(
    repositories: &'a [RepositoryOverview],
    selector: &str,
    cwd: &Path,
) -> Result<&'a RepositoryOverview, CliError> {
    if !selector.is_empty()
        && selector.bytes().all(|byte| byte.is_ascii_digit())
        && let Some(repo) = repositories
            .iter()
            .find(|repo| repo.repository.to_string() == selector)
    {
        return Ok(repo);
    }

    let candidate_path = {
        let path = Path::new(selector);
        if path.is_relative() {
            cwd.join(path)
        } else {
            path.to_path_buf()
        }
    };
    let normalized_candidate = normalize_path(&candidate_path);
    if let Some(repo) = repositories
        .iter()
        .find(|repo| normalize_path(&repo.path) == normalized_candidate)
    {
        return Ok(repo);
    }

    let name_matches: Vec<&RepositoryOverview> = repositories
        .iter()
        .filter(|repo| repo.name == selector)
        .collect();
    match name_matches.as_slice() {
        [single] => Ok(*single),
        [] => Err(CliError::UnknownRepository {
            selector: selector.to_owned(),
            available: format_repository_choices(repositories),
        }),
        many => Err(CliError::AmbiguousRepository {
            selector: selector.to_owned(),
            matches: format_repository_list(many),
        }),
    }
}

fn normalize_path(path: &Path) -> PathBuf {
    let resolved = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    strip_windows_verbatim_prefix(resolved)
}

#[cfg(target_os = "windows")]
fn strip_windows_verbatim_prefix(path: PathBuf) -> PathBuf {
    if let Some(stripped) = path.to_str().and_then(|s| s.strip_prefix(r"\\?\")) {
        PathBuf::from(stripped)
    } else {
        path
    }
}

#[cfg(not(target_os = "windows"))]
fn strip_windows_verbatim_prefix(path: PathBuf) -> PathBuf {
    path
}

fn format_repository_choices(repositories: &[RepositoryOverview]) -> String {
    if repositories.is_empty() {
        return String::from("(none registered)");
    }
    let refs: Vec<&RepositoryOverview> = repositories.iter().collect();
    format_repository_list(&refs)
}

fn format_repository_list(repositories: &[&RepositoryOverview]) -> String {
    repositories
        .iter()
        .map(|repo| {
            format!(
                "{} ({}, {})",
                repo.repository,
                repo.name,
                repo.path.display()
            )
        })
        .collect::<Vec<_>>()
        .join("; ")
}

/// Connects to the running `leiser-harness-service` and executes `cli`, writing
/// formatted text or JSON to `out`.
///
/// # Errors
///
/// Returns [`CliError`] if the data directory cannot be resolved, the service
/// is unreachable, repository selection fails or a service command errors.
pub async fn run(cli: &Cli, cwd: &Path, out: &mut impl Write) -> Result<(), CliError> {
    let data_dir = resolve_data_dir(cli.data_dir.as_deref())?;
    let (client, _notifications) = match LocalClient::connect(&data_dir).await {
        Ok(connected) => connected,
        Err(ConnectError::NotRunning { data_dir }) => {
            return Err(CliError::NotRunning {
                endpoint_file: data_dir.join(ENDPOINT_FILE),
                data_dir,
            });
        }
        Err(source) => return Err(CliError::Connect { data_dir, source }),
    };

    match &cli.action {
        Action::Status => execute_status(&client, &data_dir, cli.json, out).await,
        Action::Queue(QueueAction::List { repo }) => {
            execute_queue_list(&client, repo.as_deref(), cwd, cli.json, out).await
        }
        Action::Queue(QueueAction::Add {
            repo,
            task,
            agent_cli,
            permission_mode,
        }) => {
            execute_queue_add(
                &client,
                &QueueAddRequest {
                    selector: repo.as_deref(),
                    cwd,
                    task,
                    agent_cli: *agent_cli,
                    permission_mode: *permission_mode,
                    json: cli.json,
                },
                out,
            )
            .await
        }
    }
}

async fn list_repositories(client: &LocalClient) -> Result<Vec<RepositoryOverview>, CliError> {
    match client.execute(Command::ListRepositories).await? {
        Response::Repositories { repositories } => Ok(repositories),
        _ => Err(CliError::UnexpectedResponse {
            expected: "Repositories",
        }),
    }
}

async fn execute_status(
    client: &LocalClient,
    data_dir: &Path,
    json: bool,
    out: &mut impl Write,
) -> Result<(), CliError> {
    let Response::ServiceStatus { update_waiting, .. } =
        client.execute(Command::ReadServiceStatus).await?
    else {
        return Err(CliError::UnexpectedResponse {
            expected: "ServiceStatus",
        });
    };
    let repositories = list_repositories(client).await?;
    let Response::Chats { chats } = client.execute(Command::ListChats).await? else {
        return Err(CliError::UnexpectedResponse { expected: "Chats" });
    };

    let output = StatusOutput {
        data_dir: data_dir.to_path_buf(),
        service_version: client.service_version().to_owned(),
        service_pid: client.service_pid(),
        update_waiting,
        repositories,
        chats,
    };

    if json {
        write_json(out, &output)
    } else {
        print_status(out, &output)
    }
}

async fn execute_queue_list(
    client: &LocalClient,
    selector: Option<&str>,
    cwd: &Path,
    json: bool,
    out: &mut impl Write,
) -> Result<(), CliError> {
    let repositories = list_repositories(client).await?;
    let targets: Vec<RepositoryOverview> = if let Some(selector) = selector {
        vec![resolve_repository(&repositories, Some(selector), cwd)?.clone()]
    } else {
        repositories
    };

    let mut entries = Vec::with_capacity(targets.len());
    for repository in targets {
        let Response::Queue { answer } = client
            .execute(Command::Queue {
                command: QueueCommand::Read {
                    repository: repository.repository,
                },
            })
            .await?
        else {
            return Err(CliError::UnexpectedResponse { expected: "Queue" });
        };
        let (QueueAnswer::Queue { queue } | QueueAnswer::Enqueued { queue, .. }) = answer;
        entries.push(QueueListEntry { repository, queue });
    }

    let output = QueueListOutput { queues: entries };
    if json {
        write_json(out, &output)
    } else {
        print_queue_list(out, &output)
    }
}

#[derive(Debug)]
struct QueueAddRequest<'a> {
    selector: Option<&'a str>,
    cwd: &'a Path,
    task: &'a QueueTask,
    agent_cli: Option<AgentCli>,
    permission_mode: Option<PermissionMode>,
    json: bool,
}

async fn execute_queue_add(
    client: &LocalClient,
    request: &QueueAddRequest<'_>,
    out: &mut impl Write,
) -> Result<(), CliError> {
    let repositories = list_repositories(client).await?;
    let repository = resolve_repository(&repositories, request.selector, request.cwd)?.clone();
    let Response::Queue {
        answer: QueueAnswer::Enqueued { item, queue },
    } = client
        .execute(Command::Queue {
            command: QueueCommand::Enqueue {
                repository: repository.repository,
                task: request.task.clone(),
                agent_cli: request.agent_cli,
                permission_mode: request.permission_mode,
            },
        })
        .await?
    else {
        return Err(CliError::UnexpectedResponse {
            expected: "QueueAnswer::Enqueued",
        });
    };

    let output = QueueAddOutput {
        repository,
        item,
        task: request.task.clone(),
        queue,
    };
    if request.json {
        write_json(out, &output)
    } else {
        print_queue_add(out, &output)
    }
}

fn write_json(out: &mut impl Write, value: &impl Serialize) -> Result<(), CliError> {
    serde_json::to_writer_pretty(&mut *out, value)?;
    writeln!(out)?;
    Ok(())
}

fn print_status(out: &mut impl Write, status: &StatusOutput) -> Result<(), CliError> {
    writeln!(
        out,
        "leiser-harness-service v{} (pid {}, data dir: {}){}",
        status.service_version,
        status.service_pid,
        status.data_dir.display(),
        if status.update_waiting {
            " [update waiting]"
        } else {
            ""
        }
    )?;
    if status.repositories.is_empty() {
        writeln!(out, "Repositories: none")?;
        return Ok(());
    }
    writeln!(out, "Repositories ({}):", status.repositories.len())?;
    for repo in &status.repositories {
        let count = status
            .chats
            .iter()
            .filter(|chat| chat.repository == repo.repository)
            .count();
        writeln!(
            out,
            "  [{}] {} ({}) — {} chat(s)",
            repo.repository,
            repo.name,
            repo.path.display(),
            count
        )?;
    }
    if !status.chats.is_empty() {
        writeln!(out, "Chats ({}):", status.chats.len())?;
        for chat in &status.chats {
            let title = chat.title.as_deref().unwrap_or("(untitled)");
            writeln!(
                out,
                "  #{} [repo {}] {:?} — {}",
                chat.chat, chat.repository, chat.status, title
            )?;
        }
    }
    Ok(())
}

fn print_queue_list(out: &mut impl Write, list: &QueueListOutput) -> Result<(), CliError> {
    if list.queues.is_empty() {
        writeln!(out, "No repositories registered.")?;
        return Ok(());
    }
    for entry in &list.queues {
        writeln!(
            out,
            "Repository [{}] {} ({}) — queue {} (concurrency {})",
            entry.repository.repository,
            entry.repository.name,
            entry.repository.path.display(),
            format_queue_status(&entry.queue.status),
            entry.queue.settings.concurrency
        )?;
        if entry.queue.items.is_empty() {
            writeln!(out, "  (queue empty)")?;
            continue;
        }
        for item in &entry.queue.items {
            writeln!(out, "  {}", format_queue_item(item))?;
        }
    }
    Ok(())
}

fn print_queue_add(out: &mut impl Write, added: &QueueAddOutput) -> Result<(), CliError> {
    writeln!(
        out,
        "Enqueued task {} in repository [{}] {}: {}",
        added.item,
        added.repository.repository,
        added.repository.name,
        added.task.title()
    )?;
    Ok(())
}

fn format_queue_status(status: &QueueStatus) -> String {
    match status {
        QueueStatus::Idle => String::from("idle"),
        QueueStatus::Running => String::from("running"),
        QueueStatus::Paused => String::from("paused"),
        QueueStatus::WaitingForUsageLimits { resets_in_seconds } => match resets_in_seconds {
            Some(secs) => format!("waiting for usage limits (resets in {secs}s)"),
            None => String::from("waiting for usage limits"),
        },
        QueueStatus::WaitingForUpdate => String::from("waiting for update"),
    }
}

fn format_queue_item(item: &QueueItem) -> String {
    let mut line = format!(
        "#{} [{}] {}",
        item.item,
        format_item_state(&item.state),
        item.title
    );
    if let Some(agent_cli) = item.agent_cli {
        let _ = write!(line, " ({agent_cli})");
    }
    if let Some(mode) = item.permission_mode {
        let _ = write!(line, " [{mode}]");
    }
    line
}

fn format_item_state(state: &QueueItemState) -> String {
    match state {
        QueueItemState::Queued => String::from("queued"),
        QueueItemState::Starting => String::from("starting"),
        QueueItemState::Running {
            chat,
            waiting_for_you,
        } => {
            if *waiting_for_you {
                format!("running chat #{chat}, waiting for approval")
            } else {
                format!("running chat #{chat}")
            }
        }
        QueueItemState::NeedsAttention { chat, reason } => match chat {
            Some(chat) => format!("needs attention chat #{chat}: {reason:?}"),
            None => format!("needs attention: {reason:?}"),
        },
        QueueItemState::Completed { chat } => format!("completed chat #{chat}"),
        QueueItemState::PullRequestReady { chat, number, .. } => {
            format!("PR #{number} ready (chat #{chat})")
        }
        QueueItemState::Cancelled { chat } => match chat {
            Some(chat) => format!("cancelled chat #{chat}"),
            None => String::from("cancelled"),
        },
    }
}

#[cfg(test)]
mod tests {
    use std::error::Error;
    use std::ffi::OsString;
    use std::path::{Path, PathBuf};

    use leiser_service::{AgentCli, PermissionMode, QueueTask, RepositoryOverview};

    use super::{
        Action, CliError, ParseOutcome, QueueAction, parse_args, resolve_data_dir_with,
        resolve_repository,
    };

    type TestResult<T = ()> = Result<T, Box<dyn Error>>;

    fn repo(id: u64, name: &str, path: &str) -> Result<RepositoryOverview, serde_json::Error> {
        Ok(RepositoryOverview {
            repository: serde_json::from_value(serde_json::Value::from(id))?,
            name: name.to_owned(),
            path: PathBuf::from(path),
        })
    }

    #[test]
    fn parses_status_and_global_flags() -> TestResult {
        let ParseOutcome::Run(cli) = parse_args(["--json", "--data-dir", "/tmp/lh", "status"])?
        else {
            return Err("expected Run".into());
        };
        assert!(cli.json);
        assert_eq!(cli.data_dir, Some(PathBuf::from("/tmp/lh")));
        assert_eq!(cli.action, Action::Status);

        let ParseOutcome::Help = parse_args(["queue", "--help"])? else {
            return Err("expected Help".into());
        };
        Ok(())
    }

    #[test]
    fn parses_queue_list_and_add_variants() -> TestResult {
        let ParseOutcome::Run(list_cli) = parse_args(["queue", "list", "--repo=nova", "--json"])?
        else {
            return Err("expected Run".into());
        };
        assert!(list_cli.json);
        assert_eq!(
            list_cli.action,
            Action::Queue(QueueAction::List {
                repo: Some(String::from("nova")),
            })
        );

        let ParseOutcome::Run(prompt_cli) = parse_args([
            "queue",
            "add",
            "Fix Windows tests",
            "--repo",
            "1",
            "--cli",
            "claude",
            "--mode",
            "auto-accept-edits",
        ])?
        else {
            return Err("expected Run".into());
        };
        assert_eq!(
            prompt_cli.action,
            Action::Queue(QueueAction::Add {
                repo: Some(String::from("1")),
                task: QueueTask::Prompt {
                    text: String::from("Fix Windows tests"),
                },
                agent_cli: Some(AgentCli::ClaudeCode),
                permission_mode: Some(PermissionMode::AutoAcceptEdits),
            })
        );

        let ParseOutcome::Run(issue_cli) = parse_args([
            "queue",
            "add",
            "--issue",
            "140",
            "--cli=codex",
            "--mode=full-access",
        ])?
        else {
            return Err("expected Run".into());
        };
        assert_eq!(
            issue_cli.action,
            Action::Queue(QueueAction::Add {
                repo: None,
                task: QueueTask::Issue { number: 140 },
                agent_cli: Some(AgentCli::Codex),
                permission_mode: Some(PermissionMode::FullAccess),
            })
        );
        Ok(())
    }

    #[test]
    fn rejects_invalid_arguments_and_combinations() {
        for case in [
            vec!["queue", "add"],
            vec!["queue", "add", "Prompt", "--issue", "42"],
            vec!["queue", "add", "   "],
            vec!["queue", "add", "--issue", "0"],
            vec!["queue", "add", "Prompt", "--cli", "unknown"],
            vec!["queue", "add", "Prompt", "--mode", "yolo"],
            vec!["status", "--repo", "nova"],
            vec!["queue", "list", "--issue", "1"],
            vec!["unknown"],
        ] {
            let outcome = parse_args(case);
            assert!(matches!(outcome, Err(CliError::Usage(_))));
        }
    }

    #[test]
    fn resolves_data_dir_with_explicit_and_env_precedence() -> TestResult {
        let explicit = resolve_data_dir_with(Some(Path::new("/custom/dir")), |_| {
            Some(OsString::from("/from/env"))
        })?;
        assert_eq!(explicit, PathBuf::from("/custom/dir"));

        let from_env = resolve_data_dir_with(None, |key| {
            (key == "LEISER_DATA_DIR").then(|| OsString::from("/env/data"))
        })?;
        assert_eq!(from_env, PathBuf::from("/env/data"));
        Ok(())
    }

    #[test]
    fn resolves_repository_by_id_path_name_cwd_and_rejects_ambiguity() -> TestResult {
        let repos = vec![
            repo(1, "nova", "/work/team-a/nova")?,
            repo(2, "nova", "/work/team-b/nova")?,
            repo(3, "harness", "/work/harness")?,
        ];

        assert_eq!(
            resolve_repository(&repos, Some("2"), Path::new("/"))?.repository,
            repos[1].repository
        );
        assert_eq!(
            resolve_repository(&repos, Some("harness"), Path::new("/"))?.repository,
            repos[2].repository
        );
        assert_eq!(
            resolve_repository(&repos, Some("/work/team-a/nova"), Path::new("/"))?.repository,
            repos[0].repository
        );
        assert_eq!(
            resolve_repository(&repos, None, Path::new("/work/team-b/nova/crates/core"))?
                .repository,
            repos[1].repository
        );

        let ambiguous = resolve_repository(&repos, Some("nova"), Path::new("/"));
        assert!(matches!(
            ambiguous,
            Err(CliError::AmbiguousRepository { .. })
        ));

        let required = resolve_repository(&repos, None, Path::new("/unrelated"));
        assert!(matches!(required, Err(CliError::RepositoryRequired { .. })));
        Ok(())
    }
}
