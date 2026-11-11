//! The desktop app (ADR 0004): a Tauri shell that renders what the harness
//! service reports. It holds no chat state of its own; the UI reads chats
//! from the service and re-reads them on every service event.
//!
//! The harness service runs as its own process, so the app can be closed or
//! updated while turns run (ADR 0005, ADR 0007). The app connects to it
//! over the local interface and starts it when none runs (see
//! `connection.rs`). It forwards the interface 1:1: the `execute` command
//! takes a [`Command`] and answers with its [`Response`], and every service
//! [`Event`](leiser_service::Event) reaches the UI as `service-event`.

// Release builds on Windows open no console window next to the app.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod connection;

use std::path::PathBuf;

use connection::{ConnectionState, ServiceConnection};
use leiser_service::{Command, CommandError, Response};
use tauri::{AppHandle, Manager, State};
use tauri_plugin_dialog::DialogExt;

fn main() -> Result<(), tauri::Error> {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            // Not in the app data directory: its path has a space on macOS,
            // which some build tools in a worktree trip over.
            let worktrees = app.path().home_dir()?.join(".leiser").join("worktrees");
            let program = connection::service_program(&app.path().app_data_dir()?, &worktrees)?;
            let service = ServiceConnection::default();
            app.manage(service.clone());
            tauri::async_runtime::spawn(connection::keep_connected(
                app.handle().clone(),
                program,
                service,
            ));
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            execute,
            service_connection,
            pick_folder
        ])
        .run(tauri::generate_context!())
}

/// Runs a command in the harness service.
#[tauri::command]
async fn execute(
    service: State<'_, ServiceConnection>,
    command: Command,
) -> Result<Response, CommandError> {
    let client = service.client().ok_or(CommandError::ServiceStopped)?;
    client.execute(command).await
}

/// The app's connection to the harness service right now; changes arrive
/// as `service-connection` events. Never fails; Tauri needs a `Result` from
/// an async command that borrows state.
#[tauri::command]
async fn service_connection(
    service: State<'_, ServiceConnection>,
) -> Result<ConnectionState, CommandError> {
    Ok(service.state())
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
