//! The task queue: tasks the developer lines up per repository, a prompt
//! or an issue each, that the harness service runs by itself, one chat in
//! a new worktree per task, as slots free up.
//!
//! This module holds the queue's rules and nothing that runs: which task
//! starts next, what a chat's state means for its task, when the queue
//! waits for usage limits, and how the queue is stored. The service runs
//! its decisions on the chats (`service/queue.rs`); every input, the time
//! included, comes in as a value, so the rules are deterministic.
//!
//! - A repository's queue starts its queued tasks in order while fewer
//!   than its concurrency limit are active. A task is active from its
//!   start until its chat's turn ends: while its worktree is created, its
//!   setup script runs or a turn runs, also one that waits for an approval
//!   request.
//! - When the turn ends, the task is completed, or needs attention when
//!   the turn failed, was interrupted or stopped at a usage limit. A task
//!   whose chat opens its pull request is ready for review. Either way its
//!   slot goes to the next task.
//! - A task needs a subscription that is not at a usage limit: where its
//!   chat would start, or an interchangeable one it would move to. A task
//!   without one waits; when no queued task has one, the queue waits for
//!   usage limits until one resets, a task completes on it or the
//!   developer resumes the queue.
//! - The developer can take a task's chat up again: a task that needed
//!   attention is running again once its chat works. The developer's own
//!   turns are not held back by the concurrency limit.
//!
//! The queue is kept in `queue.json` in the data directory with a storage
//! version of its own ([`QUEUE_VERSION`]), next to `state.json` (ADR
//! 0006). An older harness service never reads the file, so adding it
//! needed no new version of `state.json`. A restart puts a task that was
//! starting back in line, and marks a running one as needing attention:
//! the turn did not survive the restart. A crash in the moment between a
//! task's worktree chat being stored and the queue learning of it leaves
//! that chat behind and starts the task again.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::store::{StorageError, Store};
use crate::{
    AgentCli, ChatId, CommandError, PermissionMode, Provider, RepositoryId, SubscriptionId,
};

/// The most tasks a repository's queue runs at once.
pub const MAX_CONCURRENCY: u32 = 8;
/// The tasks a repository's queue runs at once until the developer
/// changes it.
const DEFAULT_CONCURRENCY: u32 = 1;
/// The longest title of a task, in characters.
const TITLE_LENGTH: usize = 80;
const QUEUE_FILE: &str = "queue.json";

/// The storage version of `queue.json`.
///
/// - 1: the repositories' queue settings, the tasks in order with their
///   state, and the usage limits the queue knows of.
const QUEUE_VERSION: u32 = 1;

/// A task in a repository's queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct QueueItemId(pub(crate) u64);

impl std::fmt::Display for QueueItemId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// What a queued task asks the agent to do.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum QueueTask {
    /// The text is the chat's first message.
    Prompt { text: String },
    /// An issue of the repository on its git host, which the agent reads
    /// itself.
    Issue { number: u64 },
}

impl QueueTask {
    /// How the queue names the task: the prompt's first line, or the
    /// issue's number.
    #[must_use]
    pub fn title(&self) -> String {
        match self {
            Self::Prompt { text } => {
                let line = text.lines().find(|line| !line.trim().is_empty());
                let line = line.unwrap_or_default().trim();
                if line.chars().count() > TITLE_LENGTH {
                    let cut: String = line.chars().take(TITLE_LENGTH - 1).collect();
                    format!("{}…", cut.trim_end())
                } else {
                    line.to_owned()
                }
            }
            Self::Issue { number } => format!("Issue #{number}"),
        }
    }

    /// The first message of the task's chat.
    #[must_use]
    pub fn message(&self) -> String {
        match self {
            Self::Prompt { text } => text.trim().to_owned(),
            Self::Issue { number } => format!(
                "Work on issue #{number} of this repository. Read the issue on the \
                 repository's git host first, for example with `gh issue view {number}`, \
                 then do the work it asks for. Name the issue in your commits' and pull \
                 request's description as `Closes #{number}`."
            ),
        }
    }
}

/// How a repository's queue runs its tasks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueSettings {
    /// How many tasks run at once, each in a worktree of its own: 1 to
    /// [`MAX_CONCURRENCY`].
    pub concurrency: u32,
    /// The agent CLI tasks start on; `None`: the service's default.
    #[serde(default)]
    pub agent_cli: Option<AgentCli>,
    /// The permission mode tasks start in; `None`: the repository's.
    #[serde(default)]
    pub permission_mode: Option<PermissionMode>,
}

impl Default for QueueSettings {
    fn default() -> Self {
        Self {
            concurrency: DEFAULT_CONCURRENCY,
            agent_cli: None,
            permission_mode: None,
        }
    }
}

/// A command to a repository's queue; see [`Command::Queue`](crate::Command::Queue).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum QueueCommand {
    /// Reads the repository's queue.
    Read { repository: RepositoryId },
    /// Adds a task at the end of the repository's queue. The agent CLI
    /// and permission mode it names take the place of the queue's.
    Enqueue {
        repository: RepositoryId,
        task: QueueTask,
        #[serde(default)]
        agent_cli: Option<AgentCli>,
        #[serde(default)]
        permission_mode: Option<PermissionMode>,
    },
    /// Moves a queued task to place `to` among its repository's queued
    /// tasks, counted from 0; a place past the end is the end.
    Move { item: QueueItemId, to: usize },
    /// Takes a task out of line. A running task's turn is interrupted;
    /// its chat stays.
    Cancel { item: QueueItemId },
    /// Removes a task that is no longer queued or running from the list.
    /// Its chat stays.
    Remove { item: QueueItemId },
    /// Starts no more tasks of the repository; running ones go on.
    Pause { repository: RepositoryId },
    /// Starts the repository's tasks again, and tries subscriptions the
    /// queue waited for at a usage limit again.
    Resume { repository: RepositoryId },
    /// Changes how the repository's queue runs its tasks, from its next
    /// start on.
    ChangeSettings {
        repository: RepositoryId,
        settings: QueueSettings,
    },
}

/// The answer to a [`QueueCommand`]: the repository's queue after it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum QueueAnswer {
    Queue {
        queue: RepositoryQueue,
    },
    /// [`QueueCommand::Enqueue`] added `item`.
    Enqueued {
        item: QueueItemId,
        queue: RepositoryQueue,
    },
}

/// A repository's queue as the desktop app shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepositoryQueue {
    pub repository: RepositoryId,
    pub settings: QueueSettings,
    pub status: QueueStatus,
    /// Every task in line order: queued ones in the order they start in.
    pub items: Vec<QueueItem>,
}

