//! What the harness service keeps on disk, so chats survive a restart.
//!
//! Layout of the data directory (see ADR 0006):
//!
//! - `state.json`: repositories, subscriptions, chats, their agent session
//!   ids per agent CLI, their subscription per provider and a pending
//!   handoff. Holds
//!   the storage version of the whole directory and is replaced atomically
//!   on every change.
//! - `chats/<chat id>.jsonl`: one chat's transcript, one
//!   [`TranscriptEntry`] per line, only ever appended to.
//! - `subscriptions/<subscription id>/`: a subscription's config directory
//!   for its agent CLI (see `subscriptions.rs`). API keys are not here:
//!   they are in the OS keychain.
//! - `repo-checks/<repository id>.json`: the repository's last
//!   `repo-check` result, replaced atomically when the next one finishes.
//!   It has its own storage version, [`REPO_CHECK_VERSION`].
//! - `todos/<chat id>.json`: one chat's todo list, replaced atomically on
//!   every change. A chat without the file has an empty todo list. Added in
//!   version 2 without a version bump: an older harness service never
//!   reads or writes these files, so it cannot damage them.
//!
//! The stored types are separate from the protocol types, except for
//! [`TranscriptEntry`] and the `repo-check` report. A change that old data no longer parses with bumps
//! [`VERSION`] and migrates the older version in [`Store::open`].

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::repo_check::Outcome;
use crate::{AgentCli, AgentEvent, PermissionMode, Provider, SubscriptionKind, TranscriptEntry};

/// The storage version this harness service writes.
///
/// - 1: one agent session per chat; transcript entries without agent CLI.
/// - 2: a chat can change agent CLI: it keeps one agent session per agent
///   CLI and a pending handoff, and agent entries name their agent CLI.
///   Later additions that version 2 data without them reads as defaults:
///   whether a chat works in a worktree, whether it is done and when its
///   status last changed, its permission mode (Auto), and
///   transcript entries for finished repository scripts.
/// - 3: subscriptions, and each chat's subscription per provider. Later
///   additions that version 3 data without them reads as defaults: a
///   chat's permission mode (Auto), transcript entries for the
///   developer's answers and for continuing on another subscription,
///   whether a subscription is interchangeable (no), and a chat's usage
///   limit and the messages it holds back (none).
const VERSION: u32 = 3;

const STATE_FILE: &str = "state.json";
const CHATS_DIR: &str = "chats";
const SUBSCRIPTIONS_DIR: &str = "subscriptions";

const REPO_CHECKS_DIR: &str = "repo-checks";

/// The storage version of a stored `repo-check` result.
///
/// - 1: the outcome of the run, with the report before opt-outs.
const REPO_CHECK_VERSION: u32 = 1;

#[derive(Serialize)]
struct RepoCheckRef<'a> {
    version: u32,
    outcome: &'a Outcome,
}

#[derive(Deserialize)]
struct StoredRepoCheck {
    version: u32,
    outcome: serde_json::Value,
}
const TODOS_DIR: &str = "todos";

/// Why the harness service could not read or write its data.
#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("could not access {}: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("{} is corrupt: {source}", path.display())]
    Corrupt {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error(
        "storage version {found} is not supported; this harness service \
         reads versions up to {supported}"
    )]
    UnsupportedVersion { found: u32, supported: u32 },
}

