//! Host accounts (#35): every repository is bound to one host account with
//! repository-local git settings only. Commits carry its commit author, the
//! top bar shows it, and a push that would sign in as another account is
//! blocked.

use std::error::Error;
use std::path::{Path, PathBuf};
use std::time::Duration;

use leiser_service::{
    AgentEvent, Branch, ChatId, Checkout, Command, CommandError, CommitAuthor, Event,
    HostAccountBinding, HostAccountId, HostAccountOverview, LoginOutcome, RepositoryId, Response,
    ServiceHandle,
};
use leiser_testkit::{
    FAKE_CREDENTIAL_HELPER, FAKE_GLOBAL_AUTHOR, Script, Step, TestRepo, TestService, collect_turn,
};
use tokio::sync::broadcast;

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

const FAKE_AGENT_CLI: &str = env!("CARGO_BIN_EXE_fake-agent-cli");
const GIT_HOST: &str = "git.example.invalid";
const NAME: &str = "Work";
const LOGIN: &str = "work-account";

fn author() -> CommitAuthor {
    CommitAuthor {
        name: "Work Author".to_owned(),
        email: "work@example.invalid".to_owned(),
    }
}

/// Every turn commits what is in the working directory.
fn committing() -> Script {
    (0..3).fold(Script::new(), |script, number| {
        script.turn([Step::Commit {
            message: format!("Change {number}"),
        }])
    })
}

fn start_service(script: &Script) -> TestResult<TestService> {
    Ok(TestService::start(Path::new(FAKE_AGENT_CLI), script)?)
}

fn configure(repo: &TestRepo, config: &str) -> TestResult {
    let dir = repo.path().join(".leiser");
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("config.toml"), config)?;
    Ok(())
}

fn bind(repo: &TestRepo, name: &str) -> TestResult {
    configure(repo, &format!("host_account = {name:?}\n"))
}

async fn add_host_account(service: &ServiceHandle) -> TestResult<HostAccountId> {
    match service
        .execute(Command::AddHostAccount {
            name: NAME.to_owned(),
            git_host: GIT_HOST.to_owned(),
            login: LOGIN.to_owned(),
            author: author(),
        })
        .await?
    {
        Response::HostAccountAdded { host_account } => Ok(host_account),
        response => Err(format!("expected HostAccountAdded, got {response:?}").into()),
    }
}

async fn host_accounts(service: &ServiceHandle) -> TestResult<Vec<HostAccountOverview>> {
    match service.execute(Command::ListHostAccounts).await? {
        Response::HostAccounts { host_accounts } => Ok(host_accounts),
        response => Err(format!("expected HostAccounts, got {response:?}").into()),
    }
}

/// Logs the host account in and waits until the login has ended.
async fn log_in(service: &ServiceHandle, host_account: HostAccountId) -> TestResult<LoginOutcome> {
    let mut events = service.subscribe();
    let response = service
        .execute(Command::LogInHostAccount { host_account })
        .await?;
    assert_eq!(response, Response::HostAccountLoginStarted);
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match events.recv().await {
                Ok(Event::HostAccountLoginFinished {
                    host_account: finished,
                    outcome,
                }) if finished == host_account => return Ok(outcome),
                Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(error) => return Err(error.into()),
            }
        }
    })
    .await?
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
) -> Result<ChatId, CommandError> {
    match service
        .execute(Command::StartChat {
            repository,
            checkout: Checkout::NewWorktree,
        })
        .await?
    {
        Response::ChatStarted { chat } => Ok(chat),
        response => Err(CommandError::AgentCli {
            reason: format!("expected ChatStarted, got {response:?}"),
        }),
    }
}

/// Sends a message and returns the agent's answers in the turn.
async fn commit_in_turn(service: &ServiceHandle, chat: ChatId) -> TestResult<Vec<String>> {
    let mut events = service.subscribe();
    service
        .execute(Command::SendMessage {
            chat,
            text: "commit it".to_owned(),
        })
        .await?;
    let turn = collect_turn(&mut events, chat).await?;
    Ok(turn
        .into_iter()
        .filter_map(|event| match event {
            AgentEvent::Message { text } => Some(text),
            _ => None,
        })
        .collect())
}

struct TopBar {
    branch: Branch,
    host_account: HostAccountBinding,
    commit_author: Option<CommitAuthor>,
}

async fn top_bar(service: &ServiceHandle, chat: ChatId) -> TestResult<TopBar> {
    match service.execute(Command::ReadChat { chat }).await? {
        Response::Chat {
            branch,
            host_account,
            commit_author,
            ..
        } => Ok(TopBar {
            branch,
            host_account,
            commit_author,
        }),
        response => Err(format!("expected Chat, got {response:?}").into()),
    }
}

