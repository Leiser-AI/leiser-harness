// The desktop app's UI. It holds no chat state of its own: it reads
// repositories, their repo-check results and the selected chat from the
// harness service and reads them again on every service event. The only
// thing it keeps is what is selected: a chat, a repository's repo-check or
// a repository's CLI extensions panel.

import type {
  AgentEvent,
  Answer,
  Branch,
  ChatId,
  ChatOverview,
  PermissionMode,
  Question,
  ChatStatus,
  Checkout,
  Provider,
  Finding,
  OptOuts,
  RepoCheck,
  RepositoryId,
  RepositoryOverview,
  RepositoryScript,
  ServiceEvent,
  SubscriptionId,
  SubscriptionOverview,
  ServiceConnection,
  Todo,
  TranscriptEntry,
  UsageLimit,
} from "./protocol";
import * as extensions from "./extensions";
import { icon, type IconName } from "./icons";
import * as service from "./service";
import * as terminal from "./terminal";
import {
  AGENT_CLI_NAMES,
  AGENT_CLI_PROVIDERS,
  PERMISSION_MODE_NAMES,
  PROVIDER_NAMES,
  type ChatView,
} from "./service";

let selectedChat: ChatId | null = null;
// Shown instead of a chat when set.
let selectedCheck: RepositoryId | null = null;
// The repository whose CLI extensions panel is open instead of a chat.
let selectedExtensions: RepositoryId | null = null;
// Drops answers to older reads that arrive after newer ones.
let readCount = 0;

const repositoriesNav = byId("repositories");
const topBar = byId("top-bar");
const transcript = byId("transcript");
const composer = byId<HTMLFormElement>("composer");
const messageInput = byId<HTMLTextAreaElement>("message");
const sendButton = byId<HTMLButtonElement>("send");
const interruptButton = byId<HTMLButtonElement>("interrupt");
const composerOptions = byId("composer-options");
const errorBox = byId("error");
const subscriptionsDialog = byId<HTMLDialogElement>("subscriptions-dialog");
const subscriptionsError = byId("subscriptions-error");
const subscriptionList = byId("subscription-list");
const addSubscriptionForm = byId<HTMLFormElement>("add-subscription");
const newProvider = byId<HTMLSelectElement>("new-provider");
const newName = byId<HTMLInputElement>("new-name");
const newKind = byId<HTMLSelectElement>("new-kind");
const newKeyLabel = byId("new-key-label");
const newKey = byId<HTMLInputElement>("new-key");
const loginSection = byId("login-section");
const loginOutput = byId("login-output");
const loginAnswer = byId<HTMLFormElement>("login-answer");
const loginAnswerText = byId<HTMLInputElement>("login-answer-text");

// The lines of logins run since the app started; the service does not
// keep them.
const loginLines: string[] = [];
// The subscription whose login printed last; the answer goes to it.
let answeringLogin: number | null = null;

const repoCheckView = byId("repo-check");
const serviceNotice = byId("service-notice");
const todosPanel = byId("todos");
const todoList = byId("todo-list");
const todoForm = byId<HTMLFormElement>("todo-form");
const todoInput = byId<HTMLInputElement>("todo-text");
extensions.mount(byId("extensions"));

function byId<T extends HTMLElement = HTMLElement>(id: string): T {
  const element = document.getElementById(id);
  if (!element) throw new Error(`missing element #${id}`);
  return element as T;
}

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

/** A button that shows only an icon; `label` names it for screen readers. */
function iconButton(name: IconName, label: string, className = ""): HTMLButtonElement {
  const button = el("button", `icon-button ${className}`.trim());
  button.type = "button";
  button.setAttribute("aria-label", label);
  button.append(icon(name));
  return button;
}

/**
 * Fills the top bar: the title and the view's buttons on the first line,
 * facts such as the repository and branch on the second.
 */
function renderTopBar(title: string, facts: HTMLElement[] = [], actions: HTMLElement[] = []): void {
  const heading = el("h1", "top-bar-title", title);
  heading.title = title;
  const first = el("div", "top-bar-row");
  first.append(heading, ...actions);
  const rows: HTMLElement[] = [first];
  if (facts.length > 0) {
    const second = el("div", "top-bar-facts");
    second.append(...facts);
    rows.push(second);
  }
  topBar.replaceChildren(...rows);
}

/** A top bar button; in a narrow window only its icon shows. */
function topBarAction(iconName: IconName, text: string, className = ""): HTMLButtonElement {
  const button = el("button", `top-bar-action ${className}`.trim());
  button.type = "button";
  button.append(icon(iconName), el("span", "action-label", text));
  return button;
}

function showError(error: unknown): void {
  errorBox.textContent = error instanceof Error ? error.message : String(error);
  errorBox.hidden = false;
}

function clearError(): void {
  errorBox.hidden = true;
}

/** Runs an action the developer started and shows why it failed. */
async function act(action: () => Promise<void>): Promise<void> {
  clearError();
  try {
    await action();
  } catch (error) {
    showError(error);
  }
}

// Whether the selected chat runs a turn, as last read from the service.
let selectedWorking = false;
// The approval requests and questions the selected chat waits for.
let selectedOpenRequests: string[] = [];

