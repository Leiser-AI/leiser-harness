// Talking to the harness service through the app's Rust side.

import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

import type {
  AgentCli,
  Branch,
  ChatId,
  ChatOverview,
  ChatStatus,
  Command,
  CommandError,
  Provider,
  QueueReason,
  RepositoryId,
  RepositoryOverview,
  Response,
  ServiceEvent,
  SignIn,
  SubscriptionId,
  SubscriptionOverview,
  TranscriptEntry,
} from "./protocol";

/** A chat as the service reports it. */
export interface ChatView {
  status: ChatStatus;
  branch: Branch;
  agentCli: AgentCli;
  /** `null`: the agent CLI's own default login. */
  subscription: SubscriptionId | null;
  transcript: TranscriptEntry[];
}

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
    case "service_stopped":
      return "The harness service has stopped.";
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
    ? {
        status: response.status,
        branch: response.branch,
        agentCli: response.agent_cli,
        subscription: response.subscription,
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
