//! Contract test: the real Claude Code runs the developer's `repo-check`
//! skill read-only, and the harness can read its report.
//!
//! It needs Claude Code signed in to one of the developer's subscriptions
//! and the `repo-check` skill in `~/.claude/skills/`. The skill reads the
//! whole repository and the global agent configuration, so a run takes
//! minutes. Ignored by default and never run in CI. Run it locally, from a
//! plain terminal (not from inside a Claude Code session):
//!
//! ```text
//! cargo test -p leiser-testkit --test repo_check_contract -- --ignored --nocapture
//! ```
//!
//! `LEISER_CLAUDE_CODE` names the program to run (default: `claude` on
//! `PATH`).

use std::error::Error;
use std::process::Command as Process;
use std::sync::Arc;
use std::time::Duration;

use leiser_service::{
    AgentCli, ClaudeCodeDriver, Command, Event, RepoCheck, RepositoryId, Response, ServiceConfig,
};
use leiser_testkit::TestRepo;

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

/// The skill takes minutes on a real model.
const CHECK_TIMEOUT: Duration = Duration::from_secs(30 * 60);

fn driver() -> ClaudeCodeDriver {
    std::env::var_os("LEISER_CLAUDE_CODE").map_or_else(ClaudeCodeDriver::installed, |program| {
        ClaudeCodeDriver::new(program)
    })
}

fn git(repo: &TestRepo, args: &[&str]) -> TestResult<String> {
    let output = Process::new("git")
        .arg("-C")
        .arg(repo.path())
        .args(args)
        .output()?;
    if !output.status.success() {
        return Err(format!("git {args:?} failed: {}", output.status).into());
    }
    Ok(String::from_utf8(output.stdout)?)
}

/// A small repository with a rules file and one commit.
fn sample_repo() -> TestResult<TestRepo> {
    let repo = TestRepo::new()?;
    std::fs::write(
        repo.path().join("AGENTS.md"),
        "# sample\n\nNever push to main.\n",
    )?;
    std::fs::write(repo.path().join("CLAUDE.md"), "@AGENTS.md\n")?;
    git(&repo, &["add", "."])?;
    git(
        &repo,
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "commit",
            "--quiet",
            "-m",
            "Add rules",
        ],
    )?;
    Ok(repo)
}

#[tokio::test]
#[ignore = "needs Claude Code signed in and the repo-check skill; run locally with --ignored"]
async fn claude_code_runs_repo_check_read_only_and_its_report_is_read() -> TestResult {
    let repo = sample_repo()?;
    let head = git(&repo, &["rev-parse", "HEAD"])?;
    let data = tempfile::tempdir()?;
    let claude_code = Arc::new(driver());
    let service = leiser_service::start(
        ServiceConfig::new(
            AgentCli::ClaudeCode,
            claude_code.clone(),
            data.path().to_path_buf(),
        )
        .with_repo_check(claude_code),
    )?;
    let app = service.connect();
    let mut events = app.subscribe();

    let Response::RepositoryAdded { repository } = app
        .execute(Command::AddRepository {
            path: repo.path().to_path_buf(),
        })
        .await?
    else {
        return Err("expected RepositoryAdded".into());
    };
    wait_until_finished(&mut events, repository).await?;

    let Response::RepoCheck { check } = app.execute(Command::ReadRepoCheck { repository }).await?
    else {
        return Err("expected RepoCheck".into());
    };
    let RepoCheck::Done { report, .. } = &check else {
        return Err(format!("expected a readable report, got {check:?}").into());
    };
    // Shows what the harness read, for comparing with the skill's output.
    writeln_report(report)?;
    assert!(
        report
            .must
            .iter()
            .chain(&report.should)
            .all(|finding| !finding.title.is_empty()),
        "{report:?}"
    );
    assert_eq!(
        git(&repo, &["status", "--porcelain", "--untracked-files=all"])?,
        ""
    );
    assert_eq!(git(&repo, &["rev-parse", "HEAD"])?, head);
    service.shutdown().await;
    Ok(())
}

async fn wait_until_finished(
    events: &mut tokio::sync::broadcast::Receiver<Event>,
    repository: RepositoryId,
) -> TestResult {
    tokio::time::timeout(CHECK_TIMEOUT, async {
        loop {
            if let Event::RepoCheckFinished { repository: from } = events.recv().await?
                && from == repository
            {
                return Ok::<_, tokio::sync::broadcast::error::RecvError>(());
            }
        }
    })
    .await??;
    Ok(())
}

fn writeln_report(report: &leiser_service::RepoCheckReport) -> std::io::Result<()> {
    use std::io::Write;
    let mut err = std::io::stderr().lock();
    writeln!(err, "commit: {:?}\n{}", report.commit, report.summary)?;
    for (rank, findings) in [("Must", &report.must), ("Should", &report.should)] {
        for finding in findings {
            writeln!(err, "{rank}: {} {:?}", finding.title, finding.checks)?;
        }
    }
    Ok(())
}
