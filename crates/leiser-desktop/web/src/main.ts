// The desktop app's UI. It holds no chat state of its own: it reads
// repositories, their repo-check results and the selected chat from the
// harness service and reads them again on every service event. What it
// keeps is how the developer looks at them: what is selected (a chat, a
// repository's repo-check, its wayfinder map or its CLI extensions panel),
// whether a chat shows its diff panel instead of its transcript, and which
// chats show their todo list. A chat is always open: while none is
// selected, the new chat the developer is about to start (`newChat.ts`).

import type {
  AgentCli,
  AgentEvent,
  Answer,
  AutoFix,
  AutoFixStop,
  AutoFixTrigger,
  Branch,
  ChatId,
  ChatOverview,
  ChatPlace,
  PermissionMode,
  Question,
  ChatStatus,
  Checkout,
  HostAccountBinding,
  HostAccountOverview,
  Provider,
  PullRequestState,
  Finding,
  OptOuts,
  RepoCheck,
  RepositoryId,
  RepositoryOverview,
  RepositoryScript,
  ServiceEvent,
  StartOn,
  SubscriptionId,
  SubscriptionOverview,
  ServiceConnection,
  Ticket,
  Todo,
  TranscriptEntry,
  UsageLimit,
  WayfinderMap,
  WayfinderMaps,
} from "./protocol";
import * as diffPanel from "./diff";
import * as extensions from "./extensions";
import { icon, type IconName } from "./icons";
import * as menu from "./menu";
import { menuButton, type MenuButtonOptions, type MenuSection } from "./menu";
import * as newChat from "./newChat";
import * as service from "./service";
import * as terminal from "./terminal";
import * as wayfinder from "./wayfinder";
import {
  AGENT_CLI_NAMES,
  AGENT_CLI_PROVIDERS,
  PERMISSION_MODE_NAMES,
  PROVIDER_NAMES,
  type ChatDefaults,
  type ChatView,
} from "./service";

/**
 * What the main area shows: the new chat the developer is about to start,
 * a chat, or one of a repository's views instead of a chat.
 */
type Selection =
  | { type: "new_chat" }
  | { type: "chat"; chat: ChatId }
  | { type: "repo_check"; repository: RepositoryId }
  | { type: "wayfinder_map"; repository: RepositoryId }
  | { type: "extensions"; repository: RepositoryId };

let selected: Selection = { type: "new_chat" };

/** The selected chat, if a chat is selected. */
function selectedChat(): ChatId | null {
  return selected.type === "chat" ? selected.chat : null;
}

/** The repository whose `view` is selected, if it is. */
function selectedRepository(view: "repo_check" | "wayfinder_map" | "extensions"): RepositoryId | null {
  return selected.type === view ? selected.repository : null;
}

// The map the developer picked, by repository, when one has several.
const pickedMaps = new Map<RepositoryId, number>();
// A fingerprint of what the wayfinder view shows now; the view is built
// again only when it changes, so other events keep the developer's focus
// and scroll position.
let renderedMapFingerprint: string | null = null;
// Drops answers to older reads that arrive after newer ones.
let readCount = 0;

const repositoriesNav = byId("repositories");
const topBar = byId("top-bar");
const transcript = byId("transcript");
const composer = byId<HTMLFormElement>("composer");
const messageInput = byId<HTMLTextAreaElement>("message");
const sendButton = byId<HTMLButtonElement>("send");
const interruptButton = byId<HTMLButtonElement>("interrupt");
const composerContext = byId("composer-context");
const composerBox = byId("composer-box");
const composerOptions = byId("composer-options");
const composerAgent = byId("composer-agent");
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
const hostAccountsDialog = byId<HTMLDialogElement>("host-accounts-dialog");
const pullRequestDialog = byId<HTMLDialogElement>("pull-request-dialog");
const pullRequestError = byId("pull-request-error");
const pullRequestForm = byId<HTMLFormElement>("pull-request-form");
const pullRequestTarget = byId("pull-request-target");
const pullRequestTitle = byId<HTMLInputElement>("pull-request-title-input");
const pullRequestBody = byId<HTMLTextAreaElement>("pull-request-body");
const pullRequestSubmit = byId<HTMLButtonElement>("pull-request-submit");
// The chat the pull request dialog opens a pull request for.
let pullRequestChat: ChatId | null = null;
// The chat whose open pull request was last checked on the git host, so
// selecting a chat checks it once rather than on every refresh.
let checkedPullRequestOf: ChatId | null = null;
const hostAccountsError = byId("host-accounts-error");
const hostAccountList = byId("host-account-list");
const addHostAccountForm = byId<HTMLFormElement>("add-host-account");
const hostLoginSection = byId("host-login-section");
const hostLoginOutput = byId("host-login-output");
// The lines of host account logins run since the app started.
const hostLoginLines: string[] = [];
// The last push of the selected chat, shown in the top bar until another chat is selected.
let lastPush: { chat: ChatId; branch: string } | null = null;

// The lines of logins run since the app started; the service does not
// keep them.
const loginLines: string[] = [];
// The subscription whose login printed last; the answer goes to it.
let answeringLogin: number | null = null;

const repoCheckView = byId("repo-check");
const wayfinderView = byId("wayfinder");
const serviceNotice = byId("service-notice");
const todosPanel = byId("todos");
const todoList = byId("todo-list");
const todoForm = byId<HTMLFormElement>("todo-form");
const todoInput = byId<HTMLInputElement>("todo-text");
extensions.mount(byId("extensions"));
diffPanel.mount(byId("diff"), showError);

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
 * facts about the view, such as repo-check's repository and commit, on
 * the second.
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

