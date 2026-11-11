//! The local interface of the harness service. Every type here crosses the
//! boundary to the desktop app, so all of them serialize.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// A repository added to the harness.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct RepositoryId(pub(crate) u64);

/// A chat: one conversation of the developer with agents about one task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ChatId(pub(crate) u64);

/// A subscription registered in the harness.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct SubscriptionId(pub(crate) u64);

/// A host account registered in the harness.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct HostAccountId(pub(crate) u64);

/// The name and email address commits are made with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitAuthor {
    pub name: String,
    pub email: String,
}

impl std::fmt::Display for CommitAuthor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} <{}>", self.name, self.email)
    }
}

/// A company that offers models. Each agent CLI runs on one provider's
/// subscriptions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Provider {
    Anthropic,
    #[serde(rename = "openai")]
    OpenAi,
}

impl std::fmt::Display for Provider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Anthropic => "Anthropic",
            Self::OpenAi => "OpenAI",
        })
    }
}

impl Provider {
    /// The agent CLI that runs on the provider's subscriptions.
    #[must_use]
    pub fn agent_cli(self) -> AgentCli {
        match self {
            Self::Anthropic => AgentCli::ClaudeCode,
            Self::OpenAi => AgentCli::Codex,
        }
    }
}

/// An API key. It never shows in `Debug` output, so it cannot reach a log
/// by accident, and the harness service keeps it only in the OS keychain.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ApiKey(String);

impl ApiKey {
    #[must_use]
    pub fn new(key: impl Into<String>) -> Self {
        Self(key.into())
    }

    /// The key itself, to hand to the agent CLI or the keychain.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for ApiKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ApiKey(<redacted>)")
    }
}

/// What the developer types into a running login, such as the code Claude
/// Code's login asks for. Kept out of `Debug` output like an [`ApiKey`].
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct LoginAnswer(String);

impl LoginAnswer {
    #[must_use]
    pub fn new(text: impl Into<String>) -> Self {
        Self(text.into())
    }

    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for LoginAnswer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("LoginAnswer(<redacted>)")
    }
}

/// How a new subscription signs in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SignIn {
    /// A plan signed in by the agent CLI's own login, which the harness
    /// service runs in the subscription's config directory.
    Login,
    /// An API key, kept in the OS keychain.
    ApiKey { api_key: ApiKey },
}

/// How a registered subscription signs in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubscriptionKind {
    Login,
    ApiKey,
}

/// A todo of a chat's todo list. Numbered per chat from 1; a removed
/// todo's id is never used again.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct TodoId(pub(crate) u64);

impl std::fmt::Display for TodoId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// An agent CLI the harness service can run a chat on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentCli {
    ClaudeCode,
    Codex,
}

impl std::fmt::Display for AgentCli {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::ClaudeCode => "Claude Code",
            Self::Codex => "Codex",
        })
    }
}

/// How much an agent session may do without the developer's approval.
/// Each driver maps it onto its agent CLI's own mode; where an agent CLI
/// lacks a mode, the chat runs Supervised. The agent CLI's sandbox applies
/// in every mode.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum PermissionMode {
    /// Asks before commands and file changes.
    Supervised,
    /// Changes files without asking; other actions still ask.
    AutoAcceptEdits,
    /// The agent CLI's automatic review approves routine actions and asks
    /// about the others.
    #[default]
    Auto,
    /// Never asks.
    FullAccess,
}

impl std::fmt::Display for PermissionMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Supervised => "Supervised",
            Self::AutoAcceptEdits => "Auto-accept edits",
            Self::Auto => "Auto",
            Self::FullAccess => "Full access",
        })
    }
}

impl AgentCli {
    /// The provider whose subscriptions the agent CLI runs on.
    #[must_use]
    pub fn provider(self) -> Provider {
        match self {
            Self::ClaudeCode => Provider::Anthropic,
            Self::Codex => Provider::OpenAi,
        }
    }
}

/// Where a chat works.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Checkout {
    /// The repository's main checkout, on whatever branch it has.
    Main,
    /// A new git worktree on a new branch from the repository's base
    /// branch, as the remote has it now. The repository's setup script
    /// runs in it before the agent starts. The default.
    #[default]
    NewWorktree,
    /// The worktree another chat of the repository works in, on its
    /// branch: "new chat in this worktree".
    Worktree { chat: ChatId },
}

/// A script of the repository, set in its `.leiser/config.toml`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepositoryScript {
    /// Runs after a chat's new worktree is created, before the agent
    /// starts, for example `pnpm install`.
    Setup,
    /// Runs when the last chat in a worktree is done, for example
    /// `cargo clean`.
    Done,
}

