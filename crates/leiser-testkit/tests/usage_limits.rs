//! Usage limits: a chat that hits one shows which limit and when it
//! resets, continues on another subscription with one click, and moves by
//! itself only between interchangeable subscriptions of one provider.

use std::error::Error;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use leiser_service::{
    AgentCli, AgentEvent, CLAUDE_CONFIG_DIR, CODEX_HOME, ChatId, ChatSettings, ChatStatus,
    Checkout, Command, CommandError, Event, LoginOutcome, Provider, Response, ServiceHandle,
    SignIn, SubscriptionId, TranscriptEntry, UsageLimit,
};
use leiser_testkit::{
    FAKE_USAGE_LIMIT_FILE, FakeDriver, FakeUsageLimit, Gate, Script, Step, TestRepo, TestService,
    collect_turn, next_event,
};
use tokio::sync::broadcast;

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

const FAKE_AGENT_CLI: &str = env!("CARGO_BIN_EXE_fake-agent-cli");
const LOGIN_TIMEOUT: Duration = Duration::from_secs(10);
const FIVE_HOUR_LIMIT: &str = "5-hour limit";
const AN_HOUR: u64 = 3600;

fn claude_service(script: &Script) -> TestResult<TestService> {
    Ok(TestService::start(Path::new(FAKE_AGENT_CLI), script)?)
}

fn codex_service(script: &Script) -> TestResult<TestService> {
    let driver =
        FakeDriver::new(Path::new(FAKE_AGENT_CLI), script)?.standing_in_for(AgentCli::Codex);
    Ok(TestService::with_driver(AgentCli::Codex, Arc::new(driver))?)
}

/// Chats start on Claude Code and can change to Codex.
fn claude_and_codex_service() -> TestResult<TestService> {
    let program = Path::new(FAKE_AGENT_CLI);
    let claude: Arc<dyn leiser_service::Driver> =
        Arc::new(FakeDriver::new(program, &Script::new())?);
    let codex: Arc<dyn leiser_service::Driver> =
        Arc::new(FakeDriver::new(program, &Script::new())?.standing_in_for(AgentCli::Codex));
    Ok(TestService::with_drivers(
        (AgentCli::ClaudeCode, claude),
        vec![(AgentCli::Codex, codex)],
    )?)
}