/** What went wrong, in words to show the developer. */
function errorText(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

function showError(error: unknown): void {
  errorBox.textContent = errorText(error);
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

/**
 * What a new chat would start with, or why the harness service cannot
 * say: the repository's config is invalid, or a service update waits.
 */
type NewChatDefaults =
  | { type: "read"; defaults: ChatDefaults }
  | { type: "failed"; reason: string }
  | { type: "service_update_waiting" };

async function readDefaults(repository: RepositoryId, updateWaiting: boolean): Promise<NewChatDefaults> {
  // While an update waits, the running service may be older than the app
  // and not know chat defaults or a new chat's settings. It never answers
  // a command it does not know, so the app does not ask.
  if (updateWaiting) return { type: "service_update_waiting" };
  try {
    return { type: "read", defaults: await service.readChatDefaults(repository) };
  } catch (error) {
    return { type: "failed", reason: errorText(error) };
  }
}

async function refresh(): Promise<void> {
  try {
    await readAndRender();
  } finally {
    // A menu whose button the render removed has nothing left to choose for.
    menu.sync();
  }
}

async function readAndRender(): Promise<void> {
  // While there is no connection, the service has nothing to read.
  if (connection.state !== "connected") {
    renderServiceNotice(false);
    return;
  }
  const read = ++readCount;
  const chat = selectedChat();
  const [repositories, chats, subscriptions, hostAccounts, view, todos, updateWaiting] = await Promise.all([
    service.listRepositories(),
    service.listChats(),
    service.listSubscriptions(),
    service.listHostAccounts(),
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
  // The new chat shows what it would start with in its repository.
  const startsIn = selected.type === "new_chat" ? newChat.repository(repositories) : null;
  const defaults = startsIn === null ? null : await readDefaults(startsIn.repository, updateWaiting);
  // Read from what the service last polled, not from the git host.
  const maps = new Map(
    await Promise.all(
      repositories.map(
        async (overview) =>
          [
            overview.repository,
            await service.readWayfinderMaps(overview.repository).catch(() => null),
          ] as const,
      ),
    ),
  );
  if (read !== readCount) return;
  renderServiceNotice(updateWaiting);
  renderSidebar(repositories, chats, checks, maps, startsIn?.repository ?? null);
  // The subscriptions dialog can open over any view.
  renderSubscriptions(subscriptions);
  renderHostAccounts(hostAccounts);
  wayfinderView.hidden = true;
  if (selected.type === "extensions") {
    const shownRepository = selected.repository;
    const shown = repositories.find((overview) => overview.repository === shownRepository);
    repoCheckView.hidden = true;
    diffPanel.hide();
    renderExtensionsTopBar(shown ?? null);
    renderTodos(null, null);
    terminal.show(null);
    return;
  }
  const mapRepository = repositories.find(
    (overview) => overview.repository === selectedRepository("wayfinder_map"),
  );
  if (mapRepository) {
    diffPanel.hide();
    repoCheckView.hidden = true;
    renderWayfinder(mapRepository, openMaps(maps.get(mapRepository.repository) ?? null));
    renderTodos(null, null);
    terminal.show(null);
    return;
  }
  const checkedRepository = selectedRepository("repo_check");
  const checked = repositories.find((overview) => overview.repository === checkedRepository);
  const check = checkedRepository === null ? undefined : checks.get(checkedRepository);
  if (checked && check) {
    diffPanel.hide();
    renderRepoCheck(checked, check);
    renderTodos(null, null);
    terminal.show(null);
    return;
  }
  repoCheckView.hidden = true;
  transcript.hidden = false;
  const overview = chats.find((candidate) => candidate.chat === chat) ?? null;
  if (selected.type !== "new_chat" && (view === null || overview === null)) {
    // The service has nothing to show for the selection, such as a chat
    // it no longer lists: a new chat takes its place.
    selected = { type: "new_chat" };
    await readAndRender();
    return;
  }
  if (chat === null || view === null || overview === null) {
    diffPanel.hide();
    renderNewChat(startsIn, repositories, chats, subscriptions, defaults);
    renderTodos(null, null);
    return;
  }
  renderChat(
    view,
    overview,
    repositories.find((candidate) => candidate.repository === overview.repository) ?? null,
    subscriptions,
    todos ?? [],
  );
  renderTodos(chat, todos);
  if (diffPanel.isOpen()) {
    transcript.hidden = true;
    await diffPanel.show(chat, selectedWorking);
  }
}

// The chats whose todo list the developer opened or closed. Any other
// chat shows its todo list once it has todos.
const todosOpened = new Map<ChatId, boolean>();

function todosShown(chat: ChatId, todos: Todo[]): boolean {
  return todosOpened.get(chat) ?? todos.length > 0;
}

function renderTodos(chat: ChatId | null, todos: Todo[] | null): void {
  if (chat === null || todos === null || !todosShown(chat, todos)) {
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

/**
 * Shows a new chat in `repository` (`null`: the most recently used one)
 * and puts the cursor in the composer. The chat starts with its first
 * message.
 */
async function openNewChat(repository: RepositoryId | null, checkout?: Checkout): Promise<void> {
  // An open menu chooses for the chat that was on screen.
  menu.close();
  if (leaveExtensions()) {
    newChat.open(repository, checkout);
    selected = { type: "new_chat" };
  }
  await refresh();
  if (selected.type === "new_chat") messageInput.focus();
}

/**
 * Starts the new chat on screen with its settings and selects it; `null`
 * while there is no repository to start it in.
 */
async function startNewChat(): Promise<ChatId | null> {
  const [repositories, updateWaiting] = await Promise.all([
    service.listRepositories(),
    service.serviceUpdateWaiting(),
  ]);
  const startsIn = newChat.repository(repositories);
  if (startsIn === null) return null;
  const { checkout, startOn, permissionMode } = newChat.current();
  // While a service update waits the composer shows the repository's
  // defaults (see `readDefaults`), and the chat starts with them.
  const settings = updateWaiting ? undefined : { start_on: startOn, permission_mode: permissionMode };
  const chat = await service.startChat(startsIn.repository, checkout, settings);
  newChat.remember(startsIn.repository);
  // The next new chat starts over in the same repository.
  newChat.open(startsIn.repository);
  selected = { type: "chat", chat };
  return chat;
}

async function addRepository(): Promise<void> {
  const path = await service.pickFolder();
  if (path === null) return;
  await openNewChat(await service.addRepository(path));
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
  if (selected.type !== "extensions") return true;
  if (extensions.hasUnsavedChanges()) {
    showError("Save or discard your changes to the CLI extension first.");
    return false;
  }
  selected = { type: "new_chat" };
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
  maps: Map<RepositoryId, WayfinderMaps | null>,
  /** The repository the new chat on screen starts in, if one shows. */
  newChatIn: RepositoryId | null,
): void {
  const list = el("ul", "repository-list");
  if (repositories.length === 0) {
    list.append(el("li", "empty", "Add a repository to start a chat."));
  }
  for (const repository of repositories) {
    const item = el("li", "repository");
    const header = el("div", "repository-header");
    // The repository's row opens a new chat in it, as the app shows on start.
    const name = el("button", "repository-name");
    name.type = "button";
    name.title = `${repository.path}\nNew chat in ${repository.name}`;
    name.append(icon("folder"), el("span", "repository-name-text", repository.name));
    if (repository.repository === newChatIn) {
      name.classList.add("selected");
      name.setAttribute("aria-current", "true");
    }
    name.addEventListener("click", () => act(() => openNewChat(repository.repository)));
    const showExtensions = iconButton("blocks", `CLI extensions of ${repository.name}`, "secondary");
    showExtensions.title = `Skills, MCP servers, hooks and instruction files the agent CLIs load from ${repository.name}`;
    if (repository.repository === selectedRepository("extensions")) {
      showExtensions.classList.add("selected");
      showExtensions.setAttribute("aria-current", "true");
    }
    showExtensions.addEventListener("click", () =>
      act(async () => {
        if (selectedRepository("extensions") !== repository.repository && !leaveExtensions()) return;
        selected = { type: "extensions", repository: repository.repository };
        repoCheckView.hidden = true;
        transcript.hidden = true;
        composer.hidden = true;
        await extensions.show(repository.repository);
        await refresh();
      }),
    );
    const actions = el("span", "repository-actions");
    actions.append(showExtensions);
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
        el("span", "chat-note", checkStatus(check)),
      );
      if (repository.repository === selectedRepository("repo_check")) {
        button.classList.add("selected");
        button.setAttribute("aria-current", "true");
      }
      button.addEventListener("click", () =>
        act(async () => {
          if (!leaveExtensions()) return;
          selected = { type: "repo_check", repository: repository.repository };
          await refresh();
        }),
      );
      checkItem.append(button);
      chatList.append(checkItem);
    }
    // A repository without an open map shows nothing extra.
    const open = openMaps(maps.get(repository.repository) ?? null);
    if (open.length > 0) {
      const mapItem = el("li");
      const button = el("button", "chat-link wayfinder-link");
      button.type = "button";
      button.title = open.map((map) => map.title).join("\n");
      button.append(
        icon("map"),
        el("span", "chat-title", open.length === 1 ? "Wayfinder map" : `Wayfinder maps (${open.length})`),
      );
      if (repository.repository === selectedRepository("wayfinder_map")) {
        button.classList.add("selected");
        button.setAttribute("aria-current", "true");
      }
      button.addEventListener("click", () =>
        act(async () => {
          if (!leaveExtensions()) return;
          selected = { type: "wayfinder_map", repository: repository.repository };
          await refresh();
        }),
      );
      mapItem.append(button);
      chatList.append(mapItem);
    }
    // The service lists waiting chats first, then working, then done ones.
    for (const chat of chats.filter((overview) => overview.repository === repository.repository)) {
      const chatItem = el("li");
      const button = el("button", `chat-link ${chat.status}`);
      button.type = "button";
      const status = STATUS_LABELS[chat.status];
      button.title = `${status} · ${chat.worktree === null ? "main checkout" : `worktree ${chat.worktree}`}`;
      // The status shows as a mark; its words are for screen readers and the tooltip.
      const mark = el("span", `chat-status ${chat.status}`);
      mark.append(el("span", "visually-hidden", status));
      button.append(el("span", "chat-title", chat.title ?? "New chat"), mark);
      if (chat.chat === selectedChat()) {
        button.classList.add("selected");
        button.setAttribute("aria-current", "true");
      }
      button.addEventListener("click", () =>
        act(async () => {
          if (!leaveExtensions()) return;
          selected = { type: "chat", chat: chat.chat };
          newChat.remember(chat.repository);
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

/** How close to the end, in pixels, counts as the transcript's bottom. */
const TRANSCRIPT_BOTTOM_SLACK = 40;
// Whether the transcript follows new entries: until the developer scrolls
// up, and again once they scroll back to the bottom or open another chat.
// Only scrolling up stops it: renders and layout changes move the
// transcript too, but never up.
let followTranscript = true;
// The chat the transcript last showed, to follow again in another one.
let transcriptChat: ChatId | null = null;
// Where the developer left the transcript. Hiding it (behind the diff
// panel or repo-check) loses its position; showing it again restores this.
let lastScrollTop = 0;

/** Keeps the transcript at its end while it follows, else where it was. */
function placeTranscript(): void {
  if (transcript.hidden) return;
  transcript.scrollTop = followTranscript ? transcript.scrollHeight : lastScrollTop;
}

/** The composer's send button says what Enter does now. */
function labelSendButton(label: string, hint: string): void {
  sendButton.setAttribute("aria-label", label);
  sendButton.title = `${label}: ${hint}`;
}

/** The new chat the developer is about to start: nothing but the question and the composer. */
function renderNewChat(
  startsIn: RepositoryOverview | null,
  repositories: RepositoryOverview[],
  chats: ChatOverview[],
  subscriptions: SubscriptionOverview[],
  defaults: NewChatDefaults | null,
): void {
  terminal.show(null);
  selectedWorking = false;
  selectedOpenRequests = [];
  transcriptChat = null;
  renderTopBar("New chat");
  const hero = el("li", "new-chat");
  const logo = el("div", "new-chat-logo");
  logo.append(icon("terminal"));
  hero.append(logo);
  if (startsIn === null) {
    const add = el("button", "primary", "Add repository");
    add.type = "button";
    add.prepend(icon("folderPlus"));
    add.addEventListener("click", () => act(addRepository));
    hero.append(
      el("h2", "new-chat-title", "Add a repository to start a chat"),
      el("p", "muted", "A chat works in a repository: in a worktree of its own, or in the main checkout."),
      add,
    );
    transcript.replaceChildren(hero);
    composer.hidden = true;
    return;
  }
  const title = el("h2", "new-chat-title");
  title.append(
    "What should we build in ",
    repositoryPicker("new-chat-title-repository", startsIn, repositories, {
      className: "new-chat-repository",
      inline: true,
    }),
    "?",
  );
  hero.append(title);
  if (defaults?.type === "failed") {
    hero.append(el("p", "callout danger", `A chat cannot start in ${startsIn.name} yet: ${defaults.reason}`));
  }
  transcript.replaceChildren(hero);

  const draft = newChat.current();
  composerContext.replaceChildren(
    repositoryPicker("new-chat-repository", startsIn, repositories, { icon: "folder" }),
    checkoutPicker(draft.checkout, chats),
  );
  // Without the repository's defaults the chat starts with whatever they
  // are, and its settings change once it has started: the pickers show no
  // choice. What the developer chose before shows again with the defaults.
  const repositoryDefaults = defaults?.type === "read" ? defaults.defaults : null;
  const setAndRefresh = async (set: () => void): Promise<void> => {
    set();
    await refresh();
  };
  composerOptions.replaceChildren(
    permissionModePicker(
      repositoryDefaults === null ? null : (draft.permissionMode ?? repositoryDefaults.permissionMode),
      null,
      (mode) => setAndRefresh(() => newChat.setPermissionMode(mode)),
    ),
  );
  const startOn =
    repositoryDefaults === null
      ? null
      : (draft.startOn ?? {
          provider: AGENT_CLI_PROVIDERS[repositoryDefaults.agentCli],
          subscription: repositoryDefaults.subscription,
        });
  composerAgent.replaceChildren(
    agentPicker(startOn, subscriptions, true, (choice) =>
      setAndRefresh(() => {
        if (startOn !== null) retargetWayfinderInvocation(startOn.provider, choice.provider);
        newChat.setStartOn(choice);
      }),
    ),
  );
  labelSendButton("Send", "starts the chat (Enter)");
  messageInput.placeholder = `Describe a task in ${startsIn.name}`;
  interruptButton.hidden = true;
  composerBox.hidden = false;
  composer.hidden = false;
}

/** The repository the new chat starts in, with a menu of the others and of adding one. */
function repositoryPicker(
  key: string,
  startsIn: RepositoryOverview,
  repositories: RepositoryOverview[],
  look: Pick<MenuButtonOptions, "icon" | "className" | "inline">,
): HTMLElement {
  return menuButton({
    key,
    name: "Repository",
    label: startsIn.name,
    hint: startsIn.path,
    sections: [
      {
        heading: "Start the chat in",
        items: repositories.map((repository) => ({
          label: repository.name,
          icon: "folder",
          selected: repository.repository === startsIn.repository,
          onSelect: () => void act(() => openNewChat(repository.repository)),
        })),
      },
      {
        items: [{ label: "Add repository…", icon: "folderPlus", onSelect: () => void act(addRepository) }],
      },
    ],
    ...look,
  });
}

/** Where the new chat works: a new worktree, the main checkout, or another chat's worktree. */
function checkoutPicker(checkout: Checkout, chats: ChatOverview[]): HTMLElement {
  const choose = (chosen: Checkout) => () =>
    void act(async () => {
      newChat.setCheckout(chosen);
      await refresh();
    });
  const sections: MenuSection[] = [
    {
      heading: "The chat works in",
      items: [
        {
          label: "New worktree",
          description: "On a new branch from the repository's base branch. Its setup script runs first.",
          icon: "layers",
          selected: checkout.type === "new_worktree",
          onSelect: choose({ type: "new_worktree" }),
        },
        {
          label: "Main checkout",
          description: "In the repository itself, on the branch it has checked out.",
          icon: "home",
          selected: checkout.type === "main",
          onSelect: choose({ type: "main" }),
        },
      ],
    },
  ];
  let label = checkout.type === "main" ? "Main checkout" : "New worktree";
  if (checkout.type === "worktree") {
    const other = chats.find((overview) => overview.chat === checkout.chat);
    label = `Worktree of "${other?.title ?? "New chat"}"`;
    sections[0]?.items.push({
      label,
      description: other?.worktree ?? "Next to the chat that works there, on its branch.",
      icon: "layers",
      selected: true,
      onSelect: choose(checkout),
    });
  }
  return menuButton({
    key: "new-chat-checkout",
    name: "Checkout",
    icon: checkout.type === "main" ? "home" : "layers",
    label,
    sections,
  });
}

/** A fact about where the chat works, in the line above the composer. */
function contextItem(iconName: IconName, text: string, title: string, className = ""): HTMLElement {
  const item = el("span", `context-item ${className}`.trim());
  item.title = title;
  item.append(icon(iconName), el("span", "context-text", text));
  return item;
}

function renderChat(
  view: ChatView,
  overview: ChatOverview,
  repository: RepositoryOverview | null,
  subscriptions: SubscriptionOverview[],
  todos: Todo[],
): void {
  const chat = overview.chat;
  // The terminal panel's shell runs here.
  const checkout = overview.worktree ?? repository?.path;
  renderTopBar(overview.title ?? "New chat", [], chatActions(overview, view, checkout, todos));
  const branch =
    view.place === "removed_worktree" && view.pullRequest !== null
      ? `${view.pullRequest.head} (removed)`
      : branchName(view.branch);
  composerContext.replaceChildren(
    contextItem("folder", repository?.name ?? "unknown", `Repository: ${repository?.path ?? "unknown"}`),
    contextItem("branch", branch, `Branch: ${branch}`, "mono"),
    contextItem(
      view.place === "main_checkout" ? "home" : "layers",
      CHECKOUT_LABELS[view.place],
      `Checkout: ${checkout ?? CHECKOUT_LABELS[view.place]}`,
    ),
    hostAccountItem(view.hostAccount),
    commitAuthorItem(view),
  );
  // A turn that waits for an answer still runs and can be interrupted.
  const waitsForAnswer = view.openRequests.length > 0;
  const turnRuns = view.status === "working" || waitsForAnswer;
  composerOptions.replaceChildren(
    permissionModePicker(view.permissionMode, view.appliedPermissionMode, async (mode) => {
      await service.changePermissionMode(chat, mode);
      await refresh();
    }),
  );
  composerAgent.replaceChildren(
    agentPicker(
      { provider: AGENT_CLI_PROVIDERS[view.agentCli], subscription: view.subscription },
      subscriptions,
      !turnRuns,
      async (choice) => {
        try {
          await service.changeSubscription(chat, choice.provider, choice.subscription);
        } finally {
          await refresh();
        }
      },
    ),
  );
  const removed = view.place === "removed_worktree";
  terminal.show(removed ? null : chat, checkout);
  checkPullRequestOnce(chat, view);
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
  const cleanup = cleanupCard(chat, view);
  if (cleanup !== null) entries.push(cleanup);
  // A turn that waits for an answer shows its card instead.
  if (turnRuns && !waitsForAnswer) {
    const working = el("li", "working");
    working.append(el("span", "chat-status working"), "Working…");
    entries.push(working);
  }
  if (entries.length === 0) {
    entries.push(el("li", "empty", "Send a message to start the agent."));
  }
  if (overview.chat !== transcriptChat) {
    transcriptChat = overview.chat;
    followTranscript = true;
  }
  transcript.replaceChildren(...entries);
  placeTranscript();
  selectedOpenRequests = view.openRequests;
  selectedWorking = turnRuns;
  // A text reply to a waiting chat declines its oldest request with feedback.
  if (waitsForAnswer) {
    labelSendButton("Reply", "declines the request and gives the agent your text (Enter)");
    messageInput.placeholder = "Reply instead of approving: the agent gets your text";
  } else if (turnRuns) {
    labelSendButton("Queue", "the agent gets your message when the turn ends (Enter)");
    messageInput.placeholder = "Queue a message for when the turn ends";
  } else {
    labelSendButton("Send", "starts a turn (Enter)");
    messageInput.placeholder = "Message the agent";
  }
  interruptButton.hidden = !turnRuns;
  // A chat whose worktree is gone takes no more messages; the line above
  // the message box still says where it worked.
  composerBox.hidden = removed;
  composer.hidden = false;
}

const CHECKOUT_LABELS: Record<ChatPlace, string> = {
  main_checkout: "main checkout",
  worktree: "worktree",
  removed_worktree: "worktree removed",
};

const PULL_REQUEST_STATES: Record<PullRequestState, string> = {
  open: "open",
  merged: "merged",
  closed: "closed",
};

/** Checks the selected chat's open pull request on the git host once. */
function checkPullRequestOnce(chat: ChatId, view: ChatView): void {
  if (view.pullRequest?.state !== "open" || checkedPullRequestOf === chat) return;
  checkedPullRequestOf = chat;
  // A git host that cannot be reached leaves the last known state.
  void service.checkPullRequest(chat).catch(() => undefined);
}

/** After the merge: the offer to remove the worktree and local branch. */
function cleanupCard(chat: ChatId, view: ChatView): HTMLElement | null {
  const pullRequest = view.pullRequest;
  if (pullRequest === null) return null;
  if (view.place === "removed_worktree") {
    return notice(
      "",
      "check",
      `The worktree and branch ${pullRequest.head} are removed. This chat is read-only.`,
      `Pull request #${pullRequest.number} was merged`,
    );
  }
  if (pullRequest.state !== "merged" || view.place !== "worktree") return null;
  const [item, body] = card("cleanup open", "branch", `Pull request #${pullRequest.number} is merged`);
  body.append(
    el(
      "div",
      "entry-text",
      `Remove this chat's worktree and its local branch ${pullRequest.head}? The chat stays readable.`,
    ),
  );
  const remove = el("button", "primary", "Remove worktree and branch");
  remove.type = "button";
  remove.addEventListener("click", () =>
    act(async () => {
      remove.disabled = true;
      try {
        await service.removeWorktree(chat);
      } finally {
        await refresh();
      }
    }),
  );
  const actions = el("div", "card-actions");
  actions.append(remove);
  body.append(actions);
  return item;
}

function openPullRequestDialog(overview: ChatOverview, view: ChatView): void {
  pullRequestChat = overview.chat;
  pullRequestError.hidden = true;
  pullRequestTarget.textContent = `From ${branchName(view.branch)} into the repository's base branch.`;
  pullRequestTitle.value = overview.title ?? "";
  pullRequestBody.value = "";
  pullRequestSubmit.disabled = false;
  pullRequestDialog.showModal();
  pullRequestTitle.focus();
}

/** What the harness blocks whatever the permission mode. */
const HARD_BLOCKS =
  "In every mode, Full access included, the harness blocks a push to main or development, a force push, a push as the wrong host account and AI attribution in commits.";

const PERMISSION_MODES: PermissionMode[] = ["supervised", "auto_accept_edits", "auto", "full_access"];

const PERMISSION_MODE_MEANINGS: Record<PermissionMode, string> = {
  supervised: "Asks before commands and file changes.",
  auto_accept_edits: "Changes files without asking; other actions still ask.",
  auto: "The agent CLI's automatic review approves routine actions and asks about the others.",
  full_access: "Never asks.",
};

/**
 * The chat's permission mode. `current` is `null` while a new chat's
 * repository default is unknown: there is nothing to choose from then.
 * `applied` differs from `current` when the agent CLI has no such mode.
 */
function permissionModePicker(
  current: PermissionMode | null,
  applied: PermissionMode | null,
  choose: (mode: PermissionMode) => Promise<void>,
): HTMLElement {
  const item = el("span", "composer-option");
  item.append(
    menuButton({
      key: "permission-mode",
      name: "Permission mode",
      hint:
        current === null
          ? "changes once the chat has started"
          : "how much the agent may do without your approval",
      icon: "shield",
      label: current === null ? "Repository default" : PERMISSION_MODE_NAMES[current],
      disabled: current === null,
      sections: [
        {
          heading: "Permission mode",
          items: PERMISSION_MODES.map((mode) => ({
            label: PERMISSION_MODE_NAMES[mode],
            description: PERMISSION_MODE_MEANINGS[mode],
            selected: mode === current,
            onSelect: () => void act(() => choose(mode)),
          })),
        },
      ],
      note: HARD_BLOCKS,
    }),
  );
  if (applied !== null && applied !== current) {
    const runs = el("span", "permission-applied", `runs ${PERMISSION_MODE_NAMES[applied]}`);
    runs.title = `The agent CLI has no such mode, so the chat runs ${PERMISSION_MODE_NAMES[applied]}.`;
    item.append(runs);
  }
  return item;
}

/** Who the chat's commits are by, cut short above the composer. */
function commitAuthorItem(view: ChatView): HTMLElement {
  const author =
    view.commitAuthor === null ? "no commit author" : `${view.commitAuthor.name} <${view.commitAuthor.email}>`;
  return contextItem(
    "user",
    view.commitAuthor?.name ?? author,
    `Commit author: ${author}`,
    view.commitAuthor === null ? "warning" : "",
  );
}

/** The host account the chat's repository is bound to. */
function hostAccountItem(binding: HostAccountBinding): HTMLElement {
  switch (binding.type) {
    case "bound":
      return contextItem(
        "cloud",
        `${binding.name} · ${binding.login}`,
        `Host account: ${binding.login} on ${binding.git_host}`,
      );
    case "unbound":
      return contextItem(
        "cloud",
        "No host account",
        'Host account: not bound. Bind it in the repository\'s .leiser/config.toml: host_account = "<name>"',
        "warning",
      );
    case "unknown":
      return contextItem(
        "cloud",
        `${binding.name}?`,
        `Host account: "${binding.name}" is not registered; add it under Host accounts.`,
        "warning",
      );
    case "unreadable":
      return contextItem("cloud", "Config unreadable", `Host account: ${binding.reason}`, "warning");
  }
}

/** The top bar's buttons for the selected chat. */
function chatActions(
  overview: ChatOverview,
  view: ChatView,
  checkout: string | undefined,
  todos: Todo[],
): HTMLElement[] {
  const actions: HTMLElement[] = [];
  const pullRequest = view.pullRequest;
  if (pullRequest !== null) {
    const check = topBarAction(
      "branch",
      `PR #${pullRequest.number} · ${PULL_REQUEST_STATES[pullRequest.state]}`,
      "ghost",
    );
    check.title = `${pullRequest.url}\nCheck its state on the git host again`;
    check.addEventListener("click", () =>
      act(async () => {
        try {
          await service.checkPullRequest(overview.chat);
        } finally {
          await refresh();
        }
      }),
    );
    actions.push(check);
    if (pullRequest.state === "open" && view.place !== "removed_worktree") {
      actions.push(autoFixAction(overview.chat, pullRequest.number, view.autoFix));
    }
  }
  const openTodos = todos.filter((todo) => !todo.done).length;
  const toggleTodos = topBarAction("checklist", openTodos === 0 ? "Todos" : `Todos · ${openTodos}`, "ghost");
  toggleTodos.title = "The chat's todo list, which you and the agent keep";
  toggleTodos.setAttribute("aria-pressed", String(todosShown(overview.chat, todos)));
  toggleTodos.addEventListener("click", () =>
    act(async () => {
      todosOpened.set(overview.chat, !todosShown(overview.chat, todos));
      await refresh();
    }),
  );
  // A removed worktree has nothing left to push, open or run a shell in.
  if (view.place === "removed_worktree") {
    actions.push(toggleTodos);
    return actions;
  }
  if (pullRequest === null) {
    const open = topBarAction("branch", "Open pull request", "ghost");
    open.title = "Push the chat's branch and open its pull request, after checking the host account";
    open.addEventListener("click", () => openPullRequestDialog(overview, view));
    actions.push(open);
  }
  const push = topBarAction("arrowUp", "Push", "ghost");
  push.title =
    pullRequest === null
      ? "Push the chat's branch to origin, after checking the host account"
      : `Push the chat's branch to origin, updating pull request #${pullRequest.number}`;
  push.addEventListener("click", () =>
    act(async () => {
      push.disabled = true;
      lastPush = null;
      try {
        const branch = await service.push(overview.chat);
        lastPush = { chat: overview.chat, branch };
      } finally {
        await refresh();
      }
    }),
  );
  actions.push(push);
  if (lastPush !== null && lastPush.chat === overview.chat) {
    actions.push(el("span", "top-bar-note", `pushed ${lastPush.branch}`));
  }
  const changes = topBarAction("diff", diffPanel.isOpen() ? "Show chat" : "Show changes", "ghost");
  changes.title = "Each turn's changes, the whole branch's, and restoring a turn's checkpoint";
  changes.setAttribute("aria-pressed", String(diffPanel.isOpen()));
  changes.addEventListener("click", () =>
    act(async () => {
      if (diffPanel.isOpen()) {
        diffPanel.hide();
        transcript.hidden = false;
      } else {
        await diffPanel.show(overview.chat, selectedWorking);
      }
      await refresh();
    }),
  );
  actions.push(changes);
  const toggleTerminal = topBarAction("terminal", terminal.isOpen() ? "Hide terminal" : "Terminal", "ghost");
  toggleTerminal.title = "A shell in the chat's checkout";
  toggleTerminal.setAttribute("aria-pressed", String(terminal.isOpen()));
  toggleTerminal.addEventListener("click", () => terminal.toggle(overview.chat, checkout));
  actions.push(toggleTerminal);
  if (overview.worktree !== null) {
    const sameWorktree = topBarAction("plus", "New chat in this worktree", "ghost");
    sameWorktree.title = overview.worktree;
    sameWorktree.addEventListener("click", () =>
      act(() => openNewChat(overview.repository, { type: "worktree", chat: overview.chat })),
    );
    actions.push(sameWorktree);
  }
  actions.push(toggleTodos);
  if (overview.status === "waiting_for_you") {
    const done = topBarAction("check", "Mark done", "ghost");
    done.title = "Close the chat: it stays readable, and a new chat opens";
    done.addEventListener("click", () =>
      act(async () => {
        await service.markChatDone(overview.chat);
        // The chat is closed; a fresh one in its repository takes its place.
        await openNewChat(overview.repository);
      }),
    );
    actions.push(done);
  }
  return actions;
}

/** The switch for auto-fix of the chat's open pull request. */
function autoFixAction(chat: ChatId, number: number, autoFix: AutoFix): HTMLButtonElement {
  const label = { on: "Auto-fix on", off: "Auto-fix off", stopped: "Auto-fix stopped" }[autoFix.type];
  const button = topBarAction("wrench", label, autoFix.type === "stopped" ? "ghost warning" : "ghost");
  button.setAttribute("aria-pressed", String(autoFix.type === "on"));
  button.title =
    autoFix.type === "on"
      ? `The harness watches pull request #${number} and wakes the agent when a check fails, the branch conflicts or a review comment comes in, then pushes the fix. Click to switch it off.`
      : autoFix.type === "stopped"
        ? `${autoFixStopText(autoFix.reason)} Click to switch it on again.`
        : `Click to let the harness fix pull request #${number} by itself.`;
  button.addEventListener("click", () =>
    act(async () => {
      button.disabled = true;
      try {
        await service.setAutoFix(chat, autoFix.type !== "on");
      } finally {
        await refresh();
      }
    }),
  );
  return button;
}

function autoFixStopText(reason: AutoFixStop): string {
  switch (reason.type) {
    case "no_progress":
      return `${reason.attempts} attempts in a row changed nothing on the branch.`;
    case "check_failed_again":
      return `The check ${reason.name} failed again after a fix.`;
    case "conflict_again":
      return "The branch conflicts with the base branch again after the agent merged it.";
    case "push_failed":
      return `The fix could not be pushed: ${reason.reason}`;
    case "poll_failed":
      return `The git host could not be asked about the pull request: ${reason.reason}`;
  }
}

function autoFixTriggerText(trigger: AutoFixTrigger): string {
  switch (trigger.type) {
    case "check_failed":
      return `The check ${trigger.name} failed.`;
    case "conflict":
      return `The branch conflicts with ${trigger.base}.`;
    case "comment": {
      const location = trigger.location;
      const place =
        location === null ? "" : ` on ${location.path}${location.line === null ? "" : `:${location.line}`}`;
      return `${trigger.author}${place}: ${trigger.body}`;
    }
  }
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

/** The open maps of a repository; none when they could not be read. */
function openMaps(maps: WayfinderMaps | null): WayfinderMap[] {
  return maps?.type === "read" ? maps.maps : [];
}

function renderWayfinder(repository: RepositoryOverview, maps: WayfinderMap[]): void {
  selectedWorking = false;
  composer.hidden = true;
  transcript.hidden = true;
  wayfinderView.hidden = false;
  const picked = pickedMaps.get(repository.repository);
  const map = maps.find((candidate) => candidate.number === picked) ?? maps[0];
  if (map === undefined) {
    renderTopBar("Wayfinder map", [labelled("Repository", repository.name)]);
    renderedMapFingerprint = null;
    wayfinderView.replaceChildren(el("p", "empty", "The repository has no open wayfinder map now."));
    return;
  }
  const closed = map.tickets.filter((ticket) => ticket.state.type === "closed").length;
  const openMap = topBarAction("external", "Open map");
  openMap.title = `${map.url}\nOpen the map on the git host`;
  openMap.addEventListener("click", () => act(() => service.openInBrowser(map.url)));
  renderTopBar(
    map.title,
    [
      labelled("Repository", repository.name),
      labelled("Tickets", `${closed} of ${map.tickets.length} closed`),
      labelled("Frontier", String(wayfinder.frontierCount(map))),
    ],
    [openMap],
  );

  const fingerprint = JSON.stringify([repository.repository, maps.map((candidate) => candidate.number), map]);
  if (fingerprint === renderedMapFingerprint) return;
  renderedMapFingerprint = fingerprint;
  const intro = el("div", "wayfinder-intro");
  if (maps.length > 1) {
    const picker = el("select", "wayfinder-picker");
    picker.setAttribute("aria-label", "Wayfinder map");
    for (const candidate of maps) {
      const option = el("option", "", candidate.title);
      option.value = String(candidate.number);
      option.selected = candidate.number === map.number;
      picker.append(option);
    }
    picker.addEventListener("change", () =>
      act(async () => {
        pickedMaps.set(repository.repository, Number(picker.value));
        await refresh();
      }),
    );
    intro.append(picker);
  }
  intro.append(
    el(
      "p",
      "muted",
      "Click a ticket to open it on the git host. A ticket on the frontier is open, unblocked and unclaimed: start a chat on it with its chat button.",
    ),
  );
  const legend = el("ul", "wayfinder-legend");
  for (const type of ["research", "prototype", "grilling", "task"] as const) {
    legend.append(el("li", `type-${type}`, wayfinder.TYPE_NAMES[type]));
  }
  const scroller = el("div", "wayfinder-scroller");
  scroller.append(
    wayfinder.renderGraph(map, {
      openTicket: (ticket) => void act(() => service.openInBrowser(ticket.url)),
      startChat: (ticket) => void act(() => startChatOnTicket(repository.repository, map, ticket)),
    }),
  );
  wayfinderView.replaceChildren(intro, legend, scroller);
}

/** How each agent CLI's prompt invokes a skill by name. */
const SKILL_PREFIXES: Record<AgentCli, string> = {
  claude_code: "/",
  codex: "$",
};

/**
 * Opens a new chat in a new worktree with the wayfinder skill invoked for
 * the ticket in the composer, as the chat's agent CLI takes it; the
 * developer sends it, which starts the chat.
 */
async function startChatOnTicket(repository: RepositoryId, map: WayfinderMap, ticket: Ticket): Promise<void> {
  await openNewChat(repository, { type: "new_worktree" });
  if (selected.type !== "new_chat") return;
  const defaults = await readDefaults(repository, await service.serviceUpdateWaiting());
  const agentCli = defaults.type === "read" ? defaults.defaults.agentCli : "claude_code";
  messageInput.value = `${wayfinderInvocation(agentCli)}${map.url} ${ticket.url}`;
  fitMessageInput();
  messageInput.focus();
}

/** The start of a prompt that invokes the wayfinder skill, as `agentCli` takes it. */
function wayfinderInvocation(agentCli: AgentCli): string {
  return `${SKILL_PREFIXES[agentCli]}wayfinder `;
}

/**
 * Keeps a wayfinder invocation in the composer working when the new chat
 * changes its agent CLI: each agent CLI invokes a skill its own way.
 */
function retargetWayfinderInvocation(from: Provider, to: Provider): void {
  const [before, after] = [agentCliOf(from), agentCliOf(to)];
  if (before === undefined || after === undefined) return;
  const invocation = wayfinderInvocation(before);
  if (!messageInput.value.startsWith(invocation)) return;
  messageInput.value = wayfinderInvocation(after) + messageInput.value.slice(invocation.length);
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

function labelled(label: string, value: string): HTMLElement {
  const item = el("span", "top-bar-item");
  item.append(el("span", "top-bar-label", label), el("span", "top-bar-value", value));
  // The label can be hidden in a narrow column; the tooltip still names it.
  item.title = `${label}: ${value}`;
  return item;
}

/** The agent CLIs a chat can run on, in the order the pickers list them. */
const AGENT_CLIS = Object.keys(AGENT_CLI_PROVIDERS) as AgentCli[];

function agentCliOf(provider: Provider): AgentCli | undefined {
  return AGENT_CLIS.find((candidate) => AGENT_CLI_PROVIDERS[candidate] === provider);
}

/** What a chat runs on: a provider's subscription, or its agent CLI's default login. */
type SubscriptionChoice = StartOn;

/**
 * The agent CLI and subscription the chat's next turns run on: every agent
 * CLI's default login and every subscription, grouped by provider.
 * Choosing one of another provider moves the chat to that provider's agent
 * CLI. `current` is `null` while a new chat's repository default is
 * unknown: there is nothing to choose from then. A started chat changes
 * only `betweenTurns`.
 */
function agentPicker(
  current: SubscriptionChoice | null,
  subscriptions: SubscriptionOverview[],
  betweenTurns: boolean,
  choose: (choice: SubscriptionChoice) => Promise<void>,
): HTMLElement {
  const isCurrent = (choice: SubscriptionChoice): boolean =>
    choice.provider === current?.provider && choice.subscription === current.subscription;
  let chosenName: string | null = null;
  const sections: MenuSection[] = [];
  for (const agentCli of AGENT_CLIS) {
    const provider = AGENT_CLI_PROVIDERS[agentCli];
    const section: MenuSection = { heading: `${AGENT_CLI_NAMES[agentCli]} · ${PROVIDER_NAMES[provider]}`, items: [] };
    const add = (label: string, description: string, subscription: SubscriptionId | null): void => {
      const choice = { provider, subscription };
      if (isCurrent(choice)) chosenName = label;
      section.items.push({
        label,
        description,
        selected: isCurrent(choice),
        onSelect: () => void act(() => choose(choice)),
      });
    };
    add("Default login", `What ${AGENT_CLI_NAMES[agentCli]} is signed in with by itself.`, null);
    for (const subscription of subscriptions.filter((overview) => overview.provider === provider)) {
      add(
        subscription.name,
        subscription.kind === "login" ? "Subscription with a login of its own." : "Subscription with an API key.",
        subscription.subscription,
      );
    }
    if (
      current !== null &&
      provider === current.provider &&
      current.subscription !== null &&
      !subscriptions.some((overview) => overview.subscription === current.subscription)
    ) {
      add("Removed subscription", "It no longer exists; choose another.", current.subscription);
    }
    sections.push(section);
  }
  sections.push({
    items: [
      {
        label: "Manage subscriptions…",
        icon: "key",
        onSelect: () => void act(openSubscriptions),
      },
    ],
  });
  const agentCli = current === null ? undefined : agentCliOf(current.provider);
  return menuButton({
    key: "agent",
    name: "Agent CLI and subscription",
    hint:
      current === null
        ? "changes once the chat has started"
        : betweenTurns
          ? "what the chat's next turns run on"
          : "changes between turns",
    icon: "agent",
    label: agentCli === undefined ? "Repository default" : AGENT_CLI_NAMES[agentCli],
    detail: chosenName ?? undefined,
    disabled: current === null || !betweenTurns,
    sections,
  });
}

/** Opens the subscriptions dialog on what the service lists now. */
async function openSubscriptions(): Promise<void> {
  await refresh();
  subscriptionsDialog.showModal();
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

function renderHostAccounts(hostAccounts: HostAccountOverview[]): void {
  if (hostAccounts.length === 0) {
    hostAccountList.replaceChildren(
      el("li", "muted", "None yet: repositories commit and push with your global git settings."),
    );
    return;
  }
  hostAccountList.replaceChildren(...hostAccounts.map(hostAccountRow));
}

function hostAccountRow(account: HostAccountOverview): HTMLElement {
  const row = el("li", "subscription-row host-account-row");
  const name = el("strong", "host-account-name", account.name);
  const details = el(
    "span",
    "host-account-details",
    `${account.login} on ${account.git_host} · ${account.author.name} <${account.author.email}>` +
      (account.logging_in ? " · logging in…" : ""),
  );
  details.title = account.config_dir;
  const login = el("button", "ghost", "Log in");
  login.type = "button";
  login.title = "Run the git host's login (gh auth login) for this account";
  login.addEventListener("click", () =>
    actInHostDialog(async () => {
      await service.logInHostAccount(account.host_account);
      await refresh();
    }),
  );
  const remove = el("button", "ghost danger", "Remove");
  remove.type = "button";
  remove.addEventListener("click", () =>
    actInHostDialog(async () => {
      // A second click confirms: removing deletes the account's login.
      if (remove.dataset.confirm !== "yes") {
        remove.dataset.confirm = "yes";
        remove.textContent = "Remove for good";
        return;
      }
      await service.removeHostAccount(account.host_account);
      await refresh();
    }),
  );
  const text = el("span", "host-account-text");
  text.append(name, details);
  const actions = el("span", "subscription-actions");
  actions.append(login, remove);
  row.append(text, actions);
  return row;
}

async function actInHostDialog(action: () => Promise<void>): Promise<void> {
  hostAccountsError.hidden = true;
  try {
    await action();
  } catch (error) {
    hostAccountsError.textContent = errorText(error);
    hostAccountsError.hidden = false;
  }
}

function showHostLogin(event: ServiceEvent): void {
  if (event.type === "host_account_login_output") {
    hostLoginLines.push(event.text);
  } else if (event.type === "host_account_login_finished") {
    hostLoginLines.push(
      event.outcome.type === "signed_in" ? "Signed in." : `Login failed: ${event.outcome.reason}`,
    );
  } else {
    return;
  }
  hostLoginOutput.textContent = hostLoginLines.join("\n");
  hostLoginSection.hidden = false;
}

/** Like `act`, for actions in the subscriptions dialog. */
async function actInDialog(action: () => Promise<void>): Promise<void> {
  subscriptionsError.hidden = true;
  try {
    await action();
  } catch (error) {
    subscriptionsError.textContent = errorText(error);
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
    case "auto_fix_triggered": {
      const item = notice("auto-fix", "wrench", "", `Auto-fix · pull request #${entry.pull_request}`);
      const list = el("ul", "auto-fix-triggers");
      list.append(...entry.triggers.map((trigger) => el("li", "", autoFixTriggerText(trigger))));
      item.querySelector(".entry-text")?.replaceWith(list);
      return item;
    }
    case "auto_fix_stopped":
      return notice(
        "failed",
        "alert",
        `${autoFixStopText(entry.reason)} Look at the pull request, then switch auto-fix on again in the top bar.`,
        "Auto-fix stopped",
      );
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
  const started = selectedChat();
  if (text === "" || (started === null && selected.type !== "new_chat")) return;
  messageInput.disabled = true;
  sendButton.disabled = true;
  try {
    await act(async () => {
      // The new chat starts with its first message.
      const chat = started ?? (await startNewChat());
      if (chat === null) return;
      const waitingFor = started === null ? undefined : selectedOpenRequests[0];
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

transcript.addEventListener("scroll", () => {
  if (transcript.hidden) return;
  const top = transcript.scrollTop;
  if (transcript.scrollHeight - top - transcript.clientHeight < TRANSCRIPT_BOTTOM_SLACK) {
    followTranscript = true;
  } else if (top < lastScrollTop) {
    followTranscript = false;
  }
  lastScrollTop = top;
});

// The transcript changes size after a render: when the composer appears,
// and when it shows again after the diff panel.
new ResizeObserver(placeTranscript).observe(transcript);

todoForm.addEventListener("submit", (event) => {
  event.preventDefault();
  const text = todoInput.value.trim();
  const chat = selectedChat();
  if (chat === null || text === "") return;
  void act(async () => {
    await service.addTodo(chat, text);
    todoInput.value = "";
    await refresh();
  });
});

interruptButton.addEventListener("click", () =>
  act(async () => {
    const chat = selectedChat();
    if (chat === null) return;
    await service.interruptTurn(chat);
    await refresh();
  }),
);

byId("new-chat").prepend(icon("compose"));
byId("add-repository").append(icon("folderPlus"));
byId("open-subscriptions").prepend(icon("key"));
byId("open-host-accounts").prepend(icon("cloud"));
sendButton.append(icon("arrowUp"));
interruptButton.append(icon("stop"));

const onMac = navigator.platform.startsWith("Mac");
byId("new-chat-shortcut").textContent = onMac ? "⌘N" : "Ctrl+N";
byId("new-chat").setAttribute("aria-keyshortcuts", onMac ? "Meta+N" : "Control+N");

byId("new-chat").addEventListener("click", () => act(() => openNewChat(null)));
byId("add-repository").addEventListener("click", () => act(addRepository));

document.addEventListener("keydown", (event) => {
  const modifier = onMac ? event.metaKey : event.ctrlKey;
  if (!modifier || event.shiftKey || event.altKey || event.key.toLowerCase() !== "n") return;
  // A dialog is modal: its fields keep the keyboard.
  if (document.querySelector("dialog[open]") !== null) return;
  event.preventDefault();
  void act(() => openNewChat(null));
});

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

byId("open-subscriptions").addEventListener("click", () => act(openSubscriptions));

byId("close-subscriptions").addEventListener("click", () => subscriptionsDialog.close());

byId("open-host-accounts").addEventListener("click", () =>
  act(async () => {
    await refresh();
    hostAccountsDialog.showModal();
  }),
);

byId("close-host-accounts").addEventListener("click", () => hostAccountsDialog.close());

byId("close-pull-request").addEventListener("click", () => pullRequestDialog.close());

pullRequestForm.addEventListener("submit", (event) => {
  event.preventDefault();
  const chat = pullRequestChat;
  if (chat === null) return;
  pullRequestError.hidden = true;
  pullRequestSubmit.disabled = true;
  void (async () => {
    try {
      await service.openPullRequest(chat, pullRequestTitle.value, pullRequestBody.value);
      pullRequestDialog.close();
    } catch (error) {
      pullRequestError.textContent = errorText(error);
      pullRequestError.hidden = false;
    } finally {
      pullRequestSubmit.disabled = false;
      await refresh().catch(showError);
    }
  })();
});

addHostAccountForm.addEventListener("submit", (event) => {
  event.preventDefault();
  void actInHostDialog(async () => {
    const value = (id: string) => byId<HTMLInputElement>(id).value;
    await service.addHostAccount(value("new-host-name"), value("new-host-git-host"), value("new-host-login"), {
      name: value("new-host-author-name"),
      email: value("new-host-author-email"),
    });
    for (const id of ["new-host-name", "new-host-login", "new-host-author-name", "new-host-author-email"]) {
      byId<HTMLInputElement>(id).value = "";
    }
    await refresh();
  });
});

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
      showHostLogin(event);
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
  // The app opens on a new chat, ready to type in.
  if (selected.type === "new_chat") messageInput.focus();
});