/// A request to the harness service.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Command {
    /// Adds a local git repository.
    AddRepository { path: PathBuf },
    /// Starts a chat in a repository, by default in a new worktree.
    ///
    /// Answers once the chat's worktree exists; its setup script may still
    /// run then. Messages sent meanwhile wait for it
    /// ([`QueueReason::SettingUp`]).
    StartChat {
        repository: RepositoryId,
        #[serde(default)]
        checkout: Checkout,
    },
    /// Sends the developer's message to the chat's agent session, starting
    /// the agent session if the chat has none yet. Fails while a turn runs;
    /// queue or steer the message instead.
    ///
    /// The first message after [`Command::ChangeAgentCli`] goes to that
    /// agent CLI's earlier agent session, resumed, with a handoff of the
    /// chat's entries since; without one, to a new agent session with a
    /// handoff of the chat so far.
    SendMessage { chat: ChatId, text: String },
    /// Changes the agent CLI that the chat's next message goes to, and
    /// ends the chat's agent session on the agent CLI it leaves. A new chat
    /// starts on the service's default agent CLI. Fails while a turn runs.
    ChangeAgentCli { chat: ChatId, agent_cli: AgentCli },
    /// Sends the message when the running turn ends, as the next turn.
    /// Queued messages are sent in order, one turn each. Without a running
    /// turn the message is sent at once.
    QueueMessage { chat: ChatId, text: String },
    /// Delivers the message into the running turn when the agent CLI can
    /// take it mid-turn, and queues it otherwise. Without a running turn
    /// the message is sent at once.
    SteerMessage { chat: ChatId, text: String },
    /// Asks the agent CLI to stop the running turn. The turn ends with
    /// [`AgentEvent::TurnInterrupted`]; queued messages are sent after it.
    InterruptTurn { chat: ChatId },
    /// Marks the chat as done. When it is the last chat in its worktree
    /// that is not done, the repository's done script runs there. A later
    /// message takes the chat up again. Fails while a turn runs.
    MarkChatDone { chat: ChatId },
    /// Changes the chat's permission mode. A new chat starts in its
    /// repository's default. The mode applies from the chat's next turn; a
    /// running turn keeps the mode it started with.
    ChangePermissionMode {
        chat: ChatId,
        permission_mode: PermissionMode,
    },
    /// Answers an approval request or a question of the chat's running
    /// turn (see [`AgentEvent::ApprovalRequested`] and
    /// [`AgentEvent::QuestionAsked`]).
    AnswerRequest {
        chat: ChatId,
        request: String,
        answer: Answer,
    },
    /// Lists every repository added to the harness.
    ListRepositories,
    /// Lists every chat with its current status: chats waiting for the
    /// developer first, then working ones, then done ones; within each,
    /// the chat that changed last first.
    ListChats,
    /// Reads a chat's status, branch, subscription and transcript.
    ReadChat { chat: ChatId },
    /// Registers a subscription. A login subscription gets its own config
    /// directory, in which the agent CLI's login runs at once; see
    /// [`Event::LoginOutput`] and [`Event::LoginFinished`]. An API key is
    /// stored in the OS keychain. Names are unique per provider.
    AddSubscription {
        provider: Provider,
        name: String,
        sign_in: SignIn,
    },
    /// Renames a subscription.
    RenameSubscription {
        subscription: SubscriptionId,
        name: String,
    },
    /// Removes a subscription with its config directory and keychain
    /// entry, and ends the agent sessions running on it. Chats that use it
    /// fail to send until they change to another subscription.
    RemoveSubscription { subscription: SubscriptionId },
    /// Runs the agent CLI's login of a login subscription again, for
    /// example after it expired. A login still running is stopped first.
    LogIn { subscription: SubscriptionId },
    /// Types a line into the subscription's running login, such as the
    /// code Claude Code's login asks for once the developer signed in in
    /// the browser.
    AnswerLogin {
        subscription: SubscriptionId,
        answer: LoginAnswer,
    },
    /// Lists every registered subscription.
    ListSubscriptions,
    /// Marks a subscription as interchangeable with the provider's other
    /// interchangeable subscriptions, or no longer. When a chat on an
    /// interchangeable subscription hits a usage limit, the harness service
    /// continues it on another interchangeable subscription of the same
    /// provider by itself, as [`Command::ContinueOnSubscription`] would. It
    /// never switches automatically from or to any other subscription.
    SetInterchangeable {
        subscription: SubscriptionId,
        interchangeable: bool,
    },
    /// Changes the subscription that the chat's next turns on `provider`
    /// use; `None` is the agent CLI's own default login. A new chat starts
    /// on its repository's default subscription per provider, set in its
    /// `.leiser/config.toml`. Where the agent CLI can continue its agent
    /// session on the new subscription (Codex), it does; otherwise the next
    /// message goes to a new agent session with a handoff. Fails while a
    /// turn runs.
    ChangeSubscription {
        chat: ChatId,
        provider: Provider,
        subscription: Option<SubscriptionId>,
    },
    /// Registers a host account: one user account `login` on the git host
    /// `git_host` (such as `github.com`), and the commit author its
    /// repositories commit with. It gets a `gh` config directory of its
    /// own, in which [`Command::LogInHostAccount`] signs it in. Names are
    /// unique. A repository is bound to it by name, as
    /// `host_account = "<name>"` in its `.leiser/config.toml`.
    AddHostAccount {
        name: String,
        git_host: String,
        login: String,
        author: CommitAuthor,
    },
    /// Removes a host account with its `gh` config directory. Repositories
    /// bound to it cannot start chats or push until they are bound again.
    RemoveHostAccount { host_account: HostAccountId },
    /// Runs the git host's login (`gh auth login`) in the host account's
    /// config directory; see [`Event::HostAccountLoginOutput`] and
    /// [`Event::HostAccountLoginFinished`]. A login still running is
    /// stopped first.
    LogInHostAccount { host_account: HostAccountId },
    /// Lists every registered host account.
    ListHostAccounts,
    /// Pushes the chat's branch to the repository's `origin`, after the
    /// harness has checked that the push signs in as the repository's host
    /// account. Refuses a repository without a host account, and a push
    /// that would sign in as another account
    /// ([`CommandError::HostAccountMismatch`]). Answers once the push is
    /// done.
    Push { chat: ChatId },
    /// Pushes the chat's branch as [`Command::Push`] does, after the same
    /// host account check, and opens a pull request from it into the
    /// repository's base branch (its `base_branch`, else `origin`'s default
    /// branch) with the host account's credentials. A chat leads to at
    /// most one pull request: later pushes update it, and a second one is
    /// refused ([`CommandError::PullRequestExists`]). Answers once the pull
    /// request is open; [`Event::PullRequestChanged`] follows.
    OpenPullRequest {
        chat: ChatId,
        title: String,
        body: String,
    },
    /// Asks the git host for the state of the chat's pull request and
    /// keeps it; a change is reported as [`Event::PullRequestChanged`].
    /// Once it is merged, the developer can remove the chat's worktree
    /// ([`Command::RemoveWorktree`]).
    CheckPullRequest { chat: ChatId },
    /// Removes the chat's worktree and its local branch, after the chat's
    /// pull request was merged (as the last [`Command::CheckPullRequest`]
    /// saw). Refuses while a turn runs there, and while the worktree has
    /// uncommitted changes or commits that were never pushed. Every chat in
    /// the worktree is marked done and stays readable, but takes no more
    /// messages.
    RemoveWorktree { chat: ChatId },
    /// Switches auto-fix on or off for the chat (on by default). While it
    /// is on and the chat's pull request is open, the harness service
    /// polls the git host and, when a check fails, the branch conflicts
    /// with the base branch or a review comment comes in, wakes the chat's
    /// agent to fix it and pushes the fix. Switching it on again after it
    /// stopped by itself ([`AutoFix::Stopped`]) starts over.
    SetAutoFix { chat: ChatId, on: bool },
    /// After the chat's last turn stopped at a usage limit: changes the
    /// subscription of the chat's agent CLI to `subscription`, as
    /// [`Command::ChangeSubscription`] does, and continues the chat there in
    /// a new turn. Codex continues the same agent session; Claude Code
    /// continues in a new agent session with a handoff. Messages queued when
    /// the limit was hit are sent after this turn. Fails when the chat's
    /// last turn did not stop at a usage limit.
    ContinueOnSubscription {
        chat: ChatId,
        subscription: Option<SubscriptionId>,
    },
    /// Runs `repo-check` on the repository again, read-only, on the
    /// repository's default Anthropic subscription (or Claude Code's own
    /// default login without one). It also runs by itself when the
    /// repository is added. Fails while it runs.
    RunRepoCheck { repository: RepositoryId },
    /// Reads the repository's last `repo-check` result, without the checks
    /// that its `.leiser/config.toml` switches off.
    ReadRepoCheck { repository: RepositoryId },
    /// Reads a chat's todo list.
    ListTodos { chat: ChatId },
    /// Adds a todo at the end of the chat's todo list. Its text is trimmed
    /// and must not be empty.
    AddTodo { chat: ChatId, text: String },
    /// Checks a todo (`done`) or unchecks it.
    SetTodoDone {
        chat: ChatId,
        todo: TodoId,
        done: bool,
    },
    /// Removes a todo from the chat's todo list.
    RemoveTodo { chat: ChatId, todo: TodoId },
    /// Reads the harness service's version and whether an update of it
    /// waits for running turns to end.
    ReadServiceStatus,
    /// Lists the CLI extensions each agent CLI loads from the repository:
    /// instruction files, skills, MCP servers and hooks. They belong to the
    /// agent CLI; nothing is converted between agent CLIs.
    ListCliExtensions { repository: RepositoryId },
    /// Reads a file that holds CLI extensions, by the
    /// [`CliExtension::file`] or [`UnreadableCliExtensionFile::file`] that
    /// [`Command::ListCliExtensions`] reported for the repository.
    ReadCliExtensionFile {
        repository: RepositoryId,
        file: PathBuf,
    },
    /// Saves the developer's edit to such a file, in the agent CLI's own
    /// format. Fails when the file no longer holds `previous`, the text it
    /// was read with, and refuses JSON or TOML the agent CLI cannot parse.
    SaveCliExtensionFile {
        repository: RepositoryId,
        file: PathBuf,
        text: String,
        previous: String,
    },
    /// Lists the checkpoints of every turn of the chat, oldest first, and
    /// the base its branch is compared to.
    ListCheckpoints { chat: ChatId },
    /// Reads the changes of one turn of the chat, or of its whole branch.
    ReadDiff { chat: ChatId, range: DiffRange },
    /// Puts the files of the chat's working directory back to how they
    /// were at a checkpoint: files the checkpoint has get its contents,
    /// other files are removed. Files git ignores stay as they are, and so
    /// do the commits and the branch: what the restore changed shows as
    /// changes not staged yet, and staged changes are unstaged. The next
    /// message tells the agent. Fails while a turn runs in any chat in the
    /// same working directory.
    RestoreCheckpoint {
        chat: ChatId,
        turn: u64,
        moment: CheckpointMoment,
    },
}