/** What a transcript entry needs to know about the rest of the chat. */
interface ChatContext {
  chat: ChatId;
  open: Set<string>;
  answers: Map<string, Answer>;
  /** The name of a subscription, or of the default login for `null`. */
  subscriptionName: (subscription: SubscriptionId | null) => string;
  /** The agent CLI of the last agent message shown; its name shows again only on a change. */
  lastAgentCli: string | null;
}

// The connection to the harness service, as the app's Rust side reports it.
let connection: ServiceConnection = { state: "connecting" };

async function refresh(): Promise<void> {
  // While there is no connection, the service has nothing to read.
  if (connection.state !== "connected") {
    renderServiceNotice(false);
    return;
  }
  const read = ++readCount;
  const chat = selectedChat;
  const [repositories, chats, subscriptions, view, todos, updateWaiting] = await Promise.all([
    service.listRepositories(),
    service.listChats(),
    service.listSubscriptions(),
    chat === null ? Promise.resolve(null) : service.readChat(chat),
    chat === null ? Promise.resolve(null) : service.listTodos(chat),
    service.serviceUpdateWaiting(),
  ]);
  // A service older than the app has no repo-check; the rest still shows.
  const checks = new Map(
    (
      await Promise.all(
        repositories.map(async (overview) => {
          const check = await service.readRepoCheck(overview.repository).catch(() => null);
          return check === null ? null : ([overview.repository, check] as const);
        }),
      )
    ).filter((entry) => entry !== null),
  );
  if (read !== readCount) return;
  renderServiceNotice(updateWaiting);
  renderSidebar(repositories, chats, checks);
  // The subscriptions dialog can open over any view.
  renderSubscriptions(subscriptions);
  if (selectedExtensions !== null) {
    const shown = repositories.find((overview) => overview.repository === selectedExtensions);
    repoCheckView.hidden = true;
    renderExtensionsTopBar(shown ?? null);
    renderTodos(null, null);
    terminal.show(null);
    return;
  }
  const checked = repositories.find((overview) => overview.repository === selectedCheck);
  const check = selectedCheck === null ? undefined : checks.get(selectedCheck);
  if (checked && check) {
    renderRepoCheck(checked, check);
    renderTodos(null, null);
    terminal.show(null);
    return;
  }
  repoCheckView.hidden = true;
  transcript.hidden = false;
  const overview = chats.find((candidate) => candidate.chat === chat) ?? null;
  renderChat(
    view,
    overview,
    repositories.find((candidate) => candidate.repository === overview?.repository) ?? null,
    subscriptions,
  );
  renderTodos(chat, todos);
}

function renderTodos(chat: ChatId | null, todos: Todo[] | null): void {
  if (chat === null || todos === null) {
    todosPanel.hidden = true;
    return;
  }
  const items = todos.map((todo) => {
    const item = el("li", todo.done ? "todo done" : "todo");
    const label = el("label");
    const check = el("input");
    check.type = "checkbox";
    check.checked = todo.done;
    check.addEventListener("change", () =>
      act(async () => {
        await service.setTodoDone(chat, todo.id, check.checked);
        await refresh();
      }),
    );
    label.append(check, el("span", "todo-text", todo.text));
    const remove = iconButton("close", `Remove "${todo.text}"`, "todo-remove");
    remove.title = "Remove this todo";
    remove.addEventListener("click", () =>
      act(async () => {
        await service.removeTodo(chat, todo.id);
        await refresh();
      }),
    );
    item.append(label, remove);
    return item;
  });
  if (items.length === 0) {
    items.push(el("li", "empty", "No todos yet."));
  }
  todoList.replaceChildren(...items);
  todosPanel.hidden = false;
}

const STATUS_LABELS: Record<ChatStatus, string> = {
  working: "working",
  waiting_for_you: "waiting for you",
  done: "done",
};

const SCRIPT_NAMES: Record<RepositoryScript, string> = {
  setup: "Setup script",
  done: "Done script",
};

/** Starts a chat, selects it and puts the cursor in the composer. */
async function startAndSelect(repository: RepositoryId, checkout?: Checkout): Promise<void> {
  if (!leaveExtensions()) return;
  selectedChat = await service.startChat(repository, checkout);
  selectedCheck = null;
  await refresh();
  messageInput.focus();
}

function renderServiceNotice(updateWaiting: boolean): void {
  let text: string | null = null;
  switch (connection.state) {
    case "connecting":
      text = "Connecting to the harness service…";
      break;
    case "unavailable":
      text = `The harness service is not available: ${connection.reason}`;
      break;
    case "connected":
      if (updateWaiting) {
        text =
          "A harness service update is waiting. It is applied once no turn runs; " +
          "then every chat continues where it was.";
      }
      break;
  }
  serviceNotice.textContent = text ?? "";
  serviceNotice.hidden = text === null;
}

/** Leaves the CLI extensions panel; refuses while it has unsaved changes. */
function leaveExtensions(): boolean {
  if (selectedExtensions === null) return true;
  if (extensions.hasUnsavedChanges()) {
    showError("Save or discard your changes to the CLI extension first.");
    return false;
  }
  selectedExtensions = null;
  extensions.hide();
  transcript.hidden = false;
  return true;
}

function renderExtensionsTopBar(repository: RepositoryOverview | null): void {
  renderTopBar("CLI extensions", [labelled("Repository", repository?.name ?? "unknown")]);
}