/// The contents of `state.json`, storage version 3.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct StateV3 {
    pub next_id: u64,
    pub repositories: Vec<RepositoryV1>,
    pub subscriptions: Vec<SubscriptionV3>,
    pub chats: Vec<ChatV3>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SubscriptionV3 {
    pub id: u64,
    pub provider: Provider,
    pub name: String,
    pub kind: SubscriptionKind,
    /// Whether chats may move to and from it by themselves at a usage
    /// limit.
    #[serde(default)]
    pub interchangeable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ChatV3 {
    pub id: u64,
    pub repository: u64,
    pub working_directory: PathBuf,
    /// The agent CLI the chat's next message goes to.
    pub agent_cli: AgentCli,
    /// The chat's agent sessions, at most one per agent CLI.
    pub agent_sessions: Vec<AgentSessionV2>,
    /// The handoff the chat's next message carries.
    pub handoff: Option<HandoffV2>,
    /// Whether `working_directory` is a worktree rather than the main
    /// checkout.
    pub worktree: bool,
    /// Whether the developer marked the chat as done.
    pub done: bool,
    /// When the chat's status last changed, in the harness service's count
    /// of status changes; the sidebar lists the latest first.
    pub status_changed: u64,
    /// The chat's permission mode. Chats stored before permission modes
    /// existed run in the default, Auto.
    #[serde(default)]
    pub permission_mode: PermissionMode,
    /// The chat's subscription per provider; a provider without one runs
    /// on the agent CLI's own default login.
    pub subscriptions: Vec<ChatSubscriptionV3>,
    /// The usage limit the chat's last turn stopped at, until the next
    /// turn starts.
    #[serde(default)]
    pub usage_limit: Option<UsageLimitV3>,
    /// Messages queued when the usage limit was hit; they go out after the
    /// chat's next turn.
    #[serde(default)]
    pub held: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct UsageLimitV3 {
    pub limit: String,
    /// The subscription that hit it; `None` is the default login.
    pub subscription: Option<u64>,
    /// When it resets, in seconds since the Unix epoch.
    pub resets_at: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ChatSubscriptionV3 {
    pub provider: Provider,
    pub subscription: u64,
}

/// The contents of `state.json`, storage version 2.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct StateV2 {
    next_id: u64,
    repositories: Vec<RepositoryV1>,
    chats: Vec<ChatV2>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ChatV2 {
    id: u64,
    repository: u64,
    working_directory: PathBuf,
    /// The agent CLI the chat's next message goes to.
    agent_cli: AgentCli,
    /// The chat's agent sessions, at most one per agent CLI.
    agent_sessions: Vec<AgentSessionV2>,
    /// The handoff the chat's next message carries.
    handoff: Option<HandoffV2>,
    /// Whether `working_directory` is a worktree rather than the main
    /// checkout.
    #[serde(default)]
    worktree: bool,
    /// Whether the developer marked the chat as done.
    #[serde(default)]
    done: bool,
    /// When the chat's status last changed; `0` for chats stored before
    /// the count existed.
    #[serde(default)]
    status_changed: u64,
    /// The chat's permission mode; chats stored before permission modes
    /// existed run in the default, Auto.
    #[serde(default)]
    permission_mode: PermissionMode,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct AgentSessionV2 {
    pub agent_cli: AgentCli,
    /// The agent CLI's id of the agent session.
    pub id: String,
    /// How many transcript entries, from the first, the agent session has
    /// seen; set when the chat changes away from its agent CLI.
    pub known: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum HandoffV2 {
    /// For a new agent session: the chat so far.
    NewSession,
    /// For a resumed agent session: the entries after the first `known`.
    Resume { known: usize },
}

/// The contents of `todos/<chat id>.json`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct TodoListV1 {
    /// The last todo id given out; ids are never used again.
    pub last_id: u64,
    pub todos: Vec<TodoV1>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct TodoV1 {
    pub id: u64,
    pub text: String,
    pub done: bool,
}

/// The contents of `state.json`, storage version 1.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct StateV1 {
    next_id: u64,
    repositories: Vec<RepositoryV1>,
    chats: Vec<ChatV1>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RepositoryV1 {
    pub id: u64,
    pub path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ChatV1 {
    id: u64,
    repository: u64,
    working_directory: PathBuf,
    /// The agent CLI's id of the chat's agent session, once it has one.
    agent_session: Option<String>,
}

/// A transcript line of storage version 1.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum TranscriptEntryV1 {
    UserMessage { text: String },
    Agent { event: AgentEvent },
}

/// Only the version, read before the rest so each version parses on its own.
#[derive(Deserialize)]
struct Versioned {
    version: u32,
}

#[derive(Serialize)]
struct VersionedRef<'a> {
    version: u32,
    #[serde(flatten)]
    state: &'a StateV3,
}

#[derive(Deserialize)]
struct VersionedV3 {
    #[serde(flatten)]
    state: StateV3,
}

#[derive(Deserialize)]
struct VersionedV2 {
    #[serde(flatten)]
    state: StateV2,
}

#[derive(Deserialize)]
struct VersionedV1 {
    #[serde(flatten)]
    state: StateV1,
}

/// The harness service's data directory.
#[derive(Debug)]
pub(crate) struct Store {
    dir: PathBuf,
}

impl Store {
    /// Opens the data directory, creating it if needed, and reads the state.
    /// Data of an older storage version is migrated; version 1 knew one
    /// agent CLI, taken to be `agent_cli`.
    pub fn open(dir: &Path, agent_cli: AgentCli) -> Result<(Self, StateV3), StorageError> {
        let store = Self {
            dir: dir.to_path_buf(),
        };
        for dir in [CHATS_DIR, TODOS_DIR] {
            let dir = store.dir.join(dir);
            fs::create_dir_all(&dir).map_err(|source| StorageError::Io { path: dir, source })?;
        }
        let path = store.dir.join(STATE_FILE);
        let state = match fs::read(&path) {
            Ok(bytes) => parse_state(&path, &bytes)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => Stored::V3(StateV3::default()),
            Err(source) => return Err(StorageError::Io { path, source }),
        };
        let state = match state {
            Stored::V1(state) => {
                for chat in &state.chats {
                    store.repair_transcript(chat.id)?;
                }
                let state = store.migrate_v1(state, agent_cli)?;
                store.migrate_v2(state)?
            }
            Stored::V2(state) => {
                for chat in &state.chats {
                    store.repair_transcript(chat.id)?;
                }
                store.migrate_v2(state)?
            }
            Stored::V3(state) => {
                for chat in &state.chats {
                    store.repair_transcript(chat.id)?;
                }
                state
            }
        };
        Ok((store, state))
    }

    /// Converts storage version 2 to 3 and writes it: no subscriptions yet,
    /// so every chat runs on the agent CLIs' own default logins.
    fn migrate_v2(&self, state: StateV2) -> Result<StateV3, StorageError> {
        let state = StateV3 {
            next_id: state.next_id,
            repositories: state.repositories,
            subscriptions: Vec::new(),
            chats: state
                .chats
                .into_iter()
                .map(|chat| ChatV3 {
                    id: chat.id,
                    repository: chat.repository,
                    working_directory: chat.working_directory,
                    agent_cli: chat.agent_cli,
                    agent_sessions: chat.agent_sessions,
                    handoff: chat.handoff,
                    worktree: chat.worktree,
                    done: chat.done,
                    status_changed: chat.status_changed,
                    permission_mode: chat.permission_mode,
                    subscriptions: Vec::new(),
                    usage_limit: None,
                    held: Vec::new(),
                })
                .collect(),
        };
        self.save(&state)?;
        Ok(state)
    }

    /// Converts storage version 1 to 2. Each transcript is replaced
    /// atomically, and `state.json` is written only after the migration to
    /// the current version, so a crash midway leaves version 1, which
    /// migrates again: version 1 reads converted lines.
    fn migrate_v1(&self, state: StateV1, agent_cli: AgentCli) -> Result<StateV2, StorageError> {
        for chat in &state.chats {
            let path = self.transcript_path(chat.id);
            let text = match fs::read_to_string(&path) {
                Ok(text) => text,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(source) => return Err(StorageError::Io { path, source }),
            };
            let mut converted = Vec::new();
            for line in text.lines() {
                let entry =
                    match serde_json::from_str(line).map_err(|source| StorageError::Corrupt {
                        path: path.clone(),
                        source,
                    })? {
                        TranscriptEntryV1::UserMessage { text } => {
                            TranscriptEntry::UserMessage { text }
                        }
                        TranscriptEntryV1::Agent { event } => {
                            TranscriptEntry::Agent { agent_cli, event }
                        }
                    };
                serde_json::to_writer(&mut converted, &entry).map_err(|source| {
                    StorageError::Io {
                        path: path.clone(),
                        source: source.into(),
                    }
                })?;
                converted.push(b'\n');
            }
            replace(&path, &converted)?;
        }
        let state = StateV2 {
            next_id: state.next_id,
            repositories: state.repositories,
            chats: state
                .chats
                .into_iter()
                .map(|chat| ChatV2 {
                    id: chat.id,
                    repository: chat.repository,
                    working_directory: chat.working_directory,
                    agent_cli,
                    agent_sessions: chat
                        .agent_session
                        .map(|id| AgentSessionV2 {
                            agent_cli,
                            id,
                            known: 0,
                        })
                        .into_iter()
                        .collect(),
                    handoff: None,
                    worktree: false,
                    done: false,
                    status_changed: 0,
                    permission_mode: PermissionMode::default(),
                })
                .collect(),
        };
        Ok(state)
    }

    /// Replaces `state.json` atomically: a crash leaves the old or the new
    /// state, never a mix.
    pub fn save(&self, state: &StateV3) -> Result<(), StorageError> {
        let path = self.dir.join(STATE_FILE);
        let mut bytes = serde_json::to_vec_pretty(&VersionedRef {
            version: VERSION,
            state,
        })
        .map_err(|source| StorageError::Io {
            path: path.clone(),
            source: source.into(),
        })?;
        bytes.push(b'\n');
        replace(&path, &bytes)
    }

    /// Appends an entry to a chat's transcript.
    pub fn append(&self, chat: u64, entry: &TranscriptEntry) -> Result<(), StorageError> {
        let path = self.transcript_path(chat);
        let io_error = |source| StorageError::Io {
            path: path.clone(),
            source,
        };
        let mut line = serde_json::to_vec(entry).map_err(|source| io_error(source.into()))?;
        line.push(b'\n');
        // One write per line: a crash can only cut off the last line,
        // which `repair_transcript` removes on the next start.
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .and_then(|mut file| file.write_all(&line))
            .map_err(io_error)
    }

    /// Reads a chat's transcript; a chat without one has an empty one.
    pub fn transcript(&self, chat: u64) -> Result<Vec<TranscriptEntry>, StorageError> {
        let path = self.transcript_path(chat);
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(source) => return Err(StorageError::Io { path, source }),
        };
        text.lines()
            .map(|line| {
                serde_json::from_str(line).map_err(|source| StorageError::Corrupt {
                    path: path.clone(),
                    source,
                })
            })
            .collect()
    }

    /// Reads a chat's todo list; a chat without one has an empty one.
    pub fn todos(&self, chat: u64) -> Result<TodoListV1, StorageError> {
        let path = self.todos_path(chat);
        match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|source| StorageError::Corrupt { path, source }),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(TodoListV1::default()),
            Err(source) => Err(StorageError::Io { path, source }),
        }
    }

    /// Replaces a chat's todo list atomically.
    pub fn save_todos(&self, chat: u64, todos: &TodoListV1) -> Result<(), StorageError> {
        let path = self.todos_path(chat);
        let mut bytes = serde_json::to_vec_pretty(todos).map_err(|source| StorageError::Io {
            path: path.clone(),
            source: source.into(),
        })?;
        bytes.push(b'\n');
        replace(&path, &bytes)
    }

    fn todos_path(&self, chat: u64) -> PathBuf {
        self.dir.join(TODOS_DIR).join(format!("{chat}.json"))
    }

    /// Removes a last line that a crash cut off, so the next entry starts
    /// on a line of its own.
    fn repair_transcript(&self, chat: u64) -> Result<(), StorageError> {
        let path = self.transcript_path(chat);
        let io_error = |source| StorageError::Io {
            path: path.clone(),
            source,
        };
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(source) => return Err(io_error(source)),
        };
        if bytes.is_empty() || bytes.ends_with(b"\n") {
            return Ok(());
        }
        let complete = bytes
            .iter()
            .rposition(|&byte| byte == b'\n')
            .map_or(0, |i| i + 1);
        OpenOptions::new()
            .write(true)
            .open(&path)
            .and_then(|file| file.set_len(u64::try_from(complete).map_err(io::Error::other)?))
            .map_err(io_error)
    }

    /// The directory that holds one directory per subscription.
    pub fn subscriptions_dir(&self) -> PathBuf {
        self.dir.join(SUBSCRIPTIONS_DIR)
    }

    /// Stores the repository's last `repo-check` result.
    pub fn save_repo_check(&self, repository: u64, outcome: &Outcome) -> Result<(), StorageError> {
        let dir = self.dir.join(REPO_CHECKS_DIR);
        fs::create_dir_all(&dir).map_err(|source| StorageError::Io {
            path: dir.clone(),
            source,
        })?;
        let path = self.repo_check_path(repository);
        let mut bytes = serde_json::to_vec_pretty(&RepoCheckRef {
            version: REPO_CHECK_VERSION,
            outcome,
        })
        .map_err(|source| StorageError::Io {
            path: path.clone(),
            source: source.into(),
        })?;
        bytes.push(b'\n');
        replace(&path, &bytes)
    }

    /// The repository's last `repo-check` result; `None` if it never ran.
    pub fn repo_check(&self, repository: u64) -> Result<Option<Outcome>, StorageError> {
        let path = self.repo_check_path(repository);
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(source) => return Err(StorageError::Io { path, source }),
        };
        let corrupt = |source| StorageError::Corrupt {
            path: path.clone(),
            source,
        };
        let stored: StoredRepoCheck = serde_json::from_slice(&bytes).map_err(corrupt)?;
        if stored.version != REPO_CHECK_VERSION {
            return Err(StorageError::UnsupportedVersion {
                found: stored.version,
                supported: REPO_CHECK_VERSION,
            });
        }
        serde_json::from_value(stored.outcome)
            .map(Some)
            .map_err(corrupt)
    }

    fn repo_check_path(&self, repository: u64) -> PathBuf {
        self.dir
            .join(REPO_CHECKS_DIR)
            .join(format!("{repository}.json"))
    }

    fn transcript_path(&self, chat: u64) -> PathBuf {
        self.dir.join(CHATS_DIR).join(format!("{chat}.jsonl"))
    }
}