/// Which changes [`Command::ReadDiff`] reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DiffRange {
    /// What one turn changed: from its checkpoint before to its checkpoint
    /// after, or to the files now while the turn has no checkpoint after.
    Turn { turn: u64 },
    /// What the chat's branch changed against its base: from where the
    /// branch left the base branch to the files now, also changes not
    /// committed yet.
    Branch,
}

/// One of the two checkpoints of a turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckpointMoment {
    /// Before the agent got the turn's message.
    Before,
    /// After the turn ended.
    After,
}

impl std::fmt::Display for CheckpointMoment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Before => "before",
            Self::After => "after",
        })
    }
}

/// The checkpoints of one turn of a chat: the files of its working
/// directory before and after the turn. Each is a commit under a hidden
/// git ref, `refs/leiser/checkpoints/<chat>/<turn>/before` and `…/after`,
/// outside every branch, so it is never pushed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnCheckpoints {
    /// Counted from 1 in each chat.
    pub turn: u64,
    /// The first line of the message that started the turn.
    pub title: String,
    /// The commit with the files before the turn.
    pub before: String,
    /// The commit with the files after the turn; `None` while it runs, or
    /// when the harness service stopped before the turn ended.
    pub after: Option<String>,
}

/// The changes to one file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileDiff {
    /// Relative to the working directory, with `/` between directories.
    pub path: String,
    pub change: FileChange,
    pub patch: Patch,
}

/// What happened to a file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FileChange {
    Added,
    Modified,
    Deleted,
    /// Moved from `from`, maybe with changes.
    Renamed {
        from: String,
    },
}

/// The changed lines of a file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Patch {
    /// The changed lines with some lines around them; no hunks when only
    /// the file mode changed.
    Text { hunks: Vec<Hunk> },
    /// Git sees the file as binary, so it has no lines to show.
    Binary,
    /// More than the diff panel shows: `lines` lines of diff.
    TooLarge { lines: usize },
}

/// A run of changed lines with the lines around them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hunk {
    /// What git shows after the line numbers, such as the enclosing
    /// function; often empty.
    pub section: String,
    pub lines: Vec<DiffLine>,
}

/// One line of a [`Hunk`], with its line number in the old and new file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DiffLine {
    Context { old: u32, new: u32, text: String },
    Added { new: u32, text: String },
    Removed { old: u32, text: String },
}