function renderSidebar(
  repositories: RepositoryOverview[],
  chats: ChatOverview[],
  checks: Map<RepositoryId, RepoCheck>,
): void {
  const list = el("ul", "repository-list");
  if (repositories.length === 0) {
    list.append(el("li", "empty", "Add a repository to start a chat."));
  }
  for (const repository of repositories) {
    const item = el("li", "repository");
    const header = el("div", "repository-header");
    const name = el("span", "repository-name", repository.name);
    name.title = repository.path;
    const newChat = iconButton("plus", "New chat", "new-chat");
    newChat.title = `Start a chat in a new worktree of ${repository.name}`;
    newChat.addEventListener("click", () => act(() => startAndSelect(repository.repository)));
    const inMain = iconButton("home", "New chat in the main checkout", "new-chat secondary");
    inMain.title = `Start a chat in the main checkout of ${repository.name}`;
    inMain.addEventListener("click", () =>
      act(() => startAndSelect(repository.repository, { type: "main" })),
    );
    const showExtensions = iconButton("blocks", "CLI extensions", "secondary");
    showExtensions.title = `Skills, MCP servers, hooks and instruction files the agent CLIs load from ${repository.name}`;
    if (repository.repository === selectedExtensions) {
      showExtensions.classList.add("selected");
      showExtensions.setAttribute("aria-current", "true");
    }
    showExtensions.addEventListener("click", () =>
      act(async () => {
        if (selectedExtensions !== repository.repository && !leaveExtensions()) return;
        selectedChat = null;
        selectedCheck = null;
        selectedExtensions = repository.repository;
        repoCheckView.hidden = true;
        transcript.hidden = true;
        composer.hidden = true;
        await extensions.show(repository.repository);
        await refresh();
      }),
    );
    const actions = el("span", "repository-actions");
    actions.append(showExtensions, newChat, inMain);
    header.append(name, actions);

    const chatList = el("ul", "chat-list");
    const check = checks.get(repository.repository);
    if (check) {
      const checkItem = el("li");
      const button = el("button", "chat-link repo-check-link");
      button.type = "button";
      button.append(
        icon("shield"),
        el("span", "chat-title", "repo-check"),
        el("span", "chat-status", checkStatus(check)),
      );
      if (repository.repository === selectedCheck) {
        button.classList.add("selected");
        button.setAttribute("aria-current", "true");
      }
      button.addEventListener("click", () =>
        act(async () => {
          if (!leaveExtensions()) return;
          selectedCheck = repository.repository;
          selectedChat = null;
          await refresh();
        }),
      );
      checkItem.append(button);
      chatList.append(checkItem);
    }
    // The service lists waiting chats first, then working, then done ones.
    for (const chat of chats.filter((overview) => overview.repository === repository.repository)) {
      const chatItem = el("li");
      const button = el("button", `chat-link ${chat.status}`);
      button.type = "button";
      button.title = chat.worktree === null ? "Main checkout" : `Worktree: ${chat.worktree}`;
      button.append(
        el("span", "chat-title", chat.title ?? "New chat"),
        el("span", `chat-status ${chat.status}`, STATUS_LABELS[chat.status]),
      );
      if (chat.chat === selectedChat) {
        button.classList.add("selected");
        button.setAttribute("aria-current", "true");
      }
      button.addEventListener("click", () =>
        act(async () => {
          if (!leaveExtensions()) return;
          selectedChat = chat.chat;
          selectedCheck = null;
          await refresh();
        }),
      );
      chatItem.append(button);
      chatList.append(chatItem);
    }
    item.append(header, chatList);
    list.append(item);
  }
  repositoriesNav.replaceChildren(list);
}

function renderChat(
  view: ChatView | null,
  overview: ChatOverview | null,
  repository: RepositoryOverview | null,
  subscriptions: SubscriptionOverview[],
): void {
  if (view === null || overview === null) {
    terminal.show(null);
    selectedWorking = false;
    selectedOpenRequests = [];
    renderTopBar("No chat selected");
    transcript.replaceChildren(el("li", "empty", "Select a chat or start a new one."));
    composer.hidden = true;
    composerOptions.replaceChildren();
    return;
  }
  const chat = overview.chat;
  // The terminal panel's shell runs here.
  const checkout = overview.worktree ?? repository?.path;
  renderTopBar(
    overview.title ?? "New chat",
    [
      labelled("Repository", repository?.name ?? "unknown"),
      labelled("Branch", branchName(view.branch)),
      labelled("Checkout", overview.worktree === null ? "main checkout" : "worktree"),
    ],
    chatActions(overview, checkout),
  );
  const agentCli = el("span", "composer-agent-cli", AGENT_CLI_NAMES[view.agentCli]);
  agentCli.title = "The agent CLI this chat runs on";
  composerOptions.replaceChildren(
    agentCli,
    subscriptionPicker(view, subscriptions),
    permissionModePicker(overview.chat, view),
  );
  terminal.show(chat, checkout);
  const context: ChatContext = {
    chat,
    open: new Set(view.openRequests),
    answers: new Map(),
    lastAgentCli: null,
    subscriptionName: (subscription) =>
      subscription === null
        ? "the default login"
        : (subscriptions.find((overview) => overview.subscription === subscription)?.name ??
          "a removed subscription"),
  };
  for (const entry of view.transcript) {
    if (entry.type === "answer") context.answers.set(entry.request, entry.answer);
  }
  const entries = view.transcript.flatMap((entry) => renderEntry(entry, context) ?? []);
  if (view.usageLimit !== null) {
    entries.push(limitCard(view, view.usageLimit, subscriptions, context));
  }
  if (entries.length === 0) {
    entries.push(el("li", "empty", "Send a message to start the agent."));
  }
  const atBottom = transcript.scrollHeight - transcript.scrollTop - transcript.clientHeight < 40;
  transcript.replaceChildren(...entries);
  if (atBottom) transcript.scrollTop = transcript.scrollHeight;
  selectedOpenRequests = view.openRequests;
  // A turn that waits for an answer still runs and can be interrupted.
  selectedWorking = view.status === "working" || selectedOpenRequests.length > 0;
  // A text reply to a waiting chat declines its oldest request with feedback.
  sendButton.textContent =
    selectedOpenRequests.length > 0 ? "Reply" : selectedWorking ? "Queue" : "Send";
  messageInput.placeholder =
    selectedOpenRequests.length > 0
      ? "Reply instead of approving: the agent gets your text (Enter sends)"
      : "Message the agent (Enter sends or queues, Shift+Enter adds a line)";
  interruptButton.hidden = !selectedWorking;
  composer.hidden = false;
}

