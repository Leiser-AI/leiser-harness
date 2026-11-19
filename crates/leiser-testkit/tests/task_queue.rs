//! The task queue: the developer lines up prompts and issues per
//! repository; the harness service runs them by itself, each in a chat in
//! a new worktree, as many at once as the queue's concurrency allows, moves
//! on when a turn ends or a pull request opens, waits while every
//! subscription a task could run on is at a usage limit, and keeps the
//! queue across restarts.
//!
//! Turns wait at [`Gate`]s, so every test decides when a task's turn ends;
//! the queue is read after every [`Event::QueueChanged`], never on a timer.

use std::error::Error;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use leiser_service::{
    AgentCli, Answer, AttentionReason, ChatId, Command, CommandError, CommitAuthor, Event,
    LoginOutcome, MAX_CONCURRENCY, PermissionMode, Provider, QueueAnswer, QueueCommand, QueueError,
    QueueItem, QueueItemId, QueueItemState, QueueSettings, QueueStatus, QueueTask, RepositoryId,
    RepositoryQueue, Response, ServiceConfig, ServiceHandle, SignIn, SubscriptionId,
    TranscriptEntry,
};
use leiser_testkit::{
    FAKE_USAGE_LIMIT_FILE, FakeDriver, FakeGitHost, FakeUsageLimit, Gate, Script, Step, TestRepo,
    TestService,
};
use tokio::sync::broadcast;

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

const FAKE_AGENT_CLI: &str = env!("CARGO_BIN_EXE_fake-agent-cli");
/// How long a test waits for the queue to get where it expects.
const QUEUE_TIMEOUT: Duration = Duration::from_secs(20);
const SUBSCRIPTION: &str = "Max private";
const HOST_ACCOUNT: &str = "Work";

/// A harness service whose agent answers every message after `gate`
/// opens, and a repository with a remote, added to it.
struct Harness {
    service: TestService,
    app: ServiceHandle,
    repo: TestRepo,
    repository: RepositoryId,
}

async fn harness(script: &Script) -> TestResult<Harness> {
    let service = TestService::start(Path::new(FAKE_AGENT_CLI), script)?;
    let app = service.connect();
    let repo = TestRepo::with_remote()?;
    let repository = add_repository(&app, &repo).await?;
    Ok(Harness {
        service,
        app,
        repo,
        repository,
    })
}

/// Every turn waits for the gate, then answers with the message.
fn gated(gate: &Gate) -> Script {
    Script::new().turn([gate.step(), Step::EchoUserMessage])
}

async fn add_repository(app: &ServiceHandle, repo: &TestRepo) -> TestResult<RepositoryId> {
    match app
        .execute(Command::AddRepository {
            path: repo.path().to_path_buf(),
        })
        .await?
    {
        Response::RepositoryAdded { repository } => Ok(repository),
        response => Err(format!("expected RepositoryAdded, got {response:?}").into()),
    }
}

fn configure(repo: &TestRepo, config: &str) -> TestResult {
    let dir = repo.path().join(".leiser");
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("config.toml"), config)?;
    Ok(())
}

async fn queue(app: &ServiceHandle, command: QueueCommand) -> Result<QueueAnswer, CommandError> {
    match app.execute(Command::Queue { command }).await? {
        Response::Queue { answer } => Ok(answer),
        response => Err(CommandError::AgentCli {
            reason: format!("expected Queue, got {response:?}"),
        }),
    }
}

fn shown(answer: QueueAnswer) -> RepositoryQueue {
    match answer {
        QueueAnswer::Queue { queue } | QueueAnswer::Enqueued { queue, .. } => queue,
    }
}

async fn read(app: &ServiceHandle, repository: RepositoryId) -> TestResult<RepositoryQueue> {
    Ok(shown(queue(app, QueueCommand::Read { repository }).await?))
}