/// Whether a repository's queue starts tasks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum QueueStatus {
    /// No task is queued or running.
    Idle,
    /// Tasks run, or start as slots free up.
    Running,
    /// The developer paused it.
    Paused,
    /// Every queued task's subscriptions are at a usage limit.
    WaitingForUsageLimits {
        /// When the first of them resets, if known.
        resets_in_seconds: Option<u64>,
    },
    /// An update of the harness service waits; the queue starts no task
    /// until it is done.
    WaitingForUpdate,
}

/// A task in a repository's queue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueItem {
    pub item: QueueItemId,
    pub task: QueueTask,
    pub title: String,
    /// The agent CLI the task starts on, if it chose one.
    pub agent_cli: Option<AgentCli>,
    /// The permission mode the task starts in, if it chose one.
    pub permission_mode: Option<PermissionMode>,
    pub state: QueueItemState,
}

/// Where a task is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum QueueItemState {
    Queued,
    /// Its chat's worktree is being created.
    Starting,
    /// Its chat works on it.
    Running {
        chat: ChatId,
        /// Whether the turn waits for an approval request or a question.
        waiting_for_you: bool,
    },
    /// The developer has to look at it; its slot is free.
    NeedsAttention {
        /// `None` when no chat could be started.
        chat: Option<ChatId>,
        reason: AttentionReason,
    },
    /// Its chat's turn ended.
    Completed {
        chat: ChatId,
    },
    /// Its chat opened its pull request.
    PullRequestReady {
        chat: ChatId,
        number: u64,
        url: String,
    },
    /// The developer took it out of line.
    Cancelled {
        chat: Option<ChatId>,
    },
}

/// Why a task needs the developer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AttentionReason {
    /// Its chat could not be started, or its first message not sent.
    StartFailed { reason: String },
    /// The agent CLI reported an error.
    TurnFailed { reason: String },
    /// The turn was interrupted.
    Interrupted,
    /// The agent session ended before the turn did.
    SessionEnded,
    /// The chat stopped without a turn end, for example after its setup
    /// script failed.
    Stopped,
    /// The turn stopped at a usage limit and could not move on.
    UsageLimit {
        limit: String,
        subscription: Option<SubscriptionId>,
        resets_in_seconds: Option<u64>,
    },
    /// The harness service restarted while the turn ran.
    ServiceRestarted,
}

/// Why a queue command failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum QueueError {
    #[error("no queued task {item}")]
    UnknownItem { item: QueueItemId },
    #[error("a prompt task needs a text")]
    EmptyPrompt,
    #[error("issue numbers start at 1")]
    InvalidIssue,
    #[error("a queue runs 1 to {max} tasks at once, not {concurrency}")]
    InvalidConcurrency { concurrency: u32, max: u32 },
    #[error("task {item} is not queued")]
    NotQueued { item: QueueItemId },
    #[error("task {item} is still queued or running; cancel it first")]
    StillActive { item: QueueItemId },
    #[error("task {item} is finished already")]
    Finished { item: QueueItemId },
}

/// One subscription of a provider, or its agent CLI's default login
/// (`None`), as the queue tracks its usage limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub(crate) struct LimitKey {
    pub provider: Provider,
    pub subscription: Option<SubscriptionId>,
}

/// Whether a subscription is at a usage limit, as far as known.
/// A subscription a task could run on, with what the rest of the
/// service knows of its usage limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Candidate {
    pub key: LimitKey,
    pub known: KnownLimit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum KnownLimit {
    Free,
    /// At a limit that resets then, if known.
    Limited {
        resets_at: Option<u64>,
    },
}

/// A task the queue starts now: its chat starts with these settings and
/// `message`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TaskStart {
    pub item: QueueItemId,
    pub repository: RepositoryId,
    pub message: String,
    pub agent_cli: Option<AgentCli>,
    pub permission_mode: Option<PermissionMode>,
}

/// What the service saw of a task's chat after a step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ChatSeen {
    pub activity: Activity,
    pub pull_request: Option<OpenPullRequest>,
    /// Where the chat runs now.
    pub on: LimitKey,
}

/// A task's chat's open pull request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OpenPullRequest {
    pub number: u64,
    pub url: String,
}