const PERMISSION_MODES: PermissionMode[] = ["supervised", "auto_accept_edits", "auto", "full_access"];

function permissionModePicker(chat: ChatId, view: ChatView): HTMLElement {
  const item = el("label", "composer-option");
  const select = el("select", "permission-mode");
  select.setAttribute("aria-label", "Permission mode");
  select.title = "Permission mode: how much the agent may do without your approval";
  for (const mode of PERMISSION_MODES) {
    const option = el("option", "", PERMISSION_MODE_NAMES[mode]);
    option.value = mode;
    option.selected = mode === view.permissionMode;
    select.append(option);
  }
  select.addEventListener("change", () =>
    act(async () => {
      await service.changePermissionMode(chat, select.value as PermissionMode);
      await refresh();
    }),
  );
  item.append(icon("shield"), select);
  if (view.appliedPermissionMode !== view.permissionMode) {
    const applied = PERMISSION_MODE_NAMES[view.appliedPermissionMode];
    item.title = `The agent CLI has no such mode, so the chat runs ${applied}.`;
    item.append(el("span", "permission-applied", `runs ${applied}`));
  }
  return item;
}

/** The top bar's buttons for the selected chat. */
function chatActions(overview: ChatOverview, checkout: string | undefined): HTMLElement[] {
  const actions: HTMLElement[] = [];
  const toggleTerminal = topBarAction("terminal", terminal.isOpen() ? "Hide terminal" : "Terminal", "ghost");
  toggleTerminal.title = "A shell in the chat's checkout";
  toggleTerminal.setAttribute("aria-pressed", String(terminal.isOpen()));
  toggleTerminal.addEventListener("click", () => terminal.toggle(overview.chat, checkout));
  actions.push(toggleTerminal);
  if (overview.worktree !== null) {
    const sameWorktree = topBarAction("plus", "New chat in this worktree", "ghost");
    sameWorktree.title = overview.worktree;
    sameWorktree.addEventListener("click", () =>
      act(() => startAndSelect(overview.repository, { type: "worktree", chat: overview.chat })),
    );
    actions.push(sameWorktree);
  }
  if (overview.status === "waiting_for_you") {
    const done = topBarAction("check", "Mark done");
    done.addEventListener("click", () =>
      act(async () => {
        await service.markChatDone(overview.chat);
        await refresh();
      }),
    );
    actions.push(done);
  }
  return actions;
}

/** A short status for the sidebar. */
function checkStatus(check: RepoCheck): string {
  switch (check.state) {
    case "not_run":
      return "not run";
    case "running":
      return "running";
    case "done":
      return `${check.report.must.length} Must · ${check.report.should.length} Should`;
    case "unreadable":
      return "unreadable";
    case "failed":
      return "failed";
  }
}

function renderRepoCheck(repository: RepositoryOverview, check: RepoCheck): void {
  selectedWorking = false;
  composer.hidden = true;
  transcript.hidden = true;
  repoCheckView.hidden = false;
  const items = [labelled("Repository", repository.name)];
  if (check.state === "done" && check.report.commit !== null) {
    items.push(labelled("Commit", check.report.commit));
  }
  renderTopBar("repo-check", items);

  const run = el("button", "primary", check.state === "not_run" ? "Run repo-check" : "Run again");
  run.type = "button";
  run.disabled = check.state === "running";
  run.addEventListener("click", () =>
    act(async () => {
      await service.runRepoCheck(repository.repository);
      await refresh();
    }),
  );
  const intro = el("div", "repo-check-intro");
  intro.append(
    el("p", "muted", "repo-check only reads the repository. The harness never fixes a finding; you decide what changes."),
    run,
  );

  const body: HTMLElement[] = [intro];
  switch (check.state) {
    case "not_run":
      body.push(el("p", "empty", "repo-check has not run in this repository yet."));
      break;
    case "running":
      body.push(
        el("p", "empty", "repo-check is running. It reads the repository and the global agent configuration, which takes a few minutes."),
      );
      break;
    case "failed":
      body.push(el("p", "repo-check-problem callout danger", `repo-check did not finish: ${check.reason}`));
      break;
    case "unreadable": {
      body.push(
        el("p", "repo-check-problem callout", `The answer is not a report the harness can read: ${check.reason}`),
      );
      body.push(el("pre", "repo-check-output code-block", check.output));
      break;
    }
    case "done":
      body.push(el("p", "repo-check-summary", check.report.summary));
      body.push(findingSection("Must", "Real risks now.", check.report.must));
      body.push(findingSection("Should", "Clear wins that are cheap to make.", check.report.should));
      body.push(...optOuts(check.opt_outs));
      break;
  }
  repoCheckView.replaceChildren(...body);
}

