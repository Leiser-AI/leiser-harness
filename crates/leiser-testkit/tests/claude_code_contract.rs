//! Contract tests: the real Claude Code behaves like the fake agent CLI for
//! start, send and read events, so the service tests with the fake agent
//! CLI also hold for Claude Code.
//!
//! They need Claude Code signed in to one of the developer's subscriptions,
//! so they are ignored by default and never run in CI. Run them locally:
//!
//! ```text
//! cargo test -p leiser-testkit --test claude_code_contract -- --ignored
//! ```
//!
//! `LEISER_CLAUDE_CODE` names the program to run (default: `claude` on
//! `PATH`). Set `ANTHROPIC_MODEL=haiku` to spend less of the subscription.
//! Run them from a plain terminal, not from inside a Claude Code session,
//! whose environment changes how the child Claude Code signs in.

use std::error::Error;
use std::sync::Arc;
use std::time::Duration;

use leiser_service::{
    AgentEvent, ChatId, Checkout, ClaudeCodeDriver, Command, Driver, Event, Response,
    ServiceConfig, ServiceHandle, SessionSpec,
};
use leiser_testkit::TestRepo;
use tokio::sync::broadcast;

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

/// A real model is slower than the fake agent CLI.
const TURN_TIMEOUT: Duration = Duration::from_secs(180);

fn driver() -> ClaudeCodeDriver {
    std::env::var_os("LEISER_CLAUDE_CODE").map_or_else(ClaudeCodeDriver::installed, |program| {
        ClaudeCodeDriver::new(program)
    })
}

async fn start_chat(repo: &TestRepo) -> TestResult<(ServiceHandle, ChatId)> {
    let service = leiser_service::start(ServiceConfig {
        driver: Arc::new(driver()),
    });
    let Response::RepositoryAdded { repository } = service
        .execute(Command::AddRepository {
            path: repo.path().to_path_buf(),
        })
        .await?
    else {
        return Err("expected RepositoryAdded".into());
    };
    let Response::ChatStarted { chat } = service
        .execute(Command::StartChat {
            repository,
            checkout: Checkout::Main,
        })
        .await?
    else {
        return Err("expected ChatStarted".into());
    };
    Ok((service, chat))
}

async fn send(service: &ServiceHandle, chat: ChatId, text: &str) -> TestResult {
    let response = service
        .execute(Command::SendMessage {
            chat,
            text: text.to_owned(),
        })
        .await?;
    assert_eq!(response, Response::MessageSent);
    Ok(())
}

/// Like `leiser_testkit::collect_turn`, with time for a real model.
async fn collect_turn(
    events: &mut broadcast::Receiver<Event>,
    chat: ChatId,
) -> TestResult<Vec<AgentEvent>> {
    tokio::time::timeout(TURN_TIMEOUT, async {
        let mut turn = Vec::new();
        loop {
            let Event::Agent { chat: from, event } = events.recv().await?;
            if from != chat {
                continue;
            }
            match event {
                AgentEvent::SessionEnded => {
                    return Err(format!("the agent session ended during the turn: {turn:?}").into());
                }
                AgentEvent::TurnEnded => {
                    turn.push(event);
                    return Ok(turn);
                }
                event => turn.push(event),
            }
        }
    })
    .await
    .map_err(|_| format!("the turn did not end within {TURN_TIMEOUT:?}"))?
}

fn messages(turn: &[AgentEvent]) -> String {
    turn.iter()
        .filter_map(|event| match event {
            AgentEvent::Message { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
#[ignore = "needs Claude Code signed in to a subscription; run locally with --ignored"]
async fn a_message_reaches_claude_code_and_its_events_come_back() -> TestResult {
    // The file exists only in the chat's repository, so reading it also
    // shows that Claude Code runs there.
    let marker = "leiser-contract-marker-5d1c";
    let repo = TestRepo::new()?;
    std::fs::write(repo.path().join("marker.txt"), marker)?;
    let (service, chat) = start_chat(&repo).await?;
    let mut events = service.subscribe();

    send(
        &service,
        chat,
        "Use your Read tool to read the file marker.txt in the current directory, \
         then reply with only its contents.",
    )
    .await?;

    let turn = collect_turn(&mut events, chat).await?;
    assert_eq!(turn.last(), Some(&AgentEvent::TurnEnded));
    assert!(
        turn.iter().any(|event| matches!(
            event,
            AgentEvent::ToolCall { name, detail } if name == "Read" && detail.ends_with("marker.txt")
        )),
        "expected a Read tool call of marker.txt: {turn:?}"
    );
    assert!(
        turn.iter().any(|event| matches!(
            event,
            AgentEvent::ToolResult { name, output } if name == "Read" && output.contains(marker)
        )),
        "expected the Read tool's result with the marker: {turn:?}"
    );
    assert!(messages(&turn).contains(marker), "{turn:?}");
    Ok(())
}

#[tokio::test]
#[ignore = "needs Claude Code signed in to a subscription; run locally with --ignored"]
async fn the_next_message_continues_the_same_agent_session() -> TestResult {
    let word = "lantern-4217";
    let repo = TestRepo::new()?;
    let (service, chat) = start_chat(&repo).await?;
    let mut events = service.subscribe();

    send(
        &service,
        chat,
        &format!("Remember the code word {word}. Reply with only: OK"),
    )
    .await?;
    collect_turn(&mut events, chat).await?;
    send(
        &service,
        chat,
        "What was the code word? Reply with only the code word.",
    )
    .await?;

    // A new agent session would not know the word.
    let turn = collect_turn(&mut events, chat).await?;
    assert!(messages(&turn).contains(word), "{turn:?}");
    Ok(())
}

#[tokio::test]
#[ignore = "needs Claude Code installed; run locally with --ignored"]
async fn an_agent_session_ends_when_its_input_is_dropped() -> TestResult {
    let repo = TestRepo::new()?;
    let mut session = driver().start(SessionSpec {
        working_directory: repo.path().to_path_buf(),
    })?;

    drop(session.input);

    let last = tokio::time::timeout(TURN_TIMEOUT, async {
        let mut last = None;
        while let Some(event) = session.events.recv().await {
            last = Some(event);
        }
        last
    })
    .await?;
    assert_eq!(last, Some(AgentEvent::SessionEnded));
    Ok(())
}
