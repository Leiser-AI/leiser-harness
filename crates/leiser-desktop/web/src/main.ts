// The desktop app's UI. It holds no chat state of its own: it reads
// repositories and the selected chat from the harness service and reads
// them again on every service event. The only thing it keeps is which chat,
// or which repository's CLI extensions panel, is selected.

import type {
  AgentEvent,
  Branch,
  ChatId,
  ChatOverview,
  ChatStatus,
  Checkout,
  Provider,
  RepositoryId,
  RepositoryOverview,
  RepositoryScript,
  ServiceEvent,
  SubscriptionOverview,
  ServiceConnection,
  Todo,
  TranscriptEntry,
} from "./protocol";
import * as extensions from "./extensions";
import * as service from "./service";
import { AGENT_CLI_NAMES, AGENT_CLI_PROVIDERS, PROVIDER_NAMES, type ChatView } from "./service";

let selectedChat: ChatId | null = null;
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
  if (read !== readCount) return;
  renderServiceNotice(updateWaiting);
  renderSidebar(repositories, chats);
  // The subscriptions dialog can open over any view.
  renderSubscriptions(subscriptions);
  if (selectedExtensions !== null) {
    const shown = repositories.find((overview) => overview.repository === selectedExtensions);
    renderExtensionsTopBar(shown ?? null);
    return;
  }
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
    const remove = el("button", "todo-remove", "×");
    remove.type = "button";
    remove.title = "Remove this todo";
    remove.setAttribute("aria-label", `Remove "${todo.text}"`);
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
  topBar.replaceChildren(
    labelled("Repository", repository?.name ?? "unknown"),
    labelled("Panel", "CLI extensions"),
  );
}