function findingSection(rank: "Must" | "Should", meaning: string, findings: Finding[]): HTMLElement {
  const section = el("section", `findings ${rank.toLowerCase()}`);
  const heading = el("h2", "", rank);
  heading.append(
    el("span", `badge ${rank === "Must" ? "danger" : "warning"}`, String(findings.length)),
    el("span", "findings-meaning", meaning),
  );
  section.append(heading);
  if (findings.length === 0) {
    section.append(el("p", "empty", `No ${rank} findings.`));
    return section;
  }
  const list = el("ol", "finding-list");
  for (const finding of findings) {
    const item = el("li", "finding");
    const title = el("div", "finding-title", finding.title);
    for (const check of finding.checks) title.append(el("span", "check-id", check));
    item.append(title, el("div", "finding-details", finding.details));
    list.append(item);
  }
  section.append(list);
  return section;
}

function optOuts(optOuts: OptOuts): HTMLElement[] {
  if (optOuts.type === "unreadable") {
    return [el("p", "repo-check-problem callout", `Nothing is switched off: ${optOuts.reason}`)];
  }
  if (optOuts.checks.length === 0) return [];
  const section = el("section", "findings switched-off");
  section.append(el("h2", "", "Switched off in .leiser/config.toml"));
  const list = el("ul", "finding-list");
  for (const off of optOuts.checks) {
    const item = el("li", "finding");
    item.append(el("span", "check-id", off.check), el("span", "", ` ${off.reason}`));
    list.append(item);
  }
  section.append(list);
  return [section];
}

/** The usage limit the last turn stopped at, with one button per subscription to continue on. */
function limitCard(
  view: ChatView,
  limit: UsageLimit,
  subscriptions: SubscriptionOverview[],
  context: ChatContext,
): HTMLElement {
  const resets =
    limit.resets_in_seconds === null
      ? "The agent CLI did not say when it resets."
      : `It resets in ${formatDuration(limit.resets_in_seconds)}.`;
  const [item, body] = card(
    "limit open",
    "alert",
    `Usage limit reached on ${context.subscriptionName(limit.subscription)}`,
  );
  body.append(el("div", "entry-text", `${limit.limit}. ${resets} Wait and send a message, or continue on:`));
  const provider = AGENT_CLI_PROVIDERS[view.agentCli];
  const choices: (SubscriptionId | null)[] = subscriptions
    .filter((overview) => overview.provider === provider)
    .map((overview) => overview.subscription);
  choices.push(null);
  const actions = el("div", "card-actions");
  for (const choice of choices.filter((choice) => choice !== limit.subscription)) {
    const label = choice === null ? "Default login" : context.subscriptionName(choice);
    const button = el("button", "", label);
    button.type = "button";
    button.title = `Continue this chat on ${label}`;
    button.addEventListener("click", () =>
      act(async () => {
        try {
          await service.continueOnSubscription(context.chat, choice);
        } finally {
          await refresh();
        }
      }),
    );
    actions.append(button);
  }
  body.append(actions);
  return item;
}

function labelled(label: string, value: string | HTMLElement): HTMLElement {
  const item = el("span", "top-bar-item");
  const shown = typeof value === "string" ? el("span", "top-bar-value", value) : value;
  item.append(el("span", "top-bar-label", label), shown);
  return item;
}

const DEFAULT_LOGIN = "default";

/** The chat's subscription, changeable between turns. */
function subscriptionPicker(view: ChatView, subscriptions: SubscriptionOverview[]): HTMLElement {
  const provider = AGENT_CLI_PROVIDERS[view.agentCli];
  const select = el("select", "subscription-picker");
  select.title = "The subscription the chat's next turns use";
  select.setAttribute("aria-label", "Subscription");
  select.append(new Option(`${AGENT_CLI_NAMES[view.agentCli]} default login`, DEFAULT_LOGIN));
  for (const subscription of subscriptions.filter((overview) => overview.provider === provider)) {
    select.append(new Option(subscription.name, String(subscription.subscription)));
  }
  if (
    view.subscription !== null &&
    !subscriptions.some((overview) => overview.subscription === view.subscription)
  ) {
    select.append(new Option("Removed subscription", String(view.subscription)));
  }
  select.value = view.subscription === null ? DEFAULT_LOGIN : String(view.subscription);
  select.disabled = view.status === "working";
  select.addEventListener("change", () =>
    act(async () => {
      if (selectedChat === null) return;
      const chosen = select.value === DEFAULT_LOGIN ? null : Number(select.value);
      try {
        await service.changeSubscription(selectedChat, provider, chosen);
      } finally {
        await refresh();
      }
    }),
  );
  const item = el("label", "composer-option");
  item.append(icon("key"), select);
  return item;
}