/// The answer to a successful [`Command`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Response {
    RepositoryAdded {
        repository: RepositoryId,
    },
    ChatStarted {
        chat: ChatId,
    },
    /// The message was sent and starts a turn.
    MessageSent,
    /// The message waits for the running turn to end.
    MessageQueued {
        reason: QueueReason,
    },
    /// The message was delivered into the running turn.
    MessageSteered,
    /// The agent CLI was asked to stop the running turn.
    InterruptRequested,
    /// The chat's next message goes to the new agent CLI.
    AgentCliChanged,
    ChatMarkedDone,
    /// The chat's next turn runs in the new permission mode.
    PermissionModeChanged,
    /// The answer has been passed to the agent.
    RequestAnswered,
    Repositories {
        repositories: Vec<RepositoryOverview>,
    },
    Chats {
        chats: Vec<ChatOverview>,
    },
    Chat {
        status: ChatStatus,
        /// The branch checked out where the chat works.
        branch: Branch,
        /// The agent CLI the chat's next message goes to.
        agent_cli: AgentCli,
        /// The subscription the chat's agent CLI runs on; `None` is the
        /// agent CLI's own default login.
        subscription: Option<SubscriptionId>,
        /// The chat's permission mode, as the developer chose it.
        permission_mode: PermissionMode,
        /// The mode the chat's agent CLI runs it in: Supervised where the
        /// agent CLI lacks `permission_mode`.
        applied_permission_mode: PermissionMode,
        /// The approval requests and questions the running turn waits for,
        /// oldest first.
        open_requests: Vec<String>,
        /// The host account the chat's repository is bound to.
        host_account: HostAccountBinding,
        /// Who commits where the chat works are made by, as git reports it
        /// there now; `None` when git has no author.
        commit_author: Option<CommitAuthor>,
        /// The usage limit the chat's last turn stopped at, until the next
        /// turn starts. See [`Command::ContinueOnSubscription`].
        usage_limit: Option<UsageLimit>,
        /// Where the chat works.
        place: ChatPlace,
        /// The chat's pull request, as the harness last saw it. Boxed to
        /// keep [`Response`] small.
        pull_request: Option<Box<PullRequest>>,
        /// Whether the harness fixes the chat's pull request by itself.
        auto_fix: AutoFix,
        transcript: Vec<TranscriptEntry>,
    },
    SubscriptionAdded {
        subscription: SubscriptionId,
    },
    SubscriptionRenamed,
    SubscriptionRemoved,
    /// The agent CLI's login runs; its output and end arrive as events.
    LoginStarted,
    /// The answer reached the running login.
    LoginAnswered,
    Subscriptions {
        subscriptions: Vec<SubscriptionOverview>,
    },
    /// The chat's next turns on the provider use the new subscription.
    SubscriptionChanged,
    HostAccountAdded {
        host_account: HostAccountId,
    },
    HostAccountRemoved,
    /// The git host's login runs; its output and end arrive as events.
    HostAccountLoginStarted,
    HostAccounts {
        host_accounts: Vec<HostAccountOverview>,
    },
    /// The chat's branch is on `origin` now.
    Pushed {
        branch: String,
    },
    /// The chat's branch is on `origin` and the pull request is open.
    PullRequestOpened {
        pull_request: PullRequest,
    },
    /// The chat's pull request as the git host reports it now.
    PullRequest {
        pull_request: PullRequest,
    },
    /// The chat's worktree and local branch are gone.
    WorktreeRemoved,
    /// Auto-fix of the chat is on or off as asked.
    AutoFixSet {
        auto_fix: AutoFix,
    },
    InterchangeableSet,
    /// The chat continues on the new subscription in a new turn.
    ContinuedOnSubscription,
    /// `repo-check` runs; [`Event::RepoCheckFinished`] follows.
    RepoCheckStarted,
    RepoCheck {
        check: RepoCheck,
    },
    Todos {
        todos: Vec<Todo>,
    },
    TodoAdded {
        todo: TodoId,
    },
    TodoUpdated,
    TodoRemoved,
    ServiceStatus {
        /// The version of the running harness service.
        version: String,
        /// An update of the harness service waits until no turn runs.
        update_waiting: bool,
    },
    CliExtensions {
        extensions: Vec<CliExtension>,
        /// Files that hold CLI extensions but could not be parsed, so their
        /// extensions are missing above. They can still be read and saved.
        unreadable: Vec<UnreadableCliExtensionFile>,
    },
    CliExtensionFile {
        text: String,
    },
    CliExtensionFileSaved,
    Checkpoints {
        /// What [`DiffRange::Branch`] compares to, such as `origin/main`;
        /// `HEAD` when the chat's repository has no base branch.
        branch_base: String,
        turns: Vec<TurnCheckpoints>,
    },
    Diff {
        files: Vec<FileDiff>,
    },
    CheckpointRestored,
}

/// A registered subscription.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubscriptionOverview {
    pub subscription: SubscriptionId,
    pub provider: Provider,
    pub name: String,
    pub kind: SubscriptionKind,
    /// The agent CLI's config directory for this subscription. For Codex a
    /// shadow home: the shared Codex home with its own `auth.json`.
    pub home: PathBuf,
    /// Whether the agent CLI's login runs right now.
    pub logging_in: bool,
    /// Whether a chat that hits a usage limit may move to or from this
    /// subscription by itself; see [`Command::SetInterchangeable`].
    pub interchangeable: bool,
}

/// A usage limit that stopped a chat's turn, as far as the agent CLI
/// reported it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageLimit {
    /// Which limit, as the agent CLI words it, such as `5-hour limit`.
    pub limit: String,
    /// The subscription that hit it; `None` is the agent CLI's own default
    /// login.
    pub subscription: Option<SubscriptionId>,
    /// How long until the limit resets, from now; `None` when the agent CLI
    /// did not say.
    pub resets_in_seconds: Option<u64>,
}

/// A registered host account.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostAccountOverview {
    pub host_account: HostAccountId,
    pub name: String,
    pub git_host: String,
    pub login: String,
    pub author: CommitAuthor,
    /// The `gh` config directory the host account is signed in in.
    pub config_dir: PathBuf,
    /// Whether the git host's login runs right now.
    pub logging_in: bool,
}

/// The host account a repository is bound to, as its `.leiser/config.toml`
/// names it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HostAccountBinding {
    /// The config names no host account.
    Unbound,
    Bound {
        host_account: HostAccountId,
        name: String,
        git_host: String,
        login: String,
    },
    /// The config names a host account that is not registered.
    Unknown { name: String },
    /// The config could not be read.
    Unreadable { reason: String },
}

/// How a login ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LoginOutcome {
    SignedIn,
    Failed { reason: String },
}

/// One entry of a chat's todo list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Todo {
    pub id: TodoId,
    pub text: String,
    /// Whether the todo is checked.
    pub done: bool,
}

/// A skill, MCP server, hook or instruction file an agent CLI loads by
/// itself from the repository.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CliExtension {
    pub agent_cli: AgentCli,
    pub kind: CliExtensionKind,
    /// The file name of an instruction file, the skill's or MCP server's
    /// name, or the event a hook runs on.
    pub name: String,
    /// Where the agent CLI loads it from, relative to the repository.
    pub path: PathBuf,
    /// The file that holds it: [`CliExtension::path`] with symlinks
    /// resolved, relative to the repository, or absolute when a symlink
    /// leads out of it. Two agent CLIs that load the same file report the
    /// same `file`; read and save it by this.
    pub file: PathBuf,
}