function renderSidebar(repositories: RepositoryOverview[], chats: ChatOverview[]): void {
  const list = el("ul", "repository-list");
  if (repositories.length === 0) {
    list.append(el("li", "empty", "Add a repository to start a chat."));
  }
  for (const repository of repositories) {
    const item = el("li", "repository");
    const header = el("div", "repository-header");
    const name = el("span", "repository-name", repository.name);
    name.title = repository.path;
    const newChat = el("button", "new-chat", "New chat");
    newChat.type = "button";
    newChat.title = `Start a chat in a new worktree of ${repository.name}`;
    newChat.addEventListener("click", () => act(() => startAndSelect(repository.repository)));
    const inMain = el("button", "new-chat secondary", "Main");
    inMain.type = "button";
    inMain.title = `Start a chat in the main checkout of ${repository.name}`;
    inMain.addEventListener("click", () =>
      act(() => startAndSelect(repository.repository, { type: "main" })),
    );
    const showExtensions = el("button", "new-chat", "Extensions");
    showExtensions.type = "button";
    showExtensions.title = `Skills, MCP servers, hooks and instruction files the agent CLIs load from ${repository.name}`;
    if (repository.repository === selectedExtensions) {
      showExtensions.classList.add("selected");
      showExtensions.setAttribute("aria-current", "true");
    }
    showExtensions.addEventListener("click", () =>
      act(async () => {
        if (selectedExtensions !== repository.repository && !leaveExtensions()) return;
        selectedChat = null;
        selectedExtensions = repository.repository;
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
    selectedWorking = false;
    topBar.replaceChildren(el("span", "muted", "No chat selected"));
    transcript.replaceChildren(el("li", "empty", "Select a chat or start a new one."));
    composer.hidden = true;
    return;
  }
  topBar.replaceChildren(
    labelled("Repository", repository?.name ?? "unknown"),
    labelled("Branch", branchName(view.branch)),
    labelled("Checkout", overview.worktree === null ? "main checkout" : "worktree"),
    labelled("Agent CLI", AGENT_CLI_NAMES[view.agentCli]),
    subscriptionPicker(view, subscriptions),
    ...chatActions(overview),
  );
  const entries = view.transcript.map(renderEntry);
  if (entries.length === 0) {
    entries.push(el("li", "empty", "Send a message to start the agent."));
  }
  const atBottom = transcript.scrollHeight - transcript.scrollTop - transcript.clientHeight < 40;
  transcript.replaceChildren(...entries);
  if (atBottom) transcript.scrollTop = transcript.scrollHeight;
  selectedWorking = view.status === "working";
  sendButton.textContent = selectedWorking ? "Queue" : "Send";
  interruptButton.hidden = !selectedWorking;
  composer.hidden = false;
}

/** The top bar's buttons for the selected chat. */
function chatActions(overview: ChatOverview): HTMLElement[] {
  const actions: HTMLElement[] = [];
  if (overview.worktree !== null) {
    const sameWorktree = el("button", "top-bar-action", "New chat in this worktree");
    sameWorktree.type = "button";
    sameWorktree.title = overview.worktree;
    sameWorktree.addEventListener("click", () =>
      act(() => startAndSelect(overview.repository, { type: "worktree", chat: overview.chat })),
    );
    actions.push(sameWorktree);
  }
  if (overview.status === "waiting_for_you") {
    const done = el("button", "top-bar-action", "Mark done");
    done.type = "button";
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
  const select = el("select", "top-bar-value subscription-picker");
  select.title = "The subscription the chat's next turns use";
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
  return labelled("Subscription", select);
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
  const status = el("span", "muted", subscription.logging_in ? `${kind} · logging in…` : kind);
  status.title = subscription.home;
  const rename = el("button", "", "Rename");
  rename.type = "button";
  rename.addEventListener("click", () =>
    actInDialog(async () => {
      await service.renameSubscription(subscription.subscription, name.value);
      await refresh();
    }),
  );
  row.append(name, status, rename);
  if (subscription.kind === "login") {
    const login = el("button", "", "Log in");
    login.type = "button";
    login.title = "Run the agent CLI's login again";
    login.addEventListener("click", () =>
      actInDialog(async () => {
        await service.logIn(subscription.subscription);
        await refresh();
      }),
    );
    row.append(login);
  }
  const remove = el("button", "danger", "Remove");
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
  row.append(remove);
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

function renderEntry(entry: TranscriptEntry): HTMLElement {
  switch (entry.type) {
    case "user_message": {
      const item = el("li", "entry user");
      item.append(el("div", "entry-label", "You"), el("div", "entry-text", entry.text));
      return item;
    }
    case "agent":
      return renderAgentEvent(entry.event, AGENT_CLI_NAMES[entry.agent_cli]);
    case "script_finished": {
      const exit = entry.exit_code === null ? "did not finish" : `exit ${entry.exit_code}`;
      const item = el("li", "entry tool-result script");
      if (entry.exit_code !== 0) item.classList.add("failed");
      item.append(output(`${SCRIPT_NAMES[entry.script]} · ${exit}`, entry.output));
      return item;
    }
  }
}

function output(label: string, text: string): HTMLElement {
  const details = el("details");
  details.open = text.split("\n").length <= 8;
  details.append(el("summary", "entry-label", label), el("pre", "", text));
  return details;
}

function renderAgentEvent(event: AgentEvent, agentCli: string): HTMLElement {
  switch (event.type) {
    case "message": {
      const item = el("li", "entry agent");
      item.append(el("div", "entry-label", agentCli), el("div", "entry-text", event.text));
      return item;
    }
    case "tool_call": {
      const item = el("li", "entry tool-call");
      item.append(el("div", "entry-label", `Tool call · ${event.name}`), el("code", "", event.detail));
      return item;
    }
    case "tool_result": {
      const item = el("li", "entry tool-result");
      item.append(output(`Result · ${event.name}`, event.output));
      return item;
    }
    case "command_finished": {
      const exit = event.exit_code === null ? "stopped" : `exit ${event.exit_code}`;
      const item = el("li", "entry tool-result");
      if (event.exit_code !== 0) item.classList.add("failed");
      item.append(el("code", "", event.command), output(`Command · ${exit}`, event.output));
      return item;
    }
    case "approval_requested": {
      const item = el("li", "entry notice approval");
      item.append(el("div", "entry-label", "Approval requested"), el("div", "entry-text", event.description));
      return item;
    }
    case "usage_limit_reached": {
      const resets =
        event.resets_in_seconds === null ? "" : ` · resets in ${formatDuration(event.resets_in_seconds)}`;
      return el("li", "entry notice limit", `Usage limit reached: ${event.limit}${resets}`);
    }
    case "turn_ended":
      return el("li", "turn-ended", "Turn ended");
    case "turn_interrupted":
      return el("li", "turn-ended", "Turn interrupted");
    case "turn_failed": {
      const item = el("li", "entry notice failed");
      item.append(el("div", "entry-label", "Turn failed"), el("div", "entry-text", event.reason));
      return item;
    }
    case "session_ended":
      return el("li", "entry notice", "The agent session ended.");
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
      // Sent at once when no turn runs; otherwise it waits for the turn,
      // or for the worktree's setup script, to end.
      await service.queueMessage(chat, text);
      messageInput.value = "";
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

byId("add-repository").addEventListener("click", () =>
  act(async () => {
    const path = await service.pickFolder();
    if (path === null) return;
    await service.addRepository(path);
    await refresh();
  }),
);

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