/// Replaces a file atomically: write a temporary file, then rename it.
fn replace(path: &Path, bytes: &[u8]) -> Result<(), StorageError> {
    let mut temporary = path.as_os_str().to_owned();
    temporary.push(".tmp");
    let temporary = PathBuf::from(temporary);
    let io_error = |source| StorageError::Io {
        path: path.to_path_buf(),
        source,
    };
    let mut file = File::create(&temporary).map_err(io_error)?;
    file.write_all(bytes).map_err(io_error)?;
    file.sync_all().map_err(io_error)?;
    fs::rename(&temporary, path).map_err(io_error)
}

/// The state as stored, in its storage version.
enum Stored {
    V1(StateV1),
    V2(StateV2),
    V3(StateV3),
}

fn parse_state(path: &Path, bytes: &[u8]) -> Result<Stored, StorageError> {
    let corrupt = |source| StorageError::Corrupt {
        path: path.to_path_buf(),
        source,
    };
    let Versioned { version } = serde_json::from_slice(bytes).map_err(corrupt)?;
    match version {
        // A new version gets a variant of `Stored`; `Store::open` migrates
        // the older ones to the current one.
        1 => Ok(Stored::V1(
            serde_json::from_slice::<VersionedV1>(bytes)
                .map_err(corrupt)?
                .state,
        )),
        2 => Ok(Stored::V2(
            serde_json::from_slice::<VersionedV2>(bytes)
                .map_err(corrupt)?
                .state,
        )),
        3 => Ok(Stored::V3(
            serde_json::from_slice::<VersionedV3>(bytes)
                .map_err(corrupt)?
                .state,
        )),
        found => Err(StorageError::UnsupportedVersion {
            found,
            supported: VERSION,
        }),
    }
}
