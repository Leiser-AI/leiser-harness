// The harness service's local interface, as it arrives over Tauri.
// Mirrors `crates/leiser-service/src/protocol.rs`; keep both in step.

export type RepositoryId = number;
export type ChatId = number;

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
  | { type: "run_repo_check"; repository: RepositoryId }
  | { type: "read_repo_check"; repository: RepositoryId }
  | { type: "list_cli_extensions"; repository: RepositoryId }
  | { type: "read_cli_extension_file"; repository: RepositoryId; file: string }
  | {
      type: "save_cli_extension_file";
      repository: RepositoryId;
      file: string;
      text: string;
      previous: string;
    };

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
  | { type: "repo_check_started" }
  | { type: "repo_check"; check: RepoCheck }
  | { type: "cli_extensions"; extensions: CliExtension[]; unreadable: UnreadableCliExtensionFile[] }
  | { type: "cli_extension_file"; text: string }
  | { type: "cli_extension_file_saved" };

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
  | { type: "storage"; reason: string }
  | { type: "repo_check_not_configured" }
  | { type: "repo_check_running"; repository: RepositoryId }
  | { type: "not_a_cli_extension_file"; file: string }
  | { type: "cli_extension_file_changed"; file: string }
  | { type: "invalid_cli_extension_file"; file: string; reason: string }
  | { type: "cli_extension_file_access"; file: string; reason: string }
  | { type: "service_stopped" };

/** A rule of `repo-check`, such as `D3`. */
export type CheckId = string;

export type RepoCheck =
  | { state: "not_run" }
  | { state: "running" }
  | { state: "done"; report: RepoCheckReport; opt_outs: OptOuts }
  | { state: "unreadable"; reason: string; output: string }
  | { state: "failed"; reason: string };

export interface RepoCheckReport {
  commit: string | null;
  summary: string;
  must: Finding[];
  should: Finding[];
}

export interface Finding {
  title: string;
  checks: CheckId[];
  details: string;
}

export type OptOuts =
  | { type: "applied"; checks: SwitchedOffCheck[] }
  | { type: "unreadable"; reason: string };

export interface SwitchedOffCheck {
  check: CheckId;
  reason: string;
}

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

/** A skill, MCP server, hook or instruction file an agent CLI loads. */
export interface CliExtension {
  agent_cli: AgentCli;
  kind: CliExtensionKind;
  name: string;
  /** Where the agent CLI loads it from, relative to the repository. */
  path: string;
  /** The file that holds it, symlinks resolved; read and save by this. */
  file: string;
}

export type CliExtensionKind =
  | { type: "instruction_file"; imported_by: string | null }
  | { type: "skill"; description: string | null }
  | { type: "mcp_server"; launch: string | null }
  | { type: "hook"; matcher: string | null; commands: string[] };

export interface UnreadableCliExtensionFile {
  agent_cli: AgentCli;
  path: string;
  file: string;
  reason: string;
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
  | { type: "turn_failed"; reason: string }
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
  | { type: "repo_check_started"; repository: RepositoryId }
  | { type: "repo_check_finished"; repository: RepositoryId };
