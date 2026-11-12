//! Worktree per chat (#31): every chat works in its own git worktree and
//! branch by default, set up by the repository's setup script; the sidebar
//! shows which chats work, wait for the developer or are done.

use std::error::Error;
use std::path::{Path, PathBuf};
use std::process::Command as Process;
use std::sync::Arc;

use leiser_service::{
    AgentCli, AgentEvent, Branch, ChatId, ChatOverview, ChatSettings, ChatStatus, Checkout,
    Command, CommandError, Event, QueueReason, RepositoryId, RepositoryScript, Response,
    ServiceHandle, TranscriptEntry,
};
use leiser_testkit::{
    DEFAULT_BRANCH, FakeDriver, Gate, Script, Step, TestRepo, TestService, collect_turn, next_event,
};
use serde_json::json;
use tokio::sync::broadcast;

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

const FAKE_AGENT_CLI: &str = env!("CARGO_BIN_EXE_fake-agent-cli");

fn start_service(script: &Script) -> TestResult<TestService> {
    Ok(TestService::start(Path::new(FAKE_AGENT_CLI), script)?)
}

/// Writes the repository's `.leiser/config.toml` in its main checkout.
fn configure(repo: &TestRepo, config: &str) -> TestResult {
    let dir = repo.path().join(".leiser");
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("config.toml"), config)?;
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

async fn send(service: &ServiceHandle, chat: ChatId, text: &str) -> TestResult<Response> {
    Ok(service
        .execute(Command::SendMessage {
            chat,
            text: text.to_owned(),
        })
        .await?)
}

async fn mark_done(service: &ServiceHandle, chat: ChatId) -> TestResult {
    let response = service.execute(Command::MarkChatDone { chat }).await?;
    assert_eq!(response, Response::ChatMarkedDone);
    Ok(())
}

async fn list_chats(service: &ServiceHandle) -> TestResult<Vec<ChatOverview>> {
    match service.execute(Command::ListChats).await? {
        Response::Chats { chats } => Ok(chats),
        response => Err(format!("expected Chats, got {response:?}").into()),
    }
}

async fn overview(service: &ServiceHandle, chat: ChatId) -> TestResult<ChatOverview> {
    list_chats(service)
        .await?
        .into_iter()
        .find(|overview| overview.chat == chat)
        .ok_or_else(|| format!("chat {chat:?} is not listed").into())
}

async fn worktree(service: &ServiceHandle, chat: ChatId) -> TestResult<PathBuf> {
    overview(service, chat)
        .await?
        .worktree
        .ok_or_else(|| format!("chat {chat:?} has no worktree").into())
}

async fn read_chat(
    service: &ServiceHandle,
    chat: ChatId,
) -> TestResult<(ChatStatus, Branch, Vec<TranscriptEntry>)> {
    match service.execute(Command::ReadChat { chat }).await? {
        Response::Chat {
            status,
            branch,
            transcript,
            ..
        } => Ok((status, branch, transcript)),
        response => Err(format!("expected Chat, got {response:?}").into()),
    }
}

/// The text of the turn's first message.
async fn first_message(
    events: &mut broadcast::Receiver<Event>,
    chat: ChatId,
) -> TestResult<String> {
    let turn = collect_turn(events, chat).await?;
    match turn.first() {
        Some(AgentEvent::Message { text }) => Ok(text.clone()),
        _ => Err(format!("expected a message first, got {turn:?}").into()),
    }
}

/// Waits for the chat's next finished script and returns its exit code and
/// output.
async fn script_finished(
    events: &mut broadcast::Receiver<Event>,
    chat: ChatId,
    expected: RepositoryScript,
) -> TestResult<(Option<i32>, String)> {
    loop {
        if let Event::ScriptFinished {
            script,
            exit_code,
            output,
            ..
        } = next_event(events, chat).await?
        {
            assert_eq!(script, expected);
            return Ok((exit_code, output));
        }
    }
}