function renderSubscriptions(subscriptions: SubscriptionOverview[]): void {
  const sections: HTMLElement[] = [];
  for (const provider of Object.keys(PROVIDER_NAMES) as Provider[]) {
    const section = el("section", "subscription-provider");
    section.append(el("h3", "", PROVIDER_NAMES[provider]));
    const list = el("ul", "subscription-rows");
    const own = subscriptions.filter((overview) => overview.provider === provider);
    if (own.length === 0) {
      list.append(el("li", "muted", "None yet: chats use the agent CLI's default login."));
    }
    for (const subscription of own) list.append(subscriptionRow(subscription));
    section.append(list);
    sections.push(section);
  }
  subscriptionList.replaceChildren(...sections);
}

function subscriptionRow(subscription: SubscriptionOverview): HTMLElement {
  const row = el("li", "subscription-row");
  const name = el("input", "subscription-name");
  name.value = subscription.name;
  name.setAttribute("aria-label", "Name");
  const kind = subscription.kind === "login" ? "Login" : "API key";
  const status = el(
    "span",
    `badge subscription-kind${subscription.logging_in ? " accent" : ""}`,
    subscription.logging_in ? `${kind} · logging in…` : kind,
  );
  status.title = subscription.home;
  const rename = el("button", "ghost", "Rename");
  rename.type = "button";
  rename.addEventListener("click", () =>
    actInDialog(async () => {
      await service.renameSubscription(subscription.subscription, name.value);
      await refresh();
    }),
  );
  const interchangeable = el("label", "subscription-interchangeable");
  const box = el("input");
  box.type = "checkbox";
  box.checked = subscription.interchangeable;
  box.addEventListener("change", () =>
    actInDialog(async () => {
      try {
        await service.setInterchangeable(subscription.subscription, box.checked);
      } finally {
        await refresh();
      }
    }),
  );
  interchangeable.title =
    "At a usage limit, chats move by themselves between this provider's interchangeable subscriptions";
  interchangeable.append(box, "Interchangeable");
  const actions = el("span", "subscription-actions");
  actions.append(rename);
  row.append(name, status, interchangeable, actions);
  if (subscription.kind === "login") {
    const login = el("button", "ghost", "Log in");
    login.type = "button";
    login.title = "Run the agent CLI's login again";
    login.addEventListener("click", () =>
      actInDialog(async () => {
        await service.logIn(subscription.subscription);
        await refresh();
      }),
    );
    actions.append(login);
  }
  const remove = el("button", "ghost danger", "Remove");
  remove.type = "button";
  remove.addEventListener("click", () =>
    actInDialog(async () => {
      // A second click confirms: removing deletes the login or the key.
      if (remove.dataset.confirm !== "yes") {
        remove.dataset.confirm = "yes";
        remove.textContent = "Remove for good";
        return;
      }
      await service.removeSubscription(subscription.subscription);
      await refresh();
    }),
  );
  actions.append(remove);
  return row;
}

/** Like `act`, for actions in the subscriptions dialog. */
async function actInDialog(action: () => Promise<void>): Promise<void> {
  subscriptionsError.hidden = true;
  try {
    await action();
  } catch (error) {
    subscriptionsError.textContent = error instanceof Error ? error.message : String(error);
    subscriptionsError.hidden = false;
  }
}

function showLogin(event: ServiceEvent): void {
  if (event.type === "login_output") {
    loginLines.push(event.text);
    answeringLogin = event.subscription;
    loginAnswer.hidden = false;
  } else if (event.type === "login_finished") {
    loginLines.push(
      event.outcome.type === "signed_in" ? "Signed in." : `Login failed: ${event.outcome.reason}`,
    );
    if (answeringLogin === event.subscription) {
      answeringLogin = null;
      loginAnswer.hidden = true;
    }
  } else {
    return;
  }
  loginOutput.textContent = loginLines.join("\n");
  loginSection.hidden = false;
}

function branchName(branch: Branch): string {
  switch (branch.type) {
    case "named":
      return branch.name;
    case "detached":
      return `detached at ${branch.commit}`;
    case "unknown":
      return "unknown";
  }
}

function renderEntry(entry: TranscriptEntry, context: ChatContext): HTMLElement | null {
  switch (entry.type) {
    case "user_message": {
      const item = el("li", "entry user");
      item.append(el("div", "entry-label visually-hidden", "You"), el("div", "entry-text", entry.text));
      return item;
    }
    // Shown on the card of the request it answers.
    case "answer":
      return null;
    case "agent":
      return renderAgentEvent(entry.event, AGENT_CLI_NAMES[entry.agent_cli], context);
    case "continued_on_subscription": {
      const how = entry.automatic ? " by itself: both subscriptions are interchangeable" : "";
      return notice("", "key", `Continued on ${context.subscriptionName(entry.subscription)}${how}.`);
    }
    case "script_finished": {
      const failed = entry.exit_code !== 0;
      const exit = entry.exit_code === null ? "did not finish" : `exit ${entry.exit_code}`;
      const item = el("li", "entry command script");
      if (failed) item.classList.add("failed");
      item.append(
        output(
          [icon("terminal"), el("span", "output-name", SCRIPT_NAMES[entry.script]), exitBadge(exit, failed)],
          entry.output,
        ),
      );
      return item;
    }
  }
}

function answer(context: ChatContext, request: string, given: Answer): Promise<void> {
  return act(async () => {
    await service.answerRequest(context.chat, request, given);
    await refresh();
  });
}

