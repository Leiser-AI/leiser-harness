//! The Gemini CLI driver, end to end through the harness service, against
//! the fake Gemini CLI, which speaks the Agent Client Protocol as
//! `gemini --acp` does. No Google account and no network are needed; the
//! fake is started through a shell script, so these tests run on Unix.
#![cfg(unix)]

use std::error::Error;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use leiser_service::{
    Access, AgentCli, AgentEvent, Answer, ChatId, ChatSettings, Checkout, Command, Driver,
    DriverError, Event, GeminiDriver, LoginOutcome, PermissionMode, Provider, Response,
    ServiceHandle, SessionSpec, SignIn, SubscriptionId, Version,
};
use leiser_testkit::{
    FAKE_GEMINI_OAUTH_FILE, FakeGemini, FakeGeminiScript, FakeGeminiStep, TestRepo, TestService,
    collect_turn, next_event,
};
use serde_json::{Value, json};
use tokio::sync::broadcast;

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

const FAKE_GEMINI: &str = env!("CARGO_BIN_EXE_fake-gemini");
const LOGIN_TIMEOUT: Duration = Duration::from_secs(10);

fn fake(script: &FakeGeminiScript) -> TestResult<FakeGemini> {
    Ok(FakeGemini::new(Path::new(FAKE_GEMINI), script)?)
}

fn service(gemini: &FakeGemini) -> TestResult<TestService> {
    let driver: Arc<dyn Driver> = Arc::new(GeminiDriver::new(gemini.program()));
    Ok(TestService::with_driver(AgentCli::Gemini, driver)?)
}