/// What a [`CliExtension`] is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CliExtensionKind {
    /// Instructions the agent CLI gives the agent, such as `AGENTS.md`.
    InstructionFile {
        /// The instruction file that imports this one with `@path`
        /// (Claude Code); `None` when the agent CLI loads it by itself.
        imported_by: Option<PathBuf>,
    },
    /// A skill directory's `SKILL.md`.
    Skill { description: Option<String> },
    McpServer {
        /// The command line that starts it, or the URL it is reached at.
        launch: Option<String>,
    },
    /// Hook handlers for one event; the [`CliExtension::name`] is the event.
    Hook {
        /// Which tools or sources the hooks run for; `None` for all.
        matcher: Option<String>,
        /// The commands, URLs or prompts the hooks run.
        commands: Vec<String>,
    },
}

/// A file that holds CLI extensions but could not be parsed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnreadableCliExtensionFile {
    pub agent_cli: AgentCli,
    /// Where the agent CLI loads it from, relative to the repository.
    pub path: PathBuf,
    /// As [`CliExtension::file`].
    pub file: PathBuf,
    pub reason: String,
}

/// A repository as the sidebar lists it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepositoryOverview {
    pub repository: RepositoryId,
    /// The name of the repository's directory.
    pub name: String,
    pub path: PathBuf,
}

/// A chat as the sidebar lists it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatOverview {
    pub chat: ChatId,
    pub repository: RepositoryId,
    /// The first line of the developer's first message; `None` until then.
    pub title: Option<String>,
    pub status: ChatStatus,
    /// The worktree the chat works in; `None` in the main checkout.
    pub worktree: Option<PathBuf>,
}

/// Where a chat works.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChatPlace {
    /// The repository's main checkout.
    MainCheckout,
    /// A worktree the harness created.
    Worktree,
    /// A worktree the harness created and has removed since, after the
    /// chat's pull request was merged. The chat stays readable.
    RemovedWorktree,
}

/// A chat's pull request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequest {
    /// The pull request's number on the git host.
    pub number: u64,
    /// Where the git host shows it.
    pub url: String,
    /// The chat's branch.
    pub head: String,
    /// The branch it is to be merged into.
    pub base: String,
    pub state: PullRequestState,
}

/// Where a pull request stands on the git host.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PullRequestState {
    Open,
    Merged,
    /// Closed without a merge.
    Closed,
}

/// Whether the harness watches a chat's open pull request and wakes the
/// chat's agent to fix it (see [`Command::SetAutoFix`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AutoFix {
    /// It watches the pull request while it is open.
    On,
    /// The developer switched it off.
    Off,
    /// It stopped by itself and handed over to the developer.
    Stopped { reason: AutoFixStop },
}

/// Why auto-fix stopped by itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AutoFixStop {
    /// `attempts` attempts in a row at failing checks or a conflict left
    /// the pull request's head commit as it was.
    NoProgress { attempts: u32 },
    /// The check failed again on the commit auto-fix pushed to fix it.
    CheckFailedAgain { name: String },
    /// The branch conflicts with the same base commit again on the commit
    /// auto-fix pushed to resolve it.
    ConflictAgain,
    /// The harness could not push the agent's fix.
    PushFailed { reason: String },
    /// The git host could not be asked about the pull request, several
    /// times in a row.
    PollFailed { reason: String },
}

/// What woke a chat's agent to fix its pull request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AutoFixTrigger {
    /// A check on the pull request's head commit failed.
    CheckFailed {
        name: String,
        /// Where the git host shows the check.
        url: String,
    },
    /// The branch conflicts with the base branch.
    Conflict { base: String },
    /// A review comment, a review that is not an approval, or a comment on
    /// the pull request.
    Comment {
        author: String,
        body: String,
        /// The file and line it is on; `None` for the whole pull request.
        location: Option<CommentLocation>,
    },
}

impl std::fmt::Display for AutoFixTrigger {
    /// One line on what happened, such as "The check `test` failed.".
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CheckFailed { name, .. } => write!(f, "The check `{name}` failed."),
            Self::Conflict { base } => write!(f, "The branch conflicts with `{base}`."),
            Self::Comment {
                author,
                body,
                location: Some(location),
            } => write!(f, "{author} on {location}: {body}"),
            Self::Comment { author, body, .. } => write!(f, "{author}: {body}"),
        }
    }
}

/// Where in a pull request's files a comment is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommentLocation {
    pub path: String,
    /// `None` for a comment on the whole file.
    pub line: Option<u64>,
}

impl std::fmt::Display for CommentLocation {
    /// `path:line`, or the path alone.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.line {
            Some(line) => write!(f, "{}:{line}", self.path),
            None => f.write_str(&self.path),
        }
    }
}

/// What is checked out in a working directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Branch {
    /// A branch, possibly without commits yet.
    Named { name: String },
    /// A commit without a branch.
    Detached { commit: String },
    /// Git could not tell.
    Unknown,
}

/// What a chat is doing right now, as the sidebar shows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChatStatus {
    /// A turn runs, or the chat's worktree is being set up.
    Working,
    /// The agent waits for the developer: for the next message, or for an
    /// answer to an approval request or a question of the running turn.
    WaitingForYou,
    /// The developer marked the chat as done.
    Done,
}

/// One entry of a chat's transcript, in the order it happened.
///
/// The harness service stores these on disk, so a change that breaks
/// reading old entries needs a storage migration (see `store.rs`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TranscriptEntry {
    /// A message the developer sent to the agent session, also a queued
    /// one once it was sent, or one steered into the running turn.
    UserMessage { text: String },
    /// An event from one of the chat's agent sessions.
    Agent {
        agent_cli: AgentCli,
        event: AgentEvent,
    },
    /// A script of the repository has finished in the chat's worktree.
    ScriptFinished {
        script: RepositoryScript,
        /// `None` when the script could not start or was stopped.
        exit_code: Option<i32>,
        /// Its standard output, then its standard error; only the end of
        /// a long output.
        output: String,
    },
    /// The developer's answer to an approval request or a question.
    Answer { request: String, answer: Answer },
    /// After a usage limit, the chat continued on another subscription of
    /// the provider; the turn that continues it follows.
    ContinuedOnSubscription {
        provider: Provider,
        /// `None` is the agent CLI's own default login.
        subscription: Option<SubscriptionId>,
        /// Whether the harness service switched by itself, between
        /// interchangeable subscriptions.
        automatic: bool,
    },
    /// Auto-fix woke the chat's agent; the turn that fixes the pull
    /// request follows.
    AutoFixTriggered {
        /// The pull request's number.
        pull_request: u64,
        triggers: Vec<AutoFixTrigger>,
    },
    /// Auto-fix stopped by itself and hands over to the developer.
    AutoFixStopped { reason: AutoFixStop },
}