async fn enqueue_task(
    app: &ServiceHandle,
    repository: RepositoryId,
    task: QueueTask,
) -> TestResult<QueueItemId> {
    match queue(
        app,
        QueueCommand::Enqueue {
            repository,
            task,
            agent_cli: None,
            permission_mode: None,
        },
    )
    .await?
    {
        QueueAnswer::Enqueued { item, .. } => Ok(item),
        answer @ QueueAnswer::Queue { .. } => {
            Err(format!("expected Enqueued, got {answer:?}").into())
        }
    }
}

async fn enqueue(
    app: &ServiceHandle,
    repository: RepositoryId,
    text: &str,
) -> TestResult<QueueItemId> {
    enqueue_task(
        app,
        repository,
        QueueTask::Prompt {
            text: text.to_owned(),
        },
    )
    .await
}

async fn pause(app: &ServiceHandle, repository: RepositoryId) -> TestResult {
    queue(app, QueueCommand::Pause { repository }).await?;
    Ok(())
}

async fn resume(app: &ServiceHandle, repository: RepositoryId) -> TestResult {
    queue(app, QueueCommand::Resume { repository }).await?;
    Ok(())
}

async fn set_concurrency(
    app: &ServiceHandle,
    repository: RepositoryId,
    concurrency: u32,
) -> Result<QueueAnswer, CommandError> {
    queue(
        app,
        QueueCommand::ChangeSettings {
            repository,
            settings: QueueSettings {
                concurrency,
                ..QueueSettings::default()
            },
        },
    )
    .await
}

/// Reads the repository's queue after every change until `until` holds,
/// and hands every queue it read to `seen`.
async fn wait_for(
    app: &ServiceHandle,
    events: &mut broadcast::Receiver<Event>,
    repository: RepositoryId,
    mut seen: impl FnMut(&RepositoryQueue),
    until: impl Fn(&RepositoryQueue) -> bool,
) -> TestResult<RepositoryQueue> {
    let mut last = None;
    let waited = tokio::time::timeout(QUEUE_TIMEOUT, async {
        loop {
            let current = read(app, repository).await?;
            seen(&current);
            if until(&current) {
                return Ok::<_, Box<dyn Error>>(current);
            }
            last = Some(current);
            loop {
                match events.recv().await {
                    Ok(Event::QueueChanged {
                        repository: changed,
                    }) if changed == repository => {
                        break;
                    }
                    Ok(_) => {}
                    // Missed events: the next read shows where it is.
                    Err(broadcast::error::RecvError::Lagged(_)) => break,
                    Err(error) => return Err(error.into()),
                }
            }
        }
    })
    .await;
    match waited {
        Ok(result) => result,
        Err(_) => Err(format!("the queue did not get there in time; last seen: {last:#?}").into()),
    }
}

async fn wait_until(
    app: &ServiceHandle,
    events: &mut broadcast::Receiver<Event>,
    repository: RepositoryId,
    until: impl Fn(&RepositoryQueue) -> bool,
) -> TestResult<RepositoryQueue> {
    wait_for(app, events, repository, |_| {}, until).await
}

fn item(queue: &RepositoryQueue, id: QueueItemId) -> Option<&QueueItem> {
    queue.items.iter().find(|item| item.item == id)
}

/// The task's state, for the conditions a test waits for.
fn state(queue: &RepositoryQueue, id: QueueItemId) -> Option<&QueueItemState> {
    item(queue, id).map(|item| &item.state)
}

/// The task's state, which the queue must show.
fn found(queue: &RepositoryQueue, id: QueueItemId) -> TestResult<&QueueItemState> {
    state(queue, id).ok_or_else(|| format!("task {id} is not in the queue").into())
}

/// How many tasks hold a slot.
fn active(queue: &RepositoryQueue) -> usize {
    queue
        .items
        .iter()
        .filter(|item| {
            matches!(
                item.state,
                QueueItemState::Starting | QueueItemState::Running { .. }
            )
        })
        .count()
}

fn all(queue: &RepositoryQueue, done: impl Fn(Option<&QueueItemState>) -> bool) -> bool {
    queue.items.iter().all(|item| done(Some(&item.state)))
}

fn completed(state: Option<&QueueItemState>) -> bool {
    matches!(state, Some(QueueItemState::Completed { .. }))
}