async fn start_chat(service: &ServiceHandle, repo: &TestRepo) -> TestResult<ChatId> {
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

/// The chat's next approval request, by the id the harness service gave it.
async fn approval_request(
    events: &mut broadcast::Receiver<Event>,
    chat: ChatId,
) -> TestResult<(String, String)> {
    loop {
        if let Event::Agent {
            event:
                AgentEvent::ApprovalRequested {
                    request,
                    description,
                },
            ..
        } = next_event(events, chat).await?
        {
            return Ok((request, description));
        }
    }
}

fn messages(turn: &[AgentEvent]) -> Vec<&str> {
    turn.iter()
        .filter_map(|event| match event {
            AgentEvent::Message { text } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn a_chat_runs_on_the_gemini_cli_over_acp() -> TestResult {
    let gemini = fake(&FakeGeminiScript::new().turn([
        FakeGeminiStep::message("Looking at the repository."),
        FakeGeminiStep::Tool {
            kind: "read".to_owned(),
            title: "README.md".to_owned(),
            output: "# Test repository".to_owned(),
        },
        FakeGeminiStep::EchoPrompt,
    ]))?;
    let harness = service(&gemini)?;
    let service = harness.connect();
    let repo = TestRepo::new()?;
    let chat = start_chat(&service, &repo).await?;
    let mut events = service.subscribe();

    send(&service, chat, "summarise the README").await?;
    let turn = collect_turn(&mut events, chat).await?;

    assert_eq!(
        turn,
        [
            AgentEvent::Message {
                text: "Looking at the repository.".to_owned()
            },
            AgentEvent::ToolCall {
                name: "read".to_owned(),
                detail: "README.md".to_owned()
            },
            AgentEvent::ToolResult {
                name: "read".to_owned(),
                output: "# Test repository".to_owned()
            },
            AgentEvent::Message {
                text: "summarise the README".to_owned()
            },
            AgentEvent::TurnEnded,
        ]
    );
    let starts = gemini.starts()?;
    let args = &starts[0]["args"];
    // The repository's default, Auto, needs a sandbox the Gemini CLI does
    // not have over ACP: the chat runs Supervised.
    assert_eq!(args[0], "--acp");
    assert_eq!(args[1], "--approval-mode");
    assert_eq!(args[2], "default");
    assert_eq!(starts[0]["env"]["GEMINI_SANDBOX"], "false");
    let policy = starts[0]["policy"].as_str().ok_or("no policy file")?;
    assert!(policy.contains("run_shell_command"), "{policy}");
    assert!(policy.contains("development"), "{policy}");
    assert!(policy.contains("mcpName = \"leiser\""), "{policy}");
    let opened = gemini.sent("session/new")?;
    assert_eq!(opened[0]["params"]["mcpServers"][0]["type"], "http");
    assert_eq!(opened[0]["params"]["mcpServers"][0]["name"], "leiser");
    Ok(())
}

#[tokio::test]
async fn approving_lets_the_tool_run_and_a_declines_feedback_reaches_the_agent() -> TestResult {
    let ask = |title: &str| FakeGeminiStep::AskPermission {
        title: title.to_owned(),
    };
    let gemini = fake(
        &FakeGeminiScript::new()
            .turn([ask("cargo test"), FakeGeminiStep::message("Tests pass.")])
            .turn([ask("rm -rf target")])
            .turn([FakeGeminiStep::EchoPrompt]),
    )?;
    let harness = service(&gemini)?;
    let service = harness.connect();
    let repo = TestRepo::new()?;
    let chat = start_chat(&service, &repo).await?;
    let mut events = service.subscribe();

    send(&service, chat, "run the tests").await?;
    let (request, description) = approval_request(&mut events, chat).await?;
    assert_eq!(description, "Run `cargo test`");
    let answered = service
        .execute(Command::AnswerRequest {
            chat,
            request,
            answer: Answer::Approve,
        })
        .await?;
    assert_eq!(answered, Response::RequestAnswered);
    let turn = collect_turn(&mut events, chat).await?;
    assert!(turn.contains(&AgentEvent::ToolResult {
        name: "execute".to_owned(),
        output: "allowed".to_owned(),
    }));
    assert_eq!(messages(&turn), ["Tests pass."]);

    send(&service, chat, "clean up").await?;
    let (request, _) = approval_request(&mut events, chat).await?;
    service
        .execute(Command::AnswerRequest {
            chat,
            request,
            answer: Answer::Decline {
                feedback: Some("Use cargo clean instead".to_owned()),
            },
        })
        .await?;
    let turn = collect_turn(&mut events, chat).await?;

    // The feedback goes on in the same turn, as the next prompt.
    assert_eq!(messages(&turn), ["Use cargo clean instead"]);
    assert_eq!(turn.last(), Some(&AgentEvent::TurnEnded));
    let answers: Vec<Value> = gemini
        .log()?
        .into_iter()
        .filter(|entry| {
            entry
                .get("result")
                .is_some_and(|result| result.get("outcome").is_some())
        })
        .map(|entry| entry["result"]["outcome"].clone())
        .collect();
    assert_eq!(
        answers,
        [
            json!({"outcome": "selected", "optionId": "proceed_once"}),
            json!({"outcome": "selected", "optionId": "cancel"}),
        ]
    );
    Ok(())
}

#[tokio::test]
async fn an_interrupt_cancels_the_turn() -> TestResult {
    let gemini = fake(&FakeGeminiScript::new().turn([
        FakeGeminiStep::Tool {
            kind: "execute".to_owned(),
            title: "sleep 600".to_owned(),
            output: String::new(),
        },
        FakeGeminiStep::WaitForCancel,
    ]))?;
    let harness = service(&gemini)?;
    let service = harness.connect();
    let repo = TestRepo::new()?;
    let chat = start_chat(&service, &repo).await?;
    let mut events = service.subscribe();
    send(&service, chat, "a long task").await?;
    // The turn runs once its tool call arrives.
    loop {
        if let Event::Agent {
            event: AgentEvent::ToolResult { .. },
            ..
        } = next_event(&mut events, chat).await?
        {
            break;
        }
    }

    service.execute(Command::InterruptTurn { chat }).await?;
    let turn = collect_turn(&mut events, chat).await?;

    assert_eq!(turn.last(), Some(&AgentEvent::TurnInterrupted));
    assert_eq!(gemini.sent("session/cancel")?.len(), 1);
    Ok(())
}

#[tokio::test]
async fn after_a_restart_the_chat_loads_its_gemini_session_again() -> TestResult {
    let gemini = fake(
        &FakeGeminiScript::new()
            .turn([FakeGeminiStep::message("first answer")])
            .turn([FakeGeminiStep::message("second answer")]),
    )?;
    let harness = service(&gemini)?;
    let service = harness.connect();
    let repo = TestRepo::new()?;
    let chat = start_chat(&service, &repo).await?;
    let mut events = service.subscribe();
    send(&service, chat, "one").await?;
    collect_turn(&mut events, chat).await?;

    let harness = harness.restart().await?;
    let service = harness.connect();
    let mut events = service.subscribe();
    send(&service, chat, "two").await?;
    let turn = collect_turn(&mut events, chat).await?;

    // The replayed history gives no events: only the second turn's.
    assert_eq!(messages(&turn), ["second answer"]);
    let created = gemini.sent("session/new")?;
    let loaded = gemini.sent("session/load")?;
    assert_eq!(created.len(), 1);
    assert_eq!(loaded.len(), 1);
    assert!(
        loaded[0]["params"]["sessionId"]
            .as_str()
            .is_some_and(|id| id.starts_with("fake-gemini-")),
        "{}",
        loaded[0]
    );
    Ok(())
}

#[tokio::test]
async fn a_gemini_cli_that_cannot_load_sessions_continues_with_a_handoff() -> TestResult {
    let gemini = fake(
        &FakeGeminiScript::new()
            .without_load_session()
            .turn([FakeGeminiStep::message("first answer")])
            .turn([FakeGeminiStep::EchoPrompt]),
    )?;
    let harness = service(&gemini)?;
    let service = harness.connect();
    let repo = TestRepo::new()?;
    let chat = start_chat(&service, &repo).await?;
    let mut events = service.subscribe();
    send(&service, chat, "one").await?;
    collect_turn(&mut events, chat).await?;

    let harness = harness.restart().await?;
    let service = harness.connect();
    let mut events = service.subscribe();
    send(&service, chat, "two").await?;
    let turn = collect_turn(&mut events, chat).await?;

    // A new session; a new session's first turn is the script's first.
    let echoed = messages(&turn);
    assert!(
        echoed
            .iter()
            .any(|text| text.contains("first answer") || text.contains("<handoff>")),
        "{echoed:?}"
    );
    assert_eq!(gemini.sent("session/load")?.len(), 0);
    assert_eq!(gemini.sent("session/new")?.len(), 2);
    Ok(())
}

#[tokio::test]
async fn a_quota_error_is_a_usage_limit() -> TestResult {
    let gemini = fake(
        &FakeGeminiScript::new().turn([FakeGeminiStep::Fail {
            code: 429,
            message: "You have exhausted your daily quota on this model. \
                  Your quota will reset after 1h."
                .to_owned(),
        }]),
    )?;
    let harness = service(&gemini)?;
    let service = harness.connect();
    let repo = TestRepo::new()?;
    let chat = start_chat(&service, &repo).await?;
    let mut events = service.subscribe();

    send(&service, chat, "fix the bug").await?;
    let turn = collect_turn(&mut events, chat).await?;

    assert_eq!(
        turn,
        [
            AgentEvent::UsageLimitReached {
                limit: "Daily quota".to_owned(),
                resets_in_seconds: Some(3600),
            },
            AgentEvent::TurnEnded,
        ]
    );
    let Response::Chat { usage_limit, .. } = service.execute(Command::ReadChat { chat }).await?
    else {
        return Err("expected Chat".into());
    };
    assert_eq!(
        usage_limit.map(|limit| limit.limit),
        Some("Daily quota".to_owned())
    );
    Ok(())
}

async fn add_subscription(
    service: &ServiceHandle,
    name: &str,
    sign_in: SignIn,
) -> TestResult<SubscriptionId> {
    match service
        .execute(Command::AddSubscription {
            provider: Provider::Google,
            name: name.to_owned(),
            sign_in,
        })
        .await?
    {
        Response::SubscriptionAdded { subscription } => Ok(subscription),
        response => Err(format!("expected SubscriptionAdded, got {response:?}").into()),
    }
}

#[tokio::test]
async fn an_api_key_subscription_signs_in_with_its_key_from_the_environment() -> TestResult {
    let gemini = fake(
        &FakeGeminiScript::new().turn([FakeGeminiStep::ReportEnvironment {
            variable: "GEMINI_API_KEY".to_owned(),
        }]),
    )?;
    let harness = service(&gemini)?;
    let service = harness.connect();
    add_subscription(
        &service,
        "Gemini API",
        SignIn::ApiKey {
            api_key: leiser_service::ApiKey::new("test-gemini-key"),
        },
    )
    .await?;
    let repo = TestRepo::new()?;
    std::fs::create_dir_all(repo.path().join(".leiser"))?;
    std::fs::write(
        repo.path().join(".leiser/config.toml"),
        "[subscriptions]\ngoogle = \"Gemini API\"\n",
    )?;
    let chat = start_chat(&service, &repo).await?;
    let mut events = service.subscribe();

    send(&service, chat, "hello").await?;
    let turn = collect_turn(&mut events, chat).await?;

    assert_eq!(messages(&turn), ["test-gemini-key"]);
    let signed_in = gemini.sent("authenticate")?;
    assert_eq!(signed_in[0]["params"]["methodId"], "gemini-api-key");
    let home = gemini.starts()?[0]["env"]["GEMINI_CLI_HOME"].clone();
    assert!(
        home.as_str().is_some_and(|home| home.ends_with("gemini")),
        "{home}"
    );
    Ok(())
}

#[tokio::test]
async fn a_login_subscription_signs_in_with_google_in_its_own_home() -> TestResult {
    let gemini = fake(&FakeGeminiScript::new())?;
    let harness = service(&gemini)?;
    let service = harness.connect();
    let mut events = service.subscribe();

    let subscription = add_subscription(&service, "Gemini private", SignIn::Login).await?;
    let outcome = tokio::time::timeout(LOGIN_TIMEOUT, async {
        loop {
            if let Event::LoginFinished {
                subscription: finished,
                outcome,
            } = events.recv().await?
                && finished == subscription
            {
                return Ok::<_, Box<dyn Error>>(outcome);
            }
        }
    })
    .await??;

    assert_eq!(outcome, LoginOutcome::SignedIn);
    let Response::Subscriptions { subscriptions } =
        service.execute(Command::ListSubscriptions).await?
    else {
        return Err("expected Subscriptions".into());
    };
    let home = subscriptions
        .iter()
        .find(|overview| overview.subscription == subscription)
        .map(|overview| overview.home.clone())
        .ok_or("the subscription is not listed")?;
    assert!(home.join(".gemini").join(FAKE_GEMINI_OAUTH_FILE).is_file());
    assert_eq!(
        gemini.sent("authenticate")?[0]["params"]["methodId"],
        "oauth-personal"
    );
    Ok(())
}

// One test, so no other test of this binary starts a process while a
// script is still open for writing ("text file busy").
#[tokio::test]
async fn the_gemini_cli_starts_only_at_or_above_the_minimum_version() -> TestResult {
    let repo = TestRepo::new()?;
    let spec = SessionSpec {
        working_directory: repo.path().to_path_buf(),
        harness_mcp_url: None,
        access: Access::Chat,
        permission_mode: PermissionMode::Supervised,
        subscription: None,
    };
    let too_old = fake(&FakeGeminiScript::new().version("0.32.9"))?;
    let unreadable = fake(&FakeGeminiScript::new().version("unknown"))?;
    let nightly = fake(&FakeGeminiScript::new().version("0.71.0-nightly.20261118.g615d6bc8c"))?;

    let error = GeminiDriver::new(too_old.program())
        .start(spec.clone())
        .err();
    let Some(DriverError::UnsupportedVersion { found, minimum, .. }) = &error else {
        return Err(format!("expected UnsupportedVersion, got {error:?}").into());
    };
    assert_eq!(*found, Version::new(0, 32, 9));
    assert_eq!(*minimum, GeminiDriver::MINIMUM_VERSION);
    let message = error.as_ref().map(ToString::to_string).unwrap_or_default();
    assert!(message.contains("Update Gemini CLI"), "{message}");
    assert!(matches!(
        GeminiDriver::new(unreadable.program()).start(spec),
        Err(DriverError::UnreadableVersion { .. })
    ));
    assert_eq!(
        GeminiDriver::new(nightly.program()).version()?,
        Version::new(0, 71, 0)
    );
    Ok(())
}
