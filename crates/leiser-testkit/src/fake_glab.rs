//! A fake GitLab CLI for testing `GlabGitHost` against canned answers: the
//! `fake-glab` program answers `glab api` requests from a file in the host
//! account's config directory (`GLAB_CONFIG_DIR`), honours `If-None-Match`
//! with `304 Not Modified` as GitLab does, handles `glab auth login`, and
//! logs every request there.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The file in the config directory with the answers, by request path (or
/// `"<METHOD> <path>"` to match a specific HTTP method).
pub const FAKE_GLAB_ANSWERS_FILE: &str = "fake-glab-answers.json";
/// The file in the config directory where `fake-glab` logs requests, one
/// JSON object per line.
pub const FAKE_GLAB_REQUESTS_FILE: &str = "fake-glab-requests.jsonl";

/// What `fake-glab` answers to a request path, such as
/// `projects/org%2Frepo/merge_requests/1`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FakeGlabAnswer {
    /// The entity tag; a request whose `If-None-Match` names it gets `304
    /// Not Modified`.
    pub etag: Option<String>,
    /// JSON, or the text of a job trace as a JSON string.
    pub body: serde_json::Value,
}

/// A request `fake-glab` received.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FakeGlabRequest {
    pub method: String,
    pub path: String,
    pub hostname: Option<String>,
    pub if_none_match: Option<String>,
    #[serde(default)]
    pub fields: BTreeMap<String, String>,
    /// Ambient token/host variables that leaked into `fake-glab`'s
    /// environment, if any.
    #[serde(default)]
    pub leaked_env: Vec<String>,
    /// The HTTP status it answered with (0 for `auth login`).
    pub status: u16,
}

/// The answers and request log of `fake-glab` in one config directory.
#[derive(Debug, Clone)]
pub struct FakeGlab {
    config_dir: PathBuf,
}

impl FakeGlab {
    /// `fake-glab` run with `GLAB_CONFIG_DIR` set to `config_dir`.
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
        self.put(
            path,
            FakeGlabAnswer {
                etag: Some(etag.to_owned()),
                body,
            },
        )
    }

    /// Answers requests for `path` with `body` and no entity tag.
    ///
    /// # Errors
    ///
    /// Returns an error when the answers file cannot be read or written.
    pub fn answer_without_etag(&self, path: &str, body: serde_json::Value) -> io::Result<()> {
        self.put(path, FakeGlabAnswer { etag: None, body })
    }

    fn put(&self, path: &str, answer: FakeGlabAnswer) -> io::Result<()> {
        let mut answers = self.answers()?;
        answers.insert(path.to_owned(), answer);
        std::fs::create_dir_all(&self.config_dir)?;
        std::fs::write(
            self.config_dir.join(FAKE_GLAB_ANSWERS_FILE),
            serde_json::to_vec_pretty(&answers)?,
        )
    }

    /// The answers by request path.
    ///
    /// # Errors
    ///
    /// Returns an error when the answers file cannot be read.
    pub fn answers(&self) -> io::Result<BTreeMap<String, FakeGlabAnswer>> {
        match std::fs::read(self.config_dir.join(FAKE_GLAB_ANSWERS_FILE)) {
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
    pub fn take_requests(&self) -> io::Result<Vec<FakeGlabRequest>> {
        let path = self.config_dir.join(FAKE_GLAB_REQUESTS_FILE);
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
