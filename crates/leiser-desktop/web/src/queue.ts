// The task queue view: one repository's queue of prompts and issues that
// the harness service runs by itself, each in a chat in a new worktree.
// It shows the tasks by where they are (needs attention, running, queued,
// done), and lets the developer add, reorder, cancel and remove tasks,
// pause and resume the queue, change how many tasks run at once and jump
// into any task's chat.
//
// The view keeps only the form the developer types into: the queue
// itself comes from the harness service, which reports every change, and
// the app renders it again. The list is rebuilt only when the queue
// changed, so other events keep the developer's focus.

import type {
  AgentCli,
  AttentionReason,
  ChatId,
  PermissionMode,
  QueueCommand,
  QueueItem,
  QueueItemId,
  QueueSettings,
  QueueStatus,
  QueueTask,
  RepositoryId,
  RepositoryQueue,
} from "./protocol";
import { icon, type IconName } from "./icons";
import * as service from "./service";
import { AGENT_CLI_NAMES, PERMISSION_MODE_NAMES } from "./service";

/** What the view needs from the app. */
export interface QueueActions {
  /** Shows the chat of a task. */
  openChat(chat: ChatId): void;
  /** Reads the service again after a change. */
  changed(): Promise<void>;
  showError(error: unknown): void;
  clearError(): void;
}

/** The most tasks a queue runs at once (`MAX_CONCURRENCY` in `queue.rs`). */
const MAX_CONCURRENCY = 8;
const AGENT_CLIS: AgentCli[] = ["claude_code", "codex"];
const PERMISSION_MODES: PermissionMode[] = ["supervised", "auto_accept_edits", "auto", "full_access"];

let actions: QueueActions;
let root: HTMLElement;
let header: HTMLElement;
let lists: HTMLElement;
let form: HTMLFormElement;
let kindSelect: HTMLSelectElement;
let promptInput: HTMLTextAreaElement;
let issueInput: HTMLInputElement;
let agentSelect: HTMLSelectElement;
let modeSelect: HTMLSelectElement;
let addButton: HTMLButtonElement;

// The repository whose queue shows, and what it showed last.
let shownRepository: RepositoryId | null = null;
let renderedFingerprint: string | null = null;

function el<K extends keyof HTMLElementTagNameMap>(
  tag: K,
  className?: string,
  text?: string,
): HTMLElementTagNameMap[K] {
  const element = document.createElement(tag);
  if (className) element.className = className;
  if (text !== undefined) element.textContent = text;
  return element;
}

function button(text: string, onClick: () => Promise<void>, className = "small"): HTMLButtonElement {
  const element = el("button", className, text);
  element.type = "button";
  element.addEventListener("click", () => void run(onClick));
  return element;
}

function iconButton(name: IconName, label: string, onClick: () => Promise<void>): HTMLButtonElement {
  const element = el("button", "icon-button");
  element.type = "button";
  element.setAttribute("aria-label", label);
  element.title = label;
  element.append(icon(name));
  element.addEventListener("click", () => void run(onClick));
  return element;
}

function option(value: string, text: string, selected: boolean): HTMLOptionElement {
  const element = el("option", "", text);
  element.value = value;
  element.selected = selected;
  return element;
}

/** Runs what the developer started, then reads the service again. */
async function run(action: () => Promise<void>): Promise<void> {
  actions.clearError();
  try {
    await action();
    await actions.changed();
  } catch (error) {
    actions.showError(error);
  }
}

async function command(command: QueueCommand): Promise<void> {
  await service.queueCommand(command);
}