async fn worktree(service: &ServiceHandle, chat: ChatId) -> TestResult<PathBuf> {
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
    match top_bar(service, chat).await?.branch {
        Branch::Named { name } => Ok(name),
        branch => Err(format!("expected a branch, got {branch:?}").into()),
    }
}

fn git(dir: &Path, args: &[&str]) -> TestResult<String> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()?;
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// A bound repository with a chat in its own worktree whose agent has
/// committed once.
struct Bound {
    harness: TestService,
    service: ServiceHandle,
    repo: TestRepo,
    host_account: HostAccountId,
    chat: ChatId,
}

async fn bound_with_commit() -> TestResult<Bound> {
    let harness = start_service(&committing())?;
    let service = harness.connect();
    let repo = TestRepo::with_remote()?;
    let host_account = add_host_account(&service).await?;
    bind(&repo, NAME)?;
    let repository = add_repository(&service, &repo).await?;
    let chat = start_chat(&service, repository).await?;
    assert_eq!(commit_in_turn(&service, chat).await?, ["committed"]);
    Ok(Bound {
        harness,
        service,
        repo,
        host_account,
        chat,
    })
}

#[tokio::test]
async fn commits_in_a_bound_repository_carry_its_commit_author_from_the_local_config_only()
-> TestResult {
    let global_before = git(Path::new("."), &["config", "--global", "--list"])?;
    let bound = bound_with_commit().await?;
    let worktree = worktree(&bound.service, bound.chat).await?;

    let commit_author = git(&worktree, &["log", "-1", "--format=%an <%ae>"])?;
    let committer = git(&worktree, &["log", "-1", "--format=%cn <%ce>"])?;

    assert_eq!(commit_author, author().to_string());
    assert_eq!(committer, author().to_string());
    let local = git(bound.repo.path(), &["config", "--local", "--list"])?;
    assert!(local.contains("user.email=work@example.invalid"), "{local}");
    let helpers = git(
        bound.repo.path(),
        &[
            "config",
            "--local",
            "--null",
            "--get-all",
            &format!("credential.https://{GIT_HOST}.helper"),
        ],
    )?;
    // The empty helper first drops the global config's helpers.
    assert_eq!(helpers, format!("\0{FAKE_CREDENTIAL_HELPER}\0"));
    let global_after = git(Path::new("."), &["config", "--global", "--list"])?;
    assert_eq!(global_after, global_before, "the global git config changed");
    assert!(!global_after.contains(GIT_HOST));
    Ok(())
}

#[tokio::test]
async fn commits_in_an_unbound_repository_keep_the_global_author() -> TestResult {
    let harness = start_service(&committing())?;
    let service = harness.connect();
    let repo = TestRepo::with_remote()?;
    add_host_account(&service).await?;
    let repository = add_repository(&service, &repo).await?;
    let chat = start_chat(&service, repository).await?;

    assert_eq!(commit_in_turn(&service, chat).await?, ["committed"]);

    let worktree = worktree(&service, chat).await?;
    let (name, email) = FAKE_GLOBAL_AUTHOR;
    assert_eq!(
        git(&worktree, &["log", "-1", "--format=%an <%ae>"])?,
        format!("{name} <{email}>")
    );
    assert_eq!(
        top_bar(&service, chat).await?.host_account,
        HostAccountBinding::Unbound
    );
    Ok(())
}

#[tokio::test]
async fn the_top_bar_shows_branch_host_account_and_commit_author() -> TestResult {
    let bound = bound_with_commit().await?;

    let top_bar = top_bar(&bound.service, bound.chat).await?;

    assert!(matches!(top_bar.branch, Branch::Named { .. }));
    assert_eq!(
        top_bar.host_account,
        HostAccountBinding::Bound {
            host_account: bound.host_account,
            name: NAME.to_owned(),
            git_host: GIT_HOST.to_owned(),
            login: LOGIN.to_owned(),
        }
    );
    assert_eq!(top_bar.commit_author, Some(author()));
    Ok(())
}

#[tokio::test]
async fn a_repository_bound_to_an_unknown_host_account_cannot_start_chats() -> TestResult {
    let harness = start_service(&committing())?;
    let service = harness.connect();
    let repo = TestRepo::with_remote()?;
    bind(&repo, "Nobody")?;
    let repository = add_repository(&service, &repo).await?;

    let started = start_chat(&service, repository).await;

    assert_eq!(
        started,
        Err(CommandError::UnknownHostAccountName {
            name: "Nobody".to_owned()
        })
    );
    Ok(())
}

