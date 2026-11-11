//! Subscriptions: several per provider, signed in by the agent CLI's own
//! login in a config directory of their own or by an API key in the
//! keychain, chosen per repository and changed per turn.

use std::error::Error;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use leiser_service::{
    AgentCli, AgentEvent, ApiKey, CLAUDE_CONFIG_DIR, CODEX_HOME, ChatId, Checkout, Command,
    CommandError, Event, LoginAnswer, LoginOutcome, Provider, Response, ServiceHandle, SignIn,
    SubscriptionId, SubscriptionKind, SubscriptionOverview,
};
use leiser_testkit::{
    FAKE_LOGIN_FILE, FakeDriver, FakeLogin, Script, Step, TestRepo, TestService, UNSET,
    collect_turn,
};
use tokio::sync::broadcast;

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

const FAKE_AGENT_CLI: &str = env!("CARGO_BIN_EXE_fake-agent-cli");
const LOGIN_TIMEOUT: Duration = Duration::from_secs(10);
const API_KEY: &str = "sk-ant-test-0123456789";

/// A harness service whose chats run on Claude Code, played by the fake
/// agent CLI.
fn claude_service(script: &Script) -> TestResult<TestService> {
    Ok(TestService::start(Path::new(FAKE_AGENT_CLI), script)?)
}

/// A harness service whose chats run on Codex, played by the fake agent CLI.
fn codex_service(script: &Script) -> TestResult<TestService> {
    let driver =
        FakeDriver::new(Path::new(FAKE_AGENT_CLI), script)?.standing_in_for(AgentCli::Codex);
    Ok(TestService::with_driver(AgentCli::Codex, Arc::new(driver))?)
}

async fn add(
    service: &ServiceHandle,
    provider: Provider,
    name: &str,
    sign_in: SignIn,
) -> TestResult<SubscriptionId> {
    let response = service
        .execute(Command::AddSubscription {
            provider,
            name: name.to_owned(),
            sign_in,
        })
        .await?;
    let Response::SubscriptionAdded { subscription } = response else {
        return Err(format!("expected SubscriptionAdded, got {response:?}").into());
    };
    Ok(subscription)
}

/// Adds a login subscription and waits until its login has signed in.
async fn add_signed_in(
    service: &ServiceHandle,
    provider: Provider,
    name: &str,
) -> TestResult<SubscriptionId> {
    let mut events = service.subscribe();
    let subscription = add(service, provider, name, SignIn::Login).await?;
    let (_, outcome) = login(&mut events, subscription).await?;
    assert_eq!(outcome, LoginOutcome::SignedIn);
    Ok(subscription)
}

fn api_key(key: &str) -> SignIn {
    SignIn::ApiKey {
        api_key: ApiKey::new(key),
    }
}

/// The subscription's login output and how the login ended.
async fn login(
    events: &mut broadcast::Receiver<Event>,
    subscription: SubscriptionId,
) -> TestResult<(Vec<String>, LoginOutcome)> {
    let wait = async {
        let mut output = Vec::new();
        loop {
            match events.recv().await? {
                Event::LoginOutput {
                    subscription: from,
                    text,
                } if from == subscription => output.push(text),
                Event::LoginFinished {
                    subscription: from,
                    outcome,
                } if from == subscription => return Ok::<_, Box<dyn Error>>((output, outcome)),
                _ => {}
            }
        }
    };
    tokio::time::timeout(LOGIN_TIMEOUT, wait)
        .await
        .map_err(|_| "the login did not end in time")?
}

async fn subscriptions(service: &ServiceHandle) -> TestResult<Vec<SubscriptionOverview>> {
    match service.execute(Command::ListSubscriptions).await? {
        Response::Subscriptions { subscriptions } => Ok(subscriptions),
        response => Err(format!("expected Subscriptions, got {response:?}").into()),
    }
}

async fn overview(
    service: &ServiceHandle,
    subscription: SubscriptionId,
) -> TestResult<SubscriptionOverview> {
    subscriptions(service)
        .await?
        .into_iter()
        .find(|overview| overview.subscription == subscription)
        .ok_or_else(|| format!("subscription {subscription:?} is not listed").into())
}