/** Builds the view's form once; `render` fills the rest. */
export function mount(container: HTMLElement, queueActions: QueueActions): void {
  actions = queueActions;
  root = container;
  header = el("div", "queue-header");
  lists = el("div", "queue-lists");

  form = el("form", "queue-form");
  form.setAttribute("aria-label", "Add a task");
  kindSelect = el("select");
  kindSelect.setAttribute("aria-label", "Kind of task");
  kindSelect.append(option("prompt", "Prompt", true), option("issue", "Issue", false));
  promptInput = el("textarea", "queue-prompt");
  promptInput.rows = 3;
  promptInput.placeholder = "What should the agent do? The text is the chat's first message.";
  promptInput.setAttribute("aria-label", "Prompt");
  issueInput = el("input", "queue-issue");
  issueInput.type = "number";
  issueInput.min = "1";
  issueInput.placeholder = "Issue number, such as 42";
  issueInput.setAttribute("aria-label", "Issue number");
  issueInput.hidden = true;
  agentSelect = el("select");
  agentSelect.setAttribute("aria-label", "Agent CLI of the task");
  agentSelect.append(
    option("", "Queue's agent CLI", true),
    ...AGENT_CLIS.map((agentCli) => option(agentCli, AGENT_CLI_NAMES[agentCli], false)),
  );
  modeSelect = el("select");
  modeSelect.setAttribute("aria-label", "Permission mode of the task");
  modeSelect.append(
    option("", "Queue's permission mode", true),
    ...PERMISSION_MODES.map((mode) => option(mode, PERMISSION_MODE_NAMES[mode], false)),
  );
  addButton = el("button", "primary", "Add to queue");
  addButton.type = "submit";

  kindSelect.addEventListener("change", () => {
    const issue = kindSelect.value === "issue";
    promptInput.hidden = issue;
    issueInput.hidden = !issue;
    (issue ? issueInput : promptInput).focus();
  });
  promptInput.addEventListener("keydown", (event) => {
    if (event.key === "Enter" && (event.metaKey || event.ctrlKey)) {
      event.preventDefault();
      form.requestSubmit();
    }
  });
  form.addEventListener("submit", (event) => {
    event.preventDefault();
    void run(enqueue);
  });
  const options = el("div", "queue-form-options");
  options.append(kindSelect, agentSelect, modeSelect, addButton);
  form.append(promptInput, issueInput, options);

  root.replaceChildren(header, form, lists);
}

async function enqueue(): Promise<void> {
  if (shownRepository === null) return;
  const task: QueueTask =
    kindSelect.value === "issue"
      ? { type: "issue", number: Number.parseInt(issueInput.value, 10) || 0 }
      : { type: "prompt", text: promptInput.value };
  await command({
    type: "enqueue",
    repository: shownRepository,
    task,
    agent_cli: (agentSelect.value || null) as AgentCli | null,
    permission_mode: (modeSelect.value || null) as PermissionMode | null,
  });
  promptInput.value = "";
  issueInput.value = "";
  (task.type === "issue" ? issueInput : promptInput).focus();
}

/** Shows the repository's queue; `null` when the service could not read it. */
export function render(repository: RepositoryId, queue: RepositoryQueue | null): void {
  root.hidden = false;
  if (repository !== shownRepository) {
    shownRepository = repository;
    renderedFingerprint = null;
  }
  const fingerprint = JSON.stringify(queue);
  if (fingerprint === renderedFingerprint) return;
  renderedFingerprint = fingerprint;
  form.hidden = queue === null;
  if (queue === null) {
    header.replaceChildren(
      el("p", "empty", "The harness service could not read the task queue. It may be older than the app."),
    );
    lists.replaceChildren();
    return;
  }
  renderHeader(queue);
  renderLists(queue);
}

export function hide(): void {
  if (root.hidden) return;
  root.hidden = true;
  renderedFingerprint = null;
}

/** Whether the task holds a slot of the queue. */
function active(item: QueueItem): boolean {
  return item.state.type === "starting" || item.state.type === "running";
}

/** The facts the top bar shows about a queue. */
export function facts(queue: RepositoryQueue): { status: string; running: string } {
  return {
    status: statusText(queue.status),
    running: `${queue.items.filter(active).length} of ${queue.settings.concurrency}`,
  };
}

/**
 * A short note for the sidebar: what needs the developer first, else what
 * runs and waits, such as "1 running · 2 queued"; `null` when nothing is
 * in line.
 */
export function summary(queue: RepositoryQueue | null): string | null {
  if (queue === null) return null;
  const count = (test: (item: QueueItem) => boolean) => queue.items.filter(test).length;
  const attention = count(needsAttention);
  if (attention > 0) return `${attention} need${attention === 1 ? "s" : ""} you`;
  const queued = count((item) => item.state.type === "queued");
  if (queue.status.type === "paused" && queued > 0) return `paused · ${queued} queued`;
  const parts: string[] = [];
  const running = count(active);
  if (running > 0) parts.push(`${running} running`);
  if (queued > 0) parts.push(`${queued} queued`);
  return parts.length === 0 ? null : parts.join(" · ");
}

function needsAttention(item: QueueItem): boolean {
  return item.state.type === "needs_attention" || (item.state.type === "running" && item.state.waiting_for_you);
}

function statusText(status: QueueStatus): string {
  switch (status.type) {
    case "idle":
      return "Nothing to run";
    case "running":
      return "Running";
    case "paused":
      return "Paused";
    case "waiting_for_usage_limits":
      return status.resets_in_seconds === null
        ? "Waiting for usage limits"
        : `Waiting for usage limits, ${duration(status.resets_in_seconds)}`;
    case "waiting_for_update":
      return "Waiting for the service update";
  }
}