/** How a request ended, for a card that no longer waits. */
function outcome(given: Answer | undefined): HTMLElement {
  if (given === undefined) return el("span", "badge card-outcome", "No longer waiting: the turn ended.");
  switch (given.type) {
    case "approve":
      return el("span", "badge success card-outcome", "Approved");
    case "decline":
      return el(
        "span",
        "badge danger card-outcome",
        given.feedback === null ? "Declined" : `Declined: ${given.feedback}`,
      );
    case "answers":
      return el(
        "span",
        "badge card-outcome",
        `Answered: ${Object.values(given.answers).map((chosen) => chosen.join(", ")).join("; ")}`,
      );
  }
}

/** A transcript card with a header line; returns the card and its body. */
function card(className: string, iconName: IconName, label: string): [HTMLElement, HTMLElement] {
  const item = el("li", `entry card ${className}`);
  const header = el("div", "card-header");
  header.append(icon(iconName), el("div", "entry-label", label));
  const body = el("div", "card-body");
  item.append(header, body);
  return [item, body];
}

/** A line in the transcript about something around the agent. */
function notice(className: string, iconName: IconName, text: string, label?: string): HTMLElement {
  const item = el("li", `entry notice ${className}`.trim());
  const body = el("div", "notice-body");
  if (label !== undefined) body.append(el("div", "entry-label", label));
  body.append(el("div", "entry-text", text));
  item.append(icon(iconName), body);
  return item;
}

function approvalCard(
  event: { request: string; description: string },
  agentCli: string,
  context: ChatContext,
): HTMLElement {
  const [item, body] = card("approval", "shield", `${agentCli} asks for approval`);
  body.append(el("div", "entry-text", event.description));
  if (!context.open.has(event.request)) {
    body.append(outcome(context.answers.get(event.request)));
    return item;
  }
  item.classList.add("open");
  const approve = el("button", "primary", "Approve");
  approve.type = "button";
  approve.addEventListener("click", () => void answer(context, event.request, { type: "approve" }));
  const decline = el("button", "", "Decline");
  decline.type = "button";
  decline.addEventListener(
    "click",
    () => void answer(context, event.request, { type: "decline", feedback: null }),
  );
  const actions = el("div", "card-actions");
  actions.append(approve, decline, el("span", "card-hint", "or reply below to decline with feedback"));
  body.append(actions);
  return item;
}

function questionCard(
  event: { request: string; questions: Question[] },
  agentCli: string,
  context: ChatContext,
): HTMLElement {
  const [item, body] = card("question", "alert", `${agentCli} asks`);
  const open = context.open.has(event.request);
  const form = el("form", "card-questions");
  event.questions.forEach((question, index) => {
    const fieldset = el("fieldset");
    fieldset.append(el("legend", "", question.header === "" ? question.question : question.header));
    if (question.header !== "") fieldset.append(el("div", "entry-text", question.question));
    for (const option of question.options) {
      const label = el("label", "card-option");
      const input = el("input");
      input.type = question.multi_select ? "checkbox" : "radio";
      input.name = `question-${index}`;
      input.value = option.label;
      input.disabled = !open;
      const text = el("span");
      text.append(el("span", "option-label", option.label));
      if (option.description !== "") text.append(el("span", "option-description", ` — ${option.description}`));
      label.append(input, text);
      fieldset.append(label);
    }
    form.append(fieldset);
  });
  body.append(form);
  if (!open) {
    body.append(outcome(context.answers.get(event.request)));
    return item;
  }
  item.classList.add("open");
  const send = el("button", "primary", "Send answers");
  send.type = "submit";
  const decline = el("button", "", "Decline");
  decline.type = "button";
  decline.addEventListener(
    "click",
    () => void answer(context, event.request, { type: "decline", feedback: null }),
  );
  form.addEventListener("submit", (submitted) => {
    submitted.preventDefault();
    const answers: Record<string, string[]> = {};
    event.questions.forEach((question, index) => {
      const checked = form.querySelectorAll<HTMLInputElement>(`input[name="question-${index}"]:checked`);
      answers[question.id] = Array.from(checked, (input) => input.value);
    });
    void answer(context, event.request, { type: "answers", answers });
  });
  const actions = el("div", "card-actions");
  actions.append(send, decline, el("span", "card-hint", "or reply below in your own words"));
  form.append(actions);
  return item;
}

/** Collapsible output; open while it is short. */
function output(summary: (Node | string)[], text: string): HTMLElement {
  const details = el("details", "output");
  details.open = text.split("\n").length <= 8;
  const head = el("summary");
  head.append(...summary);
  details.append(head, el("pre", "", text));
  return details;
}

function exitBadge(text: string, failed: boolean): HTMLElement {
  return el("span", `badge ${failed ? "danger" : "success"}`, text);
}