fn running(state: Option<&QueueItemState>) -> bool {
    matches!(state, Some(QueueItemState::Running { .. }))
}

fn chat_of(state: &QueueItemState) -> TestResult<ChatId> {
    match state {
        QueueItemState::Running { chat, .. }
        | QueueItemState::Completed { chat }
        | QueueItemState::PullRequestReady { chat, .. }
        | QueueItemState::NeedsAttention {
            chat: Some(chat), ..
        }
        | QueueItemState::Cancelled { chat: Some(chat) } => Ok(*chat),
        state => Err(format!("no chat in {state:?}").into()),
    }
}

/// The chat's transcript and the requests it waits for.
async fn read_chat(
    app: &ServiceHandle,
    chat: ChatId,
) -> TestResult<(Vec<TranscriptEntry>, Vec<String>)> {
    match app.execute(Command::ReadChat { chat }).await? {
        Response::Chat {
            transcript,
            open_requests,
            ..
        } => Ok((transcript, open_requests)),
        response => Err(format!("expected Chat, got {response:?}").into()),
    }
}

/// The developer's messages in the chat, in order.
async fn messages(app: &ServiceHandle, chat: ChatId) -> TestResult<Vec<String>> {
    Ok(read_chat(app, chat)
        .await?
        .0
        .into_iter()
        .filter_map(|entry| match entry {
            TranscriptEntry::UserMessage { text } => Some(text),
            _ => None,
        })
        .collect())
}

async fn worktree_of(app: &ServiceHandle, chat: ChatId) -> TestResult<Option<PathBuf>> {
    match app.execute(Command::ListChats).await? {
        Response::Chats { chats } => Ok(chats
            .into_iter()
            .find(|overview| overview.chat == chat)
            .and_then(|overview| overview.worktree)),
        response => Err(format!("expected Chats, got {response:?}").into()),
    }
}

#[tokio::test]
async fn tasks_start_in_order_one_at_a_time_each_in_a_worktree_of_its_own() -> TestResult {
    let gate = Gate::new()?;
    let harness = harness(&gated(&gate)).await?;
    let (app, repository) = (&harness.app, harness.repository);
    let mut events = app.subscribe();

    let tasks = [
        enqueue(app, repository, "First task").await?,
        enqueue(app, repository, "Second task").await?,
        enqueue(app, repository, "Third task").await?,
    ];

    let started = wait_until(app, &mut events, repository, |queue| {
        running(state(queue, tasks[0]))
    })
    .await?;
    assert_eq!(started.settings.concurrency, 1);
    assert_eq!(started.status, QueueStatus::Running);
    assert_eq!(found(&started, tasks[1])?, &QueueItemState::Queued);
    assert_eq!(found(&started, tasks[2])?, &QueueItemState::Queued);

    gate.open()?;
    let mut most_active = 0;
    let finished = wait_for(
        app,
        &mut events,
        repository,
        |queue| most_active = most_active.max(active(queue)),
        |queue| all(queue, completed),
    )
    .await?;

    assert_eq!(most_active, 1);
    assert_eq!(finished.status, QueueStatus::Idle);
    let chats: Vec<ChatId> = tasks
        .iter()
        .map(|task| chat_of(found(&finished, *task)?))
        .collect::<TestResult<_>>()?;
    // Started in line order: each chat after the one before.
    assert!(chats.windows(2).all(|pair| pair[0] < pair[1]));
    let mut worktrees = Vec::new();
    for (chat, text) in chats
        .iter()
        .zip(["First task", "Second task", "Third task"])
    {
        assert_eq!(messages(app, *chat).await?, [text]);
        let worktree = worktree_of(app, *chat).await?.ok_or("no worktree")?;
        assert_ne!(worktree, harness.repo.path());
        worktrees.push(worktree);
    }
    let distinct: std::collections::BTreeSet<&PathBuf> = worktrees.iter().collect();
    assert_eq!(distinct.len(), 3);
    Ok(())
}

