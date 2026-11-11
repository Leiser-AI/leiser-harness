//! Hard blocks (#38): in every permission mode, Full access included, the
//! harness service refuses a push to `main` or `development`, a force
//! push, a push as the wrong host account and a push of a commit with AI
//! attribution. Nothing reaches the git host then. The repository's own
//! agent CLI settings stay as they are.

use std::error::Error;
use std::path::{Path, PathBuf};
use std::time::Duration;

use leiser_service::{
    AgentEvent, Branch, ChatId, ChatSettings, Checkout, Command, CommandError, CommitAuthor, Event,
    HostAccountId, LoginOutcome, PermissionMode, RepositoryId, Response, ServiceHandle,
};
use leiser_testkit::{DEFAULT_BRANCH, Script, Step, TestRepo, TestService, collect_turn};
use tokio::sync::broadcast;

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

const FAKE_AGENT_CLI: &str = env!("CARGO_BIN_EXE_fake-agent-cli");
const GIT_HOST: &str = "git.example.invalid";
const NAME: &str = "Work";
const LOGIN: &str = "work-account";

/// The first turn writes a file and commits it with `message`.
fn committing(message: &str) -> Script {
    Script::new().turn([
        Step::WriteFile {
            path: PathBuf::from("change.txt"),
            contents: "change\n".to_owned(),
        },
        Step::Commit {
            message: message.to_owned(),
        },
    ])
}

async fn add_host_account(service: &ServiceHandle) -> TestResult<HostAccountId> {
    match service
        .execute(Command::AddHostAccount {
            name: NAME.to_owned(),
            git_host: GIT_HOST.to_owned(),
            login: LOGIN.to_owned(),
            author: CommitAuthor {
                name: "Work Author".to_owned(),
                email: "work@example.invalid".to_owned(),
            },
        })
        .await?
    {
        Response::HostAccountAdded { host_account } => Ok(host_account),
        response => Err(format!("expected HostAccountAdded, got {response:?}").into()),
    }
}

async fn log_in(service: &ServiceHandle, host_account: HostAccountId) -> TestResult {
    let mut events = service.subscribe();
    service
        .execute(Command::LogInHostAccount { host_account })
        .await?;
    let outcome = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match events.recv().await {
                Ok(Event::HostAccountLoginFinished {
                    host_account: finished,
                    outcome,
                }) if finished == host_account => return Ok(outcome),
                Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(error) => return Err(error),
            }
        }
    })
    .await??;
    assert_eq!(outcome, LoginOutcome::SignedIn);
    Ok(())
}

async fn add_repository(service: &ServiceHandle, repo: &TestRepo) -> TestResult<RepositoryId> {
    match service
        .execute(Command::AddRepository {
            path: repo.path().to_path_buf(),
        })
        .await?
    {
        Response::RepositoryAdded { repository } => Ok(repository),
        response => Err(format!("expected RepositoryAdded, got {response:?}").into()),
    }
}

async fn start_chat(
    service: &ServiceHandle,
    repository: RepositoryId,
    checkout: Checkout,
) -> TestResult<ChatId> {
    match service
        .execute(Command::StartChat {
            repository,
            checkout,
            settings: ChatSettings::default(),
        })
        .await?
    {
        Response::ChatStarted { chat } => Ok(chat),
        response => Err(format!("expected ChatStarted, got {response:?}").into()),
    }
}

/// Sends a message; the agent writes a file and commits it.
async fn commit_in_turn(service: &ServiceHandle, chat: ChatId) -> TestResult {
    let mut events = service.subscribe();
    service
        .execute(Command::SendMessage {
            chat,
            text: "commit it".to_owned(),
        })
        .await?;
    let answers: Vec<String> = collect_turn(&mut events, chat)
        .await?
        .into_iter()
        .filter_map(|event| match event {
            AgentEvent::Message { text } => Some(text),
            _ => None,
        })
        .collect();
    assert_eq!(answers.last().map(String::as_str), Some("committed"));
    Ok(())
}

async fn working_directory(service: &ServiceHandle, chat: ChatId) -> TestResult<PathBuf> {
    match service.execute(Command::ListChats).await? {
        Response::Chats { chats } => chats
            .into_iter()
            .find(|overview| overview.chat == chat)
            .and_then(|overview| overview.worktree)
            .ok_or_else(|| format!("chat {chat:?} has no worktree").into()),
        response => Err(format!("expected Chats, got {response:?}").into()),
    }
}

