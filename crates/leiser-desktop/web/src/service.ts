// Talking to the harness service through the app's Rust side.

import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

import type {
  AgentCli,
  Answer,
  Branch,
  ChatId,
  Checkout,
  ChatOverview,
  ChatStatus,
  CliExtension,
  Command,
  CommandError,
  PermissionMode,
  Provider,
  QueueReason,
  RepoCheck,
  RepositoryId,
  RepositoryOverview,
  Response,
  ServiceConnection,
  ServiceEvent,
  SignIn,
  SubscriptionId,
  SubscriptionOverview,
  Todo,
  TodoId,
  TranscriptEntry,
  UnreadableCliExtensionFile,
  UsageLimit,
} from "./protocol";

/** A chat as the service reports it. */
export interface ChatView {
  status: ChatStatus;
  branch: Branch;
  permissionMode: PermissionMode;
  appliedPermissionMode: PermissionMode;
  /** Approval requests and questions the running turn waits for, oldest first. */
  openRequests: string[];
  agentCli: AgentCli;
  /** `null`: the agent CLI's own default login. */
  subscription: SubscriptionId | null;
  /** The usage limit the last turn stopped at, until the next turn. */
  usageLimit: UsageLimit | null;
  transcript: TranscriptEntry[];
}

export const PERMISSION_MODE_NAMES: Record<PermissionMode, string> = {
  supervised: "Supervised",
  auto_accept_edits: "Auto-accept edits",
  auto: "Auto",
  full_access: "Full access",
};

export const AGENT_CLI_NAMES: Record<AgentCli, string> = {
  claude_code: "Claude Code",
  codex: "Codex",
};

export const PROVIDER_NAMES: Record<Provider, string> = {
  anthropic: "Anthropic",
  openai: "OpenAI",
};

/** The provider whose subscriptions an agent CLI runs on. */
export const AGENT_CLI_PROVIDERS: Record<AgentCli, Provider> = {
  claude_code: "anthropic",
  codex: "openai",
};

/** A command the service refused, with the reason it gave. */
export class ServiceError extends Error {
  constructor(readonly error: CommandError) {
    super(describe(error));
  }
}

function describe(error: CommandError): string {
  switch (error.type) {
    case "not_a_git_repository":
      return `Not a git repository: ${error.path}`;
    case "unknown_repository":
      return "The harness service does not know this repository.";
    case "unknown_chat":
      return "The harness service does not know this chat.";
    case "chat_in_another_repository":
      return "That chat belongs to another repository.";
    case "not_in_a_worktree":
      return "That chat works in the main checkout, not in a worktree.";
    case "setting_up":
      return "The chat's worktree is still being set up.";
    case "worktree":
      return `Could not create the chat's worktree: ${error.reason}`;
    case "repository_config":
      return `The repository's .leiser/config.toml is invalid: ${error.reason}`;
    case "turn_running":
      return "A turn is running; queue the message or interrupt the turn.";
    case "no_turn_running":
      return "No turn is running.";
    case "unknown_request":
      return "The agent no longer waits for this answer.";
    case "answer_does_not_fit":
      return "This answer does not fit the request.";
    case "repository_config":
      return `Could not read the repository's .leiser/config.toml: ${error.reason}`;
    case "agent_cli_not_configured":
      return `${AGENT_CLI_NAMES[error.agent_cli]} is not set up in the harness service.`;
    case "agent_cli":
      return `The agent CLI failed: ${error.reason}`;
    case "empty_todo":
      return "A todo needs a text.";
    case "unknown_todo":
      return `Todo ${error.todo} no longer exists.`;
    case "storage":
      return `The harness service could not store the chat: ${error.reason}`;
    case "repo_check_not_configured":
      return "repo-check is not set up in the harness service.";
    case "repo_check_running":
      return "repo-check is already running in this repository.";
    case "not_a_cli_extension_file":
      return `${error.file} holds no CLI extension of this repository any more.`;
    case "cli_extension_file_changed":
      return `${error.file} changed since you opened it. Reload it, then edit again.`;
    case "invalid_cli_extension_file":
      return `Not saved: the agent CLI could not read ${error.file} any more. ${error.reason}`;
    case "cli_extension_file_access":
      return `Could not access ${error.file}: ${error.reason}`;
    case "service_stopped":
      return "The harness service has stopped.";
    case "no_usage_limit":
      return "The chat's last turn did not stop at a usage limit.";
    case "unknown_subscription":
      return "This chat's subscription was removed. Choose another one in the top bar.";
    case "unknown_subscription_name":
      return `The repository's .leiser/config.toml names a ${PROVIDER_NAMES[error.provider]} subscription "${error.name}" that is not registered.`;
    case "subscription_name_taken":
      return `A ${PROVIDER_NAMES[error.provider]} subscription is already named "${error.name}".`;
    case "empty_subscription_name":
      return "A subscription needs a name.";
    case "empty_api_key":
      return "Enter the API key.";
    case "wrong_provider":
      return `That subscription is not a ${PROVIDER_NAMES[error.provider]} subscription.`;
    case "not_a_login_subscription":
      return "This subscription signs in with an API key, not by login.";
    case "no_login_running":
      return "The login has already ended. Start it again with Log in.";
    case "subscription_setup":
      return `Could not set up the subscription: ${error.reason}`;
    case "keychain":
      return `The OS keychain failed: ${error.reason}`;
    case "repository_config":
      return `The repository's config is invalid: ${error.reason}`;
  }
}