#[tokio::test]
async fn a_higher_concurrency_runs_that_many_tasks_at_once() -> TestResult {
    let gate = Gate::new()?;
    let harness = harness(&gated(&gate)).await?;
    let (app, repository) = (&harness.app, harness.repository);
    let mut events = app.subscribe();
    set_concurrency(app, repository, 2).await?;

    let tasks = [
        enqueue(app, repository, "one").await?,
        enqueue(app, repository, "two").await?,
        enqueue(app, repository, "three").await?,
    ];
    let two_running = wait_until(app, &mut events, repository, |queue| {
        running(state(queue, tasks[0])) && running(state(queue, tasks[1]))
    })
    .await?;
    assert_eq!(found(&two_running, tasks[2])?, &QueueItemState::Queued);

    gate.open()?;
    let mut most_active = 0;
    wait_for(
        app,
        &mut events,
        repository,
        |queue| most_active = most_active.max(active(queue)),
        |queue| all(queue, completed),
    )
    .await?;
    assert_eq!(most_active, 2);
    Ok(())
}

#[tokio::test]
async fn a_paused_queue_starts_nothing_and_takes_new_order_and_cancellations() -> TestResult {
    let gate = Gate::new()?;
    gate.open()?;
    let harness = harness(&gated(&gate)).await?;
    let (app, repository) = (&harness.app, harness.repository);
    let mut events = app.subscribe();
    pause(app, repository).await?;

    let first = enqueue(app, repository, "first").await?;
    let second = enqueue(app, repository, "second").await?;
    let third = enqueue(app, repository, "third").await?;
    let paused = read(app, repository).await?;
    assert_eq!(paused.status, QueueStatus::Paused);
    assert!(all(&paused, |state| state == Some(&QueueItemState::Queued)));

    let moved = shown(queue(app, QueueCommand::Move { item: third, to: 0 }).await?);
    let order: Vec<QueueItemId> = moved.items.iter().map(|item| item.item).collect();
    assert_eq!(order, [third, first, second]);
    queue(app, QueueCommand::Cancel { item: second }).await?;

    resume(app, repository).await?;
    let done = wait_until(app, &mut events, repository, |queue| {
        completed(state(queue, third)) && completed(state(queue, first))
    })
    .await?;

    assert_eq!(
        found(&done, second)?,
        &QueueItemState::Cancelled { chat: None }
    );
    let third_chat = chat_of(found(&done, third)?)?;
    let first_chat = chat_of(found(&done, first)?)?;
    assert!(third_chat < first_chat, "the moved task starts first");
    assert_eq!(messages(app, third_chat).await?, ["third"]);

    queue(app, QueueCommand::Remove { item: second }).await?;
    let removed = read(app, repository).await?;
    assert!(removed.items.iter().all(|item| item.item != second));
    Ok(())
}

#[tokio::test]
async fn cancelling_a_running_task_interrupts_its_turn_keeps_its_chat_and_starts_the_next()
-> TestResult {
    let gate = Gate::new()?;
    let harness = harness(&gated(&gate)).await?;
    let (app, repository) = (&harness.app, harness.repository);
    let mut events = app.subscribe();
    let first = enqueue(app, repository, "first").await?;
    let second = enqueue(app, repository, "second").await?;
    wait_until(app, &mut events, repository, |queue| {
        running(state(queue, first))
    })
    .await?;

    let cancelled = shown(queue(app, QueueCommand::Cancel { item: first }).await?);
    let chat = chat_of(found(&cancelled, first)?)?;
    assert_eq!(
        found(&cancelled, first)?,
        &QueueItemState::Cancelled { chat: Some(chat) }
    );
    let next = wait_until(app, &mut events, repository, |queue| {
        running(state(queue, second))
    })
    .await?;
    assert_ne!(chat_of(found(&next, second)?)?, chat);
    assert_eq!(messages(app, chat).await?, ["first"]);
    gate.open()?;
    wait_until(app, &mut events, repository, |queue| {
        completed(state(queue, second))
    })
    .await?;
    Ok(())
}