async fn branch(service: &ServiceHandle, chat: ChatId) -> TestResult<String> {
    match service.execute(Command::ReadChat { chat }).await? {
        Response::Chat {
            branch: Branch::Named { name },
            ..
        } => Ok(name),
        response => Err(format!("expected a chat on a branch, got {response:?}").into()),
    }
}

/// Runs git in `dir` as the developer would, independent of the machine's
/// git config.
fn git(dir: &Path, args: &[&str]) -> TestResult<String> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "user.name=leiser test",
            "-c",
            "user.email=test@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "core.hooksPath=/dev/null",
        ])
        .args(args)
        .output()?;
    if !output.status.success() {
        return Err(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        )
        .into());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// A repository bound to a signed-in host account, with a chat in Full
/// access whose agent has committed once.
struct Bound {
    harness: TestService,
    service: ServiceHandle,
    repo: TestRepo,
    chat: ChatId,
}

struct Setup<'a> {
    message: &'a str,
    checkout: Checkout,
    /// The branch the main checkout is on when the chat starts.
    main_checkout_branch: Option<&'a str>,
    sign_in_as: Option<&'a str>,
}

impl Default for Setup<'_> {
    fn default() -> Self {
        Self {
            message: "Add the change",
            checkout: Checkout::NewWorktree,
            main_checkout_branch: None,
            sign_in_as: None,
        }
    }
}

async fn bound(setup: Setup<'_>) -> TestResult<Bound> {
    let harness = TestService::start(Path::new(FAKE_AGENT_CLI), &committing(setup.message))?;
    let service = harness.connect();
    let repo = TestRepo::with_remote()?;
    if let Some(branch) = setup.main_checkout_branch {
        repo.git(&["checkout", "--quiet", "-b", branch])?;
    }
    let host_account = add_host_account(&service).await?;
    if let Some(login) = setup.sign_in_as {
        harness.git_host().next_login_signs_in_as(login);
    }
    log_in(&service, host_account).await?;
    std::fs::create_dir_all(repo.path().join(".leiser"))?;
    std::fs::write(
        repo.path().join(".leiser/config.toml"),
        format!("host_account = {NAME:?}\n"),
    )?;
    let repository = add_repository(&service, &repo).await?;
    let chat = start_chat(&service, repository, setup.checkout).await?;
    service
        .execute(Command::ChangePermissionMode {
            chat,
            permission_mode: PermissionMode::FullAccess,
        })
        .await?;
    commit_in_turn(&service, chat).await?;
    Ok(Bound {
        harness,
        service,
        repo,
        chat,
    })
}

async fn push(bound: &Bound) -> Result<Response, CommandError> {
    bound
        .service
        .execute(Command::Push { chat: bound.chat })
        .await
}

#[tokio::test]
async fn a_push_to_main_is_blocked_in_full_access() -> TestResult {
    let bound = bound(Setup {
        checkout: Checkout::Main,
        ..Setup::default()
    })
    .await?;
    let before = bound.repo.remote_branch(DEFAULT_BRANCH)?;

    let pushed = push(&bound).await;

    let expected = CommandError::ProtectedBranch {
        branch: DEFAULT_BRANCH.to_owned(),
    };
    assert_eq!(pushed, Err(expected.clone()));
    assert!(expected.to_string().contains("push blocked"), "{expected}");
    assert_eq!(bound.repo.remote_branch(DEFAULT_BRANCH)?, before);
    Ok(())
}

#[tokio::test]
async fn a_push_to_development_is_blocked_in_full_access() -> TestResult {
    let bound = bound(Setup {
        checkout: Checkout::Main,
        main_checkout_branch: Some("development"),
        ..Setup::default()
    })
    .await?;

    let pushed = push(&bound).await;

    assert_eq!(
        pushed,
        Err(CommandError::ProtectedBranch {
            branch: "development".to_owned(),
        })
    );
    assert_eq!(bound.repo.remote_branch("development")?, None);
    Ok(())
}

#[tokio::test]
async fn a_pull_request_from_main_is_blocked_before_anything_reaches_the_git_host() -> TestResult {
    let bound = bound(Setup {
        checkout: Checkout::Main,
        ..Setup::default()
    })
    .await?;
    let before = bound.repo.remote_branch(DEFAULT_BRANCH)?;

    let opened = bound
        .service
        .execute(Command::OpenPullRequest {
            chat: bound.chat,
            title: "Change".to_owned(),
            body: "A change.".to_owned(),
        })
        .await;

    assert!(
        matches!(opened, Err(CommandError::ProtectedBranch { .. })),
        "{opened:?}"
    );
    assert_eq!(bound.repo.remote_branch(DEFAULT_BRANCH)?, before);
    assert_eq!(bound.harness.git_host().pull_requests(), Vec::new());
    Ok(())
}

