//! Shells in pseudo terminals, for the desktop app's terminal panel.
//!
//! A [`Terminal`] runs the developer's shell in a pseudo terminal (a PTY on
//! macOS and Linux, `ConPTY` on Windows) in a given directory, so programs
//! that redraw the screen work as in any terminal. Input goes in as bytes;
//! output and the shell's exit come out as [`TerminalEvent`]s.
//!
//! Dropping a terminal ends its shell and closes the pseudo terminal, which
//! also ends the programs that still run in it. If the owning process dies
//! without dropping it, the operating system closes the pseudo terminal and
//! hangs up its programs the same way.
//!
//! The shell is the developer's login shell on macOS and Linux, and
//! PowerShell on Windows (PowerShell 7 when installed).

use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::Duration;

use portable_pty::{Child, CommandBuilder, MasterPty, PtySize};

use crate::startup::StartupQuery;

mod startup;

/// How often a terminal checks whether its shell has exited.
const EXIT_POLL: Duration = Duration::from_millis(50);
/// How long a terminal waits for the output of an exited shell before it
/// reports the exit. On Windows the output only ends once the terminal is
/// dropped.
const OUTPUT_GRACE: Duration = Duration::from_millis(300);
/// How much output one [`TerminalEvent::Output`] carries at most.
const READ_BUFFER: usize = 16 * 1024;

/// The size of a terminal in character cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Size {
    pub rows: u16,
    pub cols: u16,
}

impl From<Size> for PtySize {
    fn from(size: Size) -> Self {
        Self {
            rows: size.rows.max(1),
            cols: size.cols.max(1),
            pixel_width: 0,
            pixel_height: 0,
        }
    }
}

/// What a terminal reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalEvent {
    /// Output of the shell or the programs it runs, as raw bytes for a
    /// terminal emulator. A chunk may end inside a UTF-8 character.
    Output(Vec<u8>),
    /// The shell exited. This is the terminal's last event.
    Exited { exit_code: u32 },
}

#[derive(Debug, thiserror::Error)]
pub enum TerminalError {
    #[error("{} is not a directory", directory.display())]
    NotADirectory { directory: PathBuf },
    #[error("could not open a pseudo terminal: {reason}")]
    Open { reason: String },
    #[error("could not start the shell {shell}: {reason}")]
    Start { shell: String, reason: String },
    #[error("could not write to the terminal: {0}")]
    Write(#[source] io::Error),
    #[error("could not resize the terminal: {reason}")]
    Resize { reason: String },
}

/// The receiver of a terminal's events. It runs on the terminal's own
/// threads and should not block.
pub type EventHandler = dyn Fn(TerminalEvent) + Send + Sync;

/// The pseudo terminal's input; `None` once the terminal is closed.
type Input = Arc<Mutex<Option<Box<dyn Write + Send>>>>;

/// A shell running in a pseudo terminal. Dropping it ends the shell and
/// the programs it runs.
pub struct Terminal {
    master: Option<Box<dyn MasterPty + Send>>,
    writer: Input,
    child: Arc<Mutex<Box<dyn Child + Send + Sync>>>,
    process_id: Option<u32>,
}

impl std::fmt::Debug for Terminal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Terminal")
            .field("process_id", &self.process_id)
            .finish_non_exhaustive()
    }
}

impl Terminal {
    /// Starts the developer's shell in `directory`. Its output and exit
    /// reach `on_event`.
    ///
    /// # Errors
    ///
    /// When `directory` is no directory, or the pseudo terminal or the
    /// shell cannot start.
    pub fn open(
        directory: &Path,
        size: Size,
        on_event: impl Fn(TerminalEvent) + Send + Sync + 'static,
    ) -> Result<Self, TerminalError> {
        // The PTY library falls back to the home directory on its own.
        if !directory.is_dir() {
            return Err(TerminalError::NotADirectory {
                directory: directory.to_path_buf(),
            });
        }
        let pair = portable_pty::native_pty_system()
            .openpty(size.into())
            .map_err(open_error)?;
        let command = shell_command(directory);
        let shell = command.get_shell();
        let child = pair
            .slave
            .spawn_command(command)
            .map_err(|error| TerminalError::Start {
                shell: shell.clone(),
                reason: error.to_string(),
            })?;
        // The shell holds its own end; ours would keep the terminal open
        // after the shell exits.
        drop(pair.slave);
        let process_id = child.process_id();
        let reader = pair.master.try_clone_reader().map_err(open_error)?;
        let writer = pair.master.take_writer().map_err(open_error)?;
        let terminal = Self {
            master: Some(pair.master),
            writer: Arc::new(Mutex::new(Some(writer))),
            child: Arc::new(Mutex::new(child)),
            process_id,
        };
        let on_event: Arc<EventHandler> = Arc::new(on_event);
        let (output_ended, output_end) = mpsc::channel();
        // Only `ConPTY` waits for an answer before it starts the shell; on
        // Unix a shell's own cursor queries are for the receiver to answer.
        let startup =
            cfg!(windows).then(|| (StartupQuery::default(), Arc::clone(&terminal.writer)));
        spawn("terminal-output", {
            let on_event = Arc::clone(&on_event);
            move || forward_output(reader, startup, &*on_event, &output_ended)
        });
        spawn("terminal-exit", {
            let child = Arc::clone(&terminal.child);
            move || report_exit(&child, &output_end, &*on_event)
        });
        tracing::debug!(%shell, ?process_id, "terminal opened");
        Ok(terminal)
    }