#[tokio::test]
async fn a_push_that_signs_in_as_the_bound_account_reaches_the_remote() -> TestResult {
    let bound = bound_with_commit().await?;
    assert_eq!(
        log_in(&bound.service, bound.host_account).await?,
        LoginOutcome::SignedIn
    );
    let branch = branch(&bound.service, bound.chat).await?;
    let worktree = worktree(&bound.service, bound.chat).await?;

    let response = bound
        .service
        .execute(Command::Push { chat: bound.chat })
        .await?;

    assert_eq!(
        response,
        Response::Pushed {
            branch: branch.clone()
        }
    );
    assert_eq!(
        bound.repo.remote_branch(&branch)?,
        Some(git(&worktree, &["rev-parse", "HEAD"])?)
    );
    Ok(())
}

#[tokio::test]
async fn a_push_that_would_sign_in_as_another_account_is_blocked() -> TestResult {
    let bound = bound_with_commit().await?;
    bound
        .harness
        .git_host()
        .next_login_signs_in_as("private-account");
    log_in(&bound.service, bound.host_account).await?;
    let branch = branch(&bound.service, bound.chat).await?;

    let pushed = bound
        .service
        .execute(Command::Push { chat: bound.chat })
        .await;

    let expected = CommandError::HostAccountMismatch {
        name: NAME.to_owned(),
        git_host: GIT_HOST.to_owned(),
        expected: LOGIN.to_owned(),
        actual: "private-account".to_owned(),
    };
    assert_eq!(pushed, Err(expected.clone()));
    let message = expected.to_string();
    assert!(message.contains("push blocked"), "{message}");
    assert!(message.contains("private-account"), "{message}");
    assert!(message.contains(LOGIN), "{message}");
    assert_eq!(bound.repo.remote_branch(&branch)?, None);
    Ok(())
}

#[tokio::test]
async fn a_push_before_the_host_account_signed_in_is_blocked() -> TestResult {
    let bound = bound_with_commit().await?;
    let branch = branch(&bound.service, bound.chat).await?;

    let pushed = bound
        .service
        .execute(Command::Push { chat: bound.chat })
        .await;

    assert!(
        matches!(pushed, Err(CommandError::HostAccountUnverified { .. })),
        "{pushed:?}"
    );
    assert_eq!(bound.repo.remote_branch(&branch)?, None);
    Ok(())
}

#[tokio::test]
async fn a_push_from_an_unbound_repository_is_blocked() -> TestResult {
    let harness = start_service(&committing())?;
    let service = harness.connect();
    let repo = TestRepo::with_remote()?;
    let repository = add_repository(&service, &repo).await?;
    let chat = start_chat(&service, repository).await?;
    commit_in_turn(&service, chat).await?;
    let branch = branch(&service, chat).await?;

    let pushed = service.execute(Command::Push { chat }).await;

    assert_eq!(pushed, Err(CommandError::NoHostAccount { repository }));
    assert_eq!(repo.remote_branch(&branch)?, None);
    Ok(())
}

#[tokio::test]
async fn a_binding_changed_by_hand_is_restored_before_the_push() -> TestResult {
    let bound = bound_with_commit().await?;
    log_in(&bound.service, bound.host_account).await?;
    bound.repo.git(&[
        "config",
        "--local",
        "user.email",
        "someone-else@example.invalid",
    ])?;

    bound
        .service
        .execute(Command::Push { chat: bound.chat })
        .await?;

    assert_eq!(
        bound.repo.git(&["config", "--local", "user.email"])?,
        author().email
    );
    Ok(())
}

#[tokio::test]
async fn host_accounts_are_kept_across_a_restart_and_need_distinct_names() -> TestResult {
    let harness = start_service(&Script::new())?;
    let host_account = add_host_account(&harness.connect()).await?;

    let harness = harness.restart().await?;
    let service = harness.connect();

    let listed = host_accounts(&service).await?;
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].host_account, host_account);
    assert_eq!(listed[0].name, NAME);
    assert_eq!(listed[0].login, LOGIN);
    assert_eq!(listed[0].git_host, GIT_HOST);
    assert_eq!(listed[0].author, author());
    assert_eq!(
        add_host_account(&service)
            .await
            .map_err(|error| error.to_string()),
        Err(CommandError::HostAccountNameTaken {
            name: NAME.to_owned()
        }
        .to_string())
    );
    Ok(())
}

#[tokio::test]
async fn a_removed_host_account_no_longer_binds_its_repositories() -> TestResult {
    let harness = start_service(&committing())?;
    let service = harness.connect();
    let repo = TestRepo::with_remote()?;
    let host_account = add_host_account(&service).await?;
    bind(&repo, NAME)?;
    let repository = add_repository(&service, &repo).await?;

    let removed = service
        .execute(Command::RemoveHostAccount { host_account })
        .await?;

    assert_eq!(removed, Response::HostAccountRemoved);
    assert_eq!(host_accounts(&service).await?, Vec::new());
    assert_eq!(
        start_chat(&service, repository).await,
        Err(CommandError::UnknownHostAccountName {
            name: NAME.to_owned()
        })
    );
    Ok(())
}