async fn home(service: &ServiceHandle, subscription: SubscriptionId) -> TestResult<PathBuf> {
    Ok(overview(service, subscription).await?.home)
}

fn set_defaults(repo: &TestRepo, config: &str) -> TestResult {
    let dir = repo.path().join(".leiser");
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("config.toml"), config)?;
    Ok(())
}

async fn start_chat(service: &ServiceHandle, repo: &TestRepo) -> Result<ChatId, CommandError> {
    let Response::RepositoryAdded { repository } = service
        .execute(Command::AddRepository {
            path: repo.path().to_path_buf(),
        })
        .await?
    else {
        return Err(CommandError::ServiceStopped);
    };
    match service
        .execute(Command::StartChat {
            repository,
            checkout: Checkout::Main,
        })
        .await?
    {
        Response::ChatStarted { chat } => Ok(chat),
        _ => Err(CommandError::ServiceStopped),
    }
}

/// Sends a message and returns the texts of the turn's messages.
async fn turn(
    service: &ServiceHandle,
    events: &mut broadcast::Receiver<Event>,
    chat: ChatId,
    text: &str,
) -> TestResult<Vec<String>> {
    service
        .execute(Command::SendMessage {
            chat,
            text: text.to_owned(),
        })
        .await?;
    Ok(collect_turn(events, chat)
        .await?
        .into_iter()
        .filter_map(|event| match event {
            AgentEvent::Message { text } => Some(text),
            _ => None,
        })
        .collect())
}

async fn chat_subscription(
    service: &ServiceHandle,
    chat: ChatId,
) -> TestResult<Option<SubscriptionId>> {
    match service.execute(Command::ReadChat { chat }).await? {
        Response::Chat { subscription, .. } => Ok(subscription),
        response => Err(format!("expected Chat, got {response:?}").into()),
    }
}

async fn change(
    service: &ServiceHandle,
    chat: ChatId,
    provider: Provider,
    subscription: Option<SubscriptionId>,
) -> Result<Response, CommandError> {
    service
        .execute(Command::ChangeSubscription {
            chat,
            provider,
            subscription,
        })
        .await
}

fn report(variable: &str) -> Step {
    Step::ReportEnvironment {
        variable: variable.to_owned(),
    }
}

/// Every file below `dir`, links not followed.
fn files(dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        if kind.is_dir() {
            found.extend(files(&entry.path())?);
        } else if kind.is_file() {
            found.push(entry.path());
        }
    }
    Ok(found)
}

#[tokio::test]
async fn subscriptions_are_added_named_and_removed_per_provider() -> TestResult {
    let harness = claude_service(&Script::new())?;
    let service = harness.connect();

    let private = add(
        &service,
        Provider::Anthropic,
        "Claude Max private",
        SignIn::Login,
    )
    .await?;
    let work = add(
        &service,
        Provider::Anthropic,
        "Claude Team work",
        api_key(API_KEY),
    )
    .await?;
    let openai = add(
        &service,
        Provider::OpenAi,
        "OpenAI API",
        api_key("sk-openai"),
    )
    .await?;
    service
        .execute(Command::RenameSubscription {
            subscription: work,
            name: "Claude API work".to_owned(),
        })
        .await?;
    service
        .execute(Command::RemoveSubscription {
            subscription: private,
        })
        .await?;

    let listed: Vec<_> = subscriptions(&service)
        .await?
        .into_iter()
        .map(|overview| {
            (
                overview.subscription,
                overview.provider,
                overview.name,
                overview.kind,
            )
        })
        .collect();
    assert_eq!(
        listed,
        vec![
            (
                work,
                Provider::Anthropic,
                "Claude API work".to_owned(),
                SubscriptionKind::ApiKey
            ),
            (
                openai,
                Provider::OpenAi,
                "OpenAI API".to_owned(),
                SubscriptionKind::ApiKey
            ),
        ]
    );
    Ok(())
}

