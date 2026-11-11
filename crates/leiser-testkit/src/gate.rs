//! A gate that holds a scripted turn until the test opens it.

use std::io;
use std::path::PathBuf;

use tempfile::TempDir;

use crate::Step;

/// Holds a turn of the fake agent CLI at [`Gate::step`] until [`Gate::open`]
/// is called, so a test can act while the turn runs without timing guesses.
#[derive(Debug)]
pub struct Gate {
    file: PathBuf,
    /// Holds the gate file; removed when the gate is dropped.
    _dir: TempDir,
}

impl Gate {
    /// # Errors
    ///
    /// Returns an error when the temporary directory cannot be created.
    pub fn new() -> io::Result<Self> {
        let dir = tempfile::tempdir()?;
        Ok(Self {
            file: dir.path().join("open"),
            _dir: dir,
        })
    }

    /// A step that waits until the gate is open.
    #[must_use]
    pub fn step(&self) -> Step {
        Step::WaitForGate {
            gate: self.file.clone(),
        }
    }

    /// Opens the gate; turns waiting at it go on.
    ///
    /// # Errors
    ///
    /// Returns an error when the gate file cannot be written.
    pub fn open(&self) -> io::Result<()> {
        std::fs::write(&self.file, b"")
    }
}