async function execute(command: Command): Promise<Response> {
  try {
    return await invoke<Response>("execute", { command });
  } catch (error) {
    // The app's Rust side rejects with the service's `CommandError`; any
    // other rejection (such as a broken bridge) arrives as a string.
    if (typeof error === "object" && error !== null && "type" in error) {
      throw new ServiceError(error as CommandError);
    }
    throw new Error(String(error));
  }
}

function unexpected(response: Response): never {
  throw new Error(`Unexpected answer from the harness service: ${response.type}`);
}

export async function addRepository(path: string): Promise<RepositoryId> {
  const response = await execute({ type: "add_repository", path });
  return response.type === "repository_added" ? response.repository : unexpected(response);
}

/** Starts a chat; without a checkout, in a new worktree. */
export async function startChat(repository: RepositoryId, checkout?: Checkout): Promise<ChatId> {
  const response = await execute({ type: "start_chat", repository, checkout });
  return response.type === "chat_started" ? response.chat : unexpected(response);
}

export async function markChatDone(chat: ChatId): Promise<void> {
  const response = await execute({ type: "mark_chat_done", chat });
  if (response.type !== "chat_marked_done") unexpected(response);
}

export async function sendMessage(chat: ChatId, text: string): Promise<void> {
  const response = await execute({ type: "send_message", chat, text });
  if (response.type !== "message_sent") unexpected(response);
}

/** Queues the message for when the running turn ends. */
export async function queueMessage(chat: ChatId, text: string): Promise<QueueReason | null> {
  const response = await execute({ type: "queue_message", chat, text });
  if (response.type === "message_queued") return response.reason;
  // Without a running turn the service sends the message at once.
  return response.type === "message_sent" ? null : unexpected(response);
}

export async function interruptTurn(chat: ChatId): Promise<void> {
  const response = await execute({ type: "interrupt_turn", chat });
  if (response.type !== "interrupt_requested") unexpected(response);
}

/** Changes the chat's permission mode; it applies from the next turn. */
export async function changePermissionMode(chat: ChatId, mode: PermissionMode): Promise<void> {
  const response = await execute({ type: "change_permission_mode", chat, permission_mode: mode });
  if (response.type !== "permission_mode_changed") unexpected(response);
}

/** Answers an approval request or a question of the running turn. */
export async function answerRequest(chat: ChatId, request: string, answer: Answer): Promise<void> {
  const response = await execute({ type: "answer_request", chat, request, answer });
  if (response.type !== "request_answered") unexpected(response);
}

export async function listRepositories(): Promise<RepositoryOverview[]> {
  const response = await execute({ type: "list_repositories" });
  return response.type === "repositories" ? response.repositories : unexpected(response);
}

export async function listChats(): Promise<ChatOverview[]> {
  const response = await execute({ type: "list_chats" });
  return response.type === "chats" ? response.chats : unexpected(response);
}

export async function readChat(chat: ChatId): Promise<ChatView> {
  const response = await execute({ type: "read_chat", chat });
  return response.type === "chat"
    ? {
        status: response.status,
        branch: response.branch,
        permissionMode: response.permission_mode,
        appliedPermissionMode: response.applied_permission_mode,
        openRequests: response.open_requests,
        agentCli: response.agent_cli,
        subscription: response.subscription,
        usageLimit: response.usage_limit,
        transcript: response.transcript,
      }
    : unexpected(response);
}

export async function listSubscriptions(): Promise<SubscriptionOverview[]> {
  const response = await execute({ type: "list_subscriptions" });
  return response.type === "subscriptions" ? response.subscriptions : unexpected(response);
}

/** Registers a subscription; a login subscription's login starts at once. */
export async function addSubscription(
  provider: Provider,
  name: string,
  signIn: SignIn,
): Promise<SubscriptionId> {
  const response = await execute({ type: "add_subscription", provider, name, sign_in: signIn });
  return response.type === "subscription_added" ? response.subscription : unexpected(response);
}

export async function renameSubscription(subscription: SubscriptionId, name: string): Promise<void> {
  const response = await execute({ type: "rename_subscription", subscription, name });
  if (response.type !== "subscription_renamed") unexpected(response);
}

export async function removeSubscription(subscription: SubscriptionId): Promise<void> {
  const response = await execute({ type: "remove_subscription", subscription });
  if (response.type !== "subscription_removed") unexpected(response);
}

