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
//!
//! When a chat starts waiting for the developer (an approval request or a
//! question) while the app is not in front, the app sends an OS
//! notification.
//!
//! Links to the git host, such as a wayfinder map's tickets, open in the
//! developer's browser (`open_in_browser`); the page itself cannot.
//!
//! The terminal panel's shells run in the app itself (`terminals.rs`), in
//! the chat's checkout, and end when the app quits.
//!
//! The app updates itself from its release channel's update feed
//! (`app_update.rs`).

// Release builds on Windows open no console window next to the app.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app_update;
mod connection;
mod terminals;

use std::path::PathBuf;

use connection::{ConnectionState, ServiceConnection};
use leiser_service::{Command, CommandError, Response};
use tauri::{AppHandle, Manager, RunEvent, State};
use tauri_plugin_dialog::DialogExt;
use tauri_plugin_opener::OpenerExt;
use terminals::Terminals;

fn main() -> Result<(), tauri::Error> {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .manage(app_update::FoundUpdate::default())
        .manage(Terminals::default())
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
            pick_folder,
            open_in_browser,
            terminals::open_terminal,
            terminals::write_terminal,
            terminals::resize_terminal,
            terminals::close_terminal,
            terminals::close_all_terminals,
            app_update::check_app_update,
            app_update::install_app_update,
        ])
        .build(tauri::generate_context!())?
        .run(|app, event| {
            // No shell outlives the app.
            if let RunEvent::Exit = event {
                app.state::<Terminals>().close_all();
            }
        });
    Ok(())
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

/// Why a link did not open.
#[derive(Debug, thiserror::Error, serde::Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum OpenError {
    #[error("only https links open in the browser, not {url}")]
    NotHttps { url: String },
    #[error("the browser did not open: {reason}")]
    Failed { reason: String },
}

/// Opens an `https://` link, such as a ticket on the git host, in the
/// developer's default browser.
#[tauri::command]
async fn open_in_browser(app: AppHandle, url: String) -> Result<(), OpenError> {
    if !url.starts_with("https://") {
        return Err(OpenError::NotHttps { url });
    }
    app.opener()
        .open_url(url, None::<&str>)
        .map_err(|error| OpenError::Failed {
            reason: error.to_string(),
        })
}