/// The developer's answer to an approval request or a question.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Answer {
    /// Allows the action the agent asked for. Only for approval requests.
    Approve,
    /// Refuses the action, or the questions. `feedback` is the developer's
    /// text reply, which the agent gets instead.
    Decline { feedback: Option<String> },
    /// Answers questions: per question id, the labels of the chosen
    /// options or the developer's own text. Only for questions.
    Answers {
        answers: BTreeMap<String, Vec<String>>,
    },
}

/// A question an agent asks the developer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Question {
    /// The question's id within its request; [`Answer::Answers`] keys
    /// answers by it.
    pub id: String,
    /// A short label for the question.
    pub header: String,
    pub question: String,
    /// The options to choose from; the developer may also write their own
    /// answer.
    pub options: Vec<QuestionOption>,
    /// Whether the developer may choose several options.
    pub multi_select: bool,
}

/// One option of a [`Question`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuestionOption {
    pub label: String,
    pub description: String,
}

/// Why a message was queued instead of being delivered now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueueReason {
    /// The developer queued it.
    Requested,
    /// The developer steered it, but the agent CLI cannot take a message
    /// mid-turn.
    SteerUnsupported,
    /// The chat's worktree is being set up; the agent starts after.
    SettingUp,
}

/// A repository's `repo-check`: the harness runs Timo's `repo-check`
/// skill read-only and shows what it found. It never fixes a finding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum RepoCheck {
    /// It has not run, for example because the harness service restarted
    /// while it ran.
    NotRun,
    Running,
    /// It ran and its report was read.
    Done {
        /// Without the switched-off checks.
        report: RepoCheckReport,
        opt_outs: OptOuts,
    },
    /// It ran, but its answer was not a report the harness can read; shown
    /// as it came.
    Unreadable {
        reason: String,
        output: String,
    },
    /// It could not run or did not finish.
    Failed {
        reason: String,
    },
}

/// What `repo-check` found, ranked as the skill ranks it. Its `Later`,
/// `Nicht geprüft` and `In Ordnung` sections are not shown.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoCheckReport {
    /// The commit the report names in its heading.
    pub commit: Option<String>,
    /// The report's first paragraph: the repository's profile and verdict.
    pub summary: String,
    /// Real risks now.
    pub must: Vec<Finding>,
    /// Clear wins that are cheap to make.
    pub should: Vec<Finding>,
}

/// One finding of `repo-check`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    pub title: String,
    /// The rules it breaks; empty when the report named none.
    pub checks: Vec<CheckId>,
    /// Evidence, why it matters and the lightest fix, as the report
    /// words them (`Beleg:`, `Warum:`, `Fix:`).
    pub details: String,
}