#[tokio::test]
async fn a_failed_turn_needs_attention_and_the_queue_moves_on() -> TestResult {
    let script = Script::new().turn([Step::FailTurn {
        reason: "model request failed".to_owned(),
    }]);
    let harness = harness(&script).await?;
    let (app, repository) = (&harness.app, harness.repository);
    let mut events = app.subscribe();

    let first = enqueue(app, repository, "first").await?;
    let second = enqueue(app, repository, "second").await?;
    let done = wait_until(app, &mut events, repository, |queue| {
        all(queue, |state| {
            matches!(state, Some(QueueItemState::NeedsAttention { .. }))
        })
    })
    .await?;

    for task in [first, second] {
        assert_eq!(
            found(&done, task)?,
            &QueueItemState::NeedsAttention {
                chat: Some(chat_of(found(&done, task)?)?),
                reason: AttentionReason::TurnFailed {
                    reason: "model request failed".to_owned()
                },
            }
        );
    }
    // Neither holds a slot: the queue has nothing left to run.
    assert_eq!(done.status, QueueStatus::Idle);
    Ok(())
}

#[tokio::test]
async fn a_task_waiting_for_approval_holds_its_slot_until_the_developer_answers() -> TestResult {
    let script = Script::new().turn([Step::RequestApproval {
        description: "run the migration".to_owned(),
    }]);
    let harness = harness(&script).await?;
    let (app, repository) = (&harness.app, harness.repository);
    let mut events = app.subscribe();
    queue(
        app,
        QueueCommand::ChangeSettings {
            repository,
            settings: QueueSettings {
                concurrency: 1,
                agent_cli: Some(AgentCli::ClaudeCode),
                permission_mode: Some(PermissionMode::Supervised),
            },
        },
    )
    .await?;

    let first = enqueue(app, repository, "first").await?;
    let second = enqueue(app, repository, "second").await?;
    let waiting = wait_until(app, &mut events, repository, |queue| {
        matches!(
            state(queue, first),
            Some(QueueItemState::Running {
                waiting_for_you: true,
                ..
            })
        )
    })
    .await?;
    assert_eq!(found(&waiting, second)?, &QueueItemState::Queued);

    let chat = chat_of(found(&waiting, first)?)?;
    let (_, open) = read_chat(app, chat).await?;
    app.execute(Command::AnswerRequest {
        chat,
        request: open.first().ok_or("no open request")?.clone(),
        answer: Answer::Approve,
    })
    .await?;
    wait_until(app, &mut events, repository, |queue| {
        completed(state(queue, first))
            && !matches!(state(queue, second), Some(QueueItemState::Queued))
    })
    .await?;
    Ok(())
}

#[tokio::test]
async fn an_issue_task_asks_the_agent_to_work_on_the_issue() -> TestResult {
    let gate = Gate::new()?;
    gate.open()?;
    let harness = harness(&gated(&gate)).await?;
    let (app, repository) = (&harness.app, harness.repository);
    let mut events = app.subscribe();

    let task = enqueue_task(app, repository, QueueTask::Issue { number: 12 }).await?;
    let done = wait_until(app, &mut events, repository, |queue| {
        completed(state(queue, task))
    })
    .await?;

    assert_eq!(item(&done, task).ok_or("no task")?.title, "Issue #12");
    let sent = messages(app, chat_of(found(&done, task)?)?).await?;
    assert_eq!(sent.len(), 1);
    assert!(sent[0].contains("issue #12"), "{sent:?}");
    assert!(sent[0].contains("Closes #12"), "{sent:?}");
    Ok(())
}

/// Adds a login subscription and waits until its login has signed in.
async fn add_subscription(app: &ServiceHandle, name: &str) -> TestResult<SubscriptionId> {
    let mut events = app.subscribe();
    let Response::SubscriptionAdded { subscription } = app
        .execute(Command::AddSubscription {
            provider: Provider::Anthropic,
            name: name.to_owned(),
            sign_in: SignIn::Login,
        })
        .await?
    else {
        return Err("expected SubscriptionAdded".into());
    };
    tokio::time::timeout(QUEUE_TIMEOUT, async {
        loop {
            if let Event::LoginFinished {
                subscription: finished,
                outcome,
            } = events.recv().await?
                && finished == subscription
            {
                assert_eq!(outcome, LoginOutcome::SignedIn);
                return Ok::<_, Box<dyn Error>>(());
            }
        }
    })
    .await??;
    Ok(subscription)
}