/// Adds a login subscription and waits until its login has signed in.
async fn add(
    service: &ServiceHandle,
    provider: Provider,
    name: &str,
) -> TestResult<SubscriptionId> {
    let mut events = service.subscribe();
    let response = service
        .execute(Command::AddSubscription {
            provider,
            name: name.to_owned(),
            sign_in: SignIn::Login,
        })
        .await?;
    let Response::SubscriptionAdded { subscription } = response else {
        return Err(format!("expected SubscriptionAdded, got {response:?}").into());
    };
    tokio::time::timeout(LOGIN_TIMEOUT, async {
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

async fn set_interchangeable(
    service: &ServiceHandle,
    subscription: SubscriptionId,
) -> Result<Response, CommandError> {
    service
        .execute(Command::SetInterchangeable {
            subscription,
            interchangeable: true,
        })
        .await
}

async fn home(service: &ServiceHandle, subscription: SubscriptionId) -> TestResult<PathBuf> {
    match service.execute(Command::ListSubscriptions).await? {
        Response::Subscriptions { subscriptions } => subscriptions
            .into_iter()
            .find(|overview| overview.subscription == subscription)
            .map(|overview| overview.home)
            .ok_or_else(|| format!("subscription {subscription:?} is not listed").into()),
        response => Err(format!("expected Subscriptions, got {response:?}").into()),
    }
}

/// Makes the subscription hit the 5-hour limit, which resets in an hour.
async fn hit_limit(service: &ServiceHandle, subscription: SubscriptionId) -> TestResult {
    let limit = FakeUsageLimit {
        limit: FIVE_HOUR_LIMIT.to_owned(),
        resets_in_seconds: Some(AN_HOUR),
    };
    std::fs::write(
        home(service, subscription)
            .await?
            .join(FAKE_USAGE_LIMIT_FILE),
        serde_json::to_vec(&limit)?,
    )?;
    Ok(())
}

/// A chat in the main checkout of a new repository whose default
/// subscription of `provider` is `name`.
async fn start_chat(
    service: &ServiceHandle,
    repo: &TestRepo,
    provider: Provider,
    name: &str,
) -> TestResult<ChatId> {
    let dir = repo.path().join(".leiser");
    std::fs::create_dir_all(&dir)?;
    let key = match provider {
        Provider::Anthropic => "anthropic",
        Provider::OpenAi => "openai",
    };
    std::fs::write(
        dir.join("config.toml"),
        format!("[subscriptions]\n{key} = \"{name}\"\n"),
    )?;
    let Response::RepositoryAdded { repository } = service
        .execute(Command::AddRepository {
            path: repo.path().to_path_buf(),
        })
        .await?
    else {
        return Err("expected RepositoryAdded".into());
    };
    match service
        .execute(Command::StartChat {
            repository,
            checkout: Checkout::Main,
            settings: ChatSettings::default(),
        })
        .await?
    {
        Response::ChatStarted { chat } => Ok(chat),
        response => Err(format!("expected ChatStarted, got {response:?}").into()),
    }
}

async fn send(service: &ServiceHandle, chat: ChatId, text: &str) -> TestResult {
    service
        .execute(Command::SendMessage {
            chat,
            text: text.to_owned(),
        })
        .await?;
    Ok(())
}

/// The texts of the turn's messages.
async fn messages(
    events: &mut broadcast::Receiver<Event>,
    chat: ChatId,
) -> TestResult<Vec<String>> {
    Ok(collect_turn(events, chat)
        .await?
        .into_iter()
        .filter_map(|event| match event {
            AgentEvent::Message { text } => Some(text),
            _ => None,
        })
        .collect())
}

/// The chat's next event that says it continued on another subscription.
async fn continued(
    events: &mut broadcast::Receiver<Event>,
    chat: ChatId,
) -> TestResult<(Option<SubscriptionId>, bool)> {
    loop {
        if let Event::ContinuedOnSubscription {
            subscription,
            automatic,
            ..
        } = next_event(events, chat).await?
        {
            return Ok((subscription, automatic));
        }
    }
}

struct ChatState {
    status: ChatStatus,
    subscription: Option<SubscriptionId>,
    usage_limit: Option<UsageLimit>,
    transcript: Vec<TranscriptEntry>,
}

async fn read_chat(service: &ServiceHandle, chat: ChatId) -> TestResult<ChatState> {
    match service.execute(Command::ReadChat { chat }).await? {
        Response::Chat {
            status,
            subscription,
            usage_limit,
            transcript,
            ..
        } => Ok(ChatState {
            status,
            subscription,
            usage_limit,
            transcript,
        }),
        response => Err(format!("expected Chat, got {response:?}").into()),
    }
}

async fn continue_on(
    service: &ServiceHandle,
    chat: ChatId,
    subscription: Option<SubscriptionId>,
) -> Result<Response, CommandError> {
    service
        .execute(Command::ContinueOnSubscription { chat, subscription })
        .await
}

fn report(variable: &str) -> Step {
    Step::ReportEnvironment {
        variable: variable.to_owned(),
    }
}

fn developer_messages(transcript: &[TranscriptEntry]) -> Vec<&str> {
    transcript
        .iter()
        .filter_map(|entry| match entry {
            TranscriptEntry::UserMessage { text } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn a_usage_limit_is_shown_with_the_limit_and_its_reset_time() -> TestResult {
    let harness = claude_service(&Script::new())?;
    let service = harness.connect();
    let private = add(&service, Provider::Anthropic, "Private").await?;
    let repo = TestRepo::new()?;
    let chat = start_chat(&service, &repo, Provider::Anthropic, "Private").await?;
    hit_limit(&service, private).await?;
    let mut events = service.subscribe();

    send(&service, chat, "fix the bug").await?;
    let turn = collect_turn(&mut events, chat).await?;

    assert_eq!(
        turn,
        vec![
            AgentEvent::UsageLimitReached {
                limit: FIVE_HOUR_LIMIT.to_owned(),
                resets_in_seconds: Some(AN_HOUR),
            },
            AgentEvent::TurnEnded,
        ]
    );
    let state = read_chat(&service, chat).await?;
    assert_eq!(state.status, ChatStatus::WaitingForYou);
    let Some(limit) = state.usage_limit else {
        return Err("the chat shows no usage limit".into());
    };
    assert_eq!(limit.limit, FIVE_HOUR_LIMIT);
    assert_eq!(limit.subscription, Some(private));
    // Counted from now; the test may take a moment.
    let resets = limit.resets_in_seconds.ok_or("no reset time")?;
    assert!(
        (AN_HOUR - 60..=AN_HOUR).contains(&resets),
        "resets in {resets}"
    );
    Ok(())
}

#[tokio::test]
async fn one_click_continues_a_claude_chat_in_a_new_agent_session_with_a_handoff() -> TestResult {
    let reply = [report(CLAUDE_CONFIG_DIR), Step::EchoUserMessage];
    let harness = claude_service(&Script::new().turn(reply.clone()).turn(reply))?;
    let service = harness.connect();
    let private = add(&service, Provider::Anthropic, "Private").await?;
    let work = add(&service, Provider::Anthropic, "Work").await?;
    let repo = TestRepo::new()?;
    let chat = start_chat(&service, &repo, Provider::Anthropic, "Private").await?;
    hit_limit(&service, private).await?;
    let mut events = service.subscribe();
    send(&service, chat, "fix the bug").await?;
    collect_turn(&mut events, chat).await?;

    let response = continue_on(&service, chat, Some(work)).await?;

    assert_eq!(response, Response::ContinuedOnSubscription);
    assert_eq!(continued(&mut events, chat).await?, (Some(work), false));
    let messages = messages(&mut events, chat).await?;
    assert_eq!(
        messages[0],
        home(&service, work).await?.display().to_string()
    );
    // A new agent session: it gets the chat so far, with the developer's
    // request the limit stopped, and is told to continue.
    let continuation = &messages[1];
    assert!(continuation.starts_with("<handoff>"), "{continuation}");
    assert!(continuation.contains("fix the bug"), "{continuation}");
    assert!(continuation.contains("usage limit"), "{continuation}");
    let state = read_chat(&service, chat).await?;
    assert_eq!(state.subscription, Some(work));
    assert_eq!(state.usage_limit, None);
    // The developer wrote once; the continuation is the harness's own.
    assert_eq!(developer_messages(&state.transcript), vec!["fix the bug"]);
    assert!(
        state
            .transcript
            .contains(&TranscriptEntry::ContinuedOnSubscription {
                provider: Provider::Anthropic,
                subscription: Some(work),
                automatic: false,
            })
    );
    Ok(())
}

#[tokio::test]
async fn one_click_continues_a_codex_chat_in_the_same_agent_session() -> TestResult {
    let script = Script::new()
        .turn([Step::message("first turn")])
        .turn([report(CODEX_HOME), Step::EchoUserMessage]);
    let harness = codex_service(&script)?;
    let service = harness.connect();
    let private = add(&service, Provider::OpenAi, "Private").await?;
    let work = add(&service, Provider::OpenAi, "Work").await?;
    let repo = TestRepo::new()?;
    let chat = start_chat(&service, &repo, Provider::OpenAi, "Private").await?;
    let mut events = service.subscribe();
    send(&service, chat, "one").await?;
    assert_eq!(messages(&mut events, chat).await?, vec!["first turn"]);
    hit_limit(&service, private).await?;
    send(&service, chat, "two").await?;
    collect_turn(&mut events, chat).await?;

    continue_on(&service, chat, Some(work)).await?;

    assert_eq!(continued(&mut events, chat).await?, (Some(work), false));
    let messages = messages(&mut events, chat).await?;
    // The same agent session plays its second turn, on the new
    // subscription, and needs no handoff.
    assert_eq!(
        messages[0],
        home(&service, work).await?.display().to_string()
    );
    assert!(!messages[1].contains("<handoff>"), "{}", messages[1]);
    assert!(messages[1].contains("usage limit"), "{}", messages[1]);
    Ok(())
}

#[tokio::test]
async fn continuing_needs_a_turn_that_stopped_at_a_usage_limit() -> TestResult {
    let harness = claude_service(&Script::new())?;
    let service = harness.connect();
    add(&service, Provider::Anthropic, "Private").await?;
    let work = add(&service, Provider::Anthropic, "Work").await?;
    let repo = TestRepo::new()?;
    let chat = start_chat(&service, &repo, Provider::Anthropic, "Private").await?;
    let mut events = service.subscribe();
    send(&service, chat, "one").await?;
    collect_turn(&mut events, chat).await?;

    let result = continue_on(&service, chat, Some(work)).await;

    assert_eq!(result, Err(CommandError::NoUsageLimit { chat }));
    Ok(())
}

#[tokio::test]
async fn continuing_on_a_subscription_of_another_provider_is_refused() -> TestResult {
    let harness = claude_and_codex_service()?;
    let service = harness.connect();
    let private = add(&service, Provider::Anthropic, "Private").await?;
    let codex = add(&service, Provider::OpenAi, "Codex").await?;
    let repo = TestRepo::new()?;
    let chat = start_chat(&service, &repo, Provider::Anthropic, "Private").await?;
    hit_limit(&service, private).await?;
    let mut events = service.subscribe();
    send(&service, chat, "one").await?;
    collect_turn(&mut events, chat).await?;

    let result = continue_on(&service, chat, Some(codex)).await;

    assert_eq!(
        result,
        Err(CommandError::WrongProvider {
            subscription: codex,
            provider: Provider::Anthropic,
        })
    );
    Ok(())
}

#[tokio::test]
async fn a_chat_moves_by_itself_between_interchangeable_subscriptions() -> TestResult {
    let reply = [report(CLAUDE_CONFIG_DIR), Step::EchoUserMessage];
    let harness = claude_service(&Script::new().turn(reply))?;
    let service = harness.connect();
    let private = add(&service, Provider::Anthropic, "Private").await?;
    let second = add(&service, Provider::Anthropic, "Private 2").await?;
    set_interchangeable(&service, private).await?;
    set_interchangeable(&service, second).await?;
    let repo = TestRepo::new()?;
    let chat = start_chat(&service, &repo, Provider::Anthropic, "Private").await?;
    hit_limit(&service, private).await?;
    let mut events = service.subscribe();

    send(&service, chat, "fix the bug").await?;

    let limited = collect_turn(&mut events, chat).await?;
    assert!(matches!(limited[0], AgentEvent::UsageLimitReached { .. }));
    assert_eq!(continued(&mut events, chat).await?, (Some(second), true));
    let messages = messages(&mut events, chat).await?;
    assert_eq!(
        messages[0],
        home(&service, second).await?.display().to_string()
    );
    assert!(messages[1].contains("fix the bug"), "{}", messages[1]);
    let state = read_chat(&service, chat).await?;
    assert_eq!(state.subscription, Some(second));
    assert_eq!(state.usage_limit, None);
    Ok(())
}

#[tokio::test]
async fn a_chat_does_not_move_by_itself_to_a_subscription_not_marked_interchangeable() -> TestResult
{
    let harness = claude_service(&Script::new())?;
    let service = harness.connect();
    let private = add(&service, Provider::Anthropic, "Private").await?;
    add(&service, Provider::Anthropic, "Work").await?;
    set_interchangeable(&service, private).await?;
    let repo = TestRepo::new()?;
    let chat = start_chat(&service, &repo, Provider::Anthropic, "Private").await?;
    hit_limit(&service, private).await?;
    let mut events = service.subscribe();

    send(&service, chat, "fix the bug").await?;
    collect_turn(&mut events, chat).await?;

    // The switch would have been decided before the next command runs.
    let state = read_chat(&service, chat).await?;
    assert_eq!(state.subscription, Some(private));
    assert_eq!(state.status, ChatStatus::WaitingForYou);
    assert!(state.usage_limit.is_some());
    Ok(())
}

#[tokio::test]
async fn a_chat_does_not_move_by_itself_away_from_a_subscription_not_marked_interchangeable()
-> TestResult {
    let harness = claude_service(&Script::new())?;
    let service = harness.connect();
    let work = add(&service, Provider::Anthropic, "Work").await?;
    let private = add(&service, Provider::Anthropic, "Private").await?;
    set_interchangeable(&service, private).await?;
    let repo = TestRepo::new()?;
    let chat = start_chat(&service, &repo, Provider::Anthropic, "Work").await?;
    hit_limit(&service, work).await?;
    let mut events = service.subscribe();

    send(&service, chat, "fix the bug").await?;
    collect_turn(&mut events, chat).await?;

    let state = read_chat(&service, chat).await?;
    assert_eq!(state.subscription, Some(work));
    assert!(state.usage_limit.is_some());
    Ok(())
}

#[tokio::test]
async fn a_chat_never_moves_by_itself_to_another_provider() -> TestResult {
    let harness = claude_and_codex_service()?;
    let service = harness.connect();
    let private = add(&service, Provider::Anthropic, "Private").await?;
    let codex = add(&service, Provider::OpenAi, "Codex").await?;
    set_interchangeable(&service, private).await?;
    set_interchangeable(&service, codex).await?;
    let repo = TestRepo::new()?;
    let chat = start_chat(&service, &repo, Provider::Anthropic, "Private").await?;
    hit_limit(&service, private).await?;
    let mut events = service.subscribe();

    send(&service, chat, "fix the bug").await?;
    collect_turn(&mut events, chat).await?;

    let state = read_chat(&service, chat).await?;
    assert_eq!(state.subscription, Some(private));
    assert!(state.usage_limit.is_some());
    Ok(())
}

#[tokio::test]
async fn when_every_interchangeable_subscription_is_limited_the_chat_waits() -> TestResult {
    let harness = claude_service(&Script::new())?;
    let service = harness.connect();
    let private = add(&service, Provider::Anthropic, "Private").await?;
    let second = add(&service, Provider::Anthropic, "Private 2").await?;
    set_interchangeable(&service, private).await?;
    set_interchangeable(&service, second).await?;
    let repo = TestRepo::new()?;
    let chat = start_chat(&service, &repo, Provider::Anthropic, "Private").await?;
    hit_limit(&service, private).await?;
    hit_limit(&service, second).await?;
    let mut events = service.subscribe();

    send(&service, chat, "fix the bug").await?;
    collect_turn(&mut events, chat).await?;
    assert_eq!(continued(&mut events, chat).await?, (Some(second), true));
    collect_turn(&mut events, chat).await?;

    // It does not move back to the subscription that hit its limit first.
    let state = read_chat(&service, chat).await?;
    assert_eq!(state.subscription, Some(second));
    assert_eq!(state.status, ChatStatus::WaitingForYou);
    let limit = state.usage_limit.ok_or("the chat shows no usage limit")?;
    assert_eq!(limit.subscription, Some(second));
    Ok(())
}

#[tokio::test]
async fn messages_queued_when_a_limit_is_hit_wait_for_the_chat_to_continue() -> TestResult {
    let gate = Gate::new()?;
    let script = Script::new()
        .turn([Step::message("started"), gate.step(), Step::EchoUserMessage])
        .turn([Step::EchoUserMessage]);
    let harness = claude_service(&script)?;
    let service = harness.connect();
    let private = add(&service, Provider::Anthropic, "Private").await?;
    let work = add(&service, Provider::Anthropic, "Work").await?;
    let repo = TestRepo::new()?;
    let chat = start_chat(&service, &repo, Provider::Anthropic, "Private").await?;
    let mut events = service.subscribe();
    send(&service, chat, "one").await?;
    for text in ["two", "three"] {
        service
            .execute(Command::QueueMessage {
                chat,
                text: text.to_owned(),
            })
            .await?;
    }
    // The first turn has begun, before the limit.
    loop {
        if let Event::Agent {
            event: AgentEvent::Message { text },
            ..
        } = next_event(&mut events, chat).await?
            && text == "started"
        {
            break;
        }
    }
    hit_limit(&service, private).await?;
    gate.open()?;
    assert_eq!(messages(&mut events, chat).await?, vec!["one"]);
    // "two" goes out and stops at the limit.
    collect_turn(&mut events, chat).await?;

    let state = read_chat(&service, chat).await?;
    assert_eq!(developer_messages(&state.transcript), vec!["one", "two"]);

    continue_on(&service, chat, Some(work)).await?;
    let continuation = messages(&mut events, chat).await?;
    // A new agent session plays the first turn again; its gate is open.
    assert_eq!(continuation[0], "started");
    assert!(continuation[1].contains("usage limit"), "{continuation:?}");
    assert_eq!(messages(&mut events, chat).await?, vec!["three"]);
    let state = read_chat(&service, chat).await?;
    assert_eq!(
        developer_messages(&state.transcript),
        vec!["one", "two", "three"]
    );
    Ok(())
}

#[tokio::test]
async fn interchangeable_subscriptions_and_a_usage_limit_survive_a_restart() -> TestResult {
    let harness = claude_service(&Script::new())?;
    let service = harness.connect();
    let private = add(&service, Provider::Anthropic, "Private").await?;
    let work = add(&service, Provider::Anthropic, "Work").await?;
    set_interchangeable(&service, private).await?;
    let repo = TestRepo::new()?;
    let chat = start_chat(&service, &repo, Provider::Anthropic, "Private").await?;
    hit_limit(&service, private).await?;
    let mut events = service.subscribe();
    send(&service, chat, "fix the bug").await?;
    collect_turn(&mut events, chat).await?;

    let harness = harness.restart().await?;
    let service = harness.connect();

    let Response::Subscriptions { subscriptions } =
        service.execute(Command::ListSubscriptions).await?
    else {
        return Err("expected Subscriptions".into());
    };
    let interchangeable: Vec<_> = subscriptions
        .iter()
        .map(|overview| (overview.subscription, overview.interchangeable))
        .collect();
    assert_eq!(interchangeable, vec![(private, true), (work, false)]);
    let limit = read_chat(&service, chat)
        .await?
        .usage_limit
        .ok_or("the usage limit is gone")?;
    assert_eq!(limit.limit, FIVE_HOUR_LIMIT);
    assert_eq!(
        continue_on(&service, chat, Some(work)).await?,
        Response::ContinuedOnSubscription
    );
    Ok(())
}