/** A time until something, such as "resets in 2 h 5 min". */
function duration(seconds: number): string {
  const minutes = Math.ceil(seconds / 60);
  if (minutes < 60) return `resets in ${minutes} min`;
  const hours = Math.floor(minutes / 60);
  const rest = minutes % 60;
  return rest === 0 ? `resets in ${hours} h` : `resets in ${hours} h ${rest} min`;
}

function renderHeader(queue: RepositoryQueue): void {
  const repository = queue.repository;
  const status = el("span", `badge ${statusBadge(queue.status)}`, statusText(queue.status));
  const paused = queue.status.type === "paused";
  const toggle = button(
    paused ? "Resume" : "Pause",
    () => command({ type: paused ? "resume" : "pause", repository }),
    paused ? "small primary" : "small",
  );
  toggle.title = paused
    ? "Start queued tasks again, and try subscriptions at a usage limit again"
    : "Start no more tasks; running ones go on";
  const retry =
    queue.status.type === "waiting_for_usage_limits"
      ? button("Try again", () => command({ type: "resume", repository }))
      : null;
  if (retry !== null) retry.title = "Try the subscriptions at a usage limit again";

  const change = (settings: Partial<QueueSettings>) =>
    command({ type: "change_settings", repository, settings: { ...queue.settings, ...settings } });
  const concurrency = el("select");
  concurrency.setAttribute("aria-label", "Tasks at once");
  for (let count = 1; count <= MAX_CONCURRENCY; count++) {
    concurrency.append(option(String(count), String(count), count === queue.settings.concurrency));
  }
  concurrency.addEventListener("change", () => void run(() => change({ concurrency: Number(concurrency.value) })));
  const agent = el("select");
  agent.setAttribute("aria-label", "Agent CLI of the queue");
  agent.append(
    option("", "Default agent CLI", queue.settings.agent_cli === null),
    ...AGENT_CLIS.map((agentCli) => option(agentCli, AGENT_CLI_NAMES[agentCli], agentCli === queue.settings.agent_cli)),
  );
  agent.addEventListener("change", () => void run(() => change({ agent_cli: (agent.value || null) as AgentCli | null })));
  const mode = el("select");
  mode.setAttribute("aria-label", "Permission mode of the queue");
  mode.append(
    option("", "Repository's permission mode", queue.settings.permission_mode === null),
    ...PERMISSION_MODES.map((candidate) =>
      option(candidate, PERMISSION_MODE_NAMES[candidate], candidate === queue.settings.permission_mode),
    ),
  );
  mode.addEventListener("change", () =>
    void run(() => change({ permission_mode: (mode.value || null) as PermissionMode | null })),
  );

  const statusRow = el("div", "queue-status");
  statusRow.append(status, toggle);
  if (retry !== null) statusRow.append(retry);
  const settings = el("div", "queue-settings");
  settings.append(labelled("At once", concurrency), labelled("Agent CLI", agent), labelled("Permission mode", mode));
  const intro = el(
    "p",
    "muted queue-intro",
    "Each task runs in a chat of its own, in a new worktree. When a task's turn ends or its chat opens a pull request, the next one starts.",
  );
  header.replaceChildren(statusRow, settings, intro);
}

function labelled(text: string, control: HTMLElement): HTMLElement {
  const label = el("label", "field");
  label.append(el("span", "", text), control);
  return label;
}

function statusBadge(status: QueueStatus): string {
  switch (status.type) {
    case "running":
      return "accent";
    case "paused":
    case "waiting_for_usage_limits":
    case "waiting_for_update":
      return "warning";
    case "idle":
      return "";
  }
}

function renderLists(queue: RepositoryQueue): void {
  const attention = queue.items.filter(needsAttention);
  const running = queue.items.filter((item) => active(item) && !needsAttention(item));
  const queued = queue.items.filter((item) => item.state.type === "queued");
  const done = queue.items.filter(
    (item) =>
      item.state.type === "completed" || item.state.type === "pull_request_ready" || item.state.type === "cancelled",
  );
  const sections = [
    section("Needs attention", attention, (item) => row(item, attentionNote(item), chatActions(item))),
    section("Running", running, (item) => row(item, runningNote(item), chatActions(item))),
    section("Queued", queued, (item, index) => row(item, "Queued", queuedActions(item, index, queued.length))),
    section("Done", done, (item) => row(item, doneNote(item), doneActions(item))),
  ].filter((part) => part !== null);
  if (sections.length === 0) {
    lists.replaceChildren(el("p", "empty", "No tasks yet. Add a prompt or an issue above."));
    return;
  }
  lists.replaceChildren(...sections);
}

function section(
  title: string,
  items: QueueItem[],
  render: (item: QueueItem, index: number) => HTMLElement,
): HTMLElement | null {
  if (items.length === 0) return null;
  const part = el("section", "queue-section");
  const heading = el("h2", "section-label", `${title} (${items.length})`);
  const list = el("ol", "queue-list");
  list.append(...items.map((item, index) => render(item, index)));
  part.append(heading, list);
  return part;
}