#[tokio::test]
async fn a_name_is_unique_per_provider() -> TestResult {
    let harness = claude_service(&Script::new())?;
    let service = harness.connect();
    add(&service, Provider::Anthropic, "Work", api_key(API_KEY)).await?;

    let same_provider = service
        .execute(Command::AddSubscription {
            provider: Provider::Anthropic,
            name: "Work".to_owned(),
            sign_in: api_key("sk-ant-other"),
        })
        .await;
    let other_provider = add(&service, Provider::OpenAi, "Work", api_key("sk-openai")).await;

    assert_eq!(
        same_provider,
        Err(CommandError::SubscriptionNameTaken {
            provider: Provider::Anthropic,
            name: "Work".to_owned()
        })
    );
    assert!(other_provider.is_ok(), "{other_provider:?}");
    Ok(())
}

#[tokio::test]
async fn adding_a_login_subscription_runs_the_login_in_its_own_claude_config_directory()
-> TestResult {
    let harness = claude_service(&Script::new())?;
    let service = harness.connect();
    let mut events = service.subscribe();

    let private = add(&service, Provider::Anthropic, "Private", SignIn::Login).await?;
    let (output, outcome) = login(&mut events, private).await?;
    let work = add_signed_in(&service, Provider::Anthropic, "Work").await?;

    assert_eq!(outcome, LoginOutcome::SignedIn);
    // The developer sees where to sign in.
    assert_eq!(output, vec!["Open https://login.invalid/fake to sign in."]);
    let (private_home, work_home) = (home(&service, private).await?, home(&service, work).await?);
    assert_ne!(private_home, work_home);
    for home in [&private_home, &work_home] {
        assert!(home.starts_with(harness.data_dir()), "{}", home.display());
        let login: FakeLogin = serde_json::from_slice(&std::fs::read(home.join(FAKE_LOGIN_FILE))?)?;
        assert_eq!(login.variable, CLAUDE_CONFIG_DIR);
    }
    Ok(())
}

#[tokio::test]
async fn a_login_that_asks_for_a_code_gets_the_developers_answer() -> TestResult {
    let harness = claude_service(&Script::new().login_asking_for_code())?;
    let service = harness.connect();
    let mut events = service.subscribe();
    let subscription = add(&service, Provider::Anthropic, "Private", SignIn::Login).await?;
    // The prompt has no line end, yet the developer must see it.
    let mut seen: Vec<String> = Vec::new();
    while !seen.iter().any(|line| line.starts_with("Paste code here")) {
        match tokio::time::timeout(LOGIN_TIMEOUT, events.recv()).await?? {
            Event::LoginOutput { text, .. } => seen.push(text),
            Event::LoginFinished { outcome, .. } => {
                return Err(format!("the login ended before it asked: {outcome:?}").into());
            }
            _ => {}
        }
    }

    let response = service
        .execute(Command::AnswerLogin {
            subscription,
            answer: LoginAnswer::new("code-123#state"),
        })
        .await?;

    assert_eq!(response, Response::LoginAnswered);
    assert_eq!(
        login(&mut events, subscription).await?.1,
        LoginOutcome::SignedIn
    );
    let home = home(&service, subscription).await?;
    let login: FakeLogin = serde_json::from_slice(&std::fs::read(home.join(FAKE_LOGIN_FILE))?)?;
    assert_eq!(login.code.as_deref(), Some("code-123#state"));
    Ok(())
}

#[tokio::test]
async fn answering_without_a_running_login_fails() -> TestResult {
    let harness = claude_service(&Script::new())?;
    let service = harness.connect();
    let subscription = add_signed_in(&service, Provider::Anthropic, "Private").await?;

    let result = service
        .execute(Command::AnswerLogin {
            subscription,
            answer: LoginAnswer::new("code"),
        })
        .await;

    assert_eq!(result, Err(CommandError::NoLoginRunning { subscription }));
    Ok(())
}

#[tokio::test]
async fn a_failed_login_is_reported() -> TestResult {
    let harness = claude_service(&Script::new().failing_login())?;
    let service = harness.connect();
    let mut events = service.subscribe();

    let subscription = add(&service, Provider::Anthropic, "Private", SignIn::Login).await?;

    let (_, outcome) = login(&mut events, subscription).await?;
    assert!(
        matches!(outcome, LoginOutcome::Failed { .. }),
        "expected a failed login, got {outcome:?}"
    );
    Ok(())
}

