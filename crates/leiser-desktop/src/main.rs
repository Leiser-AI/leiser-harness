//! The desktop app (ADR 0004): a Tauri shell that renders what the harness
//! service reports. It holds no chat state of its own; the UI reads chats
//! from the service and re-reads them on every service event.
//!
//! For v1 the app hosts the harness service in-process and forwards the
//! service's local interface 1:1: the `execute` command takes a
//! [`Command`] and answers with its [`Response`], and every service
//! [`Event`](leiser_service::Event) reaches the UI as `service-event`. The
//! service keeps its data in the app's data directory, so chats survive
//! restarts. Once the service runs as its own process, only this file
//! changes.

// Release builds on Windows open no console window next to the app.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod agents;

use std::path::PathBuf;

use leiser_service::{Command, CommandError, Response, Service, ServiceHandle};
use tauri::{AppHandle, Emitter, Manager, State};
use tauri_plugin_dialog::DialogExt;
use tokio::sync::broadcast::{self, error::RecvError};

/// The UI event that carries one service event.
const SERVICE_EVENT: &str = "service-event";
/// The UI event sent when service events were lost; the UI re-reads all.
const SERVICE_EVENTS_LOST: &str = "service-events-lost";

fn main() -> Result<(), tauri::Error> {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            // Not in the app data directory: its path has a space on macOS,
            // which some build tools in a worktree trip over.
            let worktrees = app.path().home_dir()?.join(".leiser").join("worktrees");
            let config = agents::service_config(app.path().app_data_dir()?, worktrees)?;
            // `start` spawns the service's command loop on the current
            // runtime, which lives as long as the app.
            let service = tauri::async_runtime::block_on(async { leiser_service::start(config) })?;
            let connection = service.connect();
            tauri::async_runtime::spawn(forward_events(
                app.handle().clone(),
                connection.subscribe(),
            ));
            app.manage(HostedService { _service: service });
            app.manage(connection);
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![execute, pick_folder])
        .run(tauri::generate_context!())
}

/// Keeps the harness service running as long as the app.
struct HostedService {
    _service: Service,
}

/// Runs a command in the harness service.
#[tauri::command]
async fn execute(
    service: State<'_, ServiceHandle>,
    command: Command,
) -> Result<Response, CommandError> {
    service.execute(command).await
}

/// Asks the developer for a folder; `None` when they cancel.
#[tauri::command]
async fn pick_folder(app: AppHandle) -> Option<PathBuf> {
    // Blocking is fine here: async commands run off the main thread.
    app.dialog()
        .file()
        .blocking_pick_folder()
        .and_then(|folder| folder.into_path().ok())
}

async fn forward_events(app: AppHandle, mut events: broadcast::Receiver<leiser_service::Event>) {
    loop {
        let sent = match events.recv().await {
            Ok(event) => app.emit(SERVICE_EVENT, event),
            Err(RecvError::Lagged(_)) => app.emit(SERVICE_EVENTS_LOST, ()),
            Err(RecvError::Closed) => return,
        };
        // Emitting fails only when the app is shutting down.
        if sent.is_err() {
            return;
        }
    }
}