function row(item: QueueItem, note: string, buttons: HTMLElement[]): HTMLElement {
  const entry = el("li", `queue-item ${item.state.type}`);
  entry.dataset.item = String(item.item);
  const text = el("div", "queue-item-text");
  const title = el("span", "queue-item-title", item.title);
  title.title = item.task.type === "prompt" ? item.task.text : item.title;
  const meta = el("span", "queue-item-note", note);
  const badges = el("span", "queue-item-badges");
  if (item.agent_cli !== null) badges.append(el("span", "badge", AGENT_CLI_NAMES[item.agent_cli]));
  if (item.permission_mode !== null) badges.append(el("span", "badge", PERMISSION_MODE_NAMES[item.permission_mode]));
  text.append(title, meta);
  const controls = el("div", "queue-item-actions");
  controls.append(...buttons);
  entry.append(text, badges, controls);
  return entry;
}

function chatOf(item: QueueItem): ChatId | null {
  switch (item.state.type) {
    case "running":
    case "completed":
    case "pull_request_ready":
      return item.state.chat;
    case "needs_attention":
    case "cancelled":
      return item.state.chat;
    case "queued":
    case "starting":
      return null;
  }
}

function openChatButton(item: QueueItem): HTMLButtonElement[] {
  const chat = chatOf(item);
  if (chat === null) return [];
  const open = button("Open chat", async () => actions.openChat(chat));
  open.title = "Show the task's chat";
  return [open];
}

/** For tasks that run or need the developer. */
function chatActions(item: QueueItem): HTMLElement[] {
  const buttons = openChatButton(item);
  if (item.state.type === "needs_attention") {
    const remove = button("Remove", () => command({ type: "remove", item: item.item }), "small ghost");
    remove.title = "Take the task off the list; its chat stays";
    buttons.push(remove);
  } else {
    buttons.push(cancelButton(item.item, "Interrupts the turn; the chat stays"));
  }
  return buttons;
}

function cancelButton(item: QueueItemId, title: string): HTMLButtonElement {
  const cancel = button("Cancel", () => command({ type: "cancel", item }), "small danger");
  cancel.title = title;
  return cancel;
}

function queuedActions(item: QueueItem, index: number, count: number): HTMLElement[] {
  const up = iconButton("chevron", "Move up", () => command({ type: "move", item: item.item, to: index - 1 }));
  up.classList.add("queue-up");
  up.disabled = index === 0;
  const down = iconButton("chevron", "Move down", () => command({ type: "move", item: item.item, to: index + 1 }));
  down.classList.add("queue-down");
  down.disabled = index === count - 1;
  return [up, down, cancelButton(item.item, "Take the task out of line")];
}

function doneActions(item: QueueItem): HTMLElement[] {
  const buttons: HTMLElement[] = [];
  if (item.state.type === "pull_request_ready") {
    const url = item.state.url;
    const open = button(`Pull request #${item.state.number}`, () => service.openInBrowser(url));
    open.prepend(icon("external"));
    open.title = `${url}\nOpen the pull request on the git host`;
    buttons.push(open);
  }
  buttons.push(...openChatButton(item));
  const remove = button("Remove", () => command({ type: "remove", item: item.item }), "small ghost");
  remove.title = "Take the task off the list; its chat stays";
  buttons.push(remove);
  return buttons;
}

function runningNote(item: QueueItem): string {
  return item.state.type === "starting" ? "Creating its worktree…" : "Working";
}

function attentionNote(item: QueueItem): string {
  if (item.state.type === "running") return "Waits for your answer to an approval request or a question";
  if (item.state.type !== "needs_attention") return "";
  return reasonText(item.state.reason);
}

function reasonText(reason: AttentionReason): string {
  switch (reason.type) {
    case "start_failed":
      return `Could not start: ${reason.reason}`;
    case "turn_failed":
      return `The turn failed: ${reason.reason}`;
    case "interrupted":
      return "The turn was interrupted";
    case "session_ended":
      return "The agent session ended before the turn did";
    case "usage_limit":
      return reason.resets_in_seconds === null
        ? `Stopped at the ${reason.limit}`
        : `Stopped at the ${reason.limit}; ${duration(reason.resets_in_seconds)}`;
    case "service_restarted":
      return "The harness service restarted during the turn";
  }
}

function doneNote(item: QueueItem): string {
  switch (item.state.type) {
    case "completed":
      return "Turn ended";
    case "pull_request_ready":
      return "Pull request open, ready for review";
    case "cancelled":
      return item.state.chat === null ? "Cancelled before it started" : "Cancelled";
    default:
      return "";
  }
}