#[tokio::test]
async fn a_login_can_run_again() -> TestResult {
    let harness = claude_service(&Script::new())?;
    let service = harness.connect();
    let subscription = add_signed_in(&service, Provider::Anthropic, "Private").await?;
    let mut events = service.subscribe();

    let response = service.execute(Command::LogIn { subscription }).await?;

    assert_eq!(response, Response::LoginStarted);
    assert_eq!(
        login(&mut events, subscription).await?.1,
        LoginOutcome::SignedIn
    );
    Ok(())
}

#[tokio::test]
async fn an_api_key_is_kept_in_the_keychain_and_never_in_a_file() -> TestResult {
    let harness = claude_service(&Script::new())?;
    let service = harness.connect();

    add(&service, Provider::Anthropic, "Work", api_key(API_KEY)).await?;

    let stored: Vec<_> = harness
        .keychain()
        .keys()
        .into_values()
        .map(|key| key.expose().to_owned())
        .collect();
    assert_eq!(stored, vec![API_KEY.to_owned()]);
    for file in files(harness.data_dir())? {
        let contents = std::fs::read(&file)?;
        assert!(
            !contents
                .windows(API_KEY.len())
                .any(|window| window == API_KEY.as_bytes()),
            "{} contains the API key",
            file.display()
        );
    }
    Ok(())
}

#[tokio::test]
async fn an_api_key_subscription_needs_a_key() -> TestResult {
    let harness = claude_service(&Script::new())?;
    let service = harness.connect();

    let result = service
        .execute(Command::AddSubscription {
            provider: Provider::Anthropic,
            name: "Work".to_owned(),
            sign_in: api_key("  "),
        })
        .await;

    assert_eq!(result, Err(CommandError::EmptyApiKey));
    Ok(())
}

#[tokio::test]
async fn a_new_chat_runs_on_the_repository_default_subscription() -> TestResult {
    let harness = claude_service(&Script::new().turn([report(CLAUDE_CONFIG_DIR)]))?;
    let service = harness.connect();
    add_signed_in(&service, Provider::Anthropic, "Private").await?;
    let work = add_signed_in(&service, Provider::Anthropic, "Work").await?;
    let repo = TestRepo::new()?;
    set_defaults(&repo, "[subscriptions]\nanthropic = \"Work\"\n")?;
    let chat = start_chat(&service, &repo).await?;
    let mut events = service.subscribe();

    let messages = turn(&service, &mut events, chat, "hello").await?;

    assert_eq!(
        messages,
        vec![home(&service, work).await?.display().to_string()]
    );
    assert_eq!(chat_subscription(&service, chat).await?, Some(work));
    Ok(())
}

#[tokio::test]
async fn an_api_key_reaches_the_agent_session_as_an_environment_variable() -> TestResult {
    let script = Script::new().turn([report("ANTHROPIC_API_KEY"), report(CLAUDE_CONFIG_DIR)]);
    let harness = claude_service(&script)?;
    let service = harness.connect();
    let work = add(&service, Provider::Anthropic, "Work", api_key(API_KEY)).await?;
    let repo = TestRepo::new()?;
    set_defaults(&repo, "[subscriptions]\nanthropic = \"Work\"\n")?;
    let chat = start_chat(&service, &repo).await?;
    let mut events = service.subscribe();

    let messages = turn(&service, &mut events, chat, "hello").await?;

    assert_eq!(
        messages,
        vec![
            API_KEY.to_owned(),
            home(&service, work).await?.display().to_string()
        ]
    );
    Ok(())
}

#[tokio::test]
async fn a_login_subscription_gets_no_inherited_api_key() -> TestResult {
    let harness = claude_service(&Script::new().turn([report("ANTHROPIC_API_KEY")]))?;
    let service = harness.connect();
    add_signed_in(&service, Provider::Anthropic, "Private").await?;
    let repo = TestRepo::new()?;
    set_defaults(&repo, "[subscriptions]\nanthropic = \"Private\"\n")?;
    let chat = start_chat(&service, &repo).await?;
    let mut events = service.subscribe();

    let messages = turn(&service, &mut events, chat, "hello").await?;

    assert_eq!(messages, vec![UNSET.to_owned()]);
    Ok(())
}

