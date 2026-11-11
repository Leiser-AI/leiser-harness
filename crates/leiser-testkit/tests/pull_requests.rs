//! Commit, push and pull request from a chat (#36): the agent commits in
//! its worktree; push and pull request happen only on the developer's
//! command, after the host account is verified; one chat leads to at most
//! one pull request; once it is merged, the harness removes the worktree
//! and local branch on request and the chat stays readable.

use std::error::Error;
use std::path::{Path, PathBuf};
use std::time::Duration;

use leiser_service::{
    AgentEvent, Branch, ChatId, ChatPlace, ChatStatus, Checkout, Command, CommandError,
    CommitAuthor, Event, HostAccountId, LoginOutcome, PullRequest, PullRequestState, RepositoryId,
    Response, ServiceHandle, TranscriptEntry,
};
use leiser_testkit::{
    DEFAULT_BRANCH, FAKE_GIT_HOST_MERGED_FILE, Script, Step, TestRepo, TestService, collect_turn,
};
use tokio::sync::broadcast;

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

const FAKE_AGENT_CLI: &str = env!("CARGO_BIN_EXE_fake-agent-cli");
const GIT_HOST: &str = "git.example.invalid";
const NAME: &str = "Work";
const LOGIN: &str = "work-account";
const TITLE: &str = "Add the greeting";
const BODY: &str = "Adds a greeting file.";

