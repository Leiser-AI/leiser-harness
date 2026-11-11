//! The harness service as its own process (ADR 0005, ADR 0007).
//!
//! ```text
//! leiser-harness-service --data-dir <dir> [--fake-agent-cli <program>]
//! ```
//!
//! Runs the harness service on `<dir>` and serves its local interface
//! there, so the desktop app can be closed or updated while turns run. One
//! service runs per data directory.
//!
//! An update replaces this program's file, for example with the desktop
//! app's bundle. The service notices, announces that an update waits, keeps
//! every turn running until none runs, then starts the new program on the
//! same data and exits. The new service resumes every agent session.

mod agents;

use std::fs::{File, TryLockError};
use std::io;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant, SystemTime};

use leiser_service::{ENDPOINT_FILE, ServeError, ServiceProgram, StartError};

/// Holds the data directory for one service at a time.
const LOCK_FILE: &str = "service.lock";
/// How long a starting service waits for the one before it to exit, as
/// after an update.
const LOCK_TIMEOUT: Duration = Duration::from_secs(10);
const LOCK_RETRY: Duration = Duration::from_millis(100);
/// How often the service checks whether its program was replaced.
const UPDATE_CHECK: Duration = Duration::from_secs(1);

#[derive(Debug, thiserror::Error)]
enum RunError {
    #[error(transparent)]
    Args(#[from] leiser_service::ArgsError),
    #[error("could not find this program's path: {0}")]
    Program(io::Error),
    #[error("could not use the data directory: {0}")]
    DataDir(io::Error),
    #[error("another harness service runs on {}", .0.display())]
    AlreadyRunning(PathBuf),
    #[error("could not start the async runtime: {0}")]
    Runtime(io::Error),
    #[error(transparent)]
    FakeAgentCli(#[from] agents::FakeAgentCliError),
    #[error(transparent)]
    Start(#[from] StartError),
    #[error(transparent)]
    Serve(#[from] ServeError),
    #[error("could not start the updated service: {0}")]
    Relaunch(io::Error),
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            // The launcher sends error output to the service log.
            #[allow(clippy::print_stderr)]
            {
                eprintln!("leiser-harness-service: {error}");
            }
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), RunError> {
    let program = std::env::current_exe().map_err(RunError::Program)?;
    let service = ServiceProgram::parse(program, std::env::args_os().skip(1))?;
    let data_dir = service.data_dir();
    std::fs::create_dir_all(data_dir).map_err(RunError::DataDir)?;
    let lock = lock(data_dir)?;

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(RunError::Runtime)?;
    runtime.block_on(serve_until_updated(&service))?;
    // Ends every agent CLI process before the new service resumes them.
    drop(runtime);
    // The new service must not read the old port.
    let _ = std::fs::remove_file(data_dir.join(ENDPOINT_FILE));
    service.launch().map_err(RunError::Relaunch)?;
    // The new service takes over the data directory once this one exits.
    drop(lock);
    Ok(())
}

/// Takes the data directory's lock, waiting for a service that is just
/// exiting.
fn lock(data_dir: &Path) -> Result<File, RunError> {
    let file = File::create(data_dir.join(LOCK_FILE)).map_err(RunError::DataDir)?;
    let deadline = Instant::now() + LOCK_TIMEOUT;
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(file),
            Err(TryLockError::WouldBlock) if Instant::now() < deadline => {
                std::thread::sleep(LOCK_RETRY);
            }
            Err(TryLockError::WouldBlock) => {
                return Err(RunError::AlreadyRunning(data_dir.to_path_buf()));
            }
            Err(TryLockError::Error(error)) => return Err(RunError::DataDir(error)),
        }
    }
}

/// Runs the service until its program was replaced and no turn runs.
async fn serve_until_updated(program: &ServiceProgram) -> Result<(), RunError> {
    let installed = Fingerprint::of(program.program());
    let service = leiser_service::start(agents::service_config(program)?)?;
    let _interface = leiser_service::serve(service.connect(), program.data_dir()).await?;
    loop {
        tokio::time::sleep(UPDATE_CHECK).await;
        // A program that is missing for a moment is being replaced.
        let current = Fingerprint::of(program.program());
        if current.is_some() && current != installed {
            break;
        }
    }
    tracing::info!("the service program was replaced; the update waits for running turns");
    service.stop_for_update().await;
    Ok(())
}

/// What tells two versions of a program file apart.
#[derive(Debug, PartialEq, Eq)]
struct Fingerprint {
    len: u64,
    modified: Option<SystemTime>,
    /// A replaced file is a new file, even with the same size and time.
    #[cfg(unix)]
    inode: u64,
}

impl Fingerprint {
    fn of(path: &Path) -> Option<Self> {
        let metadata = std::fs::metadata(path).ok()?;
        Some(Self {
            len: metadata.len(),
            modified: metadata.modified().ok(),
            #[cfg(unix)]
            inode: std::os::unix::fs::MetadataExt::ino(&metadata),
        })
    }
}