#[tokio::test]
async fn a_chat_without_a_default_runs_on_the_agent_cli_default_login() -> TestResult {
    let harness = claude_service(&Script::new().turn([report(CLAUDE_CONFIG_DIR)]))?;
    let service = harness.connect();
    add_signed_in(&service, Provider::Anthropic, "Private").await?;
    let repo = TestRepo::new()?;
    let chat = start_chat(&service, &repo).await?;
    let mut events = service.subscribe();

    let messages = turn(&service, &mut events, chat, "hello").await?;

    // The agent CLI keeps whatever config directory the developer has.
    let inherited = std::env::var(CLAUDE_CONFIG_DIR).unwrap_or_else(|_| UNSET.to_owned());
    assert_eq!(messages, vec![inherited]);
    assert_eq!(chat_subscription(&service, chat).await?, None);
    Ok(())
}

#[tokio::test]
async fn a_default_that_names_no_subscription_fails_instead_of_using_another() -> TestResult {
    let harness = claude_service(&Script::new())?;
    let service = harness.connect();
    add_signed_in(&service, Provider::Anthropic, "Private").await?;
    let repo = TestRepo::new()?;
    set_defaults(&repo, "[subscriptions]\nanthropic = \"Work\"\n")?;

    let result = start_chat(&service, &repo).await;

    assert_eq!(
        result,
        Err(CommandError::UnknownSubscriptionName {
            provider: Provider::Anthropic,
            name: "Work".to_owned()
        })
    );
    Ok(())
}

#[tokio::test]
async fn changing_the_claude_subscription_continues_in_a_new_agent_session_with_a_handoff()
-> TestResult {
    let reply = [report(CLAUDE_CONFIG_DIR), Step::EchoUserMessage];
    let harness = claude_service(&Script::new().turn(reply.clone()).turn(reply))?;
    let service = harness.connect();
    let private = add_signed_in(&service, Provider::Anthropic, "Private").await?;
    let work = add_signed_in(&service, Provider::Anthropic, "Work").await?;
    let repo = TestRepo::new()?;
    set_defaults(&repo, "[subscriptions]\nanthropic = \"Private\"\n")?;
    let chat = start_chat(&service, &repo).await?;
    let mut events = service.subscribe();
    turn(&service, &mut events, chat, "one").await?;

    change(&service, chat, Provider::Anthropic, Some(work)).await?;
    let messages = turn(&service, &mut events, chat, "two").await?;

    assert_eq!(
        messages[0],
        home(&service, work).await?.display().to_string()
    );
    // A new agent session sees the chat so far as a handoff.
    assert!(messages[1].starts_with("<handoff>"), "{}", messages[1]);
    assert!(messages[1].ends_with("two"), "{}", messages[1]);
    assert_eq!(chat_subscription(&service, chat).await?, Some(work));
    assert_ne!(private, work);
    Ok(())
}

#[tokio::test]
async fn changing_the_codex_subscription_continues_the_same_agent_session() -> TestResult {
    let script = Script::new()
        .turn([report(CODEX_HOME), Step::message("first turn")])
        .turn([report(CODEX_HOME), Step::EchoUserMessage]);
    let harness = codex_service(&script)?;
    let service = harness.connect();
    let private = add_signed_in(&service, Provider::OpenAi, "Private").await?;
    let work = add_signed_in(&service, Provider::OpenAi, "Work").await?;
    let repo = TestRepo::new()?;
    set_defaults(&repo, "[subscriptions]\nopenai = \"Private\"\n")?;
    let chat = start_chat(&service, &repo).await?;
    let mut events = service.subscribe();
    let first = turn(&service, &mut events, chat, "one").await?;

    change(&service, chat, Provider::OpenAi, Some(work)).await?;
    let second = turn(&service, &mut events, chat, "two").await?;

    assert_eq!(
        first[0],
        home(&service, private).await?.display().to_string()
    );
    // The same agent session plays its second turn, without a handoff.
    assert_eq!(
        second,
        vec![
            home(&service, work).await?.display().to_string(),
            "two".to_owned()
        ]
    );
    Ok(())
}

