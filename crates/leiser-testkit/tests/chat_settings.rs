//! What a new chat starts with: its repository's defaults, or what the
//! developer chose instead before the chat started (#88).

use std::error::Error;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use leiser_service::{
    AgentCli, AgentEvent, CODEX_HOME, ChatId, ChatSettings, ChatStatus, Checkout, Command,
    CommandError, Event, LoginOutcome, PermissionMode, Provider, RepositoryId, Response,
    ServiceHandle, SignIn, StartOn, SubscriptionId,
};
use leiser_testkit::{FakeDriver, Gate, Script, Step, TestRepo, TestService, collect_turn};

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

const FAKE_AGENT_CLI: &str = env!("CARGO_BIN_EXE_fake-agent-cli");
const LOGIN_TIMEOUT: Duration = Duration::from_secs(10);

/// A harness service whose chats run on Claude Code, played by the fake
/// agent CLI.
fn claude_service() -> TestResult<TestService> {
    Ok(TestService::start(
        Path::new(FAKE_AGENT_CLI),
        &Script::new(),
    )?)
}

/// A harness service whose chats start on Claude Code and can run on
/// Codex, both played by the fake agent CLI; Codex plays `codex`.
fn claude_and_codex_service(codex: &Script) -> TestResult<TestService> {
    let claude_code = FakeDriver::new(Path::new(FAKE_AGENT_CLI), &Script::new())?;
    let codex = FakeDriver::new(Path::new(FAKE_AGENT_CLI), codex)?.standing_in_for(AgentCli::Codex);
    Ok(TestService::with_drivers(
        (AgentCli::ClaudeCode, Arc::new(claude_code)),
        vec![(AgentCli::Codex, Arc::new(codex))],
    )?)
}

/// Adds a login subscription and waits until its login has signed in.
async fn add_signed_in(
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
    let signed_in = async {
        loop {
            if let Event::LoginFinished {
                subscription: from,
                outcome,
            } = events.recv().await?
                && from == subscription
            {
                return Ok::<_, Box<dyn Error>>(outcome);
            }
        }
    };
    let outcome = tokio::time::timeout(LOGIN_TIMEOUT, signed_in)
        .await
        .map_err(|_| "the login did not end in time")??;
    assert_eq!(outcome, LoginOutcome::SignedIn);
    Ok(subscription)
}

fn configure(repo: &TestRepo, config: &str) -> TestResult {
    let dir = repo.path().join(".leiser");
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("config.toml"), config)?;
    Ok(())
}

async fn add_repository(service: &ServiceHandle, repo: &TestRepo) -> TestResult<RepositoryId> {
    let response = service
        .execute(Command::AddRepository {
            path: repo.path().to_path_buf(),
        })
        .await?;
    let Response::RepositoryAdded { repository } = response else {
        return Err(format!("expected RepositoryAdded, got {response:?}").into());
    };
    Ok(repository)
}

async fn start_chat(
    service: &ServiceHandle,
    repository: RepositoryId,
    checkout: Checkout,
    settings: ChatSettings,
) -> Result<ChatId, CommandError> {
    match service
        .execute(Command::StartChat {
            repository,
            checkout,
            settings,
        })
        .await?
    {
        Response::ChatStarted { chat } => Ok(chat),
        _ => Err(CommandError::ServiceStopped),
    }
}

fn start_on(provider: Provider, subscription: Option<SubscriptionId>) -> ChatSettings {
    ChatSettings {
        start_on: Some(StartOn {
            provider,
            subscription,
        }),
        permission_mode: None,
    }
}

/// What the chat runs on: its agent CLI, subscription and permission mode.
async fn chat_settings(
    service: &ServiceHandle,
    chat: ChatId,
) -> TestResult<(AgentCli, Option<SubscriptionId>, PermissionMode)> {
    match service.execute(Command::ReadChat { chat }).await? {
        Response::Chat {
            agent_cli,
            subscription,
            permission_mode,
            ..
        } => Ok((agent_cli, subscription, permission_mode)),
        response => Err(format!("expected Chat, got {response:?}").into()),
    }
}

async fn chat_count(service: &ServiceHandle) -> TestResult<usize> {
    match service.execute(Command::ListChats).await? {
        Response::Chats { chats } => Ok(chats.len()),
        response => Err(format!("expected Chats, got {response:?}").into()),
    }
}