async fn subscription_home(
    app: &ServiceHandle,
    subscription: SubscriptionId,
) -> TestResult<PathBuf> {
    match app.execute(Command::ListSubscriptions).await? {
        Response::Subscriptions { subscriptions } => subscriptions
            .into_iter()
            .find(|overview| overview.subscription == subscription)
            .map(|overview| overview.home)
            .ok_or_else(|| "the subscription is not listed".into()),
        response => Err(format!("expected Subscriptions, got {response:?}").into()),
    }
}

#[tokio::test]
async fn the_queue_waits_while_every_subscription_is_at_a_usage_limit_and_resumes_on_request()
-> TestResult {
    let gate = Gate::new()?;
    gate.open()?;
    let harness = harness(&gated(&gate)).await?;
    let (app, repository) = (&harness.app, harness.repository);
    let subscription = add_subscription(app, SUBSCRIPTION).await?;
    configure(
        &harness.repo,
        &format!("[subscriptions]\nanthropic = {SUBSCRIPTION:?}\n"),
    )?;
    let limit_file = subscription_home(app, subscription)
        .await?
        .join(FAKE_USAGE_LIMIT_FILE);
    std::fs::write(
        &limit_file,
        serde_json::to_vec(&FakeUsageLimit {
            limit: "5-hour limit".to_owned(),
            resets_in_seconds: None,
        })?,
    )?;
    let mut events = app.subscribe();

    let first = enqueue(app, repository, "first").await?;
    let second = enqueue(app, repository, "second").await?;
    let waiting = wait_until(app, &mut events, repository, |queue| {
        matches!(queue.status, QueueStatus::WaitingForUsageLimits { .. })
    })
    .await?;

    assert_eq!(
        waiting.status,
        QueueStatus::WaitingForUsageLimits {
            resets_in_seconds: None
        }
    );
    assert_eq!(
        found(&waiting, first)?,
        &QueueItemState::NeedsAttention {
            chat: Some(chat_of(found(&waiting, first)?)?),
            reason: AttentionReason::UsageLimit {
                limit: "5-hour limit".to_owned(),
                subscription: Some(subscription),
                resets_in_seconds: None,
            },
        }
    );
    assert_eq!(found(&waiting, second)?, &QueueItemState::Queued);

    // The limit is gone; the developer tells the queue to try again.
    std::fs::remove_file(&limit_file)?;
    resume(app, repository).await?;
    let resumed = wait_until(app, &mut events, repository, |queue| {
        completed(state(queue, second))
    })
    .await?;
    assert_eq!(resumed.status, QueueStatus::Idle);
    Ok(())
}

