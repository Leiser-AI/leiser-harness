//! `repo-check` at the service seam: it runs read-only when a repository is
//! added and on demand, its report is read as Must and Should findings,
//! nothing is fixed, and checks switched off in `.leiser/config.toml` are
//! not reported. The fake agent CLI plays the skill's report.

use std::error::Error;
use std::path::Path;
use std::process::Command as Process;
use std::time::Duration;

use leiser_service::{
    CheckId, Command, CommandError, Event, Finding, OptOuts, RepoCheck, RepoCheckReport,
    RepositoryId, Response, ServiceHandle, SwitchedOffCheck,
};
use leiser_testkit::{Gate, SAMPLE_REPO_CHECK_REPORT, Script, Step, TestRepo, TestService};
use tokio::sync::broadcast;

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

const FAKE_AGENT_CLI: &str = env!("CARGO_BIN_EXE_fake-agent-cli");
const EVENT_TIMEOUT: Duration = Duration::from_secs(10);

/// A file the scripted `repo-check` tries to write into the repository.
const PROBE_FILE: &str = "written-by-repo-check.txt";

/// `repo-check` answers with the sample report.
fn reporting() -> Script {
    Script::new().turn([Step::message(SAMPLE_REPO_CHECK_REPORT)])
}

fn start_service(repo_check: &Script) -> TestResult<TestService> {
    Ok(TestService::with_repo_check(
        Path::new(FAKE_AGENT_CLI),
        &Script::new(),
        repo_check,
    )?)
}

async fn add_repository(service: &ServiceHandle, repo: &TestRepo) -> TestResult<RepositoryId> {
    let response = service
        .execute(Command::AddRepository {
            path: repo.path().to_path_buf(),
        })
        .await?;
    let Response::RepositoryAdded { repository } = response else {
        return Err(format!("expected RepositoryAdded, got {response:?}").into());
    };
    Ok(repository)
}

/// The next `repo-check` event of the repository.
async fn next_check_event(
    events: &mut broadcast::Receiver<Event>,
    repository: RepositoryId,
) -> TestResult<Event> {
    let event = tokio::time::timeout(EVENT_TIMEOUT, async {
        loop {
            match events.recv().await? {
                event @ (Event::RepoCheckStarted { repository: from }
                | Event::RepoCheckFinished { repository: from })
                    if from == repository =>
                {
                    return Ok::<_, broadcast::error::RecvError>(event);
                }
                _ => {}
            }
        }
    })
    .await
    .map_err(|_| "no repo-check event came in time")??;
    Ok(event)
}

async fn wait_until_finished(
    events: &mut broadcast::Receiver<Event>,
    repository: RepositoryId,
) -> TestResult {
    loop {
        if let Event::RepoCheckFinished { .. } = next_check_event(events, repository).await? {
            return Ok(());
        }
    }
}

async fn read_check(service: &ServiceHandle, repository: RepositoryId) -> TestResult<RepoCheck> {
    match service
        .execute(Command::ReadRepoCheck { repository })
        .await?
    {
        Response::RepoCheck { check } => Ok(check),
        response => Err(format!("expected RepoCheck, got {response:?}").into()),
    }
}

/// The report and opt-outs of a finished, readable `repo-check`.
async fn read_report(
    service: &ServiceHandle,
    repository: RepositoryId,
) -> TestResult<(RepoCheckReport, OptOuts)> {
    match read_check(service, repository).await? {
        RepoCheck::Done { report, opt_outs } => Ok((report, opt_outs)),
        check => Err(format!("expected a finished repo-check, got {check:?}").into()),
    }
}

/// Adds the repository and waits for its first `repo-check`.
async fn add_and_check(
    harness: &TestService,
    repo: &TestRepo,
) -> TestResult<(ServiceHandle, RepositoryId)> {
    let service = harness.connect();
    let mut events = service.subscribe();
    let repository = add_repository(&service, repo).await?;
    wait_until_finished(&mut events, repository).await?;
    Ok((service, repository))
}

fn titles(findings: &[Finding]) -> Vec<&str> {
    findings
        .iter()
        .map(|finding| finding.title.as_str())
        .collect()
}

fn checks(finding: &Finding) -> Vec<&str> {
    finding.checks.iter().map(CheckId::as_str).collect()
}

fn write_config(repo: &TestRepo, text: &str) -> TestResult {
    let dir = repo.path().join(".leiser");
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("config.toml"), text)?;
    Ok(())
}

fn git_status(repo: &TestRepo) -> TestResult<String> {
    let output = Process::new("git")
        .arg("-C")
        .arg(repo.path())
        .args(["status", "--porcelain", "--untracked-files=all"])
        .output()?;
    if !output.status.success() {
        return Err(format!("git status failed: {}", output.status).into());
    }
    Ok(String::from_utf8(output.stdout)?)
}

