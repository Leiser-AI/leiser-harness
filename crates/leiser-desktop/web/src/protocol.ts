// The harness service's local interface, as it arrives over Tauri.
// Mirrors `crates/leiser-service/src/protocol.rs`; keep both in step.

export type RepositoryId = number;
export type ChatId = number;

export type Checkout = "main";

export type Command =
  | { type: "add_repository"; path: string }
  | { type: "start_chat"; repository: RepositoryId; checkout: Checkout }
  | { type: "send_message"; chat: ChatId; text: string }
  | { type: "list_repositories" }
  | { type: "get_chat"; chat: ChatId };

export type Response =
  | { type: "repository_added"; repository: RepositoryId }
  | { type: "chat_started"; chat: ChatId }
  | { type: "message_sent" }
  | { type: "repositories"; repositories: RepositorySummary[] }
  | { type: "chat"; chat: ChatView };

export type CommandError =
  | { type: "not_a_git_repository"; path: string }
  | { type: "unknown_repository" }
  | { type: "unknown_chat" }
  | { type: "agent_cli"; reason: string }
  | { type: "service_stopped" };

export interface RepositorySummary {
  repository: RepositoryId;
  name: string;
  path: string;
  chats: ChatSummary[];
}

export interface ChatSummary {
  chat: ChatId;
  title: string | null;
}

export interface ChatView {
  chat: ChatId;
  repository: RepositoryId;
  repository_name: string;
  branch: Branch;
  transcript: TranscriptEntry[];
}

export type Branch =
  | { type: "named"; name: string }
  | { type: "detached"; commit: string }
  | { type: "unknown" };

export type TranscriptEntry =
  | { type: "user_message"; text: string }
  | { type: "agent"; event: AgentEvent };

export type AgentEvent =
  | { type: "message"; text: string }
  | { type: "tool_call"; name: string; detail: string }
  | { type: "tool_result"; name: string; output: string }
  | { type: "approval_requested"; request: string; description: string }
  | { type: "usage_limit_reached"; limit: string; resets_in_seconds: number | null }
  | { type: "turn_ended" }
  | { type: "session_ended" };

export type ServiceEvent = { type: "agent"; chat: ChatId; event: AgentEvent };
