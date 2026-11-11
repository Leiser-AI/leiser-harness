// The harness service's local interface, as it arrives over Tauri.
// Mirrors `crates/leiser-service/src/protocol.rs`; keep both in step.

export type RepositoryId = number;
export type ChatId = number;
export type TodoId = number;

export type AgentCli = "claude_code" | "codex";
export type Checkout =
  | { type: "main" }
  | { type: "new_worktree" }
  | { type: "worktree"; chat: ChatId };
export type ChatStatus = "working" | "waiting_for_you" | "done";
export type QueueReason = "requested" | "steer_unsupported" | "setting_up";
export type RepositoryScript = "setup" | "done";

export type Command =
  | { type: "add_repository"; path: string }
  | { type: "start_chat"; repository: RepositoryId; checkout?: Checkout }
  | { type: "send_message"; chat: ChatId; text: string }
  | { type: "change_agent_cli"; chat: ChatId; agent_cli: AgentCli }
  | { type: "queue_message"; chat: ChatId; text: string }
  | { type: "steer_message"; chat: ChatId; text: string }
  | { type: "interrupt_turn"; chat: ChatId }
  | { type: "mark_chat_done"; chat: ChatId }
  | { type: "list_repositories" }
  | { type: "list_chats" }
  | { type: "read_chat"; chat: ChatId }
  | { type: "list_todos"; chat: ChatId }
  | { type: "add_todo"; chat: ChatId; text: string }
  | { type: "set_todo_done"; chat: ChatId; todo: TodoId; done: boolean }
  | { type: "remove_todo"; chat: ChatId; todo: TodoId };

export type Response =
  | { type: "repository_added"; repository: RepositoryId }
  | { type: "chat_started"; chat: ChatId }
  | { type: "message_sent" }
  | { type: "message_queued"; reason: QueueReason }
  | { type: "message_steered" }
  | { type: "interrupt_requested" }
  | { type: "agent_cli_changed" }
  | { type: "chat_marked_done" }
  | { type: "repositories"; repositories: RepositoryOverview[] }
  | { type: "chats"; chats: ChatOverview[] }
  | { type: "chat"; status: ChatStatus; branch: Branch; transcript: TranscriptEntry[] }
  | { type: "todos"; todos: Todo[] }
  | { type: "todo_added"; todo: TodoId }
  | { type: "todo_updated" }
  | { type: "todo_removed" };

export type CommandError =
  | { type: "not_a_git_repository"; path: string }
  | { type: "unknown_repository"; repository: RepositoryId }
  | { type: "unknown_chat"; chat: ChatId }
  | { type: "chat_in_another_repository"; chat: ChatId }
  | { type: "not_in_a_worktree"; chat: ChatId }
  | { type: "setting_up"; chat: ChatId }
  | { type: "worktree"; reason: string }
  | { type: "repository_config"; reason: string }
  | { type: "turn_running"; chat: ChatId }
  | { type: "no_turn_running"; chat: ChatId }
  | { type: "agent_cli_not_configured"; agent_cli: AgentCli }
  | { type: "agent_cli"; reason: string }
  | { type: "empty_todo" }
  | { type: "unknown_todo"; chat: ChatId; todo: TodoId }
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
  /** The worktree the chat works in; `null` in the main checkout. */
  worktree: string | null;
}

export interface Todo {
  id: TodoId;
  text: string;
  done: boolean;
}

export type Branch =
  | { type: "named"; name: string }
  | { type: "detached"; commit: string }
  | { type: "unknown" };

export type TranscriptEntry =
  | { type: "user_message"; text: string }
  | { type: "agent"; agent_cli: AgentCli; event: AgentEvent }
  | { type: "script_finished"; script: RepositoryScript; exit_code: number | null; output: string };

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
  | { type: "script_started"; chat: ChatId; script: RepositoryScript }
  | {
      type: "script_finished";
      chat: ChatId;
      script: RepositoryScript;
      exit_code: number | null;
      output: string;
    }
  | { type: "status_changed"; chat: ChatId; status: ChatStatus }
  | { type: "todos_changed"; chat: ChatId; todos: Todo[] };
