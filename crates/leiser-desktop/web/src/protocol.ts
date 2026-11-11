// The harness service's local interface, as it arrives over Tauri.
// Mirrors `crates/leiser-service/src/protocol.rs`; keep both in step.

export type RepositoryId = number;
export type ChatId = number;
export type SubscriptionId = number;
export type HostAccountId = number;

export type Provider = "anthropic" | "openai";
export type SubscriptionKind = "login" | "api_key";
export type SignIn = { type: "login" } | { type: "api_key"; api_key: string };
export type LoginOutcome = { type: "signed_in" } | { type: "failed"; reason: string };
export type TodoId = number;

export type AgentCli = "claude_code" | "codex";
export type Checkout =
  | { type: "main" }
  | { type: "new_worktree" }
  | { type: "worktree"; chat: ChatId };
export type ChatStatus = "working" | "waiting_for_you" | "done";
export type PermissionMode = "supervised" | "auto_accept_edits" | "auto" | "full_access";
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
  | { type: "change_permission_mode"; chat: ChatId; permission_mode: PermissionMode }
  | { type: "answer_request"; chat: ChatId; request: string; answer: Answer }
  | { type: "list_repositories" }
  | { type: "list_chats" }
  | { type: "read_chat"; chat: ChatId }
  | { type: "add_subscription"; provider: Provider; name: string; sign_in: SignIn }
  | { type: "rename_subscription"; subscription: SubscriptionId; name: string }
  | { type: "remove_subscription"; subscription: SubscriptionId }
  | { type: "log_in"; subscription: SubscriptionId }
  | { type: "answer_login"; subscription: SubscriptionId; answer: string }
  | { type: "list_subscriptions" }
  | { type: "set_interchangeable"; subscription: SubscriptionId; interchangeable: boolean }
  | { type: "continue_on_subscription"; chat: ChatId; subscription: SubscriptionId | null }
  | {
      type: "change_subscription";
      chat: ChatId;
      provider: Provider;
      subscription: SubscriptionId | null;
    }
  | {
      type: "add_host_account";
      name: string;
      git_host: string;
      login: string;
      author: CommitAuthor;
    }
  | { type: "remove_host_account"; host_account: HostAccountId }
  | { type: "log_in_host_account"; host_account: HostAccountId }
  | { type: "list_host_accounts" }
  | { type: "push"; chat: ChatId }
  | { type: "run_repo_check"; repository: RepositoryId }
  | { type: "read_repo_check"; repository: RepositoryId }
  | { type: "read_service_status" }
  | { type: "list_todos"; chat: ChatId }
  | { type: "add_todo"; chat: ChatId; text: string }
  | { type: "set_todo_done"; chat: ChatId; todo: TodoId; done: boolean }
  | { type: "remove_todo"; chat: ChatId; todo: TodoId }
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
      agent_cli: AgentCli;
      subscription: SubscriptionId | null;
      host_account: HostAccountBinding;
      commit_author: CommitAuthor | null;
      usage_limit: UsageLimit | null;
      transcript: TranscriptEntry[];
    }
  | { type: "subscription_added"; subscription: SubscriptionId }
  | { type: "subscription_renamed" }
  | { type: "subscription_removed" }
  | { type: "login_started" }
  | { type: "login_answered" }
  | { type: "subscriptions"; subscriptions: SubscriptionOverview[] }
  | { type: "subscription_changed" }
  | { type: "host_account_added"; host_account: HostAccountId }
  | { type: "host_account_removed" }
  | { type: "host_account_login_started" }
  | { type: "host_accounts"; host_accounts: HostAccountOverview[] }
  | { type: "pushed"; branch: string }
  | { type: "interchangeable_set" }
  | { type: "continued_on_subscription" }
  | { type: "repo_check_started" }
  | { type: "repo_check"; check: RepoCheck }
  | { type: "service_status"; version: string; update_waiting: boolean }
  | { type: "todos"; todos: Todo[] }
  | { type: "todo_added"; todo: TodoId }
  | { type: "todo_updated" }
  | { type: "todo_removed" }
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
  | { type: "unknown_request"; chat: ChatId; request: string }
  | { type: "answer_does_not_fit"; chat: ChatId; request: string }
  | { type: "agent_cli_not_configured"; agent_cli: AgentCli }
  | { type: "agent_cli"; reason: string }
  | { type: "empty_todo" }
  | { type: "unknown_todo"; chat: ChatId; todo: TodoId }
  | { type: "storage"; reason: string }
  | { type: "unknown_subscription"; subscription: SubscriptionId }
  | { type: "unknown_subscription_name"; provider: Provider; name: string }
  | { type: "subscription_name_taken"; provider: Provider; name: string }
  | { type: "empty_subscription_name" }
  | { type: "empty_api_key" }
  | { type: "wrong_provider"; subscription: SubscriptionId; provider: Provider }
  | { type: "not_a_login_subscription"; subscription: SubscriptionId }
  | { type: "subscription_setup"; reason: string }
  | { type: "no_login_running"; subscription: SubscriptionId }
  | { type: "keychain"; reason: string }
  | { type: "repository_config"; reason: string }
  | { type: "unknown_host_account"; host_account: HostAccountId }
  | { type: "unknown_host_account_name"; name: string }
  | { type: "host_account_name_taken"; name: string }
  | { type: "empty_host_account_field"; field: string }
  | { type: "host_account_setup"; reason: string }
  | { type: "no_host_account"; repository: RepositoryId }
  | { type: "host_account_mismatch"; name: string; git_host: string; expected: string; actual: string }
  | { type: "host_account_unverified"; name: string; reason: string }
  | { type: "no_branch"; chat: ChatId }
  | { type: "push_failed"; reason: string }
  | { type: "repo_check_not_configured" }
  | { type: "repo_check_running"; repository: RepositoryId }
  | { type: "not_a_cli_extension_file"; file: string }
  | { type: "cli_extension_file_changed"; file: string }
  | { type: "invalid_cli_extension_file"; file: string; reason: string }
  | { type: "cli_extension_file_access"; file: string; reason: string }
  | { type: "no_usage_limit"; chat: ChatId }
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

