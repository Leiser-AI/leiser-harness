// Updates of the app itself (see the app's `app_update.rs`): looks for a
// new version when the app starts and every few hours, and offers it in the
// sidebar. Installing it restarts the app; running turns go on in the
// harness service meanwhile.

import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

import { icon } from "./icons";

/** How often the app looks for a new version while it runs. */
const CHECK_EVERY_MS = 4 * 60 * 60 * 1000;

interface AvailableUpdate {
  version: string;
  notes: string | null;
}

interface Progress {
  downloaded: number;
  total: number | null;
}

type State =
  | { type: "none" }
  | { type: "available"; update: AvailableUpdate }
  | { type: "installing"; update: AvailableUpdate; percent: number | null }
  | { type: "failed"; update: AvailableUpdate; reason: string };

let state: State = { type: "none" };
let button: HTMLButtonElement;

function render(): void {
  button.hidden = state.type === "none";
  button.disabled = state.type === "installing";
  button.classList.toggle("failed", state.type === "failed");
  let label = "";
  let title = "";
  switch (state.type) {
    case "none":
      break;
    case "available":
      label = `Update to ${state.update.version}`;
      title = "Installs the new version and restarts the app; running turns go on.";
      break;
    case "installing":
      label = state.percent === null ? "Updating…" : `Updating… ${state.percent}%`;
      break;
    case "failed":
      label = `Update to ${state.update.version} failed, try again`;
      title = state.reason;
      break;
  }
  const text = document.createElement("span");
  text.className = "sidebar-action-label";
  text.textContent = label;
  button.replaceChildren(icon("download"), text);
  button.title = title;
}

async function check(): Promise<void> {
  // Never replace an update that is being installed or offered again.
  if (state.type === "installing") return;
  try {
    const update = await invoke<AvailableUpdate | null>("check_app_update");
    if (update === null) {
      state = { type: "none" };
    } else if (state.type === "failed" && state.update.version === update.version) {
      // Keep saying why the last attempt failed; a click tries again.
      state = { ...state, update };
    } else {
      state = { type: "available", update };
    }
  } catch (error) {
    // No feed yet, no network: try again at the next check, quietly.
    console.warn("looking for an app update failed", error);
    return;
  }
  render();
}

async function install(): Promise<void> {
  if (state.type !== "available" && state.type !== "failed") return;
  const { update } = state;
  state = { type: "installing", update, percent: null };
  render();
  try {
    // Restarts the app when it succeeds.
    await invoke("install_app_update");
  } catch (error) {
    const reason = typeof error === "object" && error !== null && "reason" in error ? String(error.reason) : String(error);
    state = { type: "failed", update, reason };
    render();
    // The failed attempt used up the found update; find it again.
    void check();
  }
}

/** Starts looking for updates; `updateButton` offers a found one. */
export async function start(updateButton: HTMLButtonElement): Promise<void> {
  button = updateButton;
  button.addEventListener("click", () => void install());
  await listen<Progress>("app-update-progress", ({ payload }) => {
    if (state.type !== "installing") return;
    state.percent = payload.total ? Math.min(100, Math.round((payload.downloaded / payload.total) * 100)) : null;
    render();
  });
  render();
  await check();
  window.setInterval(() => void check(), CHECK_EVERY_MS);
}
