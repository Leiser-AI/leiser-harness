// Talking to the harness service through the app's Rust side.

import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

import type {
  AgentCli,
  Branch,
  ChatId,
  ChatOverview,
  ChatStatus,
  CliExtension,
  Command,
  CommandError,
  QueueReason,
  RepositoryId,
  RepositoryOverview,
  Response,
  ServiceEvent,
  TranscriptEntry,
  UnreadableCliExtensionFile,
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
    case "turn_running":
      return "A turn is running; queue the message or interrupt the turn.";
    case "no_turn_running":
      return "No turn is running.";
    case "agent_cli_not_configured":
      return `${AGENT_CLI_NAMES[error.agent_cli]} is not set up in the harness service.`;
    case "agent_cli":
      return `The agent CLI failed: ${error.reason}`;
    case "storage":
      return `The harness service could not store the chat: ${error.reason}`;
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

export async function startChat(repository: RepositoryId): Promise<ChatId> {
  const response = await execute({ type: "start_chat", repository, checkout: "main" });
  return response.type === "chat_started" ? response.chat : unexpected(response);
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
