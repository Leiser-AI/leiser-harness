// The harness service's local interface, as it arrives over Tauri.
// Mirrors `crates/leiser-service/src/protocol.rs`; keep both in step.

export type RepositoryId = number;
export type ChatId = number;
export type SubscriptionId = number;

export type Provider = "anthropic" | "openai";
export type SubscriptionKind = "login" | "api_key";
export type SignIn = { type: "login" } | { type: "api_key"; api_key: string };
export type LoginOutcome = { type: "signed_in" } | { type: "failed"; reason: string };

export type AgentCli = "claude_code" | "codex";
export type Checkout = "main";
export type ChatStatus = "idle" | "working";
export type QueueReason = "requested" | "steer_unsupported";

export type Command =
  | { type: "add_repository"; path: string }
  | { type: "start_chat"; repository: RepositoryId; checkout: Checkout }
  | { type: "send_message"; chat: ChatId; text: string }
  | { type: "change_agent_cli"; chat: ChatId; agent_cli: AgentCli }
  | { type: "queue_message"; chat: ChatId; text: string }
  | { type: "steer_message"; chat: ChatId; text: string }
  | { type: "interrupt_turn"; chat: ChatId }
  | { type: "list_repositories" }
  | { type: "list_chats" }
  | { type: "read_chat"; chat: ChatId }
  | { type: "add_subscription"; provider: Provider; name: string; sign_in: SignIn }
  | { type: "rename_subscription"; subscription: SubscriptionId; name: string }
  | { type: "remove_subscription"; subscription: SubscriptionId }
  | { type: "log_in"; subscription: SubscriptionId }
  | { type: "list_subscriptions" }
  | {
      type: "change_subscription";
      chat: ChatId;
      provider: Provider;
      subscription: SubscriptionId | null;
    };

export type Response =
  | { type: "repository_added"; repository: RepositoryId }
  | { type: "chat_started"; chat: ChatId }
  | { type: "message_sent" }
  | { type: "message_queued"; reason: QueueReason }
  | { type: "message_steered" }
  | { type: "interrupt_requested" }
  | { type: "agent_cli_changed" }
  | { type: "repositories"; repositories: RepositoryOverview[] }
  | { type: "chats"; chats: ChatOverview[] }
  | {
      type: "chat";
      status: ChatStatus;
      branch: Branch;
      agent_cli: AgentCli;
      subscription: SubscriptionId | null;
      transcript: TranscriptEntry[];
    }
  | { type: "subscription_added"; subscription: SubscriptionId }
  | { type: "subscription_renamed" }
  | { type: "subscription_removed" }
  | { type: "login_started" }
  | { type: "subscriptions"; subscriptions: SubscriptionOverview[] }
  | { type: "subscription_changed" };

export type CommandError =
  | { type: "not_a_git_repository"; path: string }
  | { type: "unknown_repository"; repository: RepositoryId }
  | { type: "unknown_chat"; chat: ChatId }
  | { type: "turn_running"; chat: ChatId }
  | { type: "no_turn_running"; chat: ChatId }
  | { type: "agent_cli_not_configured"; agent_cli: AgentCli }
  | { type: "agent_cli"; reason: string }
  | { type: "storage"; reason: string }
  | { type: "unknown_subscription"; subscription: SubscriptionId }
  | { type: "unknown_subscription_name"; provider: Provider; name: string }
  | { type: "subscription_name_taken"; provider: Provider; name: string }
  | { type: "empty_subscription_name" }
  | { type: "empty_api_key" }
  | { type: "wrong_provider"; subscription: SubscriptionId; provider: Provider }
  | { type: "not_a_login_subscription"; subscription: SubscriptionId }
  | { type: "subscription_setup"; reason: string }
  | { type: "keychain"; reason: string }
  | { type: "repository_config"; reason: string }
  | { type: "service_stopped" };

export interface RepositoryOverview {
  repository: RepositoryId;
  name: string;
  path: string;
}

export interface SubscriptionOverview {
  subscription: SubscriptionId;
  provider: Provider;
  name: string;
  kind: SubscriptionKind;
  home: string;
  logging_in: boolean;
}

export interface ChatOverview {
  chat: ChatId;
  repository: RepositoryId;
  title: string | null;
  status: ChatStatus;
}

export type Branch =
  | { type: "named"; name: string }
  | { type: "detached"; commit: string }
  | { type: "unknown" };

export type TranscriptEntry =
  | { type: "user_message"; text: string }
  | { type: "agent"; agent_cli: AgentCli; event: AgentEvent };

export type AgentEvent =
  | { type: "message"; text: string }
  | { type: "tool_call"; name: string; detail: string }
  | { type: "tool_result"; name: string; output: string }
  | { type: "command_finished"; command: string; exit_code: number | null; output: string }
  | { type: "approval_requested"; request: string; description: string }
  | { type: "usage_limit_reached"; limit: string; resets_in_seconds: number | null }
  | { type: "turn_ended" }
  | { type: "turn_interrupted" }
  | { type: "session_ended" };

export type ServiceEvent =
  | { type: "agent"; chat: ChatId; event: AgentEvent }
  | { type: "message_queued"; chat: ChatId; text: string; reason: QueueReason }
  | { type: "message_steered"; chat: ChatId; text: string }
  | { type: "queued_message_sent"; chat: ChatId; text: string }
  | { type: "login_output"; subscription: SubscriptionId; text: string }
  | { type: "login_finished"; subscription: SubscriptionId; outcome: LoginOutcome };
