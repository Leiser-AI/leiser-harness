// The desktop app's UI. It holds no chat state of its own: it reads
// repositories and the selected chat from the harness service and reads
// them again on every service event. The only thing it keeps is which chat
// is selected.

import type { AgentEvent, Branch, ChatId, ChatView, RepositorySummary, TranscriptEntry } from "./protocol";
import * as service from "./service";

let selectedChat: ChatId | null = null;
// Drops answers to older reads that arrive after newer ones.
let readCount = 0;

const repositoriesNav = byId("repositories");
const topBar = byId("top-bar");
const transcript = byId("transcript");
const composer = byId<HTMLFormElement>("composer");
const messageInput = byId<HTMLTextAreaElement>("message");
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

async function refresh(): Promise<void> {
  const read = ++readCount;
  const chat = selectedChat;
  const [repositories, view] = await Promise.all([
    service.listRepositories(),
    chat === null ? Promise.resolve(null) : service.getChat(chat),
  ]);
  if (read !== readCount) return;
  renderSidebar(repositories);
  renderChat(view);
}

function renderSidebar(repositories: RepositorySummary[]): void {
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
    newChat.title = `Start a chat in ${repository.name}`;
    newChat.addEventListener("click", () =>
      act(async () => {
        selectedChat = await service.startChat(repository.repository);
        await refresh();
        messageInput.focus();
      }),
    );
    header.append(name, newChat);

    const chats = el("ul", "chat-list");
    for (const chat of repository.chats) {
      const chatItem = el("li");
      const button = el("button", "chat-link", chat.title ?? "New chat");
      button.type = "button";
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
      chats.append(chatItem);
    }
    item.append(header, chats);
    list.append(item);
  }
  repositoriesNav.replaceChildren(list);
}

function renderChat(view: ChatView | null): void {
  if (view === null) {
    topBar.replaceChildren(el("span", "muted", "No chat selected"));
    transcript.replaceChildren(el("li", "empty", "Select a chat or start a new one."));
    composer.hidden = true;
    return;
  }
  topBar.replaceChildren(
    labelled("Repository", view.repository_name),
    labelled("Branch", branchName(view.branch)),
  );
  const entries = view.transcript.map(renderEntry);
  if (entries.length === 0) {
    entries.push(el("li", "empty", "Send a message to start the agent."));
  }
  const atBottom = transcript.scrollHeight - transcript.scrollTop - transcript.clientHeight < 40;
  transcript.replaceChildren(...entries);
  if (atBottom) transcript.scrollTop = transcript.scrollHeight;
  composer.hidden = false;
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
  if (entry.type === "user_message") {
    const item = el("li", "entry user");
    item.append(el("div", "entry-label", "You"), el("div", "entry-text", entry.text));
    return item;
  }
  return renderAgentEvent(entry.event);
}

function renderAgentEvent(event: AgentEvent): HTMLElement {
  switch (event.type) {
    case "message": {
      const item = el("li", "entry agent");
      item.append(el("div", "entry-label", "Agent"), el("div", "entry-text", event.text));
      return item;
    }
    case "tool_call": {
      const item = el("li", "entry tool-call");
      item.append(el("div", "entry-label", `Tool call · ${event.name}`), el("code", "", event.detail));
      return item;
    }
    case "tool_result": {
      const item = el("li", "entry tool-result");
      const details = el("details");
      details.open = event.output.split("\n").length <= 8;
      details.append(el("summary", "entry-label", `Result · ${event.name}`), el("pre", "", event.output));
      item.append(details);
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
  const button = composer.querySelector("button");
  messageInput.disabled = true;
  if (button) button.disabled = true;
  try {
    await act(async () => {
      await service.sendMessage(chat, text);
      messageInput.value = "";
      await refresh();
    });
  } finally {
    messageInput.disabled = false;
    if (button) button.disabled = false;
    messageInput.focus();
  }
}

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
