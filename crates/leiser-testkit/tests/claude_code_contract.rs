//! Contract tests: the real Claude Code behaves like the fake agent CLI for
//! start, send, read events, approve, queue, steer, interrupt and resume, so the service
//! tests with the fake agent CLI also hold for Claude Code.
//!
//! They need Claude Code signed in to one of the developer's subscriptions,
//! so they are ignored by default and never run in CI. Run them locally:
//!
//! ```text
//! cargo test -p leiser-testkit --test claude_code_contract -- --ignored
//! ```
//!
//! They run Claude Code as the desktop app finds it: the program
//! `LEISER_CLAUDE_CODE` names, `claude` on `PATH`, in common install
//! directories, or the copy the Claude desktop app bundles. Set `ANTHROPIC_MODEL=haiku` to spend less of the subscription.
//! Run them from a plain terminal, not from inside a Claude Code session,
//! whose environment changes how the child Claude Code signs in.

use std::error::Error;
use std::sync::Arc;
use std::time::Duration;

use leiser_service::{
    AgentCli, AgentEvent, Answer, ChatId, Checkout, ClaudeCodeDriver, Command, Driver, Event,
    PermissionMode, QueueReason, Response, ServiceHandle, SessionSpec,
};
use leiser_testkit::{TestRepo, TestService};
use tokio::sync::broadcast;

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

/// A prompt whose turn runs long enough to be interrupted or queued behind.
const LONG_TURN: &str = "Write a 1500-word story about a lighthouse keeper. Do not use any tools.";

/// A real model is slower than the fake agent CLI.
const TURN_TIMEOUT: Duration = Duration::from_secs(180);

fn driver() -> ClaudeCodeDriver {
    ClaudeCodeDriver::installed()
}

