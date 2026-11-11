//! A temporary git repository.

use std::io;
use std::path::Path;
use std::process::Command;

use tempfile::TempDir;

/// An empty git repository in a temporary directory, removed on drop.
#[derive(Debug)]
pub struct TestRepo {
    dir: TempDir,
}

impl TestRepo {
    /// # Errors
    ///
    /// Returns an error when the directory cannot be created or `git init`
    /// fails.
    pub fn new() -> io::Result<Self> {
        let dir = tempfile::tempdir()?;
        let status = Command::new("git")
            .args(["init", "--quiet"])
            .arg(dir.path())
            .status()?;
        if !status.success() {
            return Err(io::Error::other(format!("git init failed: {status}")));
        }
        Ok(Self { dir })
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        self.dir.path()
    }
}
