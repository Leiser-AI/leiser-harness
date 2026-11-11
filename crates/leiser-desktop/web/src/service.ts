// Talking to the harness service through the app's Rust side.

import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

import type {
  AgentCli,
  Branch,
  ChatId,
  Checkout,
  ChatOverview,
  ChatStatus,
  Command,
  CommandError,
  QueueReason,
  RepositoryId,
  RepositoryOverview,
  Response,
  ServiceEvent,
  Todo,
  TodoId,
  TranscriptEntry,
} from "./protocol";

/** A chat as the service reports it. */
export interface ChatView {
  status: ChatStatus;
  branch: Branch;
  transcript: TranscriptEntry[];
}

export const AGENT_CLI_NAMES: Record<AgentCli, string> = {
  claude_code: "Claude Code",
  codex: "Codex",
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
    case "service_stopped":
      return "The harness service has stopped.";
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
    ? { status: response.status, branch: response.branch, transcript: response.transcript }
    : unexpected(response);
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

/** Asks the developer for a folder; `null` when they cancel. */
export function pickFolder(): Promise<string | null> {
  return invoke<string | null>("pick_folder");
}

/** Calls `onEvent` for every service event, and `onLost` when some were lost. */
export async function subscribe(
  onEvent: (event: ServiceEvent) => void,
  onLost: () => void,
): Promise<UnlistenFn> {
  const events = await listen<ServiceEvent>("service-event", ({ payload }) => onEvent(payload));
  const lost = await listen("service-events-lost", onLost);
  return () => {
    events();
    lost();
  };
}