#[tokio::test]
async fn adding_a_repository_runs_repo_check_and_groups_its_findings_as_must_and_should()
-> TestResult {
    let harness = start_service(&reporting())?;
    let repo = TestRepo::new()?;

    let (service, repository) = add_and_check(&harness, &repo).await?;

    let (report, _) = read_report(&service, repository).await?;
    assert_eq!(report.commit.as_deref(), Some("1a2b3c4d"));
    assert!(
        report.summary.starts_with("Rust-Workspace"),
        "{}",
        report.summary
    );
    assert_eq!(
        titles(&report.must),
        [
            "Verbote nur in Prosa, keine Bremse",
            "Kein Pre-Commit-Hook, Format-Reparatur-Commits",
        ]
    );
    assert_eq!(
        titles(&report.should),
        [
            "Statusdateien im Repo",
            "Skill-Name doppelt (global)",
            "Lücken in `.gitignore`",
        ]
    );
    assert_eq!(checks(&report.must[0]), ["B1", "B2"]);
    assert_eq!(checks(&report.should[1]), ["I1", "D3"]);
    let details = &report.must[0].details;
    assert!(
        details.starts_with("Beleg: Kein `.claude/settings.json`."),
        "{details}"
    );
    assert!(
        details.contains("\nWarum: Prosa ist keine Bremse."),
        "{details}"
    );
    assert!(details.ends_with("`git push origin main`."), "{details}");
    Ok(())
}

#[tokio::test]
async fn repo_check_runs_read_only_and_changes_nothing() -> TestResult {
    let script = Script::new().turn([
        Step::WriteFile {
            path: PROBE_FILE.into(),
            contents: "a fix repo-check must not make".to_owned(),
        },
        Step::message(SAMPLE_REPO_CHECK_REPORT),
    ]);
    let harness = start_service(&script)?;
    let repo = TestRepo::new()?;

    let (service, repository) = add_and_check(&harness, &repo).await?;

    read_report(&service, repository).await?;
    assert!(!repo.path().join(PROBE_FILE).exists());
    assert_eq!(git_status(&repo)?, "");
    Ok(())
}

#[tokio::test]
async fn repo_check_runs_again_on_demand_but_not_while_it_runs() -> TestResult {
    let gate = Gate::new()?;
    let script = Script::new().turn([gate.step(), Step::message(SAMPLE_REPO_CHECK_REPORT)]);
    let harness = start_service(&script)?;
    let service = harness.connect();
    let mut events = service.subscribe();
    let repo = TestRepo::new()?;
    let repository = add_repository(&service, &repo).await?;
    assert_eq!(
        next_check_event(&mut events, repository).await?,
        Event::RepoCheckStarted { repository }
    );
    assert_eq!(read_check(&service, repository).await?, RepoCheck::Running);

    let refused = service.execute(Command::RunRepoCheck { repository }).await;
    assert_eq!(refused, Err(CommandError::RepoCheckRunning { repository }));
    gate.open()?;
    wait_until_finished(&mut events, repository).await?;
    let response = service
        .execute(Command::RunRepoCheck { repository })
        .await?;

    assert_eq!(response, Response::RepoCheckStarted);
    assert_eq!(
        next_check_event(&mut events, repository).await?,
        Event::RepoCheckStarted { repository }
    );
    assert_eq!(
        next_check_event(&mut events, repository).await?,
        Event::RepoCheckFinished { repository }
    );
    let (report, _) = read_report(&service, repository).await?;
    assert_eq!(report.must.len(), 2);
    Ok(())
}

#[tokio::test]
async fn a_check_switched_off_in_the_repository_config_is_not_reported() -> TestResult {
    let harness = start_service(&reporting())?;
    let repo = TestRepo::new()?;
    let (service, repository) = add_and_check(&harness, &repo).await?;

    // Switched off after the run: the next read leaves the checks out.
    write_config(
        &repo,
        "[repo_check.off]\n\
         D3 = \"The full check runs in CI in under a minute.\"\n\
         H1 = \"Status files are generated.\"\n",
    )?;
    let (report, opt_outs) = read_report(&service, repository).await?;

    // Must 2 named only D3 and is gone. Should 4 named I1 and D3 and keeps
    // I1; Should 3 named H1 and H2 and keeps H2.
    assert_eq!(titles(&report.must), ["Verbote nur in Prosa, keine Bremse"]);
    assert_eq!(report.should.len(), 3);
    assert_eq!(checks(&report.should[0]), ["H2"]);
    assert_eq!(checks(&report.should[1]), ["I1"]);
    let all_checks: Vec<&str> = report
        .must
        .iter()
        .chain(&report.should)
        .flat_map(checks)
        .collect();
    assert!(!all_checks.contains(&"D3"), "{all_checks:?}");
    assert!(!all_checks.contains(&"H1"), "{all_checks:?}");
    let switched_off = |check: &str, reason: &str| -> TestResult<SwitchedOffCheck> {
        Ok(SwitchedOffCheck {
            check: CheckId::parse(check).ok_or("not a check id")?,
            reason: reason.to_owned(),
        })
    };
    assert_eq!(
        opt_outs,
        OptOuts::Applied {
            checks: vec![
                switched_off("D3", "The full check runs in CI in under a minute.")?,
                switched_off("H1", "Status files are generated.")?,
            ]
        }
    );
    Ok(())
}

