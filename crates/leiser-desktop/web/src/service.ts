// Talking to the harness service through the app's Rust side.

import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

import type {
  ChatId,
  ChatView,
  Command,
  CommandError,
  RepositoryId,
  RepositorySummary,
  Response,
  ServiceEvent,
} from "./protocol";

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
    case "agent_cli":
      return `The agent CLI failed: ${error.reason}`;
    case "service_stopped":
      return "The harness service has stopped.";
  }
}

async function execute(command: Command): Promise<Response> {
  try {
    return await invoke<Response>("execute", { command });
  } catch (error) {
    throw new ServiceError(error as CommandError);
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

export async function listRepositories(): Promise<RepositorySummary[]> {
  const response = await execute({ type: "list_repositories" });
  return response.type === "repositories" ? response.repositories : unexpected(response);
}

export async function getChat(chat: ChatId): Promise<ChatView> {
  const response = await execute({ type: "get_chat", chat });
  return response.type === "chat" ? response.chat : unexpected(response);
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