#[tokio::test]
async fn a_push_that_would_rewrite_the_remote_branch_is_blocked_as_a_force_push() -> TestResult {
    let bound = bound(Setup::default()).await?;
    let branch = branch(&bound.service, bound.chat).await?;
    push(&bound).await?;
    let pushed = bound.repo.remote_branch(&branch)?;
    let directory = working_directory(&bound.service, bound.chat).await?;
    // The agent rewrites the pushed commit.
    git(
        &directory,
        &["commit", "--quiet", "--amend", "--message", "Reworded"],
    )?;

    let again = push(&bound).await;

    let expected = CommandError::ForcePush {
        branch: branch.clone(),
    };
    assert_eq!(again, Err(expected.clone()));
    assert!(expected.to_string().contains("push blocked"), "{expected}");
    assert_eq!(bound.repo.remote_branch(&branch)?, pushed);
    Ok(())
}

#[tokio::test]
async fn a_push_onto_commits_someone_else_pushed_is_blocked_as_a_force_push() -> TestResult {
    let bound = bound(Setup::default()).await?;
    let branch = branch(&bound.service, bound.chat).await?;
    bound.repo.push_to_remote(&branch, "other.txt", "other\n")?;
    let theirs = bound.repo.remote_branch(&branch)?;

    let pushed = push(&bound).await;

    assert_eq!(
        pushed,
        Err(CommandError::ForcePush {
            branch: branch.clone(),
        })
    );
    assert_eq!(bound.repo.remote_branch(&branch)?, theirs);
    Ok(())
}

#[tokio::test]
async fn a_push_that_adds_to_the_remote_branch_is_no_force_push() -> TestResult {
    let bound = bound(Setup::default()).await?;
    let branch = branch(&bound.service, bound.chat).await?;
    push(&bound).await?;
    let directory = working_directory(&bound.service, bound.chat).await?;
    std::fs::write(directory.join("more.txt"), "more\n")?;
    git(&directory, &["add", "more.txt"])?;
    git(&directory, &["commit", "--quiet", "--message", "Add more"])?;

    let again = push(&bound).await?;

    assert_eq!(
        again,
        Response::Pushed {
            branch: branch.clone()
        }
    );
    assert_eq!(
        bound.repo.remote_branch(&branch)?,
        Some(git(&directory, &["rev-parse", "HEAD"])?)
    );
    Ok(())
}

#[tokio::test]
async fn a_push_as_the_wrong_host_account_is_blocked_in_full_access() -> TestResult {
    let bound = bound(Setup {
        sign_in_as: Some("private-account"),
        ..Setup::default()
    })
    .await?;
    let branch = branch(&bound.service, bound.chat).await?;

    let pushed = push(&bound).await;

    assert_eq!(
        pushed,
        Err(CommandError::HostAccountMismatch {
            name: NAME.to_owned(),
            git_host: GIT_HOST.to_owned(),
            expected: LOGIN.to_owned(),
            actual: "private-account".to_owned(),
        })
    );
    assert_eq!(bound.repo.remote_branch(&branch)?, None);
    Ok(())
}

/// Pushes a chat whose agent committed with `message` and returns the
/// answer and whether the branch reached `origin`.
async fn push_commit(message: &str) -> TestResult<(Result<Response, CommandError>, bool)> {
    let bound = bound(Setup {
        message,
        ..Setup::default()
    })
    .await?;
    let branch = branch(&bound.service, bound.chat).await?;
    let pushed = push(&bound).await;
    Ok((pushed, bound.repo.remote_branch(&branch)?.is_some()))
}

#[tokio::test]
async fn a_commit_with_an_ai_co_author_is_not_pushed_in_full_access() -> TestResult {
    for trailer in [
        "Co-Authored-By: Claude <noreply@anthropic.com>",
        "Co-authored-by: Claude Opus 4.5 <noreply@anthropic.com>",
        "Co-authored-by: Codex <noreply@openai.com>",
        "co-authored-by: codex <codex@users.noreply.github.com>",
    ] {
        let (pushed, reached) = push_commit(&format!("Add the change\n\n{trailer}")).await?;

        let Err(CommandError::AiAttribution { commit, line }) = &pushed else {
            return Err(format!("{trailer}: expected AiAttribution, got {pushed:?}").into());
        };
        assert!(!commit.is_empty(), "{trailer}");
        assert_eq!(line, trailer);
        let message = pushed.as_ref().err().map(ToString::to_string);
        assert!(
            message.is_some_and(|message| message.contains("push blocked")),
            "{trailer}"
        );
        assert!(!reached, "{trailer}: the branch reached origin");
    }
    Ok(())
}