#[tokio::test]
async fn other_tables_of_the_repository_config_are_left_alone() -> TestResult {
    let harness = start_service(&reporting())?;
    let repo = TestRepo::new()?;
    write_config(
        &repo,
        "[host_account]\nlogin = \"someone\"\n\n[repo_check.off]\nG2 = \"Handled by a global ignore file.\"\n",
    )?;

    let (service, repository) = add_and_check(&harness, &repo).await?;

    let (report, opt_outs) = read_report(&service, repository).await?;
    assert_eq!(report.should.len(), 2);
    assert!(matches!(opt_outs, OptOuts::Applied { checks } if checks.len() == 1));
    Ok(())
}

#[tokio::test]
async fn an_unreadable_repository_config_hides_nothing_and_says_why() -> TestResult {
    let harness = start_service(&reporting())?;
    let repo = TestRepo::new()?;
    write_config(&repo, "[repo_check.off]\nD3 = \"\"\n")?;

    let (service, repository) = add_and_check(&harness, &repo).await?;

    let (report, opt_outs) = read_report(&service, repository).await?;
    assert_eq!(report.must.len(), 2);
    let OptOuts::Unreadable { reason } = opt_outs else {
        return Err(format!("expected Unreadable, got {opt_outs:?}").into());
    };
    assert!(reason.contains(".leiser/config.toml"), "{reason}");
    assert!(reason.contains("needs a reason"), "{reason}");
    Ok(())
}

#[tokio::test]
async fn an_answer_that_is_no_report_is_shown_as_it_came() -> TestResult {
    // Answers with the message it got, which shows how the skill is started.
    let script = Script::new().turn([Step::EchoUserMessage]);
    let harness = start_service(&script)?;
    let repo = TestRepo::new()?;

    let (service, repository) = add_and_check(&harness, &repo).await?;

    let check = read_check(&service, repository).await?;
    let RepoCheck::Unreadable { reason, output } = check else {
        return Err(format!("expected Unreadable, got {check:?}").into());
    };
    assert_eq!(output, "/repo-check");
    assert!(reason.contains("# repo-check"), "{reason}");
    Ok(())
}

#[tokio::test]
async fn a_repo_check_whose_agent_session_ends_early_has_failed() -> TestResult {
    // The fake agent CLI exits: repo-check sessions get no harness MCP server.
    let script = Script::new().turn([Step::ListHarnessTools]);
    let harness = start_service(&script)?;
    let repo = TestRepo::new()?;

    let (service, repository) = add_and_check(&harness, &repo).await?;

    let check = read_check(&service, repository).await?;
    let RepoCheck::Failed { reason } = check else {
        return Err(format!("expected Failed, got {check:?}").into());
    };
    assert!(
        reason.contains("ended before repo-check finished"),
        "{reason}"
    );
    Ok(())
}

#[tokio::test]
async fn the_last_result_is_kept_across_a_restart() -> TestResult {
    let harness = start_service(&reporting())?;
    let repo = TestRepo::new()?;
    let (_, repository) = add_and_check(&harness, &repo).await?;

    let harness = harness.restart().await?;

    let (report, _) = read_report(&harness.connect(), repository).await?;
    assert_eq!(report.must.len(), 2);
    assert_eq!(report.should.len(), 3);
    Ok(())
}

#[tokio::test]
async fn without_repo_check_set_up_nothing_runs() -> TestResult {
    let harness = TestService::start(Path::new(FAKE_AGENT_CLI), &Script::new())?;
    let service = harness.connect();
    let repo = TestRepo::new()?;
    let repository = add_repository(&service, &repo).await?;

    assert_eq!(read_check(&service, repository).await?, RepoCheck::NotRun);
    assert_eq!(
        service.execute(Command::RunRepoCheck { repository }).await,
        Err(CommandError::RepoCheckNotConfigured)
    );
    Ok(())
}
