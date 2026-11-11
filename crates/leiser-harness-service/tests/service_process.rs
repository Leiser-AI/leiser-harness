//! The harness service runs as its own process, outlives its clients, and
//! restarts on its new program after an update.
//!
//! Unix only: the tests stop the service with `kill`. These tests run no
//! turns (the fake agent CLI is another crate's binary); the waiting for
//! running turns is tested at the service seam in the testkit.
#![cfg(unix)]

use std::error::Error;
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::time::{Duration, Instant};

use leiser_service::{Command, ConnectError, LocalClient, Notification, Response};
use leiser_testkit::TestRepo;
use tokio::sync::mpsc;

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

const SERVICE: &str = env!("CARGO_BIN_EXE_leiser-harness-service");
const TIMEOUT: Duration = Duration::from_secs(20);

/// Stops the service processes a test started, also the ones an update
/// started.
#[derive(Default)]
struct Stopper(Vec<u32>);

impl Drop for Stopper {
    fn drop(&mut self) {
        for pid in &self.0 {
            let _ = std::process::Command::new("kill")
                .arg(pid.to_string())
                .stderr(Stdio::null())
                .status();
        }
    }
}

fn start(program: &Path, data_dir: &Path) -> std::io::Result<Child> {
    std::process::Command::new(program)
        .arg("--data-dir")
        .arg(data_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .spawn()
}

/// Connects once the service listens.
async fn connect(data_dir: &Path) -> TestResult<(LocalClient, mpsc::Receiver<Notification>)> {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        match LocalClient::connect(data_dir).await {
            Ok(connected) => return Ok(connected),
            Err(ConnectError::NotRunning { .. }) if Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(error) => return Err(error.into()),
        }
    }
}

async fn add_repository(app: &LocalClient, repo: &TestRepo) -> TestResult {
    let response = app
        .execute(Command::AddRepository {
            path: repo.path().to_path_buf(),
        })
        .await?;
    assert!(matches!(response, Response::RepositoryAdded { .. }));
    Ok(())
}

async fn repository_paths(app: &LocalClient) -> TestResult<Vec<PathBuf>> {
    match app.execute(Command::ListRepositories).await? {
        Response::Repositories { repositories } => Ok(repositories
            .into_iter()
            .map(|repository| repository.path)
            .collect()),
        response => Err(format!("expected Repositories, got {response:?}").into()),
    }
}

#[tokio::test]
async fn the_service_keeps_running_when_the_app_disconnects() -> TestResult {
    let data = tempfile::tempdir()?;
    let mut stopper = Stopper::default();
    let child = start(Path::new(SERVICE), data.path())?;
    stopper.0.push(child.id());
    let repo = TestRepo::new()?;

    let (app, notifications) = connect(data.path()).await?;
    add_repository(&app, &repo).await?;
    assert_eq!(app.service_pid(), child.id());
    drop((app, notifications));

    let (app, _notifications) = connect(data.path()).await?;
    assert_eq!(app.service_pid(), child.id());
    assert_eq!(
        repository_paths(&app).await?,
        vec![repo.path().to_path_buf()]
    );
    Ok(())
}

#[tokio::test]
async fn a_replaced_program_takes_over_once_no_turn_runs() -> TestResult {
    let bin = tempfile::tempdir()?;
    let data = tempfile::tempdir()?;
    let program = bin.path().join("leiser-harness-service");
    std::fs::copy(SERVICE, &program)?;
    let mut stopper = Stopper::default();
    let mut child = start(&program, data.path())?;
    stopper.0.push(child.id());
    let repo = TestRepo::new()?;
    let (app, mut notifications) = connect(data.path()).await?;
    add_repository(&app, &repo).await?;

    // Installs the update as a bundle update does: a new file in its place.
    let next = bin.path().join("next");
    std::fs::copy(SERVICE, &next)?;
    std::fs::rename(&next, &program)?;

    // No turn runs, so the service stops at once and the connection closes.
    tokio::time::timeout(TIMEOUT, async {
        while notifications.recv().await.is_some() {}
    })
    .await?;
    let exit = tokio::task::spawn_blocking(move || child.wait()).await??;
    assert!(exit.success(), "the old service exited with {exit}");

    let (app, _notifications) = connect(data.path()).await?;
    stopper.0.push(app.service_pid());
    assert_ne!(app.service_pid(), stopper.0[0]);
    assert_eq!(
        repository_paths(&app).await?,
        vec![repo.path().to_path_buf()]
    );
    Ok(())
}