#[tokio::test]
async fn a_commit_with_a_generated_with_line_is_not_pushed() -> TestResult {
    for line in [
        "🤖 Generated with [Claude Code](https://claude.com/claude-code)",
        "Generated with Codex",
    ] {
        let (pushed, reached) = push_commit(&format!("Add the change\n\n{line}")).await?;

        assert!(
            matches!(&pushed, Err(CommandError::AiAttribution { line: found, .. }) if found == line),
            "{line}: {pushed:?}"
        );
        assert!(!reached, "{line}: the branch reached origin");
    }
    Ok(())
}

#[tokio::test]
async fn a_commit_authored_as_an_ai_is_not_pushed() -> TestResult {
    let bound = bound(Setup::default()).await?;
    let branch = branch(&bound.service, bound.chat).await?;
    let directory = working_directory(&bound.service, bound.chat).await?;
    git(
        &directory,
        &[
            "commit",
            "--quiet",
            "--amend",
            "--no-edit",
            "--author",
            "Claude <noreply@anthropic.com>",
        ],
    )?;

    let pushed = push(&bound).await;

    assert!(
        matches!(&pushed, Err(CommandError::AiAttribution { line, .. }) if line.contains("noreply@anthropic.com")),
        "{pushed:?}"
    );
    assert_eq!(bound.repo.remote_branch(&branch)?, None);
    Ok(())
}

#[tokio::test]
async fn a_human_co_author_is_pushed() -> TestResult {
    let (pushed, reached) = push_commit(
        "Add the change\n\nCo-authored-by: Claude Dupont <claude.dupont@example.invalid>",
    )
    .await?;

    assert!(matches!(pushed, Ok(Response::Pushed { .. })), "{pushed:?}");
    assert!(reached);
    Ok(())
}

#[tokio::test]
async fn a_pull_request_with_an_ai_attributed_commit_is_not_opened() -> TestResult {
    let bound = bound(Setup {
        message: "Add the change\n\nCo-Authored-By: Claude <noreply@anthropic.com>",
        ..Setup::default()
    })
    .await?;
    let branch = branch(&bound.service, bound.chat).await?;

    let opened = bound
        .service
        .execute(Command::OpenPullRequest {
            chat: bound.chat,
            title: "Change".to_owned(),
            body: "A change.".to_owned(),
        })
        .await;

    assert!(
        matches!(opened, Err(CommandError::AiAttribution { .. })),
        "{opened:?}"
    );
    assert_eq!(bound.repo.remote_branch(&branch)?, None);
    assert_eq!(bound.harness.git_host().pull_requests(), Vec::new());
    Ok(())
}

#[tokio::test]
async fn the_repositorys_own_agent_cli_settings_are_left_as_they_are() -> TestResult {
    let claude = "{\"permissions\":{\"deny\":[\"Bash(rm -rf *)\"]}}\n";
    let codex = "approval_policy = \"untrusted\"\n";
    let harness = TestService::start(Path::new(FAKE_AGENT_CLI), &committing("Add the change"))?;
    let service = harness.connect();
    let repo = TestRepo::with_remote()?;
    std::fs::create_dir_all(repo.path().join(".claude"))?;
    std::fs::create_dir_all(repo.path().join(".codex"))?;
    std::fs::write(repo.path().join(".claude/settings.json"), claude)?;
    std::fs::write(repo.path().join(".codex/config.toml"), codex)?;
    git(repo.path(), &["add", ".claude", ".codex"])?;
    git(
        repo.path(),
        &["commit", "--quiet", "--message", "Add agent settings"],
    )?;
    git(repo.path(), &["push", "--quiet", "origin", DEFAULT_BRANCH])?;
    let repository = add_repository(&service, &repo).await?;
    let chat = start_chat(&service, repository, Checkout::NewWorktree).await?;
    service
        .execute(Command::ChangePermissionMode {
            chat,
            permission_mode: PermissionMode::FullAccess,
        })
        .await?;

    commit_in_turn(&service, chat).await?;

    let directory = working_directory(&service, chat).await?;
    for root in [repo.path(), directory.as_path()] {
        assert_eq!(
            std::fs::read_to_string(root.join(".claude/settings.json"))?,
            claude
        );
        assert_eq!(
            std::fs::read_to_string(root.join(".codex/config.toml"))?,
            codex
        );
        assert!(!root.join(".claude/settings.local.json").exists());
    }
    Ok(())
}
