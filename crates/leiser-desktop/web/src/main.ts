// The desktop app's UI. It holds no chat state of its own: it reads
// repositories, their repo-check results and the selected chat from the
// harness service and reads them again on every service event. The only
// thing it keeps is what is selected: a chat or a repository's repo-check.

import type {
  AgentEvent,
  Branch,
  ChatId,
  ChatOverview,
  Finding,
  OptOuts,
  RepoCheck,
  RepositoryId,
  RepositoryOverview,
  TranscriptEntry,
} from "./protocol";
import * as service from "./service";
import { AGENT_CLI_NAMES, type ChatView } from "./service";

let selectedChat: ChatId | null = null;
// Shown instead of a chat when set.
let selectedCheck: RepositoryId | null = null;
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
const repoCheckView = byId("repo-check");

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
  const checks = new Map(
    await Promise.all(
      repositories.map(
        async (overview) =>
          [overview.repository, await service.readRepoCheck(overview.repository)] as const,
      ),
    ),
  );
  if (read !== readCount) return;
  renderSidebar(repositories, chats, checks);
  const checked = repositories.find((overview) => overview.repository === selectedCheck);
  const check = selectedCheck === null ? undefined : checks.get(selectedCheck);
  if (checked && check) {
    renderRepoCheck(checked, check);
    return;
  }
  repoCheckView.hidden = true;
  transcript.hidden = false;
  const repository = chats.find((overview) => overview.chat === chat)?.repository;
  renderChat(view, repositories.find((overview) => overview.repository === repository) ?? null);
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
    const newChat = el("button", "new-chat", "New chat");
    newChat.type = "button";
    newChat.title = `Start a chat in ${repository.name}`;
    newChat.addEventListener("click", () =>
      act(async () => {
        selectedChat = await service.startChat(repository.repository);
        selectedCheck = null;
        await refresh();
        messageInput.focus();
      }),
    );
    header.append(name, newChat);

    const chatList = el("ul", "chat-list");
    const check = checks.get(repository.repository);
    if (check) {
      const checkItem = el("li");
      const button = el("button", "chat-link repo-check-link");
      button.type = "button";
      button.append(el("span", "chat-title", "repo-check"), el("span", "chat-status", checkStatus(check)));
      if (repository.repository === selectedCheck) {
        button.classList.add("selected");
        button.setAttribute("aria-current", "true");
      }
      button.addEventListener("click", () =>
        act(async () => {
          selectedCheck = repository.repository;
          selectedChat = null;
          await refresh();
        }),
      );
      checkItem.append(button);
      chatList.append(checkItem);
    }
    for (const chat of chats.filter((overview) => overview.repository === repository.repository)) {
      const chatItem = el("li");
      const button = el("button", "chat-link");
      button.type = "button";
      button.append(el("span", "chat-title", chat.title ?? "New chat"));
      if (chat.status === "working") {
        button.append(el("span", "chat-status", "working"));
      }
      if (chat.chat === selectedChat) {
        button.classList.add("selected");
        button.setAttribute("aria-current", "true");
      }
      button.addEventListener("click", () =>
        act(async () => {
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

function renderChat(view: ChatView | null, repository: RepositoryOverview | null): void {
  if (view === null) {
    selectedWorking = false;
    topBar.replaceChildren(el("span", "muted", "No chat selected"));
    transcript.replaceChildren(el("li", "empty", "Select a chat or start a new one."));
    composer.hidden = true;
    return;
  }
  topBar.replaceChildren(
    labelled("Repository", repository?.name ?? "unknown"),
    labelled("Branch", branchName(view.branch)),
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
  const items = [labelled("Repository", repository.name), labelled("View", "repo-check")];
  if (check.state === "done" && check.report.commit !== null) {
    items.push(labelled("Commit", check.report.commit));
  }
  topBar.replaceChildren(...items);

  const run = el("button", "", check.state === "not_run" ? "Run repo-check" : "Run again");
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
      body.push(el("p", "repo-check-problem", `repo-check did not finish: ${check.reason}`));
      break;
    case "unreadable": {
      body.push(el("p", "repo-check-problem", `The answer is not a report the harness can read: ${check.reason}`));
      body.push(el("pre", "repo-check-output", check.output));
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
  heading.append(el("span", "muted findings-meaning", meaning));
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
    return [el("p", "repo-check-problem", `Nothing is switched off: ${optOuts.reason}`)];
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
  return renderAgentEvent(entry.event, AGENT_CLI_NAMES[entry.agent_cli]);
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
      // While a turn runs, a message waits for it to end.
      if (selectedWorking) {
        await service.queueMessage(chat, text);
      } else {
        await service.sendMessage(chat, text);
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
