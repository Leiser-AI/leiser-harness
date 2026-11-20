//! Integration tests for the `leiser` CLI over `leiser_service`'s local interface.

use std::error::Error;
use std::io;
use std::path::Path;
use std::sync::Arc;

use leiser_cli::{CliError, ParseOutcome, parse_args, run};
use leiser_service::{
    AgentCli, AgentSession, AgentSessionId, Command, Driver, DriverError, LocalInterface,
    LoginProcess, PermissionMode, QueueCommand, RepositoryId, Response, SessionSpec,
    SubscriptionAccess, serve,
};
use leiser_testkit::{TestRepo, TestService};
use tokio::process::Command as ProcessCommand;

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

const LEISER_BIN: &str = env!("CARGO_BIN_EXE_leiser");

#[derive(Debug)]
struct StubDriver;

impl Driver for StubDriver {
    fn start(&self, _spec: SessionSpec) -> Result<AgentSession, DriverError> {
        Err(DriverError::Spawn(io::Error::other("stub driver")))
    }

    fn resume(
        &self,
        _spec: SessionSpec,
        _session: &AgentSessionId,
    ) -> Result<AgentSession, DriverError> {
        Err(DriverError::Spawn(io::Error::other("stub driver")))
    }

    fn supports_permission_mode(&self, _mode: PermissionMode) -> bool {
        true
    }

    fn login(&self, _access: &SubscriptionAccess) -> Result<LoginProcess, DriverError> {
        Err(DriverError::Spawn(io::Error::other("stub driver")))
    }
}

struct ServedRepo {
    service: TestService,
    _interface: LocalInterface,
    repo: TestRepo,
    repository: RepositoryId,
}

async fn setup_served_repo() -> TestResult<ServedRepo> {
    let driver: Arc<dyn Driver> = Arc::new(StubDriver);
    let service = TestService::with_drivers(
        (AgentCli::ClaudeCode, driver.clone()),
        vec![
            (AgentCli::Codex, driver.clone()),
            (AgentCli::Gemini, driver),
        ],
    )?;
    let app = service.connect();
    let repo = TestRepo::new()?;
    let Response::RepositoryAdded { repository } = app
        .execute(Command::AddRepository {
            path: repo.path().to_path_buf(),
        })
        .await?
    else {
        return Err("expected RepositoryAdded".into());
    };
    app.execute(Command::Queue {
        command: QueueCommand::Pause { repository },
    })
    .await?;
    let interface = serve(app, service.data_dir()).await?;
    Ok(ServedRepo {
        service,
        _interface: interface,
        repo,
        repository,
    })
}

fn parse_run<const N: usize>(args: [&str; N]) -> TestResult<leiser_cli::Cli> {
    match parse_args(args)? {
        ParseOutcome::Run(cli) => Ok(cli),
        ParseOutcome::Help => Err("expected CLI action, got Help".into()),
    }
}

async fn exec_text<const N: usize>(args: [&str; N], cwd: &Path) -> TestResult<String> {
    let cli = parse_run(args)?;
    let mut output = Vec::new();
    run(&cli, cwd, &mut output).await?;
    Ok(String::from_utf8(output)?)
}

async fn exec_json<const N: usize>(args: [&str; N], cwd: &Path) -> TestResult<serde_json::Value> {
    let text = exec_text(args, cwd).await?;
    Ok(serde_json::from_str(&text)?)
}

#[tokio::test]
async fn offline_error_reports_missing_endpoint_file() -> TestResult {
    let missing = tempfile::tempdir()?;
    let data_dir = missing.path().to_string_lossy().into_owned();
    let cli = parse_run(["--data-dir", &data_dir, "status"])?;
    let mut output = Vec::new();
    let Err(error) = run(&cli, missing.path(), &mut output).await else {
        return Err("expected NotRunning error".into());
    };
    assert!(matches!(error, CliError::NotRunning { .. }));
    let text = error.to_string();
    assert!(
        text.contains("local-interface.json"),
        "unexpected error: {text}"
    );
    assert!(text.contains(&data_dir), "unexpected error: {text}");
    Ok(())
}