#[tokio::test]
async fn a_codex_shadow_home_shares_the_codex_home_but_has_its_own_login() -> TestResult {
    let harness = codex_service(&Script::new())?;
    let shared = harness.codex_home();
    std::fs::write(shared.join("config.toml"), "model = \"gpt-5\"\n")?;
    std::fs::write(shared.join("auth.json"), "{\"default\": true}\n")?;
    let service = harness.connect();

    let work = add_signed_in(&service, Provider::OpenAi, "Work").await?;

    let shadow = home(&service, work).await?;
    assert!(
        shadow.starts_with(harness.data_dir()),
        "{}",
        shadow.display()
    );
    for shared_entry in ["config.toml", "sessions"] {
        assert_eq!(
            std::fs::read_link(shadow.join(shared_entry))?,
            shared.join(shared_entry)
        );
    }
    assert!(!shadow.join("auth.json").is_symlink());
    let login: FakeLogin = serde_json::from_slice(&std::fs::read(shadow.join(FAKE_LOGIN_FILE))?)?;
    assert_eq!(login.variable, CODEX_HOME);
    assert!(!shared.join(FAKE_LOGIN_FILE).exists());
    assert_eq!(
        std::fs::read_to_string(shared.join("auth.json"))?,
        "{\"default\": true}\n"
    );
    Ok(())
}

#[tokio::test]
async fn removing_a_subscription_deletes_its_key_and_config_directory() -> TestResult {
    let harness = claude_service(&Script::new())?;
    let service = harness.connect();
    let work = add(&service, Provider::Anthropic, "Work", api_key(API_KEY)).await?;
    let work_home = home(&service, work).await?;

    service
        .execute(Command::RemoveSubscription { subscription: work })
        .await?;

    assert!(harness.keychain().keys().is_empty());
    assert!(!work_home.exists());
    Ok(())
}

#[tokio::test]
async fn a_chat_on_a_removed_subscription_does_not_move_to_another_by_itself() -> TestResult {
    let harness = claude_service(&Script::new())?;
    let service = harness.connect();
    let work = add(&service, Provider::Anthropic, "Work", api_key(API_KEY)).await?;
    let repo = TestRepo::new()?;
    set_defaults(&repo, "[subscriptions]\nanthropic = \"Work\"\n")?;
    let chat = start_chat(&service, &repo).await?;
    service
        .execute(Command::RemoveSubscription { subscription: work })
        .await?;

    let result = service
        .execute(Command::SendMessage {
            chat,
            text: "hello".to_owned(),
        })
        .await;

    assert_eq!(
        result,
        Err(CommandError::UnknownSubscription { subscription: work })
    );
    Ok(())
}

#[tokio::test]
async fn a_subscription_of_another_provider_is_refused() -> TestResult {
    let harness = claude_service(&Script::new())?;
    let service = harness.connect();
    let openai = add(
        &service,
        Provider::OpenAi,
        "OpenAI API",
        api_key("sk-openai"),
    )
    .await?;
    let repo = TestRepo::new()?;
    let chat = start_chat(&service, &repo).await?;

    let result = change(&service, chat, Provider::Anthropic, Some(openai)).await;

    assert_eq!(
        result,
        Err(CommandError::WrongProvider {
            subscription: openai,
            provider: Provider::Anthropic
        })
    );
    Ok(())
}

#[tokio::test]
async fn subscriptions_and_a_chats_subscription_survive_a_restart() -> TestResult {
    let script = Script::new()
        .turn([report("ANTHROPIC_API_KEY")])
        .turn([report("ANTHROPIC_API_KEY")]);
    let harness = claude_service(&script)?;
    let service = harness.connect();
    let work = add(&service, Provider::Anthropic, "Work", api_key(API_KEY)).await?;
    let repo = TestRepo::new()?;
    let chat = start_chat(&service, &repo).await?;
    change(&service, chat, Provider::Anthropic, Some(work)).await?;

    let harness = harness.restart().await?;
    let service = harness.connect();
    let mut events = service.subscribe();
    let messages = turn(&service, &mut events, chat, "hello").await?;

    assert_eq!(overview(&service, work).await?.name, "Work");
    assert_eq!(chat_subscription(&service, chat).await?, Some(work));
    assert_eq!(messages, vec![API_KEY.to_owned()]);
    Ok(())
}

#[test]
fn an_api_key_never_shows_in_debug_output() {
    let command = Command::AddSubscription {
        provider: Provider::Anthropic,
        name: "Work".to_owned(),
        sign_in: api_key(API_KEY),
    };

    assert!(!format!("{command:?}").contains(API_KEY));
}