#[tokio::test]
async fn a_task_whose_chat_opens_its_pull_request_is_ready_for_review() -> TestResult {
    let script = Script::new().turn([
        Step::WriteFile {
            path: PathBuf::from("greeting.txt"),
            contents: "hello\n".to_owned(),
        },
        Step::Commit {
            message: "Add the greeting".to_owned(),
        },
    ]);
    let harness = harness(&script).await?;
    let (app, repository) = (&harness.app, harness.repository);
    let Response::HostAccountAdded { host_account } = app
        .execute(Command::AddHostAccount {
            name: HOST_ACCOUNT.to_owned(),
            git_host: "git.example.invalid".to_owned(),
            login: "work-account".to_owned(),
            author: CommitAuthor {
                name: "Work Author".to_owned(),
                email: "work@example.invalid".to_owned(),
            },
        })
        .await?
    else {
        return Err("expected HostAccountAdded".into());
    };
    let mut events = app.subscribe();
    app.execute(Command::LogInHostAccount { host_account })
        .await?;
    tokio::time::timeout(QUEUE_TIMEOUT, async {
        loop {
            if let Event::HostAccountLoginFinished { .. } = events.recv().await? {
                return Ok::<_, Box<dyn Error>>(());
            }
        }
    })
    .await??;
    configure(&harness.repo, &format!("host_account = {HOST_ACCOUNT:?}\n"))?;

    let task = enqueue(app, repository, "Add a greeting").await?;
    let done = wait_until(app, &mut events, repository, |queue| {
        completed(state(queue, task))
    })
    .await?;
    let chat = chat_of(found(&done, task)?)?;
    let Response::PullRequestOpened { pull_request } = app
        .execute(Command::OpenPullRequest {
            chat,
            title: "Add the greeting".to_owned(),
            body: String::new(),
        })
        .await?
    else {
        return Err("expected PullRequestOpened".into());
    };

    let ready = wait_until(app, &mut events, repository, |queue| {
        matches!(
            state(queue, task),
            Some(QueueItemState::PullRequestReady { .. })
        )
    })
    .await?;
    assert_eq!(
        found(&ready, task)?,
        &QueueItemState::PullRequestReady {
            chat,
            number: pull_request.number,
            url: pull_request.url,
        }
    );
    Ok(())
}

#[tokio::test]
async fn the_queue_survives_a_restart_and_a_cut_turn_needs_attention() -> TestResult {
    let gate = Gate::new()?;
    let harness = harness(&gated(&gate)).await?;
    let (app, repository) = (&harness.app, harness.repository);
    let mut events = app.subscribe();
    set_concurrency(app, repository, 1).await?;
    let first = enqueue(app, repository, "first").await?;
    wait_until(app, &mut events, repository, |queue| {
        running(state(queue, first))
    })
    .await?;
    pause(app, repository).await?;
    let second = enqueue(app, repository, "second").await?;
    let third = enqueue(app, repository, "third").await?;
    queue(app, QueueCommand::Move { item: third, to: 0 }).await?;
    let before = read(app, repository).await?;
    let chat = chat_of(found(&before, first)?)?;

    let service = harness.service.restart().await?;
    let app = service.connect();
    let after = read(&app, repository).await?;

    assert_eq!(after.status, QueueStatus::Paused);
    assert_eq!(after.settings, before.settings);
    let order: Vec<QueueItemId> = after.items.iter().map(|item| item.item).collect();
    assert_eq!(order, [first, third, second]);
    assert_eq!(
        found(&after, first)?,
        &QueueItemState::NeedsAttention {
            chat: Some(chat),
            reason: AttentionReason::ServiceRestarted,
        }
    );
    assert_eq!(found(&after, second)?, &QueueItemState::Queued);
    assert_eq!(found(&after, third)?, &QueueItemState::Queued);
    // New tasks get new ids after the restart.
    let fourth = enqueue(&app, repository, "fourth").await?;
    assert!(![first, second, third].contains(&fourth));
    Ok(())
}