export async function logIn(subscription: SubscriptionId): Promise<void> {
  const response = await execute({ type: "log_in", subscription });
  if (response.type !== "login_started") unexpected(response);
}

/** Types a line, such as the code from the browser, into a running login. */
export async function answerLogin(subscription: SubscriptionId, answer: string): Promise<void> {
  const response = await execute({ type: "answer_login", subscription, answer });
  if (response.type !== "login_answered") unexpected(response);
}

export async function changeSubscription(
  chat: ChatId,
  provider: Provider,
  subscription: SubscriptionId | null,
): Promise<void> {
  const response = await execute({ type: "change_subscription", chat, provider, subscription });
  if (response.type !== "subscription_changed") unexpected(response);
}

export async function setInterchangeable(
  subscription: SubscriptionId,
  interchangeable: boolean,
): Promise<void> {
  const response = await execute({ type: "set_interchangeable", subscription, interchangeable });
  if (response.type !== "interchangeable_set") unexpected(response);
}

/** After a usage limit: continues the chat on another subscription. */
export async function continueOnSubscription(
  chat: ChatId,
  subscription: SubscriptionId | null,
): Promise<void> {
  const response = await execute({ type: "continue_on_subscription", chat, subscription });
  if (response.type !== "continued_on_subscription") unexpected(response);
}

/** Runs repo-check on the repository again; it only reads. */
export async function runRepoCheck(repository: RepositoryId): Promise<void> {
  const response = await execute({ type: "run_repo_check", repository });
  if (response.type !== "repo_check_started") unexpected(response);
}

/** The repository's last repo-check, without the checks it switches off. */
export async function readRepoCheck(repository: RepositoryId): Promise<RepoCheck> {
  const response = await execute({ type: "read_repo_check", repository });
  return response.type === "repo_check" ? response.check : unexpected(response);
}

/** Whether an update of the harness service waits for running turns. */
export async function serviceUpdateWaiting(): Promise<boolean> {
  const response = await execute({ type: "read_service_status" });
  return response.type === "service_status" ? response.update_waiting : unexpected(response);
}

/** The app's connection to the harness service right now. */
export function serviceConnection(): Promise<ServiceConnection> {
  return invoke<ServiceConnection>("service_connection");
}

export async function listTodos(chat: ChatId): Promise<Todo[]> {
  const response = await execute({ type: "list_todos", chat });
  return response.type === "todos" ? response.todos : unexpected(response);
}

export async function addTodo(chat: ChatId, text: string): Promise<TodoId> {
  const response = await execute({ type: "add_todo", chat, text });
  return response.type === "todo_added" ? response.todo : unexpected(response);
}

export async function setTodoDone(chat: ChatId, todo: TodoId, done: boolean): Promise<void> {
  const response = await execute({ type: "set_todo_done", chat, todo, done });
  if (response.type !== "todo_updated") unexpected(response);
}

export async function removeTodo(chat: ChatId, todo: TodoId): Promise<void> {
  const response = await execute({ type: "remove_todo", chat, todo });
  if (response.type !== "todo_removed") unexpected(response);
}

/** The CLI extensions each agent CLI loads from a repository. */
export interface CliExtensions {
  extensions: CliExtension[];
  unreadable: UnreadableCliExtensionFile[];
}

export async function listCliExtensions(repository: RepositoryId): Promise<CliExtensions> {
  const response = await execute({ type: "list_cli_extensions", repository });
  return response.type === "cli_extensions"
    ? { extensions: response.extensions, unreadable: response.unreadable }
    : unexpected(response);
}

export async function readCliExtensionFile(repository: RepositoryId, file: string): Promise<string> {
  const response = await execute({ type: "read_cli_extension_file", repository, file });
  return response.type === "cli_extension_file" ? response.text : unexpected(response);
}

/** Saves `text` if the file still holds `previous`, the text it was opened with. */
export async function saveCliExtensionFile(
  repository: RepositoryId,
  file: string,
  text: string,
  previous: string,
): Promise<void> {
  const response = await execute({ type: "save_cli_extension_file", repository, file, text, previous });
  if (response.type !== "cli_extension_file_saved") unexpected(response);
}

/** Asks the developer for a folder; `null` when they cancel. */
export function pickFolder(): Promise<string | null> {
  return invoke<string | null>("pick_folder");
}

/**
 * Calls `onEvent` for every service event, `onLost` when some were lost,
 * and `onConnection` when the connection to the service changes, such as
 * while the service restarts after an update.
 */
export async function subscribe(
  onEvent: (event: ServiceEvent) => void,
  onLost: () => void,
  onConnection: (connection: ServiceConnection) => void,
): Promise<UnlistenFn> {
  const events = await listen<ServiceEvent>("service-event", ({ payload }) => onEvent(payload));
  const lost = await listen("service-events-lost", onLost);
  const connection = await listen<ServiceConnection>("service-connection", ({ payload }) =>
    onConnection(payload),
  );
  return () => {
    events();
    lost();
    connection();
  };
}
