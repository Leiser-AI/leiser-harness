// The terminal panel (#40): a shell in the selected chat's checkout.
//
// The shells run in the app's Rust side, one per chat, and keep running
// while another chat is selected or the panel is hidden. They end when the
// developer ends them, when they exit, and when the app quits. Each chat's
// screen stays here, so switching back shows what the shell printed.

import { Channel, invoke } from "@tauri-apps/api/core";
import { FitAddon } from "@xterm/addon-fit";
import { Terminal } from "@xterm/xterm";
import "@xterm/xterm/css/xterm.css";

import type { ChatId } from "./protocol";

/** One chat's terminal screen and, while it runs, its shell. */
interface ChatTerminal {
  screen: Terminal;
  fit: FitAddon;
  element: HTMLDivElement;
  /** The running shell; `null` before it started and after it ended. */
  shell: number | null;
  /** Whether a shell is starting right now. */
  starting: boolean;
  /** Counts the shells started here; messages of an earlier one are dropped. */
  run: number;
}

/** What the Rust side sends when a shell exits. */
interface Exit {
  exit_code: number;
}

const panel = byId("terminal-panel");
const screens = byId("terminal-screens");
const directoryLabel = byId("terminal-directory");
const endButton = byId<HTMLButtonElement>("terminal-end");
const hideButton = byId<HTMLButtonElement>("terminal-hide");

const terminals = new Map<ChatId, ChatTerminal>();
// Whether the developer wants the panel open; it stays open across chats.
let wanted = false;
let shown: ChatId | null = null;
// Notified when the panel opens or closes, so the toggle can follow.
let onChange: () => void = () => {};

function byId<T extends HTMLElement = HTMLElement>(id: string): T {
  const element = document.getElementById(id);
  if (!element) throw new Error(`missing element #${id}`);
  return element as T;
}

/** Sets up the panel. A reloaded page cannot reach the shells of the page it replaced, so they end. */
export function init(changed: () => void): void {
  onChange = changed;
  void invoke("close_all_terminals").catch(() => {});
  endButton.addEventListener("click", () => {
    if (shown !== null) end(shown);
  });
  hideButton.addEventListener("click", () => toggle(null));
  new ResizeObserver(() => fitShown()).observe(screens);
}

/** Whether the panel is open. */
export function isOpen(): boolean {
  return wanted;
}

/** Opens or closes the panel; opening it starts the chat's shell if none runs. */
export function toggle(chat: ChatId | null, directory?: string): void {
  wanted = !wanted;
  show(chat, directory);
  onChange();
}

/**
 * Shows the selected chat's terminal while the panel is open, or nothing.
 * `directory` is the chat's checkout, for the panel's header.
 */
export function show(chat: ChatId | null, directory?: string): void {
  panel.hidden = !wanted || chat === null;
  if (panel.hidden || chat === null) {
    shown = null;
    return;
  }
  directoryLabel.textContent = directory ?? "";
  directoryLabel.title = directory ?? "";
  const terminal = terminals.get(chat) ?? create(chat);
  if (shown !== chat) {
    for (const [other, { element }] of terminals) element.hidden = other !== chat;
    shown = chat;
    fitShown();
    terminal.screen.focus();
  }
  endButton.disabled = terminal.shell === null;
  if (terminal.run === 0) {
    void start(chat, terminal);
  }
}

function create(chat: ChatId): ChatTerminal {
  const element = document.createElement("div");
  element.className = "terminal-screen";
  screens.append(element);
  const style = getComputedStyle(document.documentElement);
  const screen = new Terminal({
    cursorBlink: true,
    fontFamily: 'ui-monospace, SFMono-Regular, Menlo, Consolas, "Cascadia Mono", monospace',
    fontSize: 13,
    scrollback: 5000,
    theme: {
      background: style.getPropertyValue("--terminal-bg").trim(),
      foreground: style.getPropertyValue("--terminal-fg").trim(),
      cursor: style.getPropertyValue("--terminal-fg").trim(),
    },
  });
  const fit = new FitAddon();
  screen.loadAddon(fit);
  screen.open(element);
  const terminal: ChatTerminal = {
    screen,
    fit,
    element,
    shell: null,
    starting: false,
    run: 0,
  };
  terminals.set(chat, terminal);
  screen.onData((data) => {
    if (terminal.shell !== null) {
      void invoke("write_terminal", { terminal: terminal.shell, data }).catch(() => {});
    } else if (!terminal.starting) {
      // After the shell ended, any key starts a new one.
      screen.reset();
      void start(chat, terminal);
    }
  });
  screen.onResize(({ rows, cols }) => {
    if (terminal.shell !== null) {
      void invoke("resize_terminal", { terminal: terminal.shell, rows, cols }).catch(() => {});
    }
  });
  return terminal;
}

async function start(chat: ChatId, terminal: ChatTerminal): Promise<void> {
  terminal.starting = true;
  const run = ++terminal.run;
  let exited = false;
  if (shown === chat) fitShown();
  const events = new Channel<ArrayBuffer | Exit>((message) => {
    // A shell the developer ended may still send its last output.
    if (run !== terminal.run) return;
    if (message instanceof ArrayBuffer) {
      terminal.screen.write(new Uint8Array(message));
      return;
    }
    exited = true;
    terminal.shell = null;
    terminal.screen.write(
      `\r\n\x1b[2m[The shell exited with code ${message.exit_code}. Press any key to start a new one.]\x1b[0m\r\n`,
    );
    if (shown === chat) endButton.disabled = true;
  });
  try {
    const shell = await invoke<number>("open_terminal", {
      chat,
      rows: terminal.screen.rows,
      cols: terminal.screen.cols,
      events,
    });
    // A shell that exited at once is already gone.
    if (!exited) terminal.shell = shell;
  } catch (error) {
    terminal.screen.write(
      `\x1b[31mCould not start a shell: ${String(error)}\x1b[0m\r\nPress any key to try again.\r\n`,
    );
  } finally {
    terminal.starting = false;
  }
  if (shown === chat) endButton.disabled = terminal.shell === null;
}

/** Ends the chat's shell and the programs it runs; the screen stays. */
function end(chat: ChatId): void {
  const terminal = terminals.get(chat);
  if (!terminal || terminal.shell === null) return;
  const shell = terminal.shell;
  terminal.shell = null;
  terminal.run++;
  endButton.disabled = true;
  terminal.screen.write("\r\n\x1b[2m[Shell ended. Press any key to start a new one.]\x1b[0m\r\n");
  void invoke("close_terminal", { terminal: shell }).catch(() => {});
}

function fitShown(): void {
  if (shown === null || panel.hidden) return;
  const terminal = terminals.get(shown);
  if (!terminal) return;
  try {
    terminal.fit.fit();
  } catch {
    // The panel has no size yet; the next resize fits it.
  }
}