/// Runs git in `dir` and returns its trimmed output.
fn git(dir: &Path, args: &[&str]) -> TestResult<String> {
    let output = Process::new("git").arg("-C").arg(dir).args(args).output()?;
    if !output.status.success() {
        return Err(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// The commit the remote's `branch` points to.
fn remote_commit(repo: &TestRepo, branch: &str) -> TestResult<String> {
    let line = repo.git(&["ls-remote", "origin", &format!("refs/heads/{branch}")])?;
    Ok(line
        .split_whitespace()
        .next()
        .ok_or("the remote has no such branch")?
        .to_owned())
}

fn same_dir(a: &Path, b: &Path) -> TestResult<bool> {
    Ok(a.canonicalize()? == b.canonicalize()?)
}

fn name_branch(name: &str) -> Step {
    Step::CallHarnessTool {
        name: "name_branch".to_owned(),
        arguments: json!({ "name": name }),
    }
}

#[tokio::test]
async fn a_new_chat_works_in_its_own_worktree_on_a_new_branch_from_the_remote_base_branch()
-> TestResult {
    let harness = start_service(&Script::new().turn([Step::ReportWorkingDirectory]))?;
    let service = harness.connect();
    let repo = TestRepo::with_remote()?;
    // Someone else pushed after the clone: the worktree must start from
    // the base branch as the remote has it now.
    repo.push_to_remote(DEFAULT_BRANCH, "pushed.txt", "pushed elsewhere\n")?;
    let repository = add_repository(&service, &repo).await?;
    let mut events = service.subscribe();

    let chat = start_chat(&service, repository, Checkout::NewWorktree).await?;
    send(&service, chat, "where are you?").await?;

    let directory = PathBuf::from(first_message(&mut events, chat).await?);
    assert!(same_dir(&directory, &worktree(&service, chat).await?)?);
    assert!(!same_dir(&directory, repo.path())?);
    assert_eq!(
        git(&directory, &["rev-parse", "HEAD"])?,
        remote_commit(&repo, DEFAULT_BRANCH)?
    );
    let (_, branch, _) = read_chat(&service, chat).await?;
    let Branch::Named { name } = branch else {
        return Err(format!("expected a named branch, got {branch:?}").into());
    };
    assert_ne!(name, DEFAULT_BRANCH);
    // The main checkout stays where it was.
    assert_eq!(repo.git(&["branch", "--show-current"])?, DEFAULT_BRANCH);
    assert!(!repo.path().join("pushed.txt").exists());
    Ok(())
}

#[tokio::test]
async fn a_chat_starts_in_a_new_worktree_unless_the_main_checkout_is_chosen() -> TestResult {
    let harness = start_service(&Script::new())?;
    let service = harness.connect();
    let repo = TestRepo::with_remote()?;
    let repository = add_repository(&service, &repo).await?;

    // The desktop app may leave the checkout out.
    let command: Command = serde_json::from_value(json!({
        "type": "start_chat",
        "repository": repository,
    }))?;
    let Response::ChatStarted { chat: default } = service.execute(command).await? else {
        return Err("expected ChatStarted".into());
    };
    let main = start_chat(&service, repository, Checkout::Main).await?;

    assert!(overview(&service, default).await?.worktree.is_some());
    assert_eq!(overview(&service, main).await?.worktree, None);
    Ok(())
}

#[tokio::test]
async fn the_base_branch_comes_from_the_repository_config() -> TestResult {
    let harness = start_service(&Script::new())?;
    let service = harness.connect();
    let repo = TestRepo::with_remote()?;
    repo.push_to_remote("development", "development.txt", "on development\n")?;
    configure(&repo, "[worktree]\nbase_branch = \"origin/development\"\n")?;
    let repository = add_repository(&service, &repo).await?;

    let chat = start_chat(&service, repository, Checkout::NewWorktree).await?;

    let directory = worktree(&service, chat).await?;
    assert_eq!(
        git(&directory, &["rev-parse", "HEAD"])?,
        remote_commit(&repo, "development")?
    );
    assert!(directory.join("development.txt").exists());
    Ok(())
}

#[tokio::test]
async fn new_chat_in_this_worktree_continues_on_the_same_worktree_and_branch() -> TestResult {
    let harness = start_service(&Script::new().turn([Step::ReportWorkingDirectory]))?;
    let service = harness.connect();
    let repo = TestRepo::with_remote()?;
    configure(
        &repo,
        "[worktree]\nsetup_script = \"echo set up >> setup.log\"\n",
    )?;
    let repository = add_repository(&service, &repo).await?;
    let mut events = service.subscribe();
    let first = start_chat(&service, repository, Checkout::NewWorktree).await?;
    script_finished(&mut events, first, RepositoryScript::Setup).await?;

    let second = start_chat(&service, repository, Checkout::Worktree { chat: first }).await?;
    send(&service, second, "where are you?").await?;

    let directory = PathBuf::from(first_message(&mut events, second).await?);
    assert!(same_dir(&directory, &worktree(&service, first).await?)?);
    assert_eq!(
        read_chat(&service, second).await?.1,
        read_chat(&service, first).await?.1
    );
    // The worktree was set up once, by the chat that created it.
    assert_eq!(
        std::fs::read_to_string(directory.join("setup.log"))?,
        "set up\n"
    );
    Ok(())
}

#[tokio::test]
async fn a_chat_in_the_main_checkout_has_no_worktree_to_continue_in() -> TestResult {
    let harness = start_service(&Script::new())?;
    let service = harness.connect();
    let repo = TestRepo::with_remote()?;
    let repository = add_repository(&service, &repo).await?;
    let main = start_chat(&service, repository, Checkout::Main).await?;

    let error = service
        .execute(Command::StartChat {
            repository,
            checkout: Checkout::Worktree { chat: main },
            settings: ChatSettings::default(),
        })
        .await;

    assert_eq!(error, Err(CommandError::NotInAWorktree { chat: main }));
    Ok(())
}

#[tokio::test]
async fn a_repository_without_commits_cannot_start_a_worktree() -> TestResult {
    let harness = start_service(&Script::new())?;
    let service = harness.connect();
    let repo = TestRepo::new()?;
    let repository = add_repository(&service, &repo).await?;

    let result = service
        .execute(Command::StartChat {
            repository,
            checkout: Checkout::NewWorktree,
            settings: ChatSettings::default(),
        })
        .await;

    assert!(
        matches!(result, Err(CommandError::Worktree { .. })),
        "{result:?}"
    );
    // No chat was left behind.
    assert_eq!(list_chats(&service).await?, Vec::new());
    Ok(())
}

#[tokio::test]
async fn an_invalid_repository_config_is_reported_when_a_chat_starts() -> TestResult {
    let harness = start_service(&Script::new())?;
    let service = harness.connect();
    let repo = TestRepo::with_remote()?;
    configure(
        &repo,
        "[worktree]\nbranch_pattern = \"<type>/<nonsense>\"\n",
    )?;
    let repository = add_repository(&service, &repo).await?;

    let result = service
        .execute(Command::StartChat {
            repository,
            checkout: Checkout::NewWorktree,
            settings: ChatSettings::default(),
        })
        .await;

    let Err(CommandError::RepositoryConfig { reason }) = result else {
        return Err(format!("expected RepositoryConfig, got {result:?}").into());
    };
    assert!(reason.contains("<nonsense>"), "{reason}");
    Ok(())
}

#[tokio::test]
async fn the_agent_is_asked_to_name_the_branch_after_the_default_pattern() -> TestResult {
    let script = Script::new().turn([Step::EchoUserMessage, name_branch("feat/login-form")]);
    let harness = start_service(&script)?;
    let service = harness.connect();
    let repo = TestRepo::with_remote()?;
    let repository = add_repository(&service, &repo).await?;
    let chat = start_chat(&service, repository, Checkout::NewWorktree).await?;
    let mut events = service.subscribe();

    send(&service, chat, "Build the login form").await?;

    let turn = collect_turn(&mut events, chat).await?;
    let [
        AgentEvent::Message { text: received },
        AgentEvent::Message { text: named },
        AgentEvent::TurnEnded,
    ] = turn.as_slice()
    else {
        return Err(format!("unexpected turn {turn:?}").into());
    };
    assert!(received.contains("name_branch"), "{received}");
    assert!(received.contains("<type>/<short-name>"), "{received}");
    assert!(received.ends_with("Build the login form"), "{received}");
    assert!(!named.starts_with("error"), "{named}");
    assert_eq!(
        read_chat(&service, chat).await?.1,
        Branch::Named {
            name: "feat/login-form".to_owned()
        }
    );
    // The transcript keeps the developer's own words.
    assert_eq!(
        read_chat(&service, chat).await?.2.first(),
        Some(&TranscriptEntry::UserMessage {
            text: "Build the login form".to_owned()
        })
    );
    Ok(())
}

#[tokio::test]
async fn a_branch_name_must_follow_the_repository_pattern() -> TestResult {
    let script = Script::new().turn([
        Step::EchoUserMessage,
        name_branch("feat/login-form"),
        name_branch("timo/feat/login-form"),
    ]);
    let harness = start_service(&script)?;
    let service = harness.connect();
    let repo = TestRepo::with_remote()?;
    configure(
        &repo,
        "[worktree]\nbranch_pattern = \"timo/<type>/<short-name>\"\n",
    )?;
    let repository = add_repository(&service, &repo).await?;
    let chat = start_chat(&service, repository, Checkout::NewWorktree).await?;
    let mut events = service.subscribe();

    send(&service, chat, "Build the login form").await?;

    let turn = collect_turn(&mut events, chat).await?;
    let [
        AgentEvent::Message { text: received },
        AgentEvent::Message { text: refused },
        AgentEvent::Message { text: accepted },
        AgentEvent::TurnEnded,
    ] = turn.as_slice()
    else {
        return Err(format!("unexpected turn {turn:?}").into());
    };
    assert!(received.contains("timo/<type>/<short-name>"), "{received}");
    assert!(refused.starts_with("error"), "{refused}");
    assert!(refused.contains("timo/<type>/<short-name>"), "{refused}");
    assert!(!accepted.starts_with("error"), "{accepted}");
    assert_eq!(
        read_chat(&service, chat).await?.1,
        Branch::Named {
            name: "timo/feat/login-form".to_owned()
        }
    );
    Ok(())
}

#[tokio::test]
async fn the_main_checkouts_branch_is_not_the_agents_to_name() -> TestResult {
    let script = Script::new().turn([name_branch("feat/login-form")]);
    let harness = start_service(&script)?;
    let service = harness.connect();
    let repo = TestRepo::with_remote()?;
    let repository = add_repository(&service, &repo).await?;
    let chat = start_chat(&service, repository, Checkout::Main).await?;
    let mut events = service.subscribe();

    send(&service, chat, "Build the login form").await?;

    let answer = first_message(&mut events, chat).await?;
    assert!(answer.starts_with("error"), "{answer}");
    assert_eq!(repo.git(&["branch", "--show-current"])?, DEFAULT_BRANCH);
    Ok(())
}

#[tokio::test]
async fn the_setup_script_runs_after_the_worktree_is_created_and_before_the_agent_starts()
-> TestResult {
    let gate = Gate::new()?;
    let Step::WaitForGate { gate: gate_file } = gate.step() else {
        return Err("a gate waits at a file".into());
    };
    let script = Script::new().turn([Step::ReportFile {
        path: PathBuf::from("setup.txt"),
    }]);
    let harness = start_service(&script)?;
    let service = harness.connect();
    let repo = TestRepo::with_remote()?;
    configure(
        &repo,
        &format!(
            "[worktree]\nsetup_script = '''\nwhile [ ! -e \"{}\" ]; do sleep 0.01; done\n\
             test -e README.md && echo installed > setup.txt\n'''\n",
            gate_file.display()
        ),
    )?;
    let repository = add_repository(&service, &repo).await?;
    let mut events = service.subscribe();
    let chat = start_chat(&service, repository, Checkout::NewWorktree).await?;

    // The developer may write while the worktree is being set up.
    let response = send(&service, chat, "go").await?;
    assert_eq!(
        response,
        Response::MessageQueued {
            reason: QueueReason::SettingUp
        }
    );
    assert_eq!(overview(&service, chat).await?.status, ChatStatus::Working);
    gate.open()?;

    let (exit_code, _) = script_finished(&mut events, chat, RepositoryScript::Setup).await?;
    assert_eq!(exit_code, Some(0));
    assert_eq!(first_message(&mut events, chat).await?, "installed\n");
    let (_, _, transcript) = read_chat(&service, chat).await?;
    assert!(matches!(
        transcript.first(),
        Some(TranscriptEntry::ScriptFinished {
            script: RepositoryScript::Setup,
            exit_code: Some(0),
            ..
        })
    ));
    Ok(())
}

#[tokio::test]
async fn a_failed_setup_script_is_reported_and_the_chat_goes_on() -> TestResult {
    let harness = start_service(&Script::new().turn([Step::message("still here")]))?;
    let service = harness.connect();
    let repo = TestRepo::with_remote()?;
    configure(
        &repo,
        "[worktree]\nsetup_script = \"echo cannot install >&2; exit 3\"\n",
    )?;
    let repository = add_repository(&service, &repo).await?;
    let mut events = service.subscribe();
    let chat = start_chat(&service, repository, Checkout::NewWorktree).await?;

    let (exit_code, output) = script_finished(&mut events, chat, RepositoryScript::Setup).await?;
    assert_eq!(exit_code, Some(3));
    assert!(output.contains("cannot install"), "{output}");
    assert_eq!(send(&service, chat, "go on").await?, Response::MessageSent);
    assert_eq!(first_message(&mut events, chat).await?, "still here");
    Ok(())
}

#[tokio::test]
async fn the_done_script_runs_when_the_last_chat_in_a_worktree_is_done() -> TestResult {
    let harness = start_service(&Script::new())?;
    let service = harness.connect();
    let repo = TestRepo::with_remote()?;
    configure(
        &repo,
        "[worktree]\ndone_script = \"echo cleaned >> done.log\"\n",
    )?;
    let repository = add_repository(&service, &repo).await?;
    let mut events = service.subscribe();
    let first = start_chat(&service, repository, Checkout::NewWorktree).await?;
    let second = start_chat(&service, repository, Checkout::Worktree { chat: first }).await?;
    let directory = worktree(&service, first).await?;

    mark_done(&service, first).await?;
    assert!(!directory.join("done.log").exists());
    mark_done(&service, second).await?;

    let (exit_code, _) = script_finished(&mut events, second, RepositoryScript::Done).await?;
    assert_eq!(exit_code, Some(0));
    assert_eq!(
        std::fs::read_to_string(directory.join("done.log"))?,
        "cleaned\n"
    );
    Ok(())
}

#[tokio::test]
async fn the_sidebar_shows_each_chats_status_with_waiting_chats_on_top() -> TestResult {
    let gate = Gate::new()?;
    let approval = AgentEvent::ApprovalRequested {
        request: "1".to_owned(),
        description: "Run `cargo test`".to_owned(),
    };
    let working = Script::new().turn([gate.step()]);
    let asking = Script::new().turn([
        Step::Emit {
            event: approval.clone(),
        },
        gate.step(),
    ]);
    let program = Path::new(FAKE_AGENT_CLI);
    let harness = TestService::with_drivers(
        (
            AgentCli::ClaudeCode,
            Arc::new(FakeDriver::new(program, &working)?),
        ),
        vec![(
            AgentCli::Codex,
            Arc::new(FakeDriver::new(program, &asking)?),
        )],
    )?;
    let service = harness.connect();
    let repo = TestRepo::with_remote()?;
    let repository = add_repository(&service, &repo).await?;
    let mut events = service.subscribe();

    let done = start_chat(&service, repository, Checkout::Main).await?;
    mark_done(&service, done).await?;
    let busy = start_chat(&service, repository, Checkout::Main).await?;
    send(&service, busy, "work").await?;
    let fresh = start_chat(&service, repository, Checkout::Main).await?;
    let asking = start_chat(&service, repository, Checkout::Main).await?;
    service
        .execute(Command::ChangeAgentCli {
            chat: asking,
            agent_cli: AgentCli::Codex,
        })
        .await?;
    send(&service, asking, "work").await?;
    // The service gives the request its own id, so only its kind counts.
    while !matches!(
        next_event(&mut events, asking).await?,
        Event::Agent {
            event: AgentEvent::ApprovalRequested { .. },
            ..
        }
    ) {}

    let statuses: Vec<(ChatId, ChatStatus)> = list_chats(&service)
        .await?
        .into_iter()
        .map(|chat| (chat.chat, chat.status))
        .collect();
    assert_eq!(
        statuses,
        vec![
            (asking, ChatStatus::WaitingForYou),
            (fresh, ChatStatus::WaitingForYou),
            (busy, ChatStatus::Working),
            (done, ChatStatus::Done),
        ]
    );

    // A finished turn waits for the developer and moves to the top.
    gate.open()?;
    collect_turn(&mut events, busy).await?;
    let top = list_chats(&service).await?;
    assert_eq!(
        top.first().map(|chat| (chat.chat, chat.status)),
        Some((busy, ChatStatus::WaitingForYou))
    );
    // A message to a done chat takes it up again.
    send(&service, done, "one more thing").await?;
    assert_ne!(overview(&service, done).await?.status, ChatStatus::Done);
    Ok(())
}

#[tokio::test]
async fn parallel_chats_never_touch_each_others_files() -> TestResult {
    let gate = Gate::new()?;
    let notes = PathBuf::from("notes.txt");
    let script = Script::new().turn([
        Step::WriteUserMessageToFile {
            path: notes.clone(),
        },
        gate.step(),
        Step::ReportFile {
            path: notes.clone(),
        },
    ]);
    let harness = start_service(&script)?;
    let service = harness.connect();
    let repo = TestRepo::with_remote()?;
    let repository = add_repository(&service, &repo).await?;
    // One receiver per chat: waiting for one chat's turn skips the other's.
    let mut first_events = service.subscribe();
    let mut second_events = service.subscribe();
    let first = start_chat(&service, repository, Checkout::NewWorktree).await?;
    let second = start_chat(&service, repository, Checkout::NewWorktree).await?;

    send(&service, first, "first chat's notes").await?;
    send(&service, second, "second chat's notes").await?;
    // Both turns run at once before either may go on.
    for chat in [first, second] {
        assert_eq!(overview(&service, chat).await?.status, ChatStatus::Working);
    }
    gate.open()?;

    // Each agent reads back only what it wrote itself (the message it got,
    // after the harness's note on naming the branch).
    let first_notes = first_message(&mut first_events, first).await?;
    let second_notes = first_message(&mut second_events, second).await?;
    assert!(first_notes.ends_with("first chat's notes"), "{first_notes}");
    assert!(!first_notes.contains("second"), "{first_notes}");
    assert!(
        second_notes.ends_with("second chat's notes"),
        "{second_notes}"
    );
    assert!(!second_notes.contains("first"), "{second_notes}");
    assert_ne!(
        read_chat(&service, first).await?.1,
        read_chat(&service, second).await?.1
    );
    assert!(!repo.path().join(&notes).exists());
    Ok(())
}

#[tokio::test]
async fn a_chat_keeps_its_worktree_and_done_status_after_a_restart() -> TestResult {
    let harness = start_service(&Script::new().turn([name_branch("feat/kept")]))?;
    let service = harness.connect();
    let repo = TestRepo::with_remote()?;
    let repository = add_repository(&service, &repo).await?;
    let mut events = service.subscribe();
    let chat = start_chat(&service, repository, Checkout::NewWorktree).await?;
    send(&service, chat, "name it").await?;
    collect_turn(&mut events, chat).await?;
    mark_done(&service, chat).await?;
    let directory = worktree(&service, chat).await?;

    let harness = harness.restart().await?;
    let service = harness.connect();

    let after = overview(&service, chat).await?;
    assert_eq!(after.status, ChatStatus::Done);
    assert_eq!(after.worktree, Some(directory));
    assert_eq!(
        read_chat(&service, chat).await?.1,
        Branch::Named {
            name: "feat/kept".to_owned()
        }
    );
    Ok(())
}