#[tokio::test]
async fn queue_commands_refuse_what_cannot_be_done() -> TestResult {
    let gate = Gate::new()?;
    let harness = harness(&gated(&gate)).await?;
    let (app, repository) = (&harness.app, harness.repository);
    let mut events = app.subscribe();

    let refused = |error: QueueError| Err(CommandError::Queue { error });
    assert_eq!(
        queue(
            app,
            QueueCommand::Enqueue {
                repository,
                task: QueueTask::Prompt {
                    text: "  \n".to_owned()
                },
                agent_cli: None,
                permission_mode: None,
            }
        )
        .await,
        refused(QueueError::EmptyPrompt)
    );
    assert_eq!(
        enqueue_task(app, repository, QueueTask::Issue { number: 0 })
            .await
            .map_err(|error| error.to_string()),
        Err(QueueError::InvalidIssue.to_string())
    );
    for concurrency in [0, MAX_CONCURRENCY + 1] {
        assert_eq!(
            set_concurrency(app, repository, concurrency).await,
            refused(QueueError::InvalidConcurrency {
                concurrency,
                max: MAX_CONCURRENCY
            })
        );
    }
    let unknown: RepositoryId = serde_json::from_value(serde_json::json!(1000))?;
    assert_eq!(
        queue(
            app,
            QueueCommand::Read {
                repository: unknown
            }
        )
        .await,
        Err(CommandError::UnknownRepository {
            repository: unknown
        })
    );

    let running_task = enqueue(app, repository, "running").await?;
    wait_until(app, &mut events, repository, |queue| {
        running(state(queue, running_task))
    })
    .await?;
    assert_eq!(
        queue(
            app,
            QueueCommand::Move {
                item: running_task,
                to: 0
            }
        )
        .await,
        refused(QueueError::NotQueued { item: running_task })
    );
    assert_eq!(
        queue(app, QueueCommand::Remove { item: running_task }).await,
        refused(QueueError::StillActive { item: running_task })
    );
    gate.open()?;
    wait_until(app, &mut events, repository, |queue| {
        completed(state(queue, running_task))
    })
    .await?;
    assert_eq!(
        queue(app, QueueCommand::Cancel { item: running_task }).await,
        refused(QueueError::Finished { item: running_task })
    );
    Ok(())
}

#[tokio::test]
async fn queue_data_of_version_1_loads() -> TestResult {
    let data = tempfile::tempdir()?;
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/queue-v1");
    for file in ["state.json", "queue.json"] {
        std::fs::copy(fixture.join(file), data.path().join(file))?;
    }
    let driver = FakeDriver::new(Path::new(FAKE_AGENT_CLI), &Script::new())?;
    let service = leiser_service::start(
        ServiceConfig::new(
            AgentCli::ClaudeCode,
            Arc::new(driver),
            data.path().to_path_buf(),
        )
        .with_git_host(Arc::new(FakeGitHost::new())),
    )?;
    let app = service.connect();

    let Response::Repositories { repositories } = app.execute(Command::ListRepositories).await?
    else {
        return Err("expected Repositories".into());
    };
    let repository = repositories.first().ok_or("no repository")?.repository;
    let loaded = read(&app, repository).await?;

    assert_eq!(loaded.status, QueueStatus::Paused);
    assert_eq!(
        loaded.settings,
        QueueSettings {
            concurrency: 2,
            agent_cli: Some(AgentCli::Codex),
            permission_mode: Some(PermissionMode::FullAccess),
        }
    );
    let titles: Vec<&str> = loaded
        .items
        .iter()
        .map(|item| item.title.as_str())
        .collect();
    assert_eq!(
        titles,
        [
            "Write the changelog",
            "Issue #12",
            "Fix the flaky test",
            "Update the dependencies",
            "Rename the module",
            "Add the setting",
            "Drop the old flag",
        ]
    );
    let states: Vec<&QueueItemState> = loaded.items.iter().map(|item| &item.state).collect();
    assert!(matches!(states[0], QueueItemState::Queued));
    // A task that was starting is in line again.
    assert!(matches!(states[1], QueueItemState::Queued));
    assert!(matches!(
        states[2],
        QueueItemState::NeedsAttention {
            reason: AttentionReason::ServiceRestarted,
            ..
        }
    ));
    assert!(matches!(
        states[3],
        QueueItemState::NeedsAttention {
            reason: AttentionReason::UsageLimit {
                resets_in_seconds: None,
                ..
            },
            ..
        }
    ));
    assert!(matches!(states[4], QueueItemState::Completed { .. }));
    assert!(matches!(
        states[5],
        QueueItemState::PullRequestReady { number: 3, .. }
    ));
    assert!(matches!(
        states[6],
        QueueItemState::Cancelled { chat: None }
    ));
    assert_eq!(loaded.items[1].agent_cli, Some(AgentCli::ClaudeCode));
    assert_eq!(
        loaded.items[1].permission_mode,
        Some(PermissionMode::Supervised)
    );
    // Item ids go on after the stored last one.
    let next = enqueue(&app, repository, "next").await?;
    assert_eq!(next.to_string(), "8");
    service.shutdown().await;
    Ok(())
}