/// Every turn writes a file and commits it.
fn committing() -> Script {
    (0..4).fold(Script::new(), |script, number| {
        script.turn([
            Step::WriteFile {
                path: PathBuf::from(format!("change-{number}.txt")),
                contents: format!("change {number}\n"),
            },
            Step::Commit {
                message: format!("Change {number}"),
            },
        ])
    })
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

async fn start_chat(service: &ServiceHandle, repository: RepositoryId) -> TestResult<ChatId> {
    match service
        .execute(Command::StartChat {
            repository,
            checkout: Checkout::NewWorktree,
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

struct ChatView {
    status: ChatStatus,
    branch: Branch,
    place: ChatPlace,
    pull_request: Option<PullRequest>,
    transcript: Vec<TranscriptEntry>,
}

async fn read_chat(service: &ServiceHandle, chat: ChatId) -> TestResult<ChatView> {
    match service.execute(Command::ReadChat { chat }).await? {
        Response::Chat {
            status,
            branch,
            place,
            pull_request,
            transcript,
            ..
        } => Ok(ChatView {
            status,
            branch,
            place,
            pull_request: pull_request.map(|pull_request| *pull_request),
            transcript,
        }),
        response => Err(format!("expected Chat, got {response:?}").into()),
    }
}

async fn branch(service: &ServiceHandle, chat: ChatId) -> TestResult<String> {
    match read_chat(service, chat).await?.branch {
        Branch::Named { name } => Ok(name),
        branch => Err(format!("expected a branch, got {branch:?}").into()),
    }
}

async fn open_pull_request(
    service: &ServiceHandle,
    chat: ChatId,
) -> Result<PullRequest, CommandError> {
    match service
        .execute(Command::OpenPullRequest {
            chat,
            title: TITLE.to_owned(),
            body: BODY.to_owned(),
        })
        .await?
    {
        Response::PullRequestOpened { pull_request } => Ok(pull_request),
        response => Err(CommandError::PushFailed {
            reason: format!("expected PullRequestOpened, got {response:?}"),
        }),
    }
}

async fn check_pull_request(service: &ServiceHandle, chat: ChatId) -> TestResult<PullRequest> {
    match service.execute(Command::CheckPullRequest { chat }).await? {
        Response::PullRequest { pull_request } => Ok(pull_request),
        response => Err(format!("expected PullRequest, got {response:?}").into()),
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

/// A repository bound to a signed-in host account, with a chat in its own
/// worktree whose agent has committed once.
struct Bound {
    harness: TestService,
    service: ServiceHandle,
    repo: TestRepo,
    chat: ChatId,
}

async fn bound_with_commit(sign_in_as: Option<&str>) -> TestResult<Bound> {
    let harness = TestService::start(Path::new(FAKE_AGENT_CLI), &committing())?;
    let service = harness.connect();
    let repo = TestRepo::with_remote()?;
    let host_account = add_host_account(&service).await?;
    if let Some(login) = sign_in_as {
        harness.git_host().next_login_signs_in_as(login);
    }
    log_in(&service, host_account).await?;
    std::fs::create_dir_all(repo.path().join(".leiser"))?;
    std::fs::write(
        repo.path().join(".leiser/config.toml"),
        format!("host_account = {NAME:?}\n"),
    )?;
    let repository = add_repository(&service, &repo).await?;
    let chat = start_chat(&service, repository).await?;
    commit_in_turn(&service, chat).await?;
    Ok(Bound {
        harness,
        service,
        repo,
        chat,
    })
}

#[tokio::test]
async fn the_agent_commits_on_the_chats_branch_and_nothing_is_pushed_without_a_click() -> TestResult
{
    let bound = bound_with_commit(None).await?;
    let worktree = worktree(&bound.service, bound.chat).await?;
    let branch = branch(&bound.service, bound.chat).await?;

    assert_eq!(git(&worktree, &["log", "-1", "--format=%s"])?, "Change 0");
    assert_eq!(git(&worktree, &["status", "--porcelain"])?, "");
    assert_eq!(
        bound.repo.git(&["log", "-1", "--format=%s"])?,
        "Initial commit",
        "the main checkout moved"
    );
    assert_eq!(bound.repo.remote_branch(&branch)?, None);
    assert_eq!(bound.harness.git_host().pull_requests(), Vec::new());
    Ok(())
}

#[tokio::test]
async fn a_pull_request_pushes_the_branch_and_opens_it_into_the_base_branch_as_the_bound_account()
-> TestResult {
    let bound = bound_with_commit(None).await?;
    let worktree = worktree(&bound.service, bound.chat).await?;
    let branch = branch(&bound.service, bound.chat).await?;
    let mut events = bound.service.subscribe();

    let pull_request = open_pull_request(&bound.service, bound.chat).await?;

    assert_eq!(pull_request.state, PullRequestState::Open);
    assert_eq!(pull_request.head, branch);
    assert_eq!(pull_request.base, DEFAULT_BRANCH);
    assert_eq!(
        bound.repo.remote_branch(&branch)?,
        Some(git(&worktree, &["rev-parse", "HEAD"])?)
    );
    let opened = bound.harness.git_host().pull_requests();
    assert_eq!(opened.len(), 1);
    assert_eq!(opened[0].number, pull_request.number);
    assert_eq!(opened[0].url, pull_request.url);
    assert_eq!(opened[0].head, branch);
    assert_eq!(opened[0].base, DEFAULT_BRANCH);
    assert_eq!(opened[0].title, TITLE);
    assert_eq!(opened[0].body, BODY);
    assert_eq!(opened[0].opened_by, LOGIN);
    assert_eq!(
        read_chat(&bound.service, bound.chat).await?.pull_request,
        Some(pull_request.clone())
    );
    let event = tokio::time::timeout(Duration::from_secs(5), events.recv()).await??;
    assert_eq!(
        event,
        Event::PullRequestChanged {
            chat: bound.chat,
            pull_request,
        }
    );
    Ok(())
}

#[tokio::test]
async fn a_pull_request_that_would_sign_in_as_another_account_is_blocked_before_the_push()
-> TestResult {
    let bound = bound_with_commit(Some("private-account")).await?;
    let branch = branch(&bound.service, bound.chat).await?;

    let opened = open_pull_request(&bound.service, bound.chat).await;

    assert_eq!(
        opened,
        Err(CommandError::HostAccountMismatch {
            name: NAME.to_owned(),
            git_host: GIT_HOST.to_owned(),
            expected: LOGIN.to_owned(),
            actual: "private-account".to_owned(),
        })
    );
    assert_eq!(bound.repo.remote_branch(&branch)?, None);
    assert_eq!(bound.harness.git_host().pull_requests(), Vec::new());
    assert_eq!(
        read_chat(&bound.service, bound.chat).await?.pull_request,
        None
    );
    Ok(())
}

#[tokio::test]
async fn a_pull_request_from_an_unbound_repository_is_blocked() -> TestResult {
    let harness = TestService::start(Path::new(FAKE_AGENT_CLI), &committing())?;
    let service = harness.connect();
    let repo = TestRepo::with_remote()?;
    let repository = add_repository(&service, &repo).await?;
    let chat = start_chat(&service, repository).await?;
    commit_in_turn(&service, chat).await?;

    let opened = open_pull_request(&service, chat).await;

    assert_eq!(opened, Err(CommandError::NoHostAccount { repository }));
    assert_eq!(harness.git_host().pull_requests(), Vec::new());
    Ok(())
}

#[tokio::test]
async fn a_chat_with_a_pull_request_cannot_open_another_and_further_pushes_update_it() -> TestResult
{
    let bound = bound_with_commit(None).await?;
    let first = open_pull_request(&bound.service, bound.chat).await?;
    commit_in_turn(&bound.service, bound.chat).await?;
    let worktree = worktree(&bound.service, bound.chat).await?;
    let branch = branch(&bound.service, bound.chat).await?;

    let pushed = bound
        .service
        .execute(Command::Push { chat: bound.chat })
        .await?;
    let second = open_pull_request(&bound.service, bound.chat).await;

    assert_eq!(
        pushed,
        Response::Pushed {
            branch: branch.clone()
        }
    );
    assert_eq!(
        bound.repo.remote_branch(&branch)?,
        Some(git(&worktree, &["rev-parse", "HEAD"])?)
    );
    assert_eq!(
        second,
        Err(CommandError::PullRequestExists {
            chat: bound.chat,
            number: first.number,
        })
    );
    assert_eq!(bound.harness.git_host().pull_requests().len(), 1);
    Ok(())
}

#[tokio::test]
async fn a_chat_keeps_its_pull_request_across_a_restart() -> TestResult {
    let bound = bound_with_commit(None).await?;
    let opened = open_pull_request(&bound.service, bound.chat).await?;
    let Bound {
        harness,
        repo: _repo,
        chat,
        ..
    } = bound;

    let harness = harness.restart().await?;
    let service = harness.connect();

    assert_eq!(
        read_chat(&service, chat).await?.pull_request,
        Some(opened.clone())
    );
    assert_eq!(
        open_pull_request(&service, chat).await,
        Err(CommandError::PullRequestExists {
            chat,
            number: opened.number,
        })
    );
    assert_eq!(harness.git_host().pull_requests().len(), 1);
    Ok(())
}

#[tokio::test]
async fn after_the_merge_the_worktree_and_local_branch_can_be_removed_and_the_chat_stays_readable()
-> TestResult {
    let bound = bound_with_commit(None).await?;
    let worktree = worktree(&bound.service, bound.chat).await?;
    let branch = branch(&bound.service, bound.chat).await?;
    let opened = open_pull_request(&bound.service, bound.chat).await?;
    assert_eq!(
        bound
            .service
            .execute(Command::RemoveWorktree { chat: bound.chat })
            .await,
        Err(CommandError::PullRequestNotMerged {
            chat: bound.chat,
            number: opened.number,
        })
    );
    assert_eq!(
        check_pull_request(&bound.service, bound.chat).await?.state,
        PullRequestState::Open
    );

    bound.harness.git_host().merge(opened.number);
    let checked = check_pull_request(&bound.service, bound.chat).await?;
    let removed = bound
        .service
        .execute(Command::RemoveWorktree { chat: bound.chat })
        .await?;

    assert_eq!(checked.state, PullRequestState::Merged);
    assert_eq!(removed, Response::WorktreeRemoved);
    assert!(!worktree.exists(), "{} still exists", worktree.display());
    assert_eq!(bound.repo.git(&["branch", "--list", &branch])?, "");
    let chat = read_chat(&bound.service, bound.chat).await?;
    assert_eq!(chat.place, ChatPlace::RemovedWorktree);
    assert_eq!(chat.status, ChatStatus::Done);
    assert_eq!(
        chat.pull_request.map(|pull| pull.state),
        Some(PullRequestState::Merged)
    );
    assert!(
        chat.transcript.iter().any(
            |entry| matches!(entry, TranscriptEntry::UserMessage { text } if text == "commit it")
        ),
        "{:?}",
        chat.transcript
    );
    assert_eq!(
        bound
            .service
            .execute(Command::SendMessage {
                chat: bound.chat,
                text: "more".to_owned(),
            })
            .await,
        Err(CommandError::WorktreeRemoved { chat: bound.chat })
    );
    assert_eq!(
        bound
            .service
            .execute(Command::Push { chat: bound.chat })
            .await,
        Err(CommandError::WorktreeRemoved { chat: bound.chat })
    );
    Ok(())
}

#[tokio::test]
async fn a_removed_worktree_stays_removed_across_a_restart() -> TestResult {
    let bound = bound_with_commit(None).await?;
    let opened = open_pull_request(&bound.service, bound.chat).await?;
    bound.harness.git_host().merge(opened.number);
    check_pull_request(&bound.service, bound.chat).await?;
    bound
        .service
        .execute(Command::RemoveWorktree { chat: bound.chat })
        .await?;
    let Bound {
        harness,
        repo: _repo,
        chat,
        ..
    } = bound;

    let harness = harness.restart().await?;
    let service = harness.connect();

    let view = read_chat(&service, chat).await?;
    assert_eq!(view.place, ChatPlace::RemovedWorktree);
    assert_eq!(view.status, ChatStatus::Done);
    assert_eq!(
        view.pull_request.map(|pull| pull.state),
        Some(PullRequestState::Merged)
    );
    Ok(())
}

#[tokio::test]
async fn a_worktree_with_uncommitted_changes_is_kept() -> TestResult {
    let bound = bound_with_commit(None).await?;
    let worktree = worktree(&bound.service, bound.chat).await?;
    let opened = open_pull_request(&bound.service, bound.chat).await?;
    bound.harness.git_host().merge(opened.number);
    check_pull_request(&bound.service, bound.chat).await?;
    std::fs::write(worktree.join("unsaved.txt"), "not committed\n")?;

    let removed = bound
        .service
        .execute(Command::RemoveWorktree { chat: bound.chat })
        .await;

    assert_eq!(
        removed,
        Err(CommandError::WorktreeHasChanges { chat: bound.chat })
    );
    assert!(worktree.join("unsaved.txt").exists());
    assert_eq!(
        read_chat(&bound.service, bound.chat).await?.place,
        ChatPlace::Worktree
    );
    Ok(())
}

#[tokio::test]
async fn a_worktree_with_commits_that_were_never_pushed_is_kept() -> TestResult {
    let bound = bound_with_commit(None).await?;
    let worktree = worktree(&bound.service, bound.chat).await?;
    let opened = open_pull_request(&bound.service, bound.chat).await?;
    commit_in_turn(&bound.service, bound.chat).await?;
    bound.harness.git_host().merge(opened.number);
    check_pull_request(&bound.service, bound.chat).await?;

    let removed = bound
        .service
        .execute(Command::RemoveWorktree { chat: bound.chat })
        .await;

    assert_eq!(
        removed,
        Err(CommandError::WorktreeHasUnpushedCommits { chat: bound.chat })
    );
    assert!(worktree.exists());
    Ok(())
}

#[tokio::test]
async fn a_chat_in_the_main_checkout_has_no_worktree_to_remove() -> TestResult {
    let harness = TestService::start(Path::new(FAKE_AGENT_CLI), &committing())?;
    let service = harness.connect();
    let repo = TestRepo::with_remote()?;
    let repository = add_repository(&service, &repo).await?;
    let chat = match service
        .execute(Command::StartChat {
            repository,
            checkout: Checkout::Main,
        })
        .await?
    {
        Response::ChatStarted { chat } => chat,
        response => return Err(format!("expected ChatStarted, got {response:?}").into()),
    };

    let removed = service.execute(Command::RemoveWorktree { chat }).await;

    assert_eq!(
        read_chat(&service, chat).await?.place,
        ChatPlace::MainCheckout
    );
    assert_eq!(removed, Err(CommandError::NotInAWorktree { chat }));
    Ok(())
}

#[tokio::test]
async fn the_fake_git_host_reports_a_pull_request_merged_that_its_file_lists() -> TestResult {
    let bound = bound_with_commit(None).await?;
    let opened = open_pull_request(&bound.service, bound.chat).await?;
    let config_dirs: Vec<PathBuf> =
        std::fs::read_dir(bound.harness.data_dir().join("host-accounts"))?
            .map(|entry| entry.map(|entry| entry.path().join("gh")))
            .collect::<Result<_, _>>()?;
    assert_eq!(config_dirs.len(), 1);

    std::fs::write(
        config_dirs[0].join(FAKE_GIT_HOST_MERGED_FILE),
        format!("{}\n", opened.number),
    )?;

    assert_eq!(
        check_pull_request(&bound.service, bound.chat).await?.state,
        PullRequestState::Merged
    );
    Ok(())
}