/// A chat on Claude Code; the [`TestService`] keeps the service running.
async fn start_chat(repo: &TestRepo) -> TestResult<(TestService, ServiceHandle, ChatId)> {
    let harness = TestService::with_driver(AgentCli::ClaudeCode, Arc::new(driver()))?;
    let service = harness.connect();
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
    Ok((harness, service, chat))
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
            let Event::Agent { chat: from, event } = events.recv().await? else {
                continue;
            };
            if from != chat {
                continue;
            }
            match event {
                AgentEvent::SessionEnded => {
                    return Err(format!("the agent session ended during the turn: {turn:?}").into());
                }
                AgentEvent::TurnEnded
                | AgentEvent::TurnInterrupted
                | AgentEvent::TurnFailed { .. } => {
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
    let (_harness, service, chat) = start_chat(&repo).await?;
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
    let (_harness, service, chat) = start_chat(&repo).await?;
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
#[ignore = "needs Claude Code signed in to a subscription; run locally with --ignored"]
async fn a_chat_continues_its_agent_session_after_a_service_restart() -> TestResult {
    let word = "harbor-8831";
    let repo = TestRepo::new()?;
    let (harness, service, chat) = start_chat(&repo).await?;
    let mut events = service.subscribe();
    send(
        &service,
        chat,
        &format!("Remember the code word {word}. Reply with only: OK"),
    )
    .await?;
    collect_turn(&mut events, chat).await?;

    let harness = harness.restart().await?;

    let service = harness.connect();
    let mut events = service.subscribe();
    send(
        &service,
        chat,
        "What was the code word? Reply with only the code word.",
    )
    .await?;
    // Only the resumed agent session knows the word.
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
        // No harness MCP server listens there; these tests do not use it.
        harness_mcp_url: "http://127.0.0.1:9/mcp/none".to_owned(),
        permission_mode: PermissionMode::Supervised,
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

#[tokio::test]
#[ignore = "needs Claude Code signed in to a subscription; run locally with --ignored"]
async fn an_interrupted_turn_ends_as_interrupted_and_the_chat_goes_on() -> TestResult {
    let repo = TestRepo::new()?;
    let (_harness, service, chat) = start_chat(&repo).await?;
    let mut events = service.subscribe();
    send(&service, chat, LONG_TURN).await?;
    // Claude Code writes whole messages, not parts, so there is no event
    // that marks the turn as running; a moment is enough.
    tokio::time::sleep(Duration::from_secs(2)).await;

    let response = service.execute(Command::InterruptTurn { chat }).await?;

    assert_eq!(response, Response::InterruptRequested);
    let turn = collect_turn(&mut events, chat).await?;
    assert_eq!(turn.last(), Some(&AgentEvent::TurnInterrupted), "{turn:?}");
    send(&service, chat, "Reply with only: OK").await?;
    let turn = collect_turn(&mut events, chat).await?;
    assert_eq!(turn.last(), Some(&AgentEvent::TurnEnded), "{turn:?}");
    Ok(())
}

#[tokio::test]
#[ignore = "needs Claude Code signed in to a subscription; run locally with --ignored"]
async fn queued_and_steered_messages_wait_for_the_running_turn() -> TestResult {
    let repo = TestRepo::new()?;
    let (_harness, service, chat) = start_chat(&repo).await?;
    let mut events = service.subscribe();
    send(&service, chat, LONG_TURN).await?;

    let queued = service
        .execute(Command::QueueMessage {
            chat,
            text: "Reply with only: QUEUED-1".to_owned(),
        })
        .await?;
    let steered = service
        .execute(Command::SteerMessage {
            chat,
            text: "Reply with only: STEERED-2".to_owned(),
        })
        .await?;

    assert_eq!(
        queued,
        Response::MessageQueued {
            reason: QueueReason::Requested
        }
    );
    assert_eq!(
        steered,
        Response::MessageQueued {
            reason: QueueReason::SteerUnsupported
        }
    );
    // Each message gets its own turn, in order.
    let first = collect_turn(&mut events, chat).await?;
    assert_eq!(first.last(), Some(&AgentEvent::TurnEnded), "{first:?}");
    let second = collect_turn(&mut events, chat).await?;
    assert!(messages(&second).contains("QUEUED-1"), "{second:?}");
    let third = collect_turn(&mut events, chat).await?;
    assert!(messages(&third).contains("STEERED-2"), "{third:?}");
    Ok(())
}

/// Asks Claude Code, in a Supervised chat, to create `file`, and answers
/// the approval request that comes with `answer`. Returns the agent's
/// messages after the answer, once the turn has ended normally.
async fn answer_a_command_approval(
    repo: &TestRepo,
    file: &str,
    answer: Answer,
) -> TestResult<String> {
    let (_harness, service, chat) = start_chat(repo).await?;
    service
        .execute(Command::ChangePermissionMode {
            chat,
            permission_mode: PermissionMode::Supervised,
        })
        .await?;
    let mut events = service.subscribe();
    send(
        &service,
        chat,
        &format!(
            "Use the Bash tool to run exactly this command: `touch {file}`. \
             If I decline and say why, reply with exactly what I said. Otherwise \
             reply with the single word done."
        ),
    )
    .await?;
    let request = tokio::time::timeout(TURN_TIMEOUT, async {
        loop {
            match events.recv().await? {
                Event::Agent {
                    chat: from,
                    event: AgentEvent::ApprovalRequested { request, .. },
                } if from == chat => return Ok::<_, Box<dyn Error>>(request),
                Event::Agent {
                    chat: from,
                    event: event @ (AgentEvent::TurnEnded | AgentEvent::SessionEnded),
                } if from == chat => {
                    return Err(format!("no approval request before {event:?}").into());
                }
                _ => {}
            }
        }
    })
    .await??;
    let response = service
        .execute(Command::AnswerRequest {
            chat,
            request,
            answer,
        })
        .await?;
    assert_eq!(response, Response::RequestAnswered);
    let turn = collect_turn(&mut events, chat).await?;
    assert_eq!(turn.last(), Some(&AgentEvent::TurnEnded), "{turn:?}");
    Ok(messages(&turn))
}

#[tokio::test]
#[ignore = "needs Claude Code signed in to a subscription; run locally with --ignored"]
async fn an_approved_command_runs() -> TestResult {
    let repo = TestRepo::new()?;

    answer_a_command_approval(&repo, "approved.txt", Answer::Approve).await?;

    assert!(repo.path().join("approved.txt").exists());
    Ok(())
}

#[tokio::test]
#[ignore = "needs Claude Code signed in to a subscription; run locally with --ignored"]
async fn a_declined_commands_feedback_reaches_the_agent() -> TestResult {
    let repo = TestRepo::new()?;

    let answer = answer_a_command_approval(
        &repo,
        "declined.txt",
        Answer::Decline {
            feedback: Some("The code word is pineapple.".to_owned()),
        },
    )
    .await?;

    assert!(answer.to_lowercase().contains("pineapple"), "{answer}");
    assert!(!repo.path().join("declined.txt").exists());
    Ok(())
}
