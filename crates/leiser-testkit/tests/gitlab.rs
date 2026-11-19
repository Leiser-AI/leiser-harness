//! Self-hosted GitLab support through `glab`, played by the `fake-glab`
//! program: `GLAB_CONFIG_DIR` isolation, `LEISER_GLAB` lookup, credential
//! helper formatting, `auth login`, `account_for_push`, opening merge
//! requests in nested subgroups, merge request state mapping, `GitHosts`
//! dispatch between GitHub and GitLab, and host account persistence.

use std::error::Error;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use leiser_service::{
    Command, CommitAuthor, Environment, GLAB_CONFIG_DIR, GhGitHost, GitHost, GitHostError,
    GitHostKind, GitHosts, GlabGitHost, HostAccountAccess, HostAccountBinding, LoginOutcome,
    NewPullRequest, OpenedPullRequest, PullRequestState, Response, ServiceConfig,
};
use leiser_testkit::{FakeDriver, FakeGh, FakeGlab, Script, TestRepo, TestService};
use serde_json::json;

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

const FAKE_AGENT_CLI: &str = env!("CARGO_BIN_EXE_fake-agent-cli");
const FAKE_GH: &str = env!("CARGO_BIN_EXE_fake-gh");
const FAKE_GLAB: &str = env!("CARGO_BIN_EXE_fake-glab");
const GITLAB_HOST: &str = "gitlab.example.invalid";
const REMOTE: &str = "https://gitlab.example.invalid/org/team/repo.git";
const PROJECT: &str = "projects/org%2Fteam%2Frepo";

struct Fixture {
    _dir: tempfile::TempDir,
    glab: FakeGlab,
    access: HostAccountAccess,
    host: GlabGitHost,
}

fn fixture() -> TestResult<Fixture> {
    let dir = tempfile::tempdir()?;
    let config_dir = dir.path().join("glab");
    let glab = FakeGlab::new(&config_dir);
    let access = HostAccountAccess {
        kind: GitHostKind::GitLab,
        git_host: GITLAB_HOST.to_owned(),
        login: "alice".to_owned(),
        config_dir,
    };
    Ok(Fixture {
        _dir: dir,
        glab,
        access,
        host: GlabGitHost::new(FAKE_GLAB),
    })
}

#[test]
fn credential_helper_uses_glab_config_dir_and_shell_quotes_paths() -> TestResult {
    let dir = tempfile::tempdir()?;
    let config_dir = dir.path().join("it's glab");
    let access = HostAccountAccess {
        kind: GitHostKind::GitLab,
        git_host: GITLAB_HOST.to_owned(),
        login: "alice".to_owned(),
        config_dir,
    };
    let host = GlabGitHost::new("/opt/glab bin/glab");

    let helper = host.credential_helper(&access)?;

    assert!(helper.starts_with("!unset "), "{helper}");
    assert!(helper.contains("GITLAB_TOKEN"), "{helper}");
    assert!(helper.contains("OAUTH_TOKEN"), "{helper}");
    assert!(
        helper.contains(&format!("&& {GLAB_CONFIG_DIR}='")),
        "{helper}"
    );
    assert!(helper.contains(r"it'\''s glab"), "{helper}");
    assert!(
        helper.ends_with(" '/opt/glab bin/glab' auth git-credential"),
        "{helper}"
    );
    Ok(())
}

#[test]
fn missing_glab_names_its_program_and_override_variable() -> TestResult {
    let dir = tempfile::tempdir()?;
    let access = HostAccountAccess {
        kind: GitHostKind::GitLab,
        git_host: GITLAB_HOST.to_owned(),
        login: "alice".to_owned(),
        config_dir: dir.path().join("glab"),
    };
    let host = GlabGitHost::installed_in(Environment::default());

    let result = host.credential_helper(&access);

    let Err(GitHostError::NotInstalled { program, reason }) = result else {
        return Err(format!("expected NotInstalled, got {result:?}").into());
    };
    assert_eq!(program, "glab");
    assert!(reason.contains(GlabGitHost::PROGRAM_VARIABLE), "{reason}");
    Ok(())
}

#[test]
fn installed_glab_honours_leiser_glab_variable() -> TestResult {
    let dir = tempfile::tempdir()?;
    let access = HostAccountAccess {
        kind: GitHostKind::GitLab,
        git_host: GITLAB_HOST.to_owned(),
        login: "alice".to_owned(),
        config_dir: dir.path().join("glab"),
    };
    let env = Environment::default().with(GlabGitHost::PROGRAM_VARIABLE, FAKE_GLAB);
    let host = GlabGitHost::installed_in(env);

    let helper = host.credential_helper(&access)?;

    assert!(helper.ends_with(" auth git-credential"), "{helper}");
    assert!(helper.contains(&FAKE_GLAB.replace('\\', "/")), "{helper}");
    Ok(())
}