/// The checks a repository's `.leiser/config.toml` switches off.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OptOuts {
    /// These checks are left out of the report.
    Applied { checks: Vec<SwitchedOffCheck> },
    /// The file could not be read, so nothing is left out.
    Unreadable { reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwitchedOffCheck {
    pub check: CheckId,
    pub reason: String,
}

/// A rule of `repo-check`, such as `D3`: a capital letter and a number,
/// sometimes followed by a small letter (`B3a`).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct CheckId(String);

impl CheckId {
    /// `None` when `text` is not a rule id.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let mut chars = text.chars();
        let letter = chars.next()?;
        let rest = chars.as_str();
        let digits = rest.len() - rest.trim_start_matches(|c: char| c.is_ascii_digit()).len();
        let suffix = &rest[digits..];
        let valid = letter.is_ascii_uppercase()
            && (1..=2).contains(&digits)
            && (suffix.is_empty()
                || (suffix.len() == 1 && suffix.chars().all(|c| c.is_ascii_lowercase())));
        valid.then(|| Self(text.to_owned()))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for CheckId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A text that is not a [`CheckId`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("not a repo-check rule such as `D3`: {0:?}")]
pub struct InvalidCheckId(String);

impl TryFrom<String> for CheckId {
    type Error = InvalidCheckId;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        Self::parse(&text).ok_or(InvalidCheckId(text))
    }
}

impl From<CheckId> for String {
    fn from(check: CheckId) -> Self {
        check.0
    }
}

/// Why a [`Command`] failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CommandError {
    #[error("not a git repository: {}", path.display())]
    NotAGitRepository { path: PathBuf },
    // Struct variants throughout: an internally tagged newtype variant
    // around a number does not serialize.
    #[error("unknown repository {repository:?}")]
    UnknownRepository { repository: RepositoryId },
    #[error("unknown chat {chat:?}")]
    UnknownChat { chat: ChatId },
    #[error("chat {chat:?} belongs to another repository")]
    ChatInAnotherRepository { chat: ChatId },
    #[error("chat {chat:?} works in the main checkout, not in a worktree")]
    NotInAWorktree { chat: ChatId },
    #[error("the worktree of chat {chat:?} is still being set up")]
    SettingUp { chat: ChatId },
    #[error("could not create the chat's worktree: {reason}")]
    Worktree { reason: String },
    #[error("the repository's .leiser/config.toml is invalid: {reason}")]
    RepositoryConfig { reason: String },
    #[error("a turn is running in chat {chat:?}; queue or steer the message")]
    TurnRunning { chat: ChatId },
    #[error("no turn is running in chat {chat:?}")]
    NoTurnRunning { chat: ChatId },
    #[error("no driver is configured for {agent_cli}")]
    AgentCliNotConfigured { agent_cli: AgentCli },
    #[error("no approval request or question {request:?} is open in chat {chat:?}")]
    UnknownRequest { chat: ChatId, request: String },
    #[error("request {request:?} in chat {chat:?} takes another kind of answer")]
    AnswerDoesNotFit { chat: ChatId, request: String },
    #[error("the agent CLI failed: {reason}")]
    AgentCli { reason: String },
    #[error("a todo needs a text")]
    EmptyTodo,
    #[error("chat {chat:?} has no todo {todo}")]
    UnknownTodo { chat: ChatId, todo: TodoId },
    #[error("the harness service could not store the chat: {reason}")]
    Storage { reason: String },
    #[error("unknown subscription {subscription:?}")]
    UnknownSubscription { subscription: SubscriptionId },
    #[error("no {provider} subscription is named {name:?}")]
    UnknownSubscriptionName { provider: Provider, name: String },
    #[error("a {provider} subscription is already named {name:?}")]
    SubscriptionNameTaken { provider: Provider, name: String },
    #[error("a subscription needs a name")]
    EmptySubscriptionName,
    #[error("an API key subscription needs a key")]
    EmptyApiKey,
    #[error("subscription {subscription:?} is not a {provider} subscription")]
    WrongProvider {
        subscription: SubscriptionId,
        provider: Provider,
    },
    #[error("subscription {subscription:?} signs in with an API key, not by login")]
    NotALoginSubscription { subscription: SubscriptionId },
    #[error("could not set up the subscription: {reason}")]
    SubscriptionSetup { reason: String },
    #[error("no login runs for subscription {subscription:?}")]
    NoLoginRunning { subscription: SubscriptionId },
    #[error("the OS keychain failed: {reason}")]
    Keychain { reason: String },
    #[error("repo-check is not set up in the harness service")]
    RepoCheckNotConfigured,
    #[error("repo-check is already running in repository {repository:?}")]
    RepoCheckRunning { repository: RepositoryId },
    #[error("{} holds no CLI extension of this repository", file.display())]
    NotACliExtensionFile { file: PathBuf },
    #[error("{} changed since it was read; read it again", file.display())]
    CliExtensionFileChanged { file: PathBuf },
    #[error("{} would no longer load: {reason}", file.display())]
    InvalidCliExtensionFile { file: PathBuf, reason: String },
    #[error("could not access {}: {reason}", file.display())]
    CliExtensionFileAccess { file: PathBuf, reason: String },
    #[error("unknown host account {host_account:?}")]
    UnknownHostAccount { host_account: HostAccountId },
    #[error("no host account is named {name:?}")]
    UnknownHostAccountName { name: String },
    #[error("a host account is already named {name:?}")]
    HostAccountNameTaken { name: String },
    #[error("a host account needs a {field}")]
    EmptyHostAccountField { field: String },
    #[error("could not set up the host account: {reason}")]
    HostAccountSetup { reason: String },
    #[error(
        "the repository is bound to no host account; name one as \
         `host_account = \"<name>\"` in its .leiser/config.toml"
    )]
    NoHostAccount { repository: RepositoryId },
    #[error(
        "push blocked: it would sign in to {git_host} as {actual:?}, but the \
         repository is bound to host account {name:?} ({expected:?}); log in \
         to {name:?} again as {expected:?}"
    )]
    HostAccountMismatch {
        name: String,
        git_host: String,
        expected: String,
        actual: String,
    },
    #[error("push blocked: could not check the host account {name:?}: {reason}")]
    HostAccountUnverified { name: String, reason: String },
    #[error("chat {chat:?} has no branch checked out to push")]
    NoBranch { chat: ChatId },
    #[error("the push failed: {reason}")]
    PushFailed { reason: String },
    /// A hard block: the harness never pushes to `main` or `development`.
    #[error(
        "push blocked: `{branch}` is a protected branch; work on a branch of \
         its own and open a pull request"
    )]
    ProtectedBranch { branch: String },
    /// A hard block: the push would replace commits on `origin`'s branch
    /// that the chat's branch does not have.
    #[error(
        "push blocked: it would overwrite commits on origin's `{branch}` that \
         this branch does not have (a force push)"
    )]
    ForcePush { branch: String },
    /// A hard block: a commit the push would add carries AI attribution,
    /// such as a `Co-authored-by` trailer for an AI. `line` is what was
    /// found.
    #[error(
        "push blocked: commit {commit} carries AI attribution ({line}); \
         reword the commit without it"
    )]
    AiAttribution { commit: String, line: String },
    #[error("chat {chat:?} already has pull request #{number}; push to update it")]
    PullRequestExists { chat: ChatId, number: u64 },
    #[error("chat {chat:?} is opening its pull request")]
    PullRequestOpening { chat: ChatId },
    #[error("a pull request needs a title")]
    EmptyPullRequestTitle,
    #[error("chat {chat:?} has no pull request")]
    NoPullRequest { chat: ChatId },
    #[error(
        "pull request #{number} of chat {chat:?} is not merged; check it again \
         once it is"
    )]
    PullRequestNotMerged { chat: ChatId, number: u64 },
    #[error("the pull request failed: {reason}")]
    PullRequestFailed { reason: String },
    #[error(
        "the repository has no base branch for a pull request: set \
         `base_branch` in .leiser/config.toml or `origin`'s default branch"
    )]
    NoBaseBranch,
    #[error("the worktree of chat {chat:?} was removed; the chat is read-only")]
    WorktreeRemoved { chat: ChatId },
    #[error("the worktree of chat {chat:?} has uncommitted changes; commit or discard them first")]
    WorktreeHasChanges { chat: ChatId },
    #[error(
        "the worktree of chat {chat:?} has commits that were never pushed; \
         push them or remove the worktree yourself"
    )]
    WorktreeHasUnpushedCommits { chat: ChatId },
    #[error("chat {chat:?} has no checkpoint {moment} turn {turn}")]
    UnknownCheckpoint {
        chat: ChatId,
        turn: u64,
        moment: CheckpointMoment,
    },
    #[error("git failed on the chat's checkpoints: {reason}")]
    Checkpoint { reason: String },
    #[error("the last turn of chat {chat:?} did not stop at a usage limit")]
    NoUsageLimit { chat: ChatId },
    #[error("the harness service has stopped")]
    ServiceStopped,
}