function renderAgentEvent(event: AgentEvent, agentCli: string, context: ChatContext): HTMLElement {
  switch (event.type) {
    case "message": {
      const item = el("li", "entry agent");
      // The agent CLI's name shows when it changes, such as after a handoff.
      const label = el("div", "entry-label", agentCli);
      if (context.lastAgentCli === agentCli) label.classList.add("visually-hidden");
      context.lastAgentCli = agentCli;
      item.append(label, el("div", "entry-text", event.text));
      return item;
    }
    case "tool_call": {
      const item = el("li", "entry tool-call");
      item.append(
        icon("chevron"),
        el("span", "visually-hidden", "Tool call"),
        el("span", "tool-name", event.name),
        el("code", "tool-detail", event.detail),
      );
      return item;
    }
    case "tool_result": {
      const item = el("li", "entry tool-result");
      item.append(output(["Result", el("span", "output-name", event.name)], event.output));
      return item;
    }
    case "command_finished": {
      const failed = event.exit_code !== 0;
      const exit = event.exit_code === null ? "stopped" : `exit ${event.exit_code}`;
      const item = el("li", "entry command");
      if (failed) item.classList.add("failed");
      const command = el("code", "command-text", event.command);
      command.title = event.command;
      item.append(
        output(
          [el("span", "command-prompt", "$"), el("span", "visually-hidden", "Command"), command, exitBadge(exit, failed)],
          event.output,
        ),
      );
      return item;
    }
    case "approval_requested":
      return approvalCard(event, agentCli, context);
    case "question_asked":
      return questionCard(event, agentCli, context);
    case "usage_limit_reached": {
      const resets =
        event.resets_in_seconds === null ? "" : ` · resets in ${formatDuration(event.resets_in_seconds)}`;
      return notice("limit", "alert", `Usage limit reached: ${event.limit}${resets}`);
    }
    case "turn_ended":
      return el("li", "turn-ended", "Turn ended");
    case "turn_interrupted":
      return el("li", "turn-ended", "Turn interrupted");
    case "turn_failed":
      return notice("failed", "alert", event.reason, "Turn failed");
    case "session_ended":
      return notice("", "stop", "The agent session ended.");
  }
}

function formatDuration(seconds: number): string {
  const hours = Math.floor(seconds / 3600);
  const minutes = Math.ceil((seconds % 3600) / 60);
  return hours > 0 ? `${hours} h ${minutes} min` : `${minutes} min`;
}

async function send(): Promise<void> {
  const text = messageInput.value.trim();
  if (selectedChat === null || text === "") return;
  const chat = selectedChat;
  messageInput.disabled = true;
  sendButton.disabled = true;
  try {
    await act(async () => {
      const waitingFor = selectedOpenRequests[0];
      // A reply to a waiting chat declines its oldest request with the
      // text as feedback. Otherwise the message is sent at once when no
      // turn runs, or waits for the turn, or the worktree's setup script,
      // to end.
      if (waitingFor !== undefined) {
        await service.answerRequest(chat, waitingFor, { type: "decline", feedback: text });
      } else {
        await service.queueMessage(chat, text);
      }
      messageInput.value = "";
      fitMessageInput();
      await refresh();
    });
  } finally {
    messageInput.disabled = false;
    sendButton.disabled = false;
    messageInput.focus();
  }
}

todoForm.addEventListener("submit", (event) => {
  event.preventDefault();
  const text = todoInput.value.trim();
  if (selectedChat === null || text === "") return;
  const chat = selectedChat;
  void act(async () => {
    await service.addTodo(chat, text);
    todoInput.value = "";
    await refresh();
  });
});

interruptButton.addEventListener("click", () =>
  act(async () => {
    if (selectedChat === null) return;
    await service.interruptTurn(selectedChat);
    await refresh();
  }),
);

byId("add-repository").prepend(icon("folderPlus"));
byId("open-subscriptions").prepend(icon("key"));

byId("add-repository").addEventListener("click", () =>
  act(async () => {
    const path = await service.pickFolder();
    if (path === null) return;
    await service.addRepository(path);
    await refresh();
  }),
);

/** Grows the message field with its text, up to the height CSS allows. */
function fitMessageInput(): void {
  messageInput.style.height = "auto";
  messageInput.style.height = `${messageInput.scrollHeight}px`;
}

messageInput.addEventListener("input", fitMessageInput);

composer.addEventListener("submit", (event) => {
  event.preventDefault();
  void send();
});

messageInput.addEventListener("keydown", (event) => {
  if (event.key === "Enter" && !event.shiftKey && !event.isComposing) {
    event.preventDefault();
    void send();
  }
});

byId("open-subscriptions").addEventListener("click", () =>
  act(async () => {
    await refresh();
    subscriptionsDialog.showModal();
  }),
);

byId("close-subscriptions").addEventListener("click", () => subscriptionsDialog.close());

loginAnswer.addEventListener("submit", (event) => {
  event.preventDefault();
  void actInDialog(async () => {
    if (answeringLogin === null || loginAnswerText.value.trim() === "") return;
    await service.answerLogin(answeringLogin, loginAnswerText.value);
    loginAnswerText.value = "";
  });
});

newKind.addEventListener("change", () => {
  newKeyLabel.hidden = newKind.value !== "api_key";
});

addSubscriptionForm.addEventListener("submit", (event) => {
  event.preventDefault();
  void actInDialog(async () => {
    const provider = newProvider.value as Provider;
    const signIn =
      newKind.value === "api_key"
        ? { type: "api_key" as const, api_key: newKey.value }
        : { type: "login" as const };
    await service.addSubscription(provider, newName.value, signIn);
    newName.value = "";
    newKey.value = "";
    await refresh();
  });
});

// The top bar's terminal button follows the panel.
terminal.init(() => void refresh().catch(showError));

void act(async () => {
  await service.subscribe(
    (event) => {
      showLogin(event);
      void refresh().catch(showError);
    },
    () => void refresh().catch(showError),
    (changed) => {
      connection = changed;
      if (changed.state === "connected") clearError();
      void refresh().catch(showError);
    },
  );
  // The connection may have changed before this window listened.
  connection = await service.serviceConnection();
  await refresh();
});
