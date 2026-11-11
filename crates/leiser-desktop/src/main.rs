//! The desktop app (ADR 0004): a Tauri shell that renders what the harness
//! service reports. It holds no chat state of its own; the UI reads chats
//! from the service and re-reads them on every service event.
//!
//! For v1 the app hosts the harness service in-process and forwards the
//! service's local interface 1:1: the `execute` command takes a
//! [`Command`] and answers with its [`Response`], and every service
//! [`Event`](leiser_service::Event) reaches the UI as `service-event`. Once
//! the service runs on its own (#29), only this file changes.

// Release builds on Windows open no console window next to the app.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod demo;

use std::path::PathBuf;
use std::sync::Arc;

use leiser_service::{Command, CommandError, Response, ServiceConfig, ServiceHandle};
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
            let driver = demo::driver()?;
            // `start` spawns the service's command loop on the current
            // runtime, which lives as long as the app.
            let service = tauri::async_runtime::block_on(async {
                leiser_service::start(ServiceConfig {
                    driver: Arc::new(driver),
                })
            });
            tauri::async_runtime::spawn(forward_events(app.handle().clone(), service.subscribe()));
            app.manage(service);
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![execute, pick_folder])
        .run(tauri::generate_context!())
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
