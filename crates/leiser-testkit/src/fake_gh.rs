//! A fake GitHub CLI for testing `GhGitHost` against canned answers: the
//! `fake-gh` program answers `gh api` requests from a file in the host
//! account's config directory (`GH_CONFIG_DIR`), honours `If-None-Match`
//! with `304 Not Modified` as GitHub does, and logs every request there.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The file in the config directory with the answers, by request path.
pub const FAKE_GH_ANSWERS_FILE: &str = "fake-gh-answers.json";
/// The file in the config directory where `fake-gh` logs requests, one
/// JSON object per line.
pub const FAKE_GH_REQUESTS_FILE: &str = "fake-gh-requests.jsonl";

/// What `fake-gh` answers to a request path, such as `repos/o/r/pulls/1`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FakeGhAnswer {
    /// The entity tag; a request whose `If-None-Match` names it gets `304
    /// Not Modified`.
    pub etag: Option<String>,
    /// JSON, or the text of a log as a JSON string.
    pub body: serde_json::Value,
}

/// A request `fake-gh` got.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FakeGhRequest {
    pub path: String,
    pub if_none_match: Option<String>,
    /// The HTTP status it answered with.
    pub status: u16,
}

/// The answers and request log of `fake-gh` in one config directory.
#[derive(Debug, Clone)]
pub struct FakeGh {
    config_dir: PathBuf,
}

impl FakeGh {
    /// `fake-gh` run with `GH_CONFIG_DIR` set to `config_dir`.
    #[must_use]
    pub fn new(config_dir: &Path) -> Self {
        Self {
            config_dir: config_dir.to_path_buf(),
        }
    }

    /// Answers requests for `path` with `body` and `etag`.
    ///
    /// # Errors
    ///
    /// Returns an error when the answers file cannot be read or written.
    pub fn answer(&self, path: &str, etag: &str, body: serde_json::Value) -> io::Result<()> {
        let mut answers = self.answers()?;
        answers.insert(
            path.to_owned(),
            FakeGhAnswer {
                etag: Some(etag.to_owned()),
                body,
            },
        );
        std::fs::create_dir_all(&self.config_dir)?;
        std::fs::write(
            self.config_dir.join(FAKE_GH_ANSWERS_FILE),
            serde_json::to_vec_pretty(&answers)?,
        )
    }

    /// The answers by request path.
    ///
    /// # Errors
    ///
    /// Returns an error when the answers file cannot be read.
    pub fn answers(&self) -> io::Result<BTreeMap<String, FakeGhAnswer>> {
        match std::fs::read(self.config_dir.join(FAKE_GH_ANSWERS_FILE)) {
            Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(BTreeMap::new()),
            Err(error) => Err(error),
        }
    }

    /// The requests so far, oldest first, and forgets them.
    ///
    /// # Errors
    ///
    /// Returns an error when the request log cannot be read.
    pub fn take_requests(&self) -> io::Result<Vec<FakeGhRequest>> {
        let path = self.config_dir.join(FAKE_GH_REQUESTS_FILE);
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error),
        };
        std::fs::remove_file(path)?;
        text.lines()
            .map(|line| serde_json::from_str(line).map_err(io::Error::from))
            .collect()
    }
}
