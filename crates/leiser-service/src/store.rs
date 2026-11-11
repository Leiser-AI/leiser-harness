//! What the harness service keeps on disk, so chats survive a restart.
//!
//! Layout of the data directory (see ADR 0006):
//!
//! - `state.json`: repositories, chats and their agent session ids. Holds
//!   the storage version of the whole directory and is replaced atomically
//!   on every change.
//! - `chats/<chat id>.jsonl`: one chat's transcript, one
//!   [`TranscriptEntry`] per line, only ever appended to.
//!
//! The stored types are separate from the protocol types, except for
//! [`TranscriptEntry`]. A change that old data no longer parses with bumps
//! [`VERSION`] and migrates the older version in [`Store::open`].

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::TranscriptEntry;

/// The storage version this harness service writes.
const VERSION: u32 = 1;

const STATE_FILE: &str = "state.json";
const CHATS_DIR: &str = "chats";

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

/// The contents of `state.json`, storage version 1.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct StateV1 {
    pub next_id: u64,
    pub repositories: Vec<RepositoryV1>,
    pub chats: Vec<ChatV1>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RepositoryV1 {
    pub id: u64,
    pub path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ChatV1 {
    pub id: u64,
    pub repository: u64,
    pub working_directory: PathBuf,
    /// The agent CLI's id of the chat's agent session, once it has one.
    pub agent_session: Option<String>,
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
    state: &'a StateV1,
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
    pub fn open(dir: &Path) -> Result<(Self, StateV1), StorageError> {
        let store = Self {
            dir: dir.to_path_buf(),
        };
        let chats = store.dir.join(CHATS_DIR);
        fs::create_dir_all(&chats).map_err(|source| StorageError::Io {
            path: chats.clone(),
            source,
        })?;
        let path = store.dir.join(STATE_FILE);
        let state = match fs::read(&path) {
            Ok(bytes) => parse_state(&path, &bytes)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => StateV1::default(),
            Err(source) => return Err(StorageError::Io { path, source }),
        };
        for chat in &state.chats {
            store.repair_transcript(chat.id)?;
        }
        Ok((store, state))
    }

    /// Replaces `state.json` atomically: a crash leaves the old or the new
    /// state, never a mix.
    pub fn save(&self, state: &StateV1) -> Result<(), StorageError> {
        let path = self.dir.join(STATE_FILE);
        let temporary = self.dir.join(format!("{STATE_FILE}.tmp"));
        let io_error = |source| StorageError::Io {
            path: path.clone(),
            source,
        };
        let mut bytes = serde_json::to_vec_pretty(&VersionedRef {
            version: VERSION,
            state,
        })
        .map_err(|source| io_error(source.into()))?;
        bytes.push(b'\n');
        let mut file = File::create(&temporary).map_err(io_error)?;
        file.write_all(&bytes).map_err(io_error)?;
        file.sync_all().map_err(io_error)?;
        fs::rename(&temporary, &path).map_err(io_error)
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

    fn transcript_path(&self, chat: u64) -> PathBuf {
        self.dir.join(CHATS_DIR).join(format!("{chat}.jsonl"))
    }
}

fn parse_state(path: &Path, bytes: &[u8]) -> Result<StateV1, StorageError> {
    let corrupt = |source| StorageError::Corrupt {
        path: path.to_path_buf(),
        source,
    };
    let Versioned { version } = serde_json::from_slice(bytes).map_err(corrupt)?;
    match version {
        // Add migrations here: parse the old version, convert it to the
        // current one.
        1 => Ok(serde_json::from_slice::<VersionedV1>(bytes)
            .map_err(corrupt)?
            .state),
        found => Err(StorageError::UnsupportedVersion {
            found,
            supported: VERSION,
        }),
    }
}