#[tokio::test]
async fn login_runs_glab_auth_login_for_the_self_hosted_hostname() -> TestResult {
    let fixture = fixture()?;

    let mut process = fixture.host.login(&fixture.access)?;
    let mut lines = Vec::new();
    while let Some(line) = process.output.recv().await {
        lines.push(line);
    }
    let outcome = process.outcome.await?;

    assert_eq!(outcome, LoginOutcome::SignedIn);
    assert!(
        lines
            .iter()
            .any(|line| line.contains(&format!("Logged in to {GITLAB_HOST}"))),
        "{lines:?}"
    );
    let requests = fixture.glab.take_requests()?;
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].method, "AUTH_LOGIN");
    assert_eq!(requests[0].hostname.as_deref(), Some(GITLAB_HOST));
    assert_eq!(
        requests[0].fields.get("git_protocol").map(String::as_str),
        Some("https")
    );
    assert_eq!(
        requests[0].fields.get("api_protocol").map(String::as_str),
        Some("https")
    );
    assert!(requests[0].leaked_env.is_empty(), "{requests:?}");
    Ok(())
}

#[tokio::test]
async fn account_for_push_reads_the_authenticated_gitlab_username() -> TestResult {
    let fixture = fixture()?;
    fixture
        .glab
        .answer_without_etag("user", json!({"id": 7, "username": "alice"}))?;

    let login = fixture
        .host
        .account_for_push(&fixture.access, REMOTE)
        .await?;

    assert_eq!(login, "alice");
    let requests = fixture.glab.take_requests()?;
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].path, "user");
    assert_eq!(requests[0].hostname.as_deref(), Some(GITLAB_HOST));
    assert!(requests[0].leaked_env.is_empty(), "{requests:?}");
    Ok(())
}

#[tokio::test]
async fn account_for_push_supports_self_hosted_custom_ports_and_rejects_mismatched_ports()
-> TestResult {
    let dir = tempfile::tempdir()?;
    let config_dir = dir.path().join("glab");
    let glab = FakeGlab::new(&config_dir);
    glab.answer_without_etag("user", json!({"username": "alice"}))?;
    let access = HostAccountAccess {
        kind: GitHostKind::GitLab,
        git_host: "gitlab.example.invalid:8443".to_owned(),
        login: "alice".to_owned(),
        config_dir,
    };
    let host = GlabGitHost::new(FAKE_GLAB);

    let ok = host
        .account_for_push(&access, "https://gitlab.example.invalid:8443/org/repo.git")
        .await?;
    assert_eq!(ok, "alice");

    let wrong_port = host
        .account_for_push(&access, "https://gitlab.example.invalid/org/repo.git")
        .await;
    assert!(
        matches!(wrong_port, Err(GitHostError::UnsupportedRemote { .. })),
        "{wrong_port:?}"
    );
    Ok(())
}

#[tokio::test]
async fn account_for_push_reports_not_signed_in_when_glab_fails() -> TestResult {
    let fixture = fixture()?;

    let result = fixture.host.account_for_push(&fixture.access, REMOTE).await;

    assert!(
        matches!(result, Err(GitHostError::NotSignedIn { .. })),
        "{result:?}"
    );
    Ok(())
}

#[tokio::test]
async fn open_pull_request_posts_a_gitlab_merge_request_in_a_nested_subgroup() -> TestResult {
    let fixture = fixture()?;
    fixture.glab.answer_without_etag(
        &format!("POST {PROJECT}/merge_requests"),
        json!({
            "iid": 42,
            "web_url": format!("https://{GITLAB_HOST}/org/team/repo/-/merge_requests/42"),
        }),
    )?;

    let opened = fixture
        .host
        .open_pull_request(
            &fixture.access,
            REMOTE,
            &NewPullRequest {
                head: "feat/gitlab".to_owned(),
                base: "development".to_owned(),
                title: "Add GitLab host".to_owned(),
                body: "Supports self-hosted GitLab.".to_owned(),
            },
        )
        .await?;

    assert_eq!(
        opened,
        OpenedPullRequest {
            number: 42,
            url: format!("https://{GITLAB_HOST}/org/team/repo/-/merge_requests/42"),
        }
    );
    let requests = fixture.glab.take_requests()?;
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].method, "POST");
    assert_eq!(requests[0].path, format!("{PROJECT}/merge_requests"));
    assert_eq!(requests[0].hostname.as_deref(), Some(GITLAB_HOST));
    assert_eq!(
        requests[0].fields.get("source_branch").map(String::as_str),
        Some("feat/gitlab")
    );
    assert_eq!(
        requests[0].fields.get("target_branch").map(String::as_str),
        Some("development")
    );
    assert_eq!(
        requests[0].fields.get("title").map(String::as_str),
        Some("Add GitLab host")
    );
    assert_eq!(
        requests[0].fields.get("description").map(String::as_str),
        Some("Supports self-hosted GitLab.")
    );
    Ok(())
}

