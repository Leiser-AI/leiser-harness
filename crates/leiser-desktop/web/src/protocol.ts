// The harness service's local interface, as it arrives over Tauri.
// Mirrors `crates/leiser-service/src/protocol.rs`; keep both in step.

export type RepositoryId = number;
export type ChatId = number;

export type AgentCli = "claude_code" | "codex";
export type Checkout = "main";
export type ChatStatus = "idle" | "working" | "waiting";
export type PermissionMode = "supervised" | "auto_accept_edits" | "auto" | "full_access";
export type QueueReason = "requested" | "steer_unsupported";

export type Command =
  | { type: "add_repository"; path: string }
  | { type: "start_chat"; repository: RepositoryId; checkout: Checkout }
  | { type: "send_message"; chat: ChatId; text: string }
  | { type: "change_agent_cli"; chat: ChatId; agent_cli: AgentCli }
  | { type: "queue_message"; chat: ChatId; text: string }
  | { type: "steer_message"; chat: ChatId; text: string }
  | { type: "interrupt_turn"; chat: ChatId }
  | { type: "change_permission_mode"; chat: ChatId; permission_mode: PermissionMode }
  | { type: "answer_request"; chat: ChatId; request: string; answer: Answer }
  | { type: "list_repositories" }
  | { type: "list_chats" }
  | { type: "read_chat"; chat: ChatId };

export type Response =
  | { type: "repository_added"; repository: RepositoryId }
  | { type: "chat_started"; chat: ChatId }
  | { type: "message_sent" }
  | { type: "message_queued"; reason: QueueReason }
  | { type: "message_steered" }
  | { type: "interrupt_requested" }
  | { type: "agent_cli_changed" }
  | { type: "permission_mode_changed" }
  | { type: "request_answered" }
  | { type: "repositories"; repositories: RepositoryOverview[] }
  | { type: "chats"; chats: ChatOverview[] }
  | {
      type: "chat";
      status: ChatStatus;
      branch: Branch;
      permission_mode: PermissionMode;
      applied_permission_mode: PermissionMode;
      open_requests: string[];
      transcript: TranscriptEntry[];
    };

export type CommandError =
  | { type: "not_a_git_repository"; path: string }
  | { type: "unknown_repository"; repository: RepositoryId }
  | { type: "unknown_chat"; chat: ChatId }
  | { type: "turn_running"; chat: ChatId }
  | { type: "no_turn_running"; chat: ChatId }
  | { type: "unknown_request"; chat: ChatId; request: string }
  | { type: "answer_does_not_fit"; chat: ChatId; request: string }
  | { type: "repository_config"; reason: string }
  | { type: "agent_cli_not_configured"; agent_cli: AgentCli }
  | { type: "agent_cli"; reason: string }
  | { type: "storage"; reason: string }
  | { type: "service_stopped" };

export interface RepositoryOverview {
  repository: RepositoryId;
  name: string;
  path: string;
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
  | { type: "agent"; agent_cli: AgentCli; event: AgentEvent }
  | { type: "answer"; request: string; answer: Answer };

export type Answer =
  | { type: "approve" }
  | { type: "decline"; feedback: string | null }
  | { type: "answers"; answers: Record<string, string[]> };

export interface Question {
  id: string;
  header: string;
  question: string;
  options: QuestionOption[];
  multi_select: boolean;
}

export interface QuestionOption {
  label: string;
  description: string;
}

export type AgentEvent =
  | { type: "message"; text: string }
  | { type: "tool_call"; name: string; detail: string }
  | { type: "tool_result"; name: string; output: string }
  | { type: "command_finished"; command: string; exit_code: number | null; output: string }
  | { type: "approval_requested"; request: string; description: string }
  | { type: "question_asked"; request: string; questions: Question[] }
  | { type: "usage_limit_reached"; limit: string; resets_in_seconds: number | null }
  | { type: "turn_ended" }
  | { type: "turn_interrupted" }
  | { type: "session_ended" };

export type ServiceEvent =
  | { type: "agent"; chat: ChatId; event: AgentEvent }
  | { type: "message_queued"; chat: ChatId; text: string; reason: QueueReason }
  | { type: "message_steered"; chat: ChatId; text: string }
  | { type: "queued_message_sent"; chat: ChatId; text: string }
  | { type: "request_answered"; chat: ChatId; request: string; answer: Answer };
