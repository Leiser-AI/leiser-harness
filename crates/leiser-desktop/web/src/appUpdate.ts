// Updates of the app itself (see the app's `app_update.rs`): looks for a
// new version when the app starts, every few minutes while it runs and when
// its window gets the focus again, and offers it in the sidebar. Installing
// it restarts the app; running turns go on in the harness service, which
// takes the new version as a service update.

import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";

import { icon } from "./icons";

/** How often the app looks for a new version while it runs. */
const CHECK_EVERY_MS = 10 * 60 * 1000;
/** A window that gets the focus looks again only when the last look is this old. */
const FOCUS_CHECK_MIN_AGE_MS = 60 * 1000;

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
/** When the app last looked for a new version, as `Date.now()`; 0 after a look that failed. */
let checkedAt = 0;
/** The look that is under way; only one runs at a time. */
let looking: Promise<void> | null = null;

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

/** Read through a call, so a check after an `await` sees the current state. */
function installing(): boolean {
  return state.type === "installing";
}

/** Looks for a new version, or waits for the look that is under way. */
function check(): Promise<void> {
  looking ??= look().finally(() => {
    looking = null;
  });
  return looking;
}

async function look(): Promise<void> {
  if (installing()) return;
  checkedAt = Date.now();
  let update: AvailableUpdate | null;
  try {
    update = await invoke<AvailableUpdate | null>("check_app_update");
  } catch (error) {
    // No feed yet, no network: try again at the next check, quietly.
    console.warn("looking for an app update failed", error);
    // The next focus tries again without waiting.
    checkedAt = 0;
    return;
  }
  // An install may have started while the check ran.
  if (installing()) return;
  if (update === null) {
    state = { type: "none" };
  } else if (state.type === "failed" && state.update.version === update.version) {
    // Keep saying why the last attempt failed; a click tries again.
    state = { ...state, update };
  } else {
    state = { type: "available", update };
  }
  render();
}

/** What an `UpdateError` from the app says. */
function reasonOf(error: unknown): string {
  if (typeof error === "object" && error !== null && "type" in error) {
    if ("reason" in error) return String(error.reason);
    if (error.type === "no_update") return "No update was found to install.";
  }
  return String(error);
}

async function install(): Promise<void> {
  if (state.type !== "available" && state.type !== "failed") return;
  const { update } = state;
  state = { type: "installing", update, percent: null };
  render();
  try {
    // A look under way would replace the found update while it installs.
    await looking;
    // Restarts the app when it succeeds.
    await invoke("install_app_update");
  } catch (error) {
    state = { type: "failed", update, reason: reasonOf(error) };
    render();
  }
}

/** Starts looking for updates; `updateButton` offers a found one. */
export async function start(updateButton: HTMLButtonElement): Promise<void> {
  button = updateButton;
  button.addEventListener("click", () => void install());
  await listen<Progress>("app-update-progress", ({ payload }) => {
    if (state.type !== "installing") return;
    const percent = payload.total ? Math.min(100, Math.round((payload.downloaded / payload.total) * 100)) : null;
    // Many chunks make one percent; draw only a change.
    if (percent === state.percent) return;
    state.percent = percent;
    render();
  });
  render();
  window.setInterval(() => void check(), CHECK_EVERY_MS);
  // Coming back to the app is when the developer can take an update, and a
  // window in the background may run its timers late.
  await getCurrentWindow().onFocusChanged(({ payload: focused }) => {
    if (focused && Date.now() - checkedAt >= FOCUS_CHECK_MIN_AGE_MS) void check();
  });
  await check();
}