async fn status(service: &ServiceHandle, chat: ChatId) -> TestResult<ChatStatus> {
    match service.execute(Command::ListChats).await? {
        Response::Chats { chats } => chats
            .into_iter()
            .find(|overview| overview.chat == chat)
            .map(|overview| overview.status)
            .ok_or_else(|| "the chat is not listed".into()),
        response => Err(format!("expected Chats, got {response:?}").into()),
    }
}

#[tokio::test]
async fn a_new_chat_starts_on_the_chosen_agent_cli_and_subscription() -> TestResult {
    let codex = Script::new().turn([Step::ReportEnvironment {
        variable: CODEX_HOME.to_owned(),
    }]);
    let harness = claude_and_codex_service(&codex)?;
    let service = harness.connect();
    let work = add_signed_in(&service, Provider::OpenAi, "Codex work").await?;
    let repo = TestRepo::new()?;
    let repository = add_repository(&service, &repo).await?;

    let chat = start_chat(
        &service,
        repository,
        Checkout::Main,
        start_on(Provider::OpenAi, Some(work)),
    )
    .await?;

    assert_eq!(
        chat_settings(&service, chat).await?,
        (AgentCli::Codex, Some(work), PermissionMode::Auto)
    );
    // The first turn runs there, without a change of agent CLI before it.
    let mut events = service.subscribe();
    service
        .execute(Command::SendMessage {
            chat,
            text: "hello".to_owned(),
        })
        .await?;
    let messages: Vec<String> = collect_turn(&mut events, chat)
        .await?
        .into_iter()
        .filter_map(|event| match event {
            AgentEvent::Message { text } => Some(text),
            _ => None,
        })
        .collect();
    let Response::Subscriptions { subscriptions } =
        service.execute(Command::ListSubscriptions).await?
    else {
        return Err("expected Subscriptions".into());
    };
    let home = subscriptions
        .into_iter()
        .find(|overview| overview.subscription == work)
        .ok_or("the subscription is not listed")?
        .home;
    assert_eq!(messages, vec![home.display().to_string()]);
    Ok(())
}

#[tokio::test]
async fn a_chosen_default_login_replaces_the_repository_default_subscription() -> TestResult {
    let harness = claude_service()?;
    let service = harness.connect();
    add_signed_in(&service, Provider::Anthropic, "Work").await?;
    let repo = TestRepo::new()?;
    configure(&repo, "[subscriptions]\nanthropic = \"Work\"\n")?;
    let repository = add_repository(&service, &repo).await?;

    let chat = start_chat(
        &service,
        repository,
        Checkout::Main,
        start_on(Provider::Anthropic, None),
    )
    .await?;

    let (agent_cli, subscription, _) = chat_settings(&service, chat).await?;
    assert_eq!((agent_cli, subscription), (AgentCli::ClaudeCode, None));
    Ok(())
}

#[tokio::test]
async fn a_chosen_permission_mode_replaces_the_repository_default() -> TestResult {
    let harness = claude_service()?;
    let service = harness.connect();
    let repo = TestRepo::new()?;
    configure(&repo, "permission_mode = \"supervised\"\n")?;
    let repository = add_repository(&service, &repo).await?;

    let chat = start_chat(
        &service,
        repository,
        Checkout::Main,
        ChatSettings {
            start_on: None,
            permission_mode: Some(PermissionMode::FullAccess),
        },
    )
    .await?;

    let (_, _, permission_mode) = chat_settings(&service, chat).await?;
    assert_eq!(permission_mode, PermissionMode::FullAccess);
    Ok(())
}

#[tokio::test]
async fn the_chosen_settings_hold_while_the_worktrees_setup_script_runs() -> TestResult {
    let gate = Gate::new()?;
    let Step::WaitForGate { gate: gate_file } = gate.step() else {
        return Err("a gate waits at a file".into());
    };
    let harness = claude_and_codex_service(&Script::new())?;
    let service = harness.connect();
    let repo = TestRepo::with_remote()?;
    configure(
        &repo,
        &format!(
            "[worktree]\nsetup_script = 'while [ ! -e \"{}\" ]; do sleep 0.01; done'\n",
            gate_file.display()
        ),
    )?;
    let repository = add_repository(&service, &repo).await?;

    let chat = start_chat(
        &service,
        repository,
        Checkout::NewWorktree,
        ChatSettings {
            start_on: Some(StartOn {
                provider: Provider::OpenAi,
                subscription: None,
            }),
            permission_mode: Some(PermissionMode::Supervised),
        },
    )
    .await?;

    // The setup script still runs: a change of agent CLI would be refused now.
    assert_eq!(status(&service, chat).await?, ChatStatus::Working);
    assert_eq!(
        chat_settings(&service, chat).await?,
        (AgentCli::Codex, None, PermissionMode::Supervised)
    );
    gate.open()?;
    Ok(())
}