    /// Sends input, such as typed keys, to the shell.
    ///
    /// # Errors
    ///
    /// When the pseudo terminal no longer takes input.
    pub fn write(&self, input: &[u8]) -> Result<(), TerminalError> {
        write_input(&self.writer, input).map_err(TerminalError::Write)
    }

    /// Changes the terminal's size; programs in it are told to redraw.
    ///
    /// # Errors
    ///
    /// When the operating system refuses the new size.
    pub fn resize(&self, size: Size) -> Result<(), TerminalError> {
        let Some(master) = &self.master else {
            return Ok(());
        };
        master
            .resize(size.into())
            .map_err(|error| TerminalError::Resize {
                reason: error.to_string(),
            })
    }

    /// The shell's process id, where the operating system reports one.
    #[must_use]
    pub fn process_id(&self) -> Option<u32> {
        self.process_id
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        {
            let mut child = self.child.lock().unwrap_or_else(PoisonError::into_inner);
            if matches!(child.try_wait(), Ok(None)) {
                // On Unix a hangup first, then a kill if the shell stays.
                if let Err(error) = child.kill() {
                    tracing::warn!(%error, "could not end the terminal's shell");
                }
            }
        }
        // Closing the pseudo terminal hangs up the programs still in it; on
        // Windows it ends every process attached to the console.
        self.writer
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        self.master.take();
        tracing::debug!(process_id = ?self.process_id, "terminal closed");
    }
}

fn write_input(writer: &Input, input: &[u8]) -> io::Result<()> {
    let mut writer = writer.lock().unwrap_or_else(PoisonError::into_inner);
    let writer = writer
        .as_mut()
        .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "the terminal is closed"))?;
    writer.write_all(input).and_then(|()| writer.flush())
}

fn open_error(error: impl std::fmt::Display) -> TerminalError {
    TerminalError::Open {
        reason: error.to_string(),
    }
}

/// The developer's shell, started in `directory`.
fn shell_command(directory: &Path) -> CommandBuilder {
    let mut command = windows_shell().map_or_else(CommandBuilder::new_default_prog, |shell| {
        let mut command = CommandBuilder::new(shell);
        command.arg("-NoLogo");
        command
    });
    command.cwd(directory);
    command.env("TERM", "xterm-256color");
    command.env("COLORTERM", "truecolor");
    command.env("TERM_PROGRAM", "leiser-harness");
    command
}

/// PowerShell on Windows: PowerShell 7 when it is on the `PATH`, otherwise
/// Windows PowerShell, which every Windows has. `None` elsewhere, where the
/// login shell runs.
fn windows_shell() -> Option<PathBuf> {
    if !cfg!(windows) {
        return None;
    }
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .map(|directory| directory.join("pwsh.exe"))
        .find(|candidate| candidate.is_file())
        .or_else(|| Some(PathBuf::from("powershell.exe")))
}

fn spawn(name: &str, work: impl FnOnce() + Send + 'static) {
    if let Err(error) = thread::Builder::new().name(name.to_owned()).spawn(work) {
        tracing::error!(%error, name, "could not start a terminal thread");
    }
}

/// Passes output on until the pseudo terminal closes. With `startup`, it
/// first answers `ConPTY`'s cursor query (see [`startup`]).
fn forward_output(
    mut reader: Box<dyn Read + Send>,
    mut startup: Option<(StartupQuery, Input)>,
    on_event: &EventHandler,
    output_ended: &mpsc::Sender<()>,
) {
    let mut buffer = vec![0; READ_BUFFER];
    loop {
        let read = match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => &buffer[..read],
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            // On Linux reading a closed PTY fails instead of ending.
            Err(_) => break,
        };
        let output = match &mut startup {
            Some((query, input)) => {
                let filtered = query.filter(read);
                if filtered.answer
                    && let Err(error) = write_input(input, startup::REPLY)
                {
                    tracing::warn!(%error, "could not answer the terminal's cursor query");
                }
                filtered.output
            }
            None => read.to_vec(),
        };
        if !output.is_empty() {
            on_event(TerminalEvent::Output(output));
        }
    }
    if let Some((query, _)) = startup {
        let held = query.finish();
        if !held.is_empty() {
            on_event(TerminalEvent::Output(held));
        }
    }
    let _ = output_ended.send(());
}

/// Reports the shell's exit once it is seen, after the shell's last
/// output.
fn report_exit(
    child: &Mutex<Box<dyn Child + Send + Sync>>,
    output_end: &mpsc::Receiver<()>,
    on_event: &EventHandler,
) {
    let exit_code = loop {
        let status = child
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .try_wait();
        match status {
            Ok(Some(status)) => break status.exit_code(),
            Ok(None) => thread::sleep(EXIT_POLL),
            Err(error) => {
                tracing::warn!(%error, "could not wait for the terminal's shell");
                break 1;
            }
        }
    };
    let _ = output_end.recv_timeout(OUTPUT_GRACE);
    on_event(TerminalEvent::Exited { exit_code });
}
