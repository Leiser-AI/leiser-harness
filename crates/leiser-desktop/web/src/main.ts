// The desktop app's UI. It holds no chat state of its own: it reads
// repositories and the selected chat from the harness service and reads
// them again on every service event. The only thing it keeps is which chat
// is selected.

import type {
  AgentEvent,
  Branch,
  ChatId,
  ChatOverview,
  ChatStatus,
  Checkout,
  RepositoryId,
  RepositoryOverview,
  RepositoryScript,
  TranscriptEntry,
} from "./protocol";
import * as service from "./service";
import { AGENT_CLI_NAMES, type ChatView } from "./service";

let selectedChat: ChatId | null = null;
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

async function refresh(): Promise<void> {
  const read = ++readCount;
  const chat = selectedChat;
  const [repositories, chats, view] = await Promise.all([
    service.listRepositories(),
    service.listChats(),
    chat === null ? Promise.resolve(null) : service.readChat(chat),
  ]);
  if (read !== readCount) return;
  renderSidebar(repositories, chats);
  const overview = chats.find((candidate) => candidate.chat === chat) ?? null;
  renderChat(
    view,
    overview,
    repositories.find((candidate) => candidate.repository === overview?.repository) ?? null,
  );
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
  selectedChat = await service.startChat(repository, checkout);
  await refresh();
  messageInput.focus();
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
    const actions = el("span", "repository-actions");
    actions.append(newChat, inMain);
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
    topBar.replaceChildren(el("span", "muted", "No chat selected"));
    transcript.replaceChildren(el("li", "empty", "Select a chat or start a new one."));
    composer.hidden = true;
    return;
  }
  topBar.replaceChildren(
    labelled("Repository", repository?.name ?? "unknown"),
    labelled("Branch", branchName(view.branch)),
    labelled("Checkout", overview.worktree === null ? "main checkout" : "worktree"),
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
  );
  await refresh();
});