export interface CommitAuthor {
  name: string;
  email: string;
}

export interface HostAccountOverview {
  host_account: HostAccountId;
  name: string;
  git_host: string;
  login: string;
  author: CommitAuthor;
  config_dir: string;
  logging_in: boolean;
}

/** The host account a repository's `.leiser/config.toml` binds it to. */
export type HostAccountBinding =
  | { type: "unbound" }
  | { type: "bound"; host_account: HostAccountId; name: string; git_host: string; login: string }
  | { type: "unknown"; name: string }
  | { type: "unreadable"; reason: string };

export interface SubscriptionOverview {
  subscription: SubscriptionId;
  provider: Provider;
  name: string;
  kind: SubscriptionKind;
  home: string;
  logging_in: boolean;
  /** Chats may move to and from it by themselves at a usage limit. */
  interchangeable: boolean;
}

/** The usage limit a chat's last turn stopped at. */
export interface UsageLimit {
  limit: string;
  /** `null`: the agent CLI's own default login. */
  subscription: SubscriptionId | null;
  resets_in_seconds: number | null;
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
  | { type: "script_finished"; script: RepositoryScript; exit_code: number | null; output: string }
  | { type: "answer"; request: string; answer: Answer }
  | {
      type: "continued_on_subscription";
      provider: Provider;
      subscription: SubscriptionId | null;
      automatic: boolean;
    };

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
  | {
      type: "continued_on_subscription";
      chat: ChatId;
      provider: Provider;
      subscription: SubscriptionId | null;
      automatic: boolean;
    }
  | { type: "request_answered"; chat: ChatId; request: string; answer: Answer }
  | { type: "login_output"; subscription: SubscriptionId; text: string }
  | { type: "login_finished"; subscription: SubscriptionId; outcome: LoginOutcome }
  | { type: "host_account_login_output"; host_account: HostAccountId; text: string }
  | { type: "host_account_login_finished"; host_account: HostAccountId; outcome: LoginOutcome }
  | { type: "repo_check_started"; repository: RepositoryId }
  | { type: "repo_check_finished"; repository: RepositoryId }
  | { type: "todos_changed"; chat: ChatId; todos: Todo[] }
  | { type: "service_update_waiting" };

/** The app's connection to the harness service (not part of the service's protocol). */
export type ServiceConnection =
  | { state: "connecting" }
  | { state: "connected"; service_version: string }
  | { state: "unavailable"; reason: string };