#[tokio::test]
async fn chat_defaults_are_the_repositorys() -> TestResult {
    let harness = claude_service()?;
    let service = harness.connect();
    let work = add_signed_in(&service, Provider::Anthropic, "Work").await?;
    let plain = TestRepo::new()?;
    let configured = TestRepo::new()?;
    configure(
        &configured,
        "permission_mode = \"supervised\"\n\n[subscriptions]\nanthropic = \"Work\"\n",
    )?;
    let plain = add_repository(&service, &plain).await?;
    let configured = add_repository(&service, &configured).await?;

    assert_eq!(
        service
            .execute(Command::ReadChatDefaults { repository: plain })
            .await?,
        Response::ChatDefaults {
            agent_cli: AgentCli::ClaudeCode,
            permission_mode: PermissionMode::Auto,
            subscription: None,
        }
    );
    assert_eq!(
        service
            .execute(Command::ReadChatDefaults {
                repository: configured
            })
            .await?,
        Response::ChatDefaults {
            agent_cli: AgentCli::ClaudeCode,
            permission_mode: PermissionMode::Supervised,
            subscription: Some(work),
        }
    );
    Ok(())
}

#[tokio::test]
async fn chat_defaults_of_an_invalid_config_fail_like_a_new_chat() -> TestResult {
    let harness = claude_service()?;
    let service = harness.connect();
    let repo = TestRepo::new()?;
    configure(&repo, "[subscriptions]\nanthropic = \"Nobody\"\n")?;
    let repository = add_repository(&service, &repo).await?;

    let defaults = service
        .execute(Command::ReadChatDefaults { repository })
        .await;
    let started = start_chat(
        &service,
        repository,
        Checkout::Main,
        ChatSettings::default(),
    )
    .await;

    assert!(
        matches!(defaults, Err(CommandError::UnknownSubscriptionName { .. })),
        "{defaults:?}"
    );
    assert_eq!(defaults.err(), started.err());
    Ok(())
}

#[tokio::test]
async fn a_chosen_agent_cli_that_is_not_configured_starts_no_chat() -> TestResult {
    let harness = claude_service()?;
    let service = harness.connect();
    let repo = TestRepo::new()?;
    let repository = add_repository(&service, &repo).await?;

    let result = start_chat(
        &service,
        repository,
        Checkout::Main,
        start_on(Provider::OpenAi, None),
    )
    .await;

    assert_eq!(
        result,
        Err(CommandError::AgentCliNotConfigured {
            agent_cli: AgentCli::Codex
        })
    );
    assert_eq!(chat_count(&service).await?, 0);
    Ok(())
}

#[tokio::test]
async fn a_chosen_subscription_of_another_provider_starts_no_chat() -> TestResult {
    let harness = claude_and_codex_service(&Script::new())?;
    let service = harness.connect();
    let anthropic = add_signed_in(&service, Provider::Anthropic, "Work").await?;
    let repo = TestRepo::new()?;
    let repository = add_repository(&service, &repo).await?;

    let result = start_chat(
        &service,
        repository,
        Checkout::Main,
        start_on(Provider::OpenAi, Some(anthropic)),
    )
    .await;

    assert_eq!(
        result,
        Err(CommandError::WrongProvider {
            subscription: anthropic,
            provider: Provider::OpenAi
        })
    );
    assert_eq!(chat_count(&service).await?, 0);
    Ok(())
}

#[test]
fn a_start_chat_command_without_settings_asks_for_the_defaults() -> TestResult {
    let command: Command = serde_json::from_str(r#"{"type":"start_chat","repository":1}"#)?;

    let Command::StartChat {
        checkout, settings, ..
    } = command
    else {
        return Err(format!("expected StartChat, got {command:?}").into());
    };
    assert_eq!(checkout, Checkout::NewWorktree);
    assert_eq!(settings, ChatSettings::default());
    Ok(())
}