#[tokio::test]
async fn pull_request_state_maps_gitlab_merge_request_states() -> TestResult {
    let fixture = fixture()?;
    for (raw, expected) in [
        ("opened", PullRequestState::Open),
        ("locked", PullRequestState::Open),
        ("merged", PullRequestState::Merged),
        ("closed", PullRequestState::Closed),
    ] {
        fixture.glab.answer_without_etag(
            &format!("{PROJECT}/merge_requests/9"),
            json!({"state": raw}),
        )?;
        let actual = fixture
            .host
            .pull_request_state(&fixture.access, REMOTE, 9)
            .await?;
        assert_eq!(actual, expected, "state {raw}");
    }
    fixture.glab.answer_without_etag(
        &format!("{PROJECT}/merge_requests/9"),
        json!({"state": "unexpected"}),
    )?;
    let bad = fixture
        .host
        .pull_request_state(&fixture.access, REMOTE, 9)
        .await;
    assert!(
        matches!(bad, Err(GitHostError::UnexpectedAnswer { .. })),
        "{bad:?}"
    );
    Ok(())
}

#[tokio::test]
async fn git_hosts_router_dispatches_github_and_gitlab_accounts_separately() -> TestResult {
    let dir = tempfile::tempdir()?;
    let gh_dir = dir.path().join("gh");
    let glab_dir = dir.path().join("glab");
    let gh = FakeGh::new(&gh_dir);
    let glab = FakeGlab::new(&glab_dir);
    gh.answer(
        "user",
        "\"u1\"",
        serde_json::Value::String("gh-user".to_owned()),
    )?;
    glab.answer_without_etag("user", json!({"username": "gl-user"}))?;

    let router = GitHosts::new(
        Arc::new(GhGitHost::new(FAKE_GH)),
        Arc::new(GlabGitHost::new(FAKE_GLAB)),
    );
    let github_access = HostAccountAccess {
        kind: GitHostKind::GitHub,
        git_host: "github.com".to_owned(),
        login: "gh-user".to_owned(),
        config_dir: gh_dir,
    };
    let gitlab_access = HostAccountAccess {
        kind: GitHostKind::GitLab,
        git_host: GITLAB_HOST.to_owned(),
        login: "gl-user".to_owned(),
        config_dir: glab_dir,
    };

    assert!(
        router
            .credential_helper(&github_access)?
            .starts_with("!GH_CONFIG_DIR=")
    );
    assert!(
        router
            .credential_helper(&gitlab_access)?
            .contains(&format!("&& {GLAB_CONFIG_DIR}="))
    );
    assert_eq!(
        router
            .account_for_push(&github_access, "https://github.com/owner/repo.git")
            .await?,
        "gh-user"
    );
    assert_eq!(
        router.account_for_push(&gitlab_access, REMOTE).await?,
        "gl-user"
    );
    Ok(())
}

fn start_routed_service(
    driver: Arc<FakeDriver>,
    data_dir: &Path,
    router: &GitHosts,
) -> TestResult<leiser_service::Service> {
    Ok(leiser_service::start(
        ServiceConfig::new(
            leiser_service::AgentCli::ClaudeCode,
            driver,
            data_dir.to_path_buf(),
        )
        .with_git_host(Arc::new(router.clone())),
    )?)
}

async fn add_gitlab_account(
    handle: &leiser_service::ServiceHandle,
) -> TestResult<leiser_service::HostAccountId> {
    match handle
        .execute(Command::AddHostAccount {
            name: "Company GitLab".to_owned(),
            kind: GitHostKind::GitLab,
            git_host: GITLAB_HOST.to_owned(),
            login: "alice".to_owned(),
            author: CommitAuthor {
                name: "Alice".to_owned(),
                email: "alice@example.invalid".to_owned(),
            },
        })
        .await?
    {
        Response::HostAccountAdded { host_account } => Ok(host_account),
        response => Err(format!("expected HostAccountAdded, got {response:?}").into()),
    }
}