#[tokio::test]
async fn status_and_compiled_binary_report_repositories_and_chats() -> TestResult {
    let served = setup_served_repo().await?;
    let data_dir = served.service.data_dir().to_string_lossy().into_owned();
    let repo_id = served.repository.to_string();

    let status_json = exec_json(
        ["--data-dir", &data_dir, "--json", "status"],
        served.repo.path(),
    )
    .await?;
    assert_eq!(
        status_json["repositories"][0]["repository"],
        serde_json::json!(served.repository)
    );
    assert_eq!(status_json["chats"], serde_json::json!([]));

    let status_text = exec_text(["--data-dir", &data_dir, "status"], served.repo.path()).await?;
    assert!(status_text.contains("leiser-harness-service v"));
    assert!(status_text.contains(&repo_id));

    let bin_status = ProcessCommand::new(LEISER_BIN)
        .args(["--data-dir", &data_dir, "--json", "status"])
        .output()
        .await?;
    assert!(
        bin_status.status.success(),
        "leiser binary failed: {}",
        String::from_utf8_lossy(&bin_status.stderr)
    );
    let bin_json: serde_json::Value = serde_json::from_slice(&bin_status.stdout)?;
    assert_eq!(
        bin_json["repositories"][0]["repository"],
        serde_json::json!(served.repository)
    );
    Ok(())
}

#[tokio::test]
async fn queue_add_and_list_enqueue_and_inspect_tasks() -> TestResult {
    let served = setup_served_repo().await?;
    let data_dir = served.service.data_dir().to_string_lossy().into_owned();
    let repo_id = served.repository.to_string();
    let repo_path = served.repo.path().to_string_lossy().into_owned();

    let add_prompt_text = exec_text(
        ["--data-dir", &data_dir, "queue", "add", "Fix flaky check"],
        served.repo.path(),
    )
    .await?;
    assert!(add_prompt_text.contains("Enqueued task"));
    assert!(add_prompt_text.contains("Fix flaky check"));

    let added = exec_json(
        [
            "--data-dir",
            &data_dir,
            "--json",
            "queue",
            "add",
            "--issue",
            "42",
            "--repo",
            &repo_path,
            "--cli",
            "codex",
            "--mode",
            "auto",
        ],
        served.repo.path(),
    )
    .await?;
    assert_eq!(added["task"]["type"], "issue");
    assert_eq!(added["task"]["number"], 42);

    let listed = exec_json(
        [
            "--data-dir",
            &data_dir,
            "--json",
            "queue",
            "list",
            "--repo",
            &repo_id,
        ],
        served.repo.path(),
    )
    .await?;
    let items = listed["queues"][0]["queue"]["items"]
        .as_array()
        .ok_or("expected array of queued items")?;
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["task"]["type"], "prompt");
    assert_eq!(items[0]["task"]["text"], "Fix flaky check");
    assert_eq!(items[1]["task"]["type"], "issue");
    assert_eq!(items[1]["task"]["number"], 42);
    assert_eq!(items[1]["agent_cli"], "codex");
    assert_eq!(items[1]["permission_mode"], "auto");

    let bin_queue = ProcessCommand::new(LEISER_BIN)
        .args(["--data-dir", &data_dir, "--json", "queue", "list"])
        .output()
        .await?;
    assert!(
        bin_queue.status.success(),
        "leiser queue list binary failed: {}",
        String::from_utf8_lossy(&bin_queue.stderr)
    );
    let bin_queue_json: serde_json::Value = serde_json::from_slice(&bin_queue.stdout)?;
    assert_eq!(
        bin_queue_json["queues"][0]["queue"]["items"]
            .as_array()
            .ok_or("expected array")?
            .len(),
        2
    );
    Ok(())
}
