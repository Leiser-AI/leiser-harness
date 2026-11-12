//! Updates of the app itself, from the update feed of its release channel
//! (`plugins.updater` in `tauri.conf.json`; a beta build reads the beta
//! feed, see `scripts/package.mjs`).
//!
//! The UI asks `check_app_update` now and then and offers a found update;
//! `install_app_update` downloads it, checks its signature against the
//! public key in `tauri.conf.json`, installs it and restarts the app. The
//! harness service is not stopped for it: the installer replaces its
//! program next to the app, and the service moves to the new program once
//! no turn runs (ADR 0007).
//!
//! Debug builds never look for updates: they are not installed from a
//! release.

use std::sync::{Mutex, MutexGuard, PoisonError};

use serde::Serialize;
use tauri::{AppHandle, Emitter, State};
use tauri_plugin_updater::{Update, UpdaterExt};

/// The update the last check found, until it is installed.
#[derive(Default)]
pub struct FoundUpdate(Mutex<Option<Update>>);

impl FoundUpdate {
    fn slot(&self) -> MutexGuard<'_, Option<Update>> {
        // The slot holds no invariant a panic could break.
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// An update the developer can install.
#[derive(Debug, Serialize)]
pub struct AvailableUpdate {
    /// The version it updates to.
    version: String,
    /// Its release notes, when the feed has them.
    notes: Option<String>,
}

/// How far an update's download is, as the `app-update-progress` event.
#[derive(Clone, Debug, Serialize)]
struct Progress {
    downloaded: u64,
    /// `None` when the server does not say.
    total: Option<u64>,
}

/// Why looking for or installing an update failed.
#[derive(Debug, thiserror::Error, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum UpdateError {
    #[error("no update was found to install")]
    NoUpdate,
    #[error("{reason}")]
    Failed { reason: String },
}

impl From<tauri_plugin_updater::Error> for UpdateError {
    fn from(error: tauri_plugin_updater::Error) -> Self {
        Self::Failed {
            reason: error.to_string(),
        }
    }
}

/// Looks for a newer version in the app's update feed; `None` when there
/// is none, and always in a debug build.
#[tauri::command]
pub async fn check_app_update(
    app: AppHandle,
    found: State<'_, FoundUpdate>,
) -> Result<Option<AvailableUpdate>, UpdateError> {
    if cfg!(debug_assertions) {
        return Ok(None);
    }
    let update = app.updater()?.check().await?;
    let available = update.as_ref().map(|update| AvailableUpdate {
        version: update.version.clone(),
        notes: update.body.clone(),
    });
    *found.slot() = update;
    Ok(available)
}

/// Downloads and installs the update the last check found, then restarts
/// the app. Reports the download as `app-update-progress` events.
#[tauri::command]
pub async fn install_app_update(
    app: AppHandle,
    found: State<'_, FoundUpdate>,
) -> Result<(), UpdateError> {
    let update = found.slot().take().ok_or(UpdateError::NoUpdate)?;
    let mut downloaded: u64 = 0;
    update
        .download_and_install(
            |chunk, total| {
                downloaded += chunk as u64;
                // The progress only informs; a lost event changes nothing.
                let _ = app.emit("app-update-progress", Progress { downloaded, total });
            },
            || {},
        )
        .await?;
    app.restart()
}