#[tokio::test]
async fn service_persists_gitlab_host_accounts_and_binds_glab_config_dir() -> TestResult {
    let data = tempfile::tempdir()?;
    let router = GitHosts::new(
        Arc::new(GhGitHost::new(FAKE_GH)),
        Arc::new(GlabGitHost::new(FAKE_GLAB)),
    );
    let driver = Arc::new(FakeDriver::new(Path::new(FAKE_AGENT_CLI), &Script::new())?);
    let service = start_routed_service(driver.clone(), data.path(), &router)?;
    let handle = service.connect();
    let host_account = add_gitlab_account(&handle).await?;

    let repo = TestRepo::with_remote()?;
    std::fs::create_dir_all(repo.path().join(".leiser"))?;
    std::fs::write(
        repo.path().join(".leiser/config.toml"),
        "host_account = \"Company GitLab\"\n",
    )?;
    let Response::RepositoryAdded { repository } = handle
        .execute(Command::AddRepository {
            path: repo.path().to_path_buf(),
        })
        .await?
    else {
        return Err("expected RepositoryAdded".into());
    };
    let Response::ChatStarted { chat } = handle
        .execute(Command::StartChat {
            repository,
            checkout: leiser_service::Checkout::NewWorktree,
            settings: leiser_service::ChatSettings::default(),
        })
        .await?
    else {
        return Err("expected ChatStarted".into());
    };

    let Response::Chat {
        host_account: binding,
        ..
    } = handle.execute(Command::ReadChat { chat }).await?
    else {
        return Err("expected Chat".into());
    };
    assert_eq!(
        *binding,
        HostAccountBinding::Bound {
            host_account,
            name: "Company GitLab".to_owned(),
            kind: GitHostKind::GitLab,
            git_host: GITLAB_HOST.to_owned(),
            login: "alice".to_owned(),
        }
    );

    let helpers = std::process::Command::new("git")
        .arg("-C")
        .arg(repo.path())
        .args([
            "config",
            "--local",
            "--null",
            "--get-all",
            &format!("credential.https://{GITLAB_HOST}.helper"),
        ])
        .output()?;
    let helpers = String::from_utf8_lossy(&helpers.stdout);
    assert!(
        helpers.contains("\0!unset ") && helpers.contains(&format!("&& {GLAB_CONFIG_DIR}=")),
        "{helpers:?}"
    );
    assert!(helpers.contains("auth git-credential\0"), "{helpers:?}");

    service.shutdown().await;
    let restarted = start_routed_service(driver, data.path(), &router)?;
    let Response::HostAccounts { host_accounts } = restarted
        .connect()
        .execute(Command::ListHostAccounts)
        .await?
    else {
        return Err("expected HostAccounts".into());
    };
    assert_eq!(host_accounts.len(), 1);
    assert_eq!(host_accounts[0].kind, GitHostKind::GitLab);
    assert_eq!(
        host_accounts[0]
            .config_dir
            .file_name()
            .and_then(|s| s.to_str()),
        Some("glab")
    );
    restarted.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn stored_host_accounts_without_kind_default_to_github() -> TestResult {
    let harness = TestService::start(Path::new(FAKE_AGENT_CLI), &Script::new())?;
    let service = harness.connect();
    let Response::HostAccountAdded { .. } = service
        .execute(Command::AddHostAccount {
            name: "Legacy".to_owned(),
            kind: GitHostKind::GitHub,
            git_host: "github.com".to_owned(),
            login: "legacy-user".to_owned(),
            author: CommitAuthor {
                name: "Legacy".to_owned(),
                email: "legacy@example.invalid".to_owned(),
            },
        })
        .await?
    else {
        return Err("expected HostAccountAdded".into());
    };
    // Simulate a state.json written before `kind` existed.
    let state_path = harness.data_dir().join("state.json");
    let mut state: serde_json::Value = serde_json::from_slice(&std::fs::read(&state_path)?)?;
    if let Some(account) = state["host_accounts"]
        .as_array_mut()
        .and_then(|a| a.first_mut())
        && let Some(object) = account.as_object_mut()
    {
        object.remove("kind");
    }
    std::fs::write(&state_path, serde_json::to_vec_pretty(&state)?)?;

    let harness = harness.restart().await?;
    let Response::HostAccounts { host_accounts } =
        harness.connect().execute(Command::ListHostAccounts).await?
    else {
        return Err("expected HostAccounts".into());
    };
    assert_eq!(host_accounts.len(), 1);
    assert_eq!(host_accounts[0].kind, GitHostKind::GitHub);
    assert_eq!(
        host_accounts[0]
            .config_dir
            .file_name()
            .and_then(|s| s.to_str()),
        Some("gh")
    );
    Ok(())
}

#[test]
fn remote_paths_with_nested_subgroups_and_invalid_segments_are_validated() -> TestResult {
    let dir = tempfile::tempdir()?;
    let config_dir = dir.path().join("glab");
    let access = HostAccountAccess {
        kind: GitHostKind::GitLab,
        git_host: GITLAB_HOST.to_owned(),
        login: "alice".to_owned(),
        config_dir,
    };
    let host = GlabGitHost::new(PathBuf::from(FAKE_GLAB));
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let invalid = runtime.block_on(host.pull_request_state(
        &access,
        "https://gitlab.example.invalid/only-one",
        1,
    ));
    assert!(
        matches!(invalid, Err(GitHostError::UnsupportedRemote { .. })),
        "{invalid:?}"
    );
    Ok(())
}