/// Something that happened in the harness service.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    /// An event from a chat's agent session.
    Agent { chat: ChatId, event: AgentEvent },
    /// The developer's message waits for the running turn to end.
    MessageQueued {
        chat: ChatId,
        text: String,
        reason: QueueReason,
    },
    /// The developer's message was delivered into the running turn.
    MessageSteered { chat: ChatId, text: String },
    /// A queued message was sent and starts the next turn.
    QueuedMessageSent { chat: ChatId, text: String },
    /// A script of the repository started in the chat's worktree.
    ScriptStarted {
        chat: ChatId,
        script: RepositoryScript,
    },
    /// A script of the repository has finished in the chat's worktree;
    /// also stored in the chat's transcript.
    ScriptFinished {
        chat: ChatId,
        script: RepositoryScript,
        exit_code: Option<i32>,
        output: String,
    },
    /// After a usage limit, the chat continues on another subscription in
    /// a new turn; also stored in the chat's transcript.
    ContinuedOnSubscription {
        chat: ChatId,
        provider: Provider,
        subscription: Option<SubscriptionId>,
        automatic: bool,
    },
    /// The chat's status changed without an event of its own, for example
    /// when it was marked as done.
    StatusChanged { chat: ChatId, status: ChatStatus },
    /// The developer answered an approval request or a question.
    RequestAnswered {
        chat: ChatId,
        request: String,
        answer: Answer,
    },
    /// A line the agent CLI's login printed, such as the address to open
    /// when no browser opened by itself.
    LoginOutput {
        subscription: SubscriptionId,
        text: String,
    },
    /// The agent CLI's login of a subscription has ended.
    LoginFinished {
        subscription: SubscriptionId,
        outcome: LoginOutcome,
    },
    /// A line the git host's login printed, such as the one-time code to
    /// enter in the browser.
    HostAccountLoginOutput {
        host_account: HostAccountId,
        text: String,
    },
    /// The git host's login of a host account has ended.
    HostAccountLoginFinished {
        host_account: HostAccountId,
        outcome: LoginOutcome,
    },
    /// `repo-check` started in the repository.
    RepoCheckStarted { repository: RepositoryId },
    /// `repo-check` finished in the repository; read the result with
    /// [`Command::ReadRepoCheck`].
    RepoCheckFinished { repository: RepositoryId },
    /// The chat's todo list changed, by the developer or an agent; `todos`
    /// is the whole list after the change.
    TodosChanged { chat: ChatId, todos: Vec<Todo> },
    /// The files of the chat's working directory were put back to a
    /// checkpoint ([`Command::RestoreCheckpoint`]).
    CheckpointRestored {
        chat: ChatId,
        turn: u64,
        moment: CheckpointMoment,
    },
    /// The chat's pull request was opened, or its state on the git host
    /// changed.
    PullRequestChanged {
        chat: ChatId,
        pull_request: PullRequest,
    },
    /// The chat's worktree and local branch were removed
    /// ([`Command::RemoveWorktree`]).
    WorktreeRemoved { chat: ChatId },
    /// Auto-fix of the chat was switched on or off, or stopped by itself;
    /// a stop is also stored in the chat's transcript.
    AutoFixChanged { chat: ChatId, auto_fix: AutoFix },
    /// Auto-fix woke the chat's agent; also stored in the chat's
    /// transcript. The turn's events follow.
    AutoFixTriggered {
        chat: ChatId,
        pull_request: u64,
        triggers: Vec<AutoFixTrigger>,
    },
    /// An update of the harness service waits until no turn runs. Then the
    /// service restarts on the new version and resumes every agent session.
    ServiceUpdateWaiting,
}

impl Event {
    /// The chat the event belongs to; `None` for events of no chat: logins,
    /// those of a repository and those of the whole harness service.
    #[must_use]
    pub fn chat(&self) -> Option<ChatId> {
        match self {
            Self::Agent { chat, .. }
            | Self::MessageQueued { chat, .. }
            | Self::MessageSteered { chat, .. }
            | Self::QueuedMessageSent { chat, .. }
            | Self::RequestAnswered { chat, .. }
            | Self::ScriptStarted { chat, .. }
            | Self::ScriptFinished { chat, .. }
            | Self::StatusChanged { chat, .. }
            | Self::ContinuedOnSubscription { chat, .. }
            | Self::TodosChanged { chat, .. }
            | Self::CheckpointRestored { chat, .. }
            | Self::PullRequestChanged { chat, .. }
            | Self::AutoFixChanged { chat, .. }
            | Self::AutoFixTriggered { chat, .. }
            | Self::WorktreeRemoved { chat } => Some(*chat),
            Self::LoginOutput { .. }
            | Self::LoginFinished { .. }
            | Self::HostAccountLoginOutput { .. }
            | Self::HostAccountLoginFinished { .. }
            | Self::RepoCheckStarted { .. }
            | Self::RepoCheckFinished { .. }
            | Self::ServiceUpdateWaiting => None,
        }
    }
}

/// An event from an agent session, the same for every agent CLI.
///
/// Stored in chat transcripts; see [`TranscriptEntry`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentEvent {
    /// A message from the agent.
    Message { text: String },
    /// The agent called a tool.
    ToolCall { name: String, detail: String },
    /// A tool the agent called returned. Shell commands end with
    /// [`AgentEvent::CommandFinished`] instead where the agent CLI reports
    /// their exit code.
    ToolResult { name: String, output: String },
    /// A shell command the agent ran has finished.
    CommandFinished {
        command: String,
        /// `None` when the command was stopped before it exited.
        exit_code: Option<i32>,
        output: String,
    },
    /// The agent waits for the developer's approval of an action, until
    /// [`Command::AnswerRequest`] answers it or the turn ends.
    ///
    /// `request` is unique in the chat once the harness service has
    /// recorded the event; a driver gives its own id, unique in the agent
    /// session, which the service replaces.
    ApprovalRequested {
        request: String,
        description: String,
    },
    /// The agent asks the developer questions and waits for the answers,
    /// like [`AgentEvent::ApprovalRequested`].
    QuestionAsked {
        request: String,
        questions: Vec<Question>,
    },
    /// The subscription hit a usage limit. The turn stops; see
    /// [`Command::ContinueOnSubscription`].
    UsageLimitReached {
        limit: String,
        resets_in_seconds: Option<u64>,
    },
    /// The agent finished its turn.
    TurnEnded,
    /// The turn ended early because the developer interrupted it.
    TurnInterrupted,
    /// The turn ended early because the agent CLI reported an error, such
    /// as a rejected API key. A turn that hit a usage limit ends with
    /// [`AgentEvent::UsageLimitReached`] and [`AgentEvent::TurnEnded`]
    /// instead.
    TurnFailed {
        /// The agent CLI's error message, with secrets removed.
        reason: String,
    },
    /// The agent session ended; the agent CLI exited or broke its protocol.
    SessionEnded,
}
