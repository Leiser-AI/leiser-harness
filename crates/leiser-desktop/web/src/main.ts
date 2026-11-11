// The desktop app's UI. It holds no chat state of its own: it reads
// repositories and the selected chat from the harness service and reads
// them again on every service event. The only thing it keeps is which chat,
// or which repository's CLI extensions panel, is selected.

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
  RepositoryId,
  RepositoryOverview,
  RepositoryScript,
  ServiceConnection,
  Todo,
  TranscriptEntry,
} from "./protocol";
import * as extensions from "./extensions";
import * as service from "./service";
import { AGENT_CLI_NAMES, type ChatView, PERMISSION_MODE_NAMES } from "./service";

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
// The approval requests and questions the selected chat waits for.
let selectedOpenRequests: string[] = [];

/** What a transcript entry needs to know about the rest of the chat. */
interface ChatContext {
  chat: ChatId;
  open: Set<string>;
  answers: Map<string, Answer>;
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
  const [repositories, chats, view, todos, updateWaiting] = await Promise.all([
    service.listRepositories(),
    service.listChats(),
    chat === null ? Promise.resolve(null) : service.readChat(chat),
    chat === null ? Promise.resolve(null) : service.listTodos(chat),
    service.serviceUpdateWaiting(),
  ]);
  if (read !== readCount) return;
  renderServiceNotice(updateWaiting);
  renderSidebar(repositories, chats);
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
): void {
  if (view === null || overview === null) {
    selectedWorking = false;
    selectedOpenRequests = [];
    topBar.replaceChildren(el("span", "muted", "No chat selected"));
    transcript.replaceChildren(el("li", "empty", "Select a chat or start a new one."));
    composer.hidden = true;
    return;
  }
  const chat = overview.chat;
  topBar.replaceChildren(
    labelled("Repository", repository?.name ?? "unknown"),
    labelled("Branch", branchName(view.branch)),
    labelled("Checkout", overview.worktree === null ? "main checkout" : "worktree"),
    permissionModePicker(overview.chat, view),
    ...chatActions(overview),
  );
  const context: ChatContext = { chat, open: new Set(view.openRequests), answers: new Map() };
  for (const entry of view.transcript) {
    if (entry.type === "answer") context.answers.set(entry.request, entry.answer);
  }
  const entries = view.transcript.flatMap((entry) => renderEntry(entry, context) ?? []);
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
  const item = el("label", "top-bar-item");
  const select = el("select", "permission-mode");
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
  item.append(el("span", "top-bar-label", "Permissions"), select);
  if (view.appliedPermissionMode !== view.permissionMode) {
    const applied = PERMISSION_MODE_NAMES[view.appliedPermissionMode];
    item.title = `The agent CLI has no such mode, so the chat runs ${applied}.`;
    item.append(el("span", "top-bar-value muted", `runs ${applied}`));
  }
  return item;
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

function labelled(label: string, value: string): HTMLElement {
  const item = el("span", "top-bar-item");
  item.append(el("span", "top-bar-label", label), el("span", "top-bar-value", value));
  return item;
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
      item.append(el("div", "entry-label", "You"), el("div", "entry-text", entry.text));
      return item;
    }
    // Shown on the card of the request it answers.
    case "answer":
      return null;
    case "agent":
      return renderAgentEvent(entry.event, AGENT_CLI_NAMES[entry.agent_cli], context);
    case "script_finished": {
      const exit = entry.exit_code === null ? "did not finish" : `exit ${entry.exit_code}`;
      const item = el("li", "entry tool-result script");
      if (entry.exit_code !== 0) item.classList.add("failed");
      item.append(output(`${SCRIPT_NAMES[entry.script]} · ${exit}`, entry.output));
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
function outcome(given: Answer | undefined): string {
  if (given === undefined) return "No longer waiting: the turn ended.";
  switch (given.type) {
    case "approve":
      return "Approved";
    case "decline":
      return given.feedback === null ? "Declined" : `Declined: ${given.feedback}`;
    case "answers":
      return `Answered: ${Object.values(given.answers).map((chosen) => chosen.join(", ")).join("; ")}`;
  }
}

function approvalCard(
  event: { request: string; description: string },
  agentCli: string,
  context: ChatContext,
): HTMLElement {
  const item = el("li", "entry card approval");
  item.append(
    el("div", "entry-label", `${agentCli} asks for approval`),
    el("div", "entry-text", event.description),
  );
  if (!context.open.has(event.request)) {
    item.append(el("div", "card-outcome", outcome(context.answers.get(event.request))));
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
  actions.append(approve, decline, el("span", "muted", "or reply below to decline with feedback"));
  item.append(actions);
  return item;
}

function questionCard(
  event: { request: string; questions: Question[] },
  agentCli: string,
  context: ChatContext,
): HTMLElement {
  const item = el("li", "entry card question");
  item.append(el("div", "entry-label", `${agentCli} asks`));
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
      label.append(input, el("span", "", option.label));
      if (option.description !== "") label.append(el("span", "muted", ` — ${option.description}`));
      fieldset.append(label);
    }
    form.append(fieldset);
  });
  item.append(form);
  if (!open) {
    item.append(el("div", "card-outcome", outcome(context.answers.get(event.request))));
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
  actions.append(send, decline, el("span", "muted", "or reply below in your own words"));
  form.append(actions);
  return item;
}

function output(label: string, text: string): HTMLElement {
  const details = el("details");
  details.open = text.split("\n").length <= 8;
  details.append(el("summary", "entry-label", label), el("pre", "", text));
  return details;
}

function renderAgentEvent(event: AgentEvent, agentCli: string, context: ChatContext): HTMLElement {
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
    case "approval_requested":
      return approvalCard(event, agentCli, context);
    case "question_asked":
      return questionCard(event, agentCli, context);
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

void act(async () => {
  await service.subscribe(
    () => void refresh().catch(showError),
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