/// A chat the queue watches for one of its tasks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Watched {
    pub chat: ChatId,
    /// Whether the queue needs to know how the chat's last turn ended
    /// once no turn runs: the task is running.
    pub needs_end: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Activity {
    /// Setting up or running a turn.
    Working { waiting_for_you: bool },
    Idle {
        /// How the last turn ended; [`TurnEnd::Unknown`] when the queue
        /// did not ask.
        ended: TurnEnd,
        /// The usage limit the last turn stopped at, if it did.
        usage_limit: Option<LimitSeen>,
        /// Whether the developer marked the chat as done.
        done: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TurnEnd {
    Completed,
    Failed { reason: String },
    Interrupted,
    SessionEnded,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LimitSeen {
    pub limit: String,
    pub key: LimitKey,
    pub resets_at: Option<u64>,
}

/// What [`TaskQueue::cancel`] leaves for the service to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AfterCancel {
    Nothing,
    /// Stop the chat's work: its running turn, or the first message that
    /// waits for its setup script.
    StopChat(ChatId),
}

/// Every repository's queue.
#[derive(Debug, Default)]
pub(crate) struct TaskQueue {
    last_item: u64,
    repositories: BTreeMap<RepositoryId, RepositoryState>,
    /// Every repository's tasks, in line order.
    items: Vec<Item>,
    /// Usage limits of agent CLIs' default logins that tasks stopped at,
    /// with when they reset, if known. The service keeps those of
    /// subscriptions; they come in with [`Candidate`]s.
    limits: BTreeMap<LimitKey, Option<u64>>,
    /// Subscriptions' usage limits the developer told the queue to try
    /// again ([`TaskQueue::resume`]), with the reset the service knew then.
    /// A new limit there counts again.
    forgiven: BTreeMap<LimitKey, Option<u64>>,
    /// Repositories whose queued tasks all wait for usage limits, with the
    /// first reset, if known. Set by [`TaskQueue::plan`].
    waiting_for_limits: BTreeMap<RepositoryId, Option<u64>>,
    /// Whether [`TaskQueue::plan`] has something to look at.
    plan_due: bool,
    /// Repositories whose queue changed since [`TaskQueue::take_changed`].
    changed: BTreeSet<RepositoryId>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct RepositoryState {
    settings: QueueSettings,
    paused: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Item {
    id: QueueItemId,
    repository: RepositoryId,
    task: QueueTask,
    agent_cli: Option<AgentCli>,
    permission_mode: Option<PermissionMode>,
    state: ItemState,
}

/// A task's state as kept and stored (not a protocol type):
/// [`QueueItemState`] with absolute reset times.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ItemState {
    Queued,
    Starting,
    Running {
        chat: ChatId,
        #[serde(skip)]
        waiting_for_you: bool,
    },
    NeedsAttention {
        chat: Option<ChatId>,
        reason: Attention,
    },
    Completed {
        chat: ChatId,
    },
    PullRequestReady {
        chat: ChatId,
        number: u64,
        url: String,
    },
    Cancelled {
        chat: Option<ChatId>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Attention {
    StartFailed {
        reason: String,
    },
    TurnFailed {
        reason: String,
    },
    Interrupted,
    SessionEnded,
    /// No turn end came: the first message waits for nothing, for example
    /// after the setup script failed.
    Stopped,
    UsageLimit {
        limit: String,
        subscription: Option<SubscriptionId>,
        resets_at: Option<u64>,
    },
    ServiceRestarted,
}

impl ItemState {
    /// Whether the task holds a slot of its repository's queue.
    fn active(&self) -> bool {
        matches!(self, Self::Starting | Self::Running { .. })
    }

    /// The chat the queue watches for the task.
    fn watched_chat(&self) -> Option<ChatId> {
        match self {
            Self::Running { chat, .. }
            | Self::Completed { chat }
            | Self::NeedsAttention {
                chat: Some(chat), ..
            } => Some(*chat),
            _ => None,
        }
    }

    fn shown(&self, now: u64) -> QueueItemState {
        match self {
            Self::Queued => QueueItemState::Queued,
            Self::Starting => QueueItemState::Starting,
            Self::Running {
                chat,
                waiting_for_you,
            } => QueueItemState::Running {
                chat: *chat,
                waiting_for_you: *waiting_for_you,
            },
            Self::NeedsAttention { chat, reason } => QueueItemState::NeedsAttention {
                chat: *chat,
                reason: reason.shown(now),
            },
            Self::Completed { chat } => QueueItemState::Completed { chat: *chat },
            Self::PullRequestReady { chat, number, url } => QueueItemState::PullRequestReady {
                chat: *chat,
                number: *number,
                url: url.clone(),
            },
            Self::Cancelled { chat } => QueueItemState::Cancelled { chat: *chat },
        }
    }
}

impl Attention {
    fn shown(&self, now: u64) -> AttentionReason {
        match self {
            Self::StartFailed { reason } => AttentionReason::StartFailed {
                reason: reason.clone(),
            },
            Self::TurnFailed { reason } => AttentionReason::TurnFailed {
                reason: reason.clone(),
            },
            Self::Interrupted => AttentionReason::Interrupted,
            Self::SessionEnded => AttentionReason::SessionEnded,
            Self::Stopped => AttentionReason::Stopped,
            Self::UsageLimit {
                limit,
                subscription,
                resets_at,
            } => AttentionReason::UsageLimit {
                limit: limit.clone(),
                subscription: *subscription,
                resets_in_seconds: resets_at.map(|at| at.saturating_sub(now)),
            },
            Self::ServiceRestarted => AttentionReason::ServiceRestarted,
        }
    }
}

/// The contents of `queue.json`, storage version 1. Separate from the
/// protocol types, so a change of the local interface cannot change it.
#[derive(Debug, Default, Serialize, Deserialize)]
struct QueueV1 {
    last_item: u64,
    repositories: Vec<RepositoryV1>,
    items: Vec<ItemV1>,
    limits: Vec<LimitV1>,
}

#[derive(Debug, Serialize, Deserialize)]
struct RepositoryV1 {
    repository: RepositoryId,
    settings: SettingsV1,
    paused: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
struct SettingsV1 {
    concurrency: u32,
    agent_cli: Option<AgentCli>,
    permission_mode: Option<PermissionMode>,
}

#[derive(Debug, Serialize, Deserialize)]
struct ItemV1 {
    id: QueueItemId,
    repository: RepositoryId,
    task: TaskV1,
    agent_cli: Option<AgentCli>,
    permission_mode: Option<PermissionMode>,
    state: ItemState,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum TaskV1 {
    Prompt { text: String },
    Issue { number: u64 },
}

impl From<SettingsV1> for QueueSettings {
    fn from(stored: SettingsV1) -> Self {
        Self {
            concurrency: stored.concurrency,
            agent_cli: stored.agent_cli,
            permission_mode: stored.permission_mode,
        }
    }
}

impl From<QueueSettings> for SettingsV1 {
    fn from(settings: QueueSettings) -> Self {
        Self {
            concurrency: settings.concurrency,
            agent_cli: settings.agent_cli,
            permission_mode: settings.permission_mode,
        }
    }
}

impl From<ItemV1> for Item {
    fn from(stored: ItemV1) -> Self {
        Self {
            id: stored.id,
            repository: stored.repository,
            task: match stored.task {
                TaskV1::Prompt { text } => QueueTask::Prompt { text },
                TaskV1::Issue { number } => QueueTask::Issue { number },
            },
            agent_cli: stored.agent_cli,
            permission_mode: stored.permission_mode,
            state: stored.state,
        }
    }
}

impl From<&Item> for ItemV1 {
    fn from(item: &Item) -> Self {
        Self {
            id: item.id,
            repository: item.repository,
            task: match &item.task {
                QueueTask::Prompt { text } => TaskV1::Prompt { text: text.clone() },
                QueueTask::Issue { number } => TaskV1::Issue { number: *number },
            },
            agent_cli: item.agent_cli,
            permission_mode: item.permission_mode,
            state: item.state.clone(),
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct LimitV1 {
    #[serde(flatten)]
    key: LimitKey,
    resets_at: Option<u64>,
}

#[derive(Serialize)]
struct VersionedRef<'a> {
    version: u32,
    #[serde(flatten)]
    queue: &'a QueueV1,
}

#[derive(Deserialize)]
struct Versioned {
    version: u32,
}

#[derive(Deserialize)]
struct VersionedV1 {
    #[serde(flatten)]
    queue: QueueV1,
}

impl TaskQueue {
    /// Reads the queue from the data directory; a directory without one
    /// has an empty queue. Tasks that were starting are queued again, and
    /// running ones need attention: no turn survives a restart.
    pub fn load(store: &Store) -> Result<Self, StorageError> {
        let Some(bytes) = store.read_file(QUEUE_FILE)? else {
            return Ok(Self::default());
        };
        let path = store.file_path(QUEUE_FILE);
        let corrupt = |source| StorageError::Corrupt {
            path: path.clone(),
            source,
        };
        let Versioned { version } = serde_json::from_slice(&bytes).map_err(corrupt)?;
        if version != QUEUE_VERSION {
            return Err(StorageError::UnsupportedVersion {
                found: version,
                supported: QUEUE_VERSION,
            });
        }
        let VersionedV1 { queue: stored } = serde_json::from_slice(&bytes).map_err(corrupt)?;
        let mut queue = Self {
            last_item: stored.last_item,
            repositories: stored
                .repositories
                .into_iter()
                .map(|stored| {
                    let state = RepositoryState {
                        settings: stored.settings.into(),
                        paused: stored.paused,
                    };
                    (stored.repository, state)
                })
                .collect(),
            items: stored.items.into_iter().map(Item::from).collect(),
            limits: stored
                .limits
                .into_iter()
                .map(|limit| (limit.key, limit.resets_at))
                .collect(),
            plan_due: true,
            ..Self::default()
        };
        for item in &mut queue.items {
            item.state = match std::mem::replace(&mut item.state, ItemState::Queued) {
                ItemState::Starting => ItemState::Queued,
                ItemState::Running { chat, .. } => ItemState::NeedsAttention {
                    chat: Some(chat),
                    reason: Attention::ServiceRestarted,
                },
                state => state,
            };
        }
        Ok(queue)
    }

    /// Replaces `queue.json` atomically.
    pub fn save(&self, store: &Store) -> Result<(), StorageError> {
        let stored = QueueV1 {
            last_item: self.last_item,
            repositories: self
                .repositories
                .iter()
                .map(|(repository, state)| RepositoryV1 {
                    repository: *repository,
                    settings: state.settings.into(),
                    paused: state.paused,
                })
                .collect(),
            items: self.items.iter().map(ItemV1::from).collect(),
            limits: self
                .limits
                .iter()
                .map(|(key, resets_at)| LimitV1 {
                    key: *key,
                    resets_at: *resets_at,
                })
                .collect(),
        };
        store.write_json(
            QUEUE_FILE,
            &VersionedRef {
                version: QUEUE_VERSION,
                queue: &stored,
            },
        )
    }

    /// The repository's queue as the desktop app shows it.
    pub fn view(
        &self,
        repository: RepositoryId,
        now: u64,
        update_waiting: bool,
    ) -> RepositoryQueue {
        let state = self.state(repository);
        let items: Vec<QueueItem> = self
            .items
            .iter()
            .filter(|item| item.repository == repository)
            .map(|item| QueueItem {
                item: item.id,
                task: item.task.clone(),
                title: item.task.title(),
                agent_cli: item.agent_cli,
                permission_mode: item.permission_mode,
                state: item.state.shown(now),
            })
            .collect();
        let busy = items.iter().any(|item| {
            matches!(
                item.state,
                QueueItemState::Queued | QueueItemState::Starting | QueueItemState::Running { .. }
            )
        });
        let status = if state.paused {
            QueueStatus::Paused
        } else if update_waiting && self.queued(repository).next().is_some() {
            QueueStatus::WaitingForUpdate
        } else if let Some(resets_at) = self.waiting_for_limits.get(&repository) {
            QueueStatus::WaitingForUsageLimits {
                resets_in_seconds: resets_at.map(|at| at.saturating_sub(now)),
            }
        } else if busy {
            QueueStatus::Running
        } else {
            QueueStatus::Idle
        };
        RepositoryQueue {
            repository,
            settings: state.settings,
            status,
            items,
        }
    }

    /// The repository the task is in.
    pub fn repository_of(&self, item: QueueItemId) -> Result<RepositoryId, QueueError> {
        self.item(item).map(|item| item.repository)
    }

    pub fn enqueue(
        &mut self,
        repository: RepositoryId,
        task: QueueTask,
        agent_cli: Option<AgentCli>,
        permission_mode: Option<PermissionMode>,
    ) -> Result<QueueItemId, QueueError> {
        let task = match task {
            QueueTask::Prompt { text } if text.trim().is_empty() => {
                return Err(QueueError::EmptyPrompt);
            }
            QueueTask::Issue { number: 0 } => return Err(QueueError::InvalidIssue),
            task => task,
        };
        self.last_item += 1;
        let id = QueueItemId(self.last_item);
        self.items.push(Item {
            id,
            repository,
            task,
            agent_cli,
            permission_mode,
            state: ItemState::Queued,
        });
        self.touch(repository);
        Ok(id)
    }

    /// Moves a queued task to place `to` among its repository's queued
    /// tasks.
    pub fn move_item(&mut self, id: QueueItemId, to: usize) -> Result<(), QueueError> {
        let item = self.item(id)?;
        if item.state != ItemState::Queued {
            return Err(QueueError::NotQueued { item: id });
        }
        let repository = item.repository;
        let from = self.position(id)?;
        let item = self.items.remove(from);
        // Where the `to`th queued task of the repository is now; after the
        // last one when there are fewer.
        let queued: Vec<usize> = self.queued(repository).collect();
        let index = match queued.get(to) {
            Some(&index) => index,
            None => queued.last().map_or(self.items.len(), |&last| last + 1),
        };
        self.items.insert(index, item);
        self.touch(repository);
        Ok(())
    }

    pub fn cancel(&mut self, id: QueueItemId) -> Result<AfterCancel, QueueError> {
        let index = self.position(id)?;
        let item = &mut self.items[index];
        let (state, cancelled) = match &item.state {
            ItemState::Queued | ItemState::Starting => {
                (ItemState::Cancelled { chat: None }, AfterCancel::Nothing)
            }
            ItemState::Running { chat, .. } => (
                ItemState::Cancelled { chat: Some(*chat) },
                AfterCancel::StopChat(*chat),
            ),
            ItemState::NeedsAttention { chat, .. } => {
                (ItemState::Cancelled { chat: *chat }, AfterCancel::Nothing)
            }
            ItemState::Completed { .. }
            | ItemState::PullRequestReady { .. }
            | ItemState::Cancelled { .. } => return Err(QueueError::Finished { item: id }),
        };
        item.state = state;
        let repository = item.repository;
        self.touch(repository);
        Ok(cancelled)
    }

    /// Removes a task that is neither queued nor running from the list.
    pub fn remove(&mut self, id: QueueItemId) -> Result<(), QueueError> {
        let index = self.position(id)?;
        let item = &self.items[index];
        if item.state == ItemState::Queued || item.state.active() {
            return Err(QueueError::StillActive { item: id });
        }
        let repository = item.repository;
        self.items.remove(index);
        self.touch(repository);
        Ok(())
    }

    pub fn pause(&mut self, repository: RepositoryId) {
        self.repositories.entry(repository).or_default().paused = true;
        self.touch(repository);
    }

    /// Starts the repository's tasks again and tries the usage limits the
    /// queue waited for again: its own and `known`, the subscriptions' the
    /// service knows now. Limits belong to subscriptions, not repositories,
    /// so every repository's queue tries them again.
    pub fn resume(
        &mut self,
        repository: RepositoryId,
        known: impl IntoIterator<Item = (LimitKey, Option<u64>)>,
    ) {
        self.repositories.entry(repository).or_default().paused = false;
        self.limits.clear();
        self.forgiven = known.into_iter().collect();
        self.waiting_for_limits.clear();
        // Every repository's queue may have waited for those limits.
        let repositories: Vec<RepositoryId> = self.repositories.keys().copied().collect();
        for repository in repositories {
            self.touch(repository);
        }
        self.touch(repository);
    }

    pub fn change_settings(
        &mut self,
        repository: RepositoryId,
        settings: QueueSettings,
    ) -> Result<(), QueueError> {
        if !(1..=MAX_CONCURRENCY).contains(&settings.concurrency) {
            return Err(QueueError::InvalidConcurrency {
                concurrency: settings.concurrency,
                max: MAX_CONCURRENCY,
            });
        }
        self.repositories.entry(repository).or_default().settings = settings;
        self.touch(repository);
        Ok(())
    }

    /// Looks for tasks to start again at the next [`TaskQueue::plan`], for
    /// usage limits that have reset meanwhile.
    pub fn wake(&mut self) {
        if !self.limits.is_empty() || !self.waiting_for_limits.is_empty() {
            self.plan_due = true;
        }
    }

    /// The chats the queue watches.
    pub fn watched(&self) -> Vec<Watched> {
        self.items
            .iter()
            .filter_map(|item| {
                Some(Watched {
                    chat: item.state.watched_chat()?,
                    needs_end: matches!(item.state, ItemState::Running { .. }),
                })
            })
            .collect()
    }

    /// Takes in what the service saw of a task's chat after a step.
    pub fn observe(&mut self, chat: ChatId, seen: ChatSeen) {
        let Some(item) = self
            .items
            .iter_mut()
            .find(|item| item.state.watched_chat() == Some(chat))
        else {
            return;
        };
        let repository = item.repository;
        // A turn that runs on keeps its slot, even with a pull request.
        let turn_runs = matches!(
            (&item.state, &seen.activity),
            (ItemState::Running { .. }, Activity::Working { .. })
        );
        let next = match (&item.state, seen.activity) {
            // Ready for review, however the chat got there.
            _ if !turn_runs && seen.pull_request.is_some() => {
                seen.pull_request.map(|OpenPullRequest { number, url }| {
                    ItemState::PullRequestReady { chat, number, url }
                })
            }
            (
                ItemState::Running {
                    waiting_for_you: was,
                    ..
                },
                Activity::Working { waiting_for_you },
            ) => (*was != waiting_for_you).then_some(ItemState::Running {
                chat,
                waiting_for_you,
            }),
            (
                ItemState::Running { .. },
                Activity::Idle {
                    ended, usage_limit, ..
                },
            ) => Some(match usage_limit {
                Some(limit) => {
                    // The service keeps subscriptions' limits itself.
                    if limit.key.subscription.is_none() {
                        self.limits.insert(limit.key, limit.resets_at);
                    }
                    self.forgiven.remove(&limit.key);
                    ItemState::NeedsAttention {
                        chat: Some(chat),
                        reason: Attention::UsageLimit {
                            limit: limit.limit,
                            subscription: limit.key.subscription,
                            resets_at: limit.resets_at,
                        },
                    }
                }
                None => match ended {
                    TurnEnd::Completed => {
                        // A turn ran to its end there: not limited.
                        self.limits.remove(&seen.on);
                        ItemState::Completed { chat }
                    }
                    TurnEnd::Failed { reason } => ItemState::NeedsAttention {
                        chat: Some(chat),
                        reason: Attention::TurnFailed { reason },
                    },
                    TurnEnd::Interrupted => ItemState::NeedsAttention {
                        chat: Some(chat),
                        reason: Attention::Interrupted,
                    },
                    TurnEnd::Unknown => ItemState::NeedsAttention {
                        chat: Some(chat),
                        reason: Attention::Stopped,
                    },
                    TurnEnd::SessionEnded => ItemState::NeedsAttention {
                        chat: Some(chat),
                        reason: Attention::SessionEnded,
                    },
                },
            }),
            // The developer took the chat up again.
            (ItemState::NeedsAttention { .. }, Activity::Working { waiting_for_you }) => {
                Some(ItemState::Running {
                    chat,
                    waiting_for_you,
                })
            }
            (ItemState::NeedsAttention { .. }, Activity::Idle { done: true, .. }) => {
                Some(ItemState::Completed { chat })
            }
            _ => None,
        };
        if let Some(next) = next {
            item.state = next;
            self.touch(repository);
        }
    }

    /// Picks the tasks to start now, in line order, and marks them as
    /// starting. `candidates` names the subscriptions a task could run on,
    /// or why it cannot start, which the task then needs attention for.
    pub fn plan(
        &mut self,
        now: u64,
        mut candidates: impl FnMut(&TaskStart) -> Result<Vec<Candidate>, CommandError>,
    ) -> Vec<TaskStart> {
        if !std::mem::take(&mut self.plan_due) {
            return Vec::new();
        }
        self.limits
            .retain(|_, resets_at| resets_at.is_none_or(|at| at > now));
        let before = std::mem::take(&mut self.waiting_for_limits);
        let repositories: BTreeSet<RepositoryId> = self
            .items
            .iter()
            .filter(|item| item.state == ItemState::Queued)
            .map(|item| item.repository)
            .collect();
        let mut starts = Vec::new();
        for repository in repositories {
            let state = self.state(repository);
            if state.paused {
                continue;
            }
            let active = self
                .items
                .iter()
                .filter(|item| item.repository == repository && item.state.active())
                .count();
            let mut free = usize::try_from(state.settings.concurrency)
                .unwrap_or(usize::MAX)
                .saturating_sub(active);
            // Whether a task waits for usage limits, and when they reset.
            let mut blocked = false;
            let mut resets = Vec::new();
            let queued: Vec<usize> = self.queued(repository).collect();
            for index in queued {
                if free == 0 {
                    break;
                }
                let start = Self::start_of(&self.items[index], state.settings);
                let next = match candidates(&start) {
                    Err(error) => ItemState::NeedsAttention {
                        chat: None,
                        reason: Attention::StartFailed {
                            reason: error.to_string(),
                        },
                    },
                    Ok(subscriptions) => {
                        let limits: Vec<KnownLimit> = subscriptions
                            .iter()
                            .map(|candidate| self.limit_of(candidate, now))
                            .collect();
                        if !limits.contains(&KnownLimit::Free) {
                            blocked = true;
                            resets.extend(limits.into_iter().filter_map(|limit| match limit {
                                KnownLimit::Limited { resets_at } => resets_at,
                                KnownLimit::Free => None,
                            }));
                            continue;
                        }
                        starts.push(start);
                        free -= 1;
                        ItemState::Starting
                    }
                };
                self.items[index].state = next;
            }
            if blocked && starts.iter().all(|start| start.repository != repository) {
                // The first known reset; none when no reset is known.
                self.waiting_for_limits
                    .insert(repository, resets.into_iter().min());
            }
        }
        let mut touched: BTreeSet<RepositoryId> =
            starts.iter().map(|start| start.repository).collect();
        // A repository that began or stopped waiting for usage limits.
        touched.extend(
            before
                .keys()
                .chain(self.waiting_for_limits.keys())
                .filter(|repository| {
                    before.get(repository) != self.waiting_for_limits.get(repository)
                }),
        );
        // Tasks that cannot start changed too.
        touched.extend(self.items.iter().filter_map(|item| match &item.state {
            ItemState::NeedsAttention {
                chat: None,
                reason: Attention::StartFailed { .. },
            } => Some(item.repository),
            _ => None,
        }));
        self.changed.extend(touched);
        starts
    }

    /// The chat of a starting task exists, or could not be started.
    /// Returns the chat and its first message when the task still wants
    /// it: a task cancelled meanwhile keeps the chat without a message.
    pub fn chat_started(
        &mut self,
        id: QueueItemId,
        started: Result<ChatId, CommandError>,
    ) -> Option<(ChatId, String)> {
        let index = self.position(id).ok()?;
        let item = &mut self.items[index];
        let repository = item.repository;
        let first = match (&item.state, started) {
            (ItemState::Starting, Ok(chat)) => {
                item.state = ItemState::Running {
                    chat,
                    waiting_for_you: false,
                };
                Some((chat, item.task.message()))
            }
            (ItemState::Starting, Err(error)) => {
                item.state = ItemState::NeedsAttention {
                    chat: None,
                    reason: Attention::StartFailed {
                        reason: error.to_string(),
                    },
                };
                None
            }
            (ItemState::Cancelled { chat: None }, Ok(chat)) => {
                item.state = ItemState::Cancelled { chat: Some(chat) };
                None
            }
            _ => None,
        };
        self.touch(repository);
        first
    }

    /// The first message of a running task's chat could not be sent.
    pub fn message_failed(&mut self, id: QueueItemId, error: &CommandError) {
        let Ok(index) = self.position(id) else {
            return;
        };
        let item = &mut self.items[index];
        if let ItemState::Running { chat, .. } = item.state {
            item.state = ItemState::NeedsAttention {
                chat: Some(chat),
                reason: Attention::StartFailed {
                    reason: error.to_string(),
                },
            };
            let repository = item.repository;
            self.touch(repository);
        }
    }

    /// The repositories whose queue changed since the last call.
    pub fn take_changed(&mut self) -> BTreeSet<RepositoryId> {
        std::mem::take(&mut self.changed)
    }

    fn touch(&mut self, repository: RepositoryId) {
        self.changed.insert(repository);
        self.plan_due = true;
    }

    fn state(&self, repository: RepositoryId) -> RepositoryState {
        self.repositories
            .get(&repository)
            .copied()
            .unwrap_or_default()
    }

    fn item(&self, id: QueueItemId) -> Result<&Item, QueueError> {
        self.items
            .iter()
            .find(|item| item.id == id)
            .ok_or(QueueError::UnknownItem { item: id })
    }

    fn position(&self, id: QueueItemId) -> Result<usize, QueueError> {
        self.items
            .iter()
            .position(|item| item.id == id)
            .ok_or(QueueError::UnknownItem { item: id })
    }

    /// The indices of the repository's queued tasks, in line order.
    fn queued(&self, repository: RepositoryId) -> impl Iterator<Item = usize> + '_ {
        self.items
            .iter()
            .enumerate()
            .filter(move |(_, item)| {
                item.repository == repository && item.state == ItemState::Queued
            })
            .map(|(index, _)| index)
    }

    /// The usage limit a task's subscription is at now: one the queue
    /// saw, or one the service knows that the developer did not tell the
    /// queue to try again.
    fn limit_of(&self, candidate: &Candidate, now: u64) -> KnownLimit {
        let current = |resets_at: Option<u64>| resets_at.is_none_or(|at| at > now);
        if let Some(&resets_at) = self.limits.get(&candidate.key)
            && current(resets_at)
        {
            return KnownLimit::Limited { resets_at };
        }
        match candidate.known {
            KnownLimit::Limited { resets_at }
                if current(resets_at) && self.forgiven.get(&candidate.key) != Some(&resets_at) =>
            {
                candidate.known
            }
            KnownLimit::Limited { .. } | KnownLimit::Free => KnownLimit::Free,
        }
    }

    fn start_of(item: &Item, settings: QueueSettings) -> TaskStart {
        TaskStart {
            item: item.id,
            repository: item.repository,
            message: item.task.message(),
            agent_cli: item.agent_cli.or(settings.agent_cli),
            permission_mode: item.permission_mode.or(settings.permission_mode),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_000_000;
    const REPOSITORY: RepositoryId = RepositoryId(1);
    /// The default login of Claude Code: the queue keeps its limits itself.
    const DEFAULT_LOGIN: LimitKey = LimitKey {
        provider: Provider::Anthropic,
        subscription: None,
    };
    const CLAUDE: LimitKey = LimitKey {
        provider: Provider::Anthropic,
        subscription: Some(SubscriptionId(7)),
    };
    const OTHER_CLAUDE: LimitKey = LimitKey {
        provider: Provider::Anthropic,
        subscription: Some(SubscriptionId(8)),
    };

    fn prompt(text: &str) -> QueueTask {
        QueueTask::Prompt {
            text: text.to_owned(),
        }
    }

    fn free(key: LimitKey) -> Candidate {
        Candidate {
            key,
            known: KnownLimit::Free,
        }
    }

    fn limited(key: LimitKey, resets_at: Option<u64>) -> Candidate {
        Candidate {
            key,
            known: KnownLimit::Limited { resets_at },
        }
    }

    fn on(
        candidates: &[Candidate],
    ) -> impl FnMut(&TaskStart) -> Result<Vec<Candidate>, CommandError> + use<> {
        let candidates = candidates.to_vec();
        move |_| Ok(candidates.clone())
    }

    fn enqueue(queue: &mut TaskQueue, text: &str) -> QueueItemId {
        queue.enqueue(REPOSITORY, prompt(text), None, None).unwrap()
    }

    fn states(queue: &TaskQueue) -> Vec<QueueItemState> {
        queue
            .view(REPOSITORY, NOW, false)
            .items
            .into_iter()
            .map(|item| item.state)
            .collect()
    }

    fn ids(queue: &TaskQueue) -> Vec<QueueItemId> {
        queue
            .view(REPOSITORY, NOW, false)
            .items
            .into_iter()
            .map(|item| item.item)
            .collect()
    }

    fn seen(activity: Activity) -> ChatSeen {
        ChatSeen {
            activity,
            pull_request: None,
            on: CLAUDE,
        }
    }

    fn idle(ended: TurnEnd) -> ChatSeen {
        seen(Activity::Idle {
            ended,
            usage_limit: None,
            done: false,
        })
    }

    fn working() -> ChatSeen {
        seen(Activity::Working {
            waiting_for_you: false,
        })
    }

    /// Runs the queue's first task into the usage limit of `key`.
    fn hit_limit(queue: &mut TaskQueue, key: LimitKey, resets_at: Option<u64>) {
        let first = enqueue(queue, "first");
        queue.plan(NOW, on(&[free(key)]));
        queue.chat_started(first, Ok(ChatId(3)));
        queue.observe(
            ChatId(3),
            seen(Activity::Idle {
                ended: TurnEnd::Completed,
                usage_limit: Some(LimitSeen {
                    limit: "5-hour limit".to_owned(),
                    key,
                    resets_at,
                }),
                done: false,
            }),
        );
    }

    #[test]
    fn a_long_prompt_is_cut_to_a_title() {
        let title = prompt(&format!("\n  {}\nsecond line", "a".repeat(200))).title();
        assert_eq!(title.chars().count(), TITLE_LENGTH);
        assert!(title.ends_with('…'));
    }

    #[test]
    fn a_cancelled_start_keeps_its_chat_without_a_message() {
        let mut queue = TaskQueue::default();
        let item = enqueue(&mut queue, "task");
        assert_eq!(queue.plan(NOW, on(&[free(CLAUDE)])).len(), 1);
        assert_eq!(queue.cancel(item), Ok(AfterCancel::Nothing));

        assert_eq!(queue.chat_started(item, Ok(ChatId(3))), None);
        assert_eq!(
            states(&queue),
            [QueueItemState::Cancelled {
                chat: Some(ChatId(3))
            }]
        );
    }

    #[test]
    fn a_default_login_limit_past_its_reset_no_longer_holds_tasks_back() {
        let mut queue = TaskQueue::default();
        hit_limit(&mut queue, DEFAULT_LOGIN, Some(NOW + 60));
        enqueue(&mut queue, "second");

        assert_eq!(queue.plan(NOW, on(&[free(DEFAULT_LOGIN)])), []);
        assert_eq!(
            queue.view(REPOSITORY, NOW, false).status,
            QueueStatus::WaitingForUsageLimits {
                resets_in_seconds: Some(60)
            }
        );
        queue.wake();
        assert_eq!(queue.plan(NOW + 60, on(&[free(DEFAULT_LOGIN)])).len(), 1);
    }

    #[test]
    fn an_interchangeable_subscription_without_a_limit_lets_a_task_start() {
        let mut queue = TaskQueue::default();
        enqueue(&mut queue, "task");

        let starts = queue.plan(NOW, on(&[limited(CLAUDE, None), free(OTHER_CLAUDE)]));

        assert_eq!(starts.len(), 1);
    }

    #[test]
    fn subscriptions_the_service_knows_to_be_limited_wait_until_the_developer_resumes() {
        let mut queue = TaskQueue::default();
        enqueue(&mut queue, "task");
        let both = [limited(CLAUDE, None), limited(OTHER_CLAUDE, Some(NOW + 30))];

        assert_eq!(queue.plan(NOW, on(&both)), []);
        assert_eq!(
            queue.view(REPOSITORY, NOW, false).status,
            QueueStatus::WaitingForUsageLimits {
                resets_in_seconds: Some(30)
            }
        );
        queue.resume(REPOSITORY, [(CLAUDE, None), (OTHER_CLAUDE, Some(NOW + 30))]);
        assert_eq!(queue.plan(NOW, on(&both)).len(), 1);
    }

    #[test]
    fn a_new_limit_after_a_resume_counts_again() {
        let mut queue = TaskQueue::default();
        queue.resume(REPOSITORY, [(CLAUDE, Some(NOW + 30))]);
        enqueue(&mut queue, "task");

        let starts = queue.plan(NOW, on(&[limited(CLAUDE, Some(NOW + 90))]));

        assert_eq!(starts, []);
    }

    #[test]
    fn a_task_taken_up_again_runs_and_completes() {
        let mut queue = TaskQueue::default();
        let item = enqueue(&mut queue, "one");
        queue.plan(NOW, on(&[free(CLAUDE)]));
        queue.chat_started(item, Ok(ChatId(3)));
        queue.observe(ChatId(3), idle(TurnEnd::Interrupted));
        queue.observe(ChatId(3), working());
        assert!(matches!(states(&queue)[0], QueueItemState::Running { .. }));
        queue.observe(ChatId(3), idle(TurnEnd::Completed));
        assert_eq!(
            states(&queue),
            [QueueItemState::Completed { chat: ChatId(3) }]
        );
    }

    #[test]
    fn a_pull_request_opened_while_the_turn_runs_frees_the_slot_once_it_ends() {
        let mut queue = TaskQueue::default();
        let item = enqueue(&mut queue, "one");
        enqueue(&mut queue, "two");
        queue.plan(NOW, on(&[free(CLAUDE)]));
        queue.chat_started(item, Ok(ChatId(3)));
        let pull_request = Some(OpenPullRequest {
            number: 4,
            url: "https://git.example.invalid/pull/4".to_owned(),
        });

        queue.observe(
            ChatId(3),
            ChatSeen {
                pull_request: pull_request.clone(),
                ..working()
            },
        );
        assert_eq!(queue.plan(NOW, on(&[free(CLAUDE)])), []);
        queue.observe(
            ChatId(3),
            ChatSeen {
                pull_request,
                ..idle(TurnEnd::Completed)
            },
        );
        assert!(matches!(
            states(&queue)[0],
            QueueItemState::PullRequestReady { number: 4, .. }
        ));
        assert_eq!(queue.plan(NOW, on(&[free(CLAUDE)])).len(), 1);
    }

    #[test]
    fn a_chat_that_stopped_without_a_turn_end_needs_attention() {
        let mut queue = TaskQueue::default();
        let item = enqueue(&mut queue, "one");
        queue.plan(NOW, on(&[free(CLAUDE)]));
        queue.chat_started(item, Ok(ChatId(3)));

        queue.observe(ChatId(3), idle(TurnEnd::Unknown));

        assert_eq!(
            states(&queue),
            [QueueItemState::NeedsAttention {
                chat: Some(ChatId(3)),
                reason: AttentionReason::Stopped,
            }]
        );
    }

    #[test]
    fn moving_counts_only_queued_tasks_and_a_place_past_the_end_is_last() {
        let mut queue = TaskQueue::default();
        let running = enqueue(&mut queue, "running");
        queue.plan(NOW, on(&[free(CLAUDE)]));
        let a = enqueue(&mut queue, "a");
        let b = enqueue(&mut queue, "b");
        let c = enqueue(&mut queue, "c");

        queue.move_item(c, 0).unwrap();
        assert_eq!(ids(&queue), [running, c, a, b]);
        queue.move_item(c, 99).unwrap();
        assert_eq!(ids(&queue), [running, a, b, c]);
        assert_eq!(
            queue.move_item(running, 0),
            Err(QueueError::NotQueued { item: running })
        );
    }

    #[test]
    fn starting_and_running_tasks_fill_the_slots() {
        let mut queue = TaskQueue::default();
        queue
            .change_settings(
                REPOSITORY,
                QueueSettings {
                    concurrency: 2,
                    ..QueueSettings::default()
                },
            )
            .unwrap();
        let first = enqueue(&mut queue, "one");
        enqueue(&mut queue, "two");
        enqueue(&mut queue, "three");

        assert_eq!(queue.plan(NOW, on(&[free(CLAUDE)])).len(), 2);
        queue.chat_started(first, Ok(ChatId(3)));
        queue.wake();
        assert_eq!(queue.plan(NOW, on(&[free(CLAUDE)])), []);
        queue.observe(ChatId(3), idle(TurnEnd::Completed));
        assert_eq!(queue.plan(NOW, on(&[free(CLAUDE)])).len(), 1);
    }

    #[test]
    fn a_task_that_cannot_start_needs_attention_and_the_next_one_starts() {
        let mut queue = TaskQueue::default();
        let broken = queue
            .enqueue(REPOSITORY, prompt("broken"), Some(AgentCli::Codex), None)
            .unwrap();
        let next = enqueue(&mut queue, "next");
        let refused = CommandError::AgentCliNotConfigured {
            agent_cli: AgentCli::Codex,
        };

        let starts = queue.plan(NOW, |start: &TaskStart| match start.agent_cli {
            Some(AgentCli::Codex) => Err(refused.clone()),
            _ => Ok(vec![free(CLAUDE)]),
        });

        let started: Vec<QueueItemId> = starts.iter().map(|start| start.item).collect();
        assert_eq!(started, [next]);
        assert_eq!(
            states(&queue)[0],
            QueueItemState::NeedsAttention {
                chat: None,
                reason: AttentionReason::StartFailed {
                    reason: refused.to_string()
                }
            }
        );
        assert_eq!(ids(&queue), [broken, next]);
    }

    #[test]
    fn a_task_takes_the_queues_agent_cli_and_mode_unless_it_chose_its_own() {
        let mut queue = TaskQueue::default();
        queue
            .change_settings(
                REPOSITORY,
                QueueSettings {
                    concurrency: 2,
                    agent_cli: Some(AgentCli::Codex),
                    permission_mode: Some(PermissionMode::FullAccess),
                },
            )
            .unwrap();
        enqueue(&mut queue, "queue's");
        queue
            .enqueue(
                REPOSITORY,
                prompt("own"),
                Some(AgentCli::ClaudeCode),
                Some(PermissionMode::Supervised),
            )
            .unwrap();

        let starts = queue.plan(NOW, on(&[free(CLAUDE)]));

        let chosen: Vec<_> = starts
            .iter()
            .map(|start| (start.agent_cli, start.permission_mode))
            .collect();
        assert_eq!(
            chosen,
            [
                (Some(AgentCli::Codex), Some(PermissionMode::FullAccess)),
                (Some(AgentCli::ClaudeCode), Some(PermissionMode::Supervised)),
            ]
        );
    }

    #[test]
    fn a_stored_queue_loads_with_cut_starts_and_turns_resolved() {
        let dir = tempfile::tempdir().unwrap();
        let (store, _) = Store::open(dir.path(), AgentCli::ClaudeCode).unwrap();
        let mut queue = TaskQueue::default();
        queue
            .change_settings(
                REPOSITORY,
                QueueSettings {
                    concurrency: 2,
                    ..QueueSettings::default()
                },
            )
            .unwrap();
        let running = enqueue(&mut queue, "running");
        let starting = queue
            .enqueue(REPOSITORY, QueueTask::Issue { number: 12 }, None, None)
            .unwrap();
        queue.plan(NOW, on(&[free(CLAUDE)]));
        queue.chat_started(running, Ok(ChatId(3)));
        queue.pause(REPOSITORY);
        queue.save(&store).unwrap();

        let loaded = TaskQueue::load(&store).unwrap();

        let shown = loaded.view(REPOSITORY, NOW, false);
        assert_eq!(shown.status, QueueStatus::Paused);
        assert_eq!(shown.settings.concurrency, 2);
        assert_eq!(shown.items[1].task, QueueTask::Issue { number: 12 });
        assert_eq!(
            states(&loaded),
            [
                QueueItemState::NeedsAttention {
                    chat: Some(ChatId(3)),
                    reason: AttentionReason::ServiceRestarted,
                },
                QueueItemState::Queued,
            ]
        );
        assert_eq!(ids(&loaded), [running, starting]);
    }

    #[test]
    fn a_queue_of_a_newer_storage_version_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let (store, _) = Store::open(dir.path(), AgentCli::ClaudeCode).unwrap();
        std::fs::write(
            store.file_path(QUEUE_FILE),
            format!(r#"{{"version": {}}}"#, QUEUE_VERSION + 1),
        )
        .unwrap();

        assert!(matches!(
            TaskQueue::load(&store),
            Err(StorageError::UnsupportedVersion { found, supported })
                if found == QUEUE_VERSION + 1 && supported == QUEUE_VERSION
        ));
    }
}
