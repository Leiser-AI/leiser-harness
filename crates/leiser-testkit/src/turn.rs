//! Waiting for a chat's turn in the service's events.

use std::time::Duration;

use leiser_service::{AgentEvent, ChatId, Event};
use tokio::sync::broadcast;

/// How long a test waits for a turn before it fails.
const TURN_TIMEOUT: Duration = Duration::from_secs(10);

/// Why [`collect_turn`] returned no turn.
#[derive(Debug, thiserror::Error)]
pub enum TurnError {
    #[error("the turn did not end in time")]
    Timeout,
    #[error("the agent session ended before the turn did")]
    SessionEnded,
    #[error("events were lost: {0}")]
    Events(#[from] broadcast::error::RecvError),
}

/// Collects the chat's agent events up to and including
/// [`AgentEvent::TurnEnded`]. Events of other chats are skipped.
///
/// # Errors
///
/// Returns a [`TurnError`] when the turn does not end in time, the agent
/// session ends first, or events were lost.
pub async fn collect_turn(
    events: &mut broadcast::Receiver<Event>,
    chat: ChatId,
) -> Result<Vec<AgentEvent>, TurnError> {
    collect_turn_within(events, chat, TURN_TIMEOUT).await
}

/// [`collect_turn`] with its own timeout, for real agent CLIs, which take
/// longer than the fake one.
///
/// # Errors
///
/// As [`collect_turn`].
pub async fn collect_turn_within(
    events: &mut broadcast::Receiver<Event>,
    chat: ChatId,
    timeout: Duration,
) -> Result<Vec<AgentEvent>, TurnError> {
    tokio::time::timeout(timeout, async {
        let mut turn = Vec::new();
        loop {
            let Event::Agent { chat: from, event } = events.recv().await?;
            if from != chat {
                continue;
            }
            match event {
                AgentEvent::SessionEnded => return Err(TurnError::SessionEnded),
                AgentEvent::TurnEnded => {
                    turn.push(event);
                    return Ok(turn);
                }
                event => turn.push(event),
            }
        }
    })
    .await
    .map_err(|_| TurnError::Timeout)?
}
