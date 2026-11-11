//! The terminal panel's shells (#40). Each runs in a chat's checkout: its
//! worktree, or the repository's main checkout.
//!
//! The shells belong to the app, not to the harness service: they end when
//! the app quits, and a service update does not end them. Each terminal's
//! output reaches the UI on a channel of its own as raw bytes; its exit as
//! `{"exit_code": n}`, after which the terminal is gone.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use leiser_service::{ChatId, Command, CommandError, Response};
use leiser_terminal::{Size, Terminal, TerminalError, TerminalEvent};
use serde::{Serialize, Serializer};
use tauri::State;
use tauri::ipc::{Channel, InvokeResponseBody};

use crate::connection::ServiceConnection;

/// One open terminal, as the UI addresses it.
pub type TerminalId = u64;

/// The app's open terminals.
#[derive(Debug, Clone, Default)]
pub struct Terminals(Arc<Inner>);

#[derive(Debug, Default)]
struct Inner {
    next: AtomicU64,
    open: Mutex<HashMap<TerminalId, Terminal>>,
}

impl Terminals {
    fn insert(&self, id: TerminalId, terminal: Terminal) {
        self.open().insert(id, terminal);
    }

    /// Ends one terminal's shell.
    fn close(&self, id: TerminalId) {
        // Dropped outside the lock: ending a shell can take a moment.
        let closed = self.open().remove(&id);
        drop(closed);
    }

    /// Ends every shell; when the app quits.
    pub fn close_all(&self) {
        let closed = std::mem::take(&mut *self.open());
        drop(closed);
    }

    fn with<T>(
        &self,
        id: TerminalId,
        action: impl FnOnce(&Terminal) -> Result<T, TerminalError>,
    ) -> Result<T, TerminalCommandError> {
        let open = self.open();
        let terminal = open.get(&id).ok_or(TerminalCommandError::Closed)?;
        Ok(action(terminal)?)
    }

    fn open(&self) -> std::sync::MutexGuard<'_, HashMap<TerminalId, Terminal>> {
        self.0.open.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Why a terminal command failed; the UI shows the message.
#[derive(Debug, thiserror::Error)]
pub enum TerminalCommandError {
    #[error(transparent)]
    Service(#[from] CommandError),
    #[error("the harness service does not know this chat")]
    UnknownChat,
    #[error("the harness service answered with something unexpected")]
    UnexpectedResponse,
    #[error(transparent)]
    Terminal(#[from] TerminalError),
    #[error("the terminal is closed")]
    Closed,
}

impl Serialize for TerminalCommandError {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

/// What the UI gets when a shell exits.
#[derive(Debug, Serialize)]
struct Exit {
    exit_code: u32,
}

/// Opens a terminal in the chat's checkout. Its output and exit arrive on
/// `events`.
#[tauri::command]
pub async fn open_terminal(
    service: State<'_, ServiceConnection>,
    terminals: State<'_, Terminals>,
    chat: ChatId,
    rows: u16,
    cols: u16,
    events: Channel<InvokeResponseBody>,
) -> Result<TerminalId, TerminalCommandError> {
    let directory = checkout(&service, chat).await?;
    let id = terminals.0.next.fetch_add(1, Ordering::Relaxed);
    let owner = terminals.inner().clone();
    let terminal = Terminal::open(&directory, Size { rows, cols }, move |event| {
        let sent = match event {
            TerminalEvent::Output(bytes) => events.send(InvokeResponseBody::Raw(bytes)),
            TerminalEvent::Exited { exit_code } => {
                owner.close(id);
                serde_json::to_string(&Exit { exit_code })
                    .map_err(tauri::Error::from)
                    .and_then(|exit| events.send(InvokeResponseBody::Json(exit)))
            }
        };
        if let Err(error) = sent {
            tracing::warn!(%error, "could not pass terminal output to the UI");
        }
    })?;
    terminals.insert(id, terminal);
    Ok(id)
}

/// Sends typed input to a terminal.
#[tauri::command]
pub async fn write_terminal(
    terminals: State<'_, Terminals>,
    terminal: TerminalId,
    data: String,
) -> Result<(), TerminalCommandError> {
    terminals.with(terminal, |open| open.write(data.as_bytes()))
}

/// Tells a terminal its new size, as the panel shows it.
#[tauri::command]
pub async fn resize_terminal(
    terminals: State<'_, Terminals>,
    terminal: TerminalId,
    rows: u16,
    cols: u16,
) -> Result<(), TerminalCommandError> {
    terminals.with(terminal, |open| open.resize(Size { rows, cols }))
}

/// Ends a terminal's shell and the programs it runs.
#[tauri::command]
pub async fn close_terminal(
    terminals: State<'_, Terminals>,
    terminal: TerminalId,
) -> Result<(), TerminalCommandError> {
    terminals.close(terminal);
    Ok(())
}

/// Ends every terminal. A freshly loaded UI calls it: it cannot reach the
/// terminals a page it replaced had open.
#[tauri::command]
pub async fn close_all_terminals(
    terminals: State<'_, Terminals>,
) -> Result<(), TerminalCommandError> {
    terminals.close_all();
    Ok(())
}

/// The directory a chat works in, as the harness service reports it.
async fn checkout(
    service: &ServiceConnection,
    chat: ChatId,
) -> Result<PathBuf, TerminalCommandError> {
    let client = service.client().ok_or(CommandError::ServiceStopped)?;
    let Response::Chats { chats } = client.execute(Command::ListChats).await? else {
        return Err(TerminalCommandError::UnexpectedResponse);
    };
    let overview = chats
        .into_iter()
        .find(|overview| overview.chat == chat)
        .ok_or(TerminalCommandError::UnknownChat)?;
    if let Some(worktree) = overview.worktree {
        return Ok(worktree);
    }
    let Response::Repositories { repositories } = client.execute(Command::ListRepositories).await?
    else {
        return Err(TerminalCommandError::UnexpectedResponse);
    };
    repositories
        .into_iter()
        .find(|repository| repository.repository == overview.repository)
        .map(|repository| repository.path)
        .ok_or(TerminalCommandError::UnknownChat)
}
