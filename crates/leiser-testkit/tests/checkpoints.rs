//! Diff panel with checkpoints (#39): every turn captures a checkpoint of
//! the chat's files before and after it as a hidden git ref. The diff
//! panel reads a turn's changes and the whole branch's, and the developer
//! can put the files back to a checkpoint.

use std::collections::BTreeMap;
use std::error::Error;
use std::path::{Path, PathBuf};
use std::process::Command as Process;

use leiser_service::{
    AgentEvent, ChatId, ChatSettings, Checkout, CheckpointMoment, Command, CommandError, DiffLine,
    DiffRange, Event, FileChange, FileDiff, Patch, RepositoryId, Response, ServiceHandle,
    TurnCheckpoints,
};
use leiser_testkit::{
    DEFAULT_BRANCH, Gate, Script, Step, TestRepo, TestService, collect_turn, next_event,
};

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

const FAKE_AGENT_CLI: &str = env!("CARGO_BIN_EXE_fake-agent-cli");

fn start_service(script: &Script) -> TestResult<TestService> {
    Ok(TestService::start(Path::new(FAKE_AGENT_CLI), script)?)
}

fn write(path: &str, contents: &str) -> Step {
    Step::WriteFile {
        path: PathBuf::from(path),
        contents: contents.to_owned(),
    }
}

/// A chat in a new worktree of a repository cloned from a remote whose
/// default branch has a `README.md`.
struct Setup {
    harness: TestService,
    service: ServiceHandle,
    repo: TestRepo,
    chat: ChatId,
    worktree: PathBuf,
}

async fn set_up(script: &Script) -> TestResult<Setup> {
    let harness = start_service(script)?;
    let service = harness.connect();
    let repo = TestRepo::with_remote()?;
    let repository = add_repository(&service, &repo).await?;
    let chat = start_chat(&service, repository).await?;
    let worktree = worktree(&service, chat).await?;
    Ok(Setup {
        harness,
        service,
        repo,
        chat,
        worktree,
    })
}

async fn add_repository(service: &ServiceHandle, repo: &TestRepo) -> TestResult<RepositoryId> {
    match service
        .execute(Command::AddRepository {
            path: repo.path().to_path_buf(),
        })
        .await?
    {
        Response::RepositoryAdded { repository } => Ok(repository),
        response => Err(format!("expected RepositoryAdded, got {response:?}").into()),
    }
}

async fn start_chat(service: &ServiceHandle, repository: RepositoryId) -> TestResult<ChatId> {
    match service
        .execute(Command::StartChat {
            repository,
            checkout: Checkout::NewWorktree,
            settings: ChatSettings::default(),
        })
        .await?
    {
        Response::ChatStarted { chat } => Ok(chat),
        response => Err(format!("expected ChatStarted, got {response:?}").into()),
    }
}

async fn worktree(service: &ServiceHandle, chat: ChatId) -> TestResult<PathBuf> {
    let Response::Chats { chats } = service.execute(Command::ListChats).await? else {
        return Err("expected Chats".into());
    };
    chats
        .into_iter()
        .find(|overview| overview.chat == chat)
        .and_then(|overview| overview.worktree)
        .ok_or_else(|| "the chat has no worktree".into())
}

/// Sends a message and waits for the turn to end.
async fn run_turn(
    service: &ServiceHandle,
    chat: ChatId,
    text: &str,
) -> TestResult<Vec<AgentEvent>> {
    let mut events = service.subscribe();
    service
        .execute(Command::SendMessage {
            chat,
            text: text.to_owned(),
        })
        .await?;
    Ok(collect_turn(&mut events, chat).await?)
}

async fn checkpoints(
    service: &ServiceHandle,
    chat: ChatId,
) -> TestResult<(String, Vec<TurnCheckpoints>)> {
    match service.execute(Command::ListCheckpoints { chat }).await? {
        Response::Checkpoints { branch_base, turns } => Ok((branch_base, turns)),
        response => Err(format!("expected Checkpoints, got {response:?}").into()),
    }
}

async fn diff(
    service: &ServiceHandle,
    chat: ChatId,
    range: DiffRange,
) -> TestResult<Vec<FileDiff>> {
    match service.execute(Command::ReadDiff { chat, range }).await? {
        Response::Diff { files } => Ok(files),
        response => Err(format!("expected Diff, got {response:?}").into()),
    }
}

async fn restore(
    service: &ServiceHandle,
    chat: ChatId,
    turn: u64,
    moment: CheckpointMoment,
) -> Result<Response, CommandError> {
    service
        .execute(Command::RestoreCheckpoint { chat, turn, moment })
        .await
}

/// Each changed file's path and change, in git's order.
fn changes(files: &[FileDiff]) -> Vec<(&str, &FileChange)> {
    files
        .iter()
        .map(|file| (file.path.as_str(), &file.change))
        .collect()
}

/// The added and removed lines of a file's diff, as `+text` and `-text`.
fn changed_lines(file: &FileDiff) -> TestResult<Vec<String>> {
    let Patch::Text { hunks } = &file.patch else {
        return Err(format!("expected lines for {}, got {:?}", file.path, file.patch).into());
    };
    Ok(hunks
        .iter()
        .flat_map(|hunk| &hunk.lines)
        .filter_map(|line| match line {
            DiffLine::Added { text, .. } => Some(format!("+{text}")),
            DiffLine::Removed { text, .. } => Some(format!("-{text}")),
            DiffLine::Context { .. } => None,
        })
        .collect())
}

fn file<'a>(files: &'a [FileDiff], path: &str) -> TestResult<&'a FileDiff> {
    files
        .iter()
        .find(|file| file.path == path)
        .ok_or_else(|| format!("{path} is not in the diff: {:?}", changes(files)).into())
}

/// Runs git in `dir` and returns its trimmed output.
fn git(dir: &Path, args: &[&str]) -> TestResult<String> {
    let output = Process::new("git").arg("-C").arg(dir).args(args).output()?;
    if !output.status.success() {
        return Err(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// Commits everything in `dir` as the developer would.
fn commit_all(dir: &Path, message: &str) -> TestResult {
    git(dir, &["add", "--all"])?;
    git(
        dir,
        &[
            "-c",
            "user.name=leiser test",
            "-c",
            "user.email=test@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--quiet",
            "--message",
            message,
        ],
    )?;
    Ok(())
}

/// The repository's checkpoint refs and the commits they point to.
fn checkpoint_refs(repo: &TestRepo) -> TestResult<BTreeMap<String, String>> {
    let listing = repo.git(&[
        "for-each-ref",
        "--format=%(refname) %(objectname)",
        "refs/leiser/checkpoints/",
    ])?;
    Ok(listing
        .lines()
        .filter_map(|line| line.split_once(' '))
        .map(|(name, commit)| (name.to_owned(), commit.to_owned()))
        .collect())
}

fn read(path: &Path) -> TestResult<String> {
    Ok(std::fs::read_to_string(path)?)
}

#[tokio::test]
async fn each_turn_captures_a_checkpoint_before_and_after_it_as_a_hidden_git_ref() -> TestResult {
    let script = Script::new()
        .turn([write("notes.txt", "one\n")])
        .turn([write("notes.txt", "one\ntwo\n")]);
    let setup = set_up(&script).await?;
    let (service, chat) = (&setup.service, setup.chat);
    let branch_before = git(&setup.worktree, &["rev-parse", "HEAD"])?;

    run_turn(service, chat, "write notes").await?;
    run_turn(service, chat, "add a line").await?;

    let (_, turns) = checkpoints(service, chat).await?;
    assert_eq!(
        turns.iter().map(|turn| turn.turn).collect::<Vec<_>>(),
        [1, 2]
    );
    assert_eq!(turns[0].title, "write notes");
    assert_eq!(turns[1].title, "add a line");
    let refs = checkpoint_refs(&setup.repo)?;
    assert_eq!(refs.len(), 4, "two checkpoints per turn: {refs:?}");
    for turn in &turns {
        let after = turn
            .after
            .as_deref()
            .ok_or("the turn has no checkpoint after")?;
        for (moment, commit) in [("before", turn.before.as_str()), ("after", after)] {
            let name = refs
                .iter()
                .find(|(_, target)| target.as_str() == commit)
                .map(|(name, _)| name)
                .ok_or_else(|| format!("no ref points at {commit}"))?;
            assert!(
                name.ends_with(&format!("/{}/{moment}", turn.turn)),
                "{name}"
            );
        }
    }
    // Each checkpoint holds the files as they were, also those not added.
    let before_first = &turns[0].before;
    assert!(
        git(
            &setup.worktree,
            &["cat-file", "-e", &format!("{before_first}:notes.txt")]
        )
        .is_err()
    );
    let after_second = turns[1].after.as_deref().ok_or("no checkpoint after")?;
    assert_eq!(
        git(
            &setup.worktree,
            &["show", &format!("{after_second}:notes.txt")]
        )?,
        "one\ntwo"
    );
    // Capturing leaves the branch, the index and the remote alone.
    assert_eq!(git(&setup.worktree, &["rev-parse", "HEAD"])?, branch_before);
    assert_eq!(git(&setup.worktree, &["ls-files"])?, "README.md");
    assert!(
        git(&setup.worktree, &["branch", "--list", "--all"])?
            .lines()
            .all(|branch| !branch.contains("checkpoint"))
    );
    Ok(())
}

#[tokio::test]
async fn a_turns_diff_shows_only_what_that_turn_changed() -> TestResult {
    let script = Script::new().turn([write("notes.txt", "one\n")]).turn([
        write("notes.txt", "one\ntwo\n"),
        write("todo.txt", "buy milk\n"),
    ]);
    let setup = set_up(&script).await?;
    let (service, chat) = (&setup.service, setup.chat);

    run_turn(service, chat, "write notes").await?;
    run_turn(service, chat, "more").await?;

    let first = diff(service, chat, DiffRange::Turn { turn: 1 }).await?;
    assert_eq!(changes(&first), [("notes.txt", &FileChange::Added)]);
    assert_eq!(changed_lines(&first[0])?, ["+one"]);

    let second = diff(service, chat, DiffRange::Turn { turn: 2 }).await?;
    assert_eq!(
        changes(&second),
        [
            ("notes.txt", &FileChange::Modified),
            ("todo.txt", &FileChange::Added)
        ]
    );
    let notes = file(&second, "notes.txt")?;
    assert_eq!(changed_lines(notes)?, ["+two"]);
    let Patch::Text { hunks } = &notes.patch else {
        return Err("expected lines".into());
    };
    assert_eq!(
        hunks[0].lines,
        [
            DiffLine::Context {
                old: 1,
                new: 1,
                text: "one".to_owned()
            },
            DiffLine::Added {
                new: 2,
                text: "two".to_owned()
            },
        ]
    );
    Ok(())
}

#[tokio::test]
async fn a_turns_diff_shows_renamed_deleted_and_binary_files() -> TestResult {
    let gate = Gate::new()?;
    let script = Script::new().turn([gate.step()]);
    let setup = set_up(&script).await?;
    let (service, chat) = (&setup.service, setup.chat);
    std::fs::write(
        setup.worktree.join("old-name.txt"),
        "a file to move\nwith lines\n",
    )?;
    std::fs::write(setup.worktree.join("gone.txt"), "deleted in the turn\n")?;
    commit_all(&setup.worktree, "Add files")?;

    let mut events = service.subscribe();
    service
        .execute(Command::SendMessage {
            chat,
            text: "reorganise".to_owned(),
        })
        .await?;
    // The agent works while the gate holds the turn.
    std::fs::rename(
        setup.worktree.join("old-name.txt"),
        setup.worktree.join("new-name.txt"),
    )?;
    std::fs::remove_file(setup.worktree.join("gone.txt"))?;
    std::fs::write(
        setup.worktree.join("image.bin"),
        [0_u8, 159, 146, 150, 0, 1],
    )?;
    gate.open()?;
    collect_turn(&mut events, chat).await?;

    let files = diff(service, chat, DiffRange::Turn { turn: 1 }).await?;
    assert_eq!(
        file(&files, "new-name.txt")?.change,
        FileChange::Renamed {
            from: "old-name.txt".to_owned()
        }
    );
    assert_eq!(file(&files, "gone.txt")?.change, FileChange::Deleted);
    assert_eq!(
        changed_lines(file(&files, "gone.txt")?)?,
        ["-deleted in the turn"]
    );
    let image = file(&files, "image.bin")?;
    assert_eq!(image.change, FileChange::Added);
    assert_eq!(image.patch, Patch::Binary);
    assert_eq!(files.len(), 3, "{:?}", changes(&files));
    Ok(())
}

#[tokio::test]
async fn a_running_turn_shows_its_changes_so_far() -> TestResult {
    let gate = Gate::new()?;
    let script = Script::new().turn([write("draft.txt", "first draft\n"), gate.step()]);
    let setup = set_up(&script).await?;
    let (service, chat) = (&setup.service, setup.chat);
    let mut events = service.subscribe();

    service
        .execute(Command::SendMessage {
            chat,
            text: "draft it".to_owned(),
        })
        .await?;
    // The file is written once the agent says so.
    loop {
        if let Event::Agent {
            event: AgentEvent::Message { .. },
            ..
        } = next_event(&mut events, chat).await?
        {
            break;
        }
    }

    let (_, turns) = checkpoints(service, chat).await?;
    assert_eq!(turns.len(), 1);
    assert_eq!(turns[0].after, None);
    let files = diff(service, chat, DiffRange::Turn { turn: 1 }).await?;
    assert_eq!(changes(&files), [("draft.txt", &FileChange::Added)]);
    // Restoring would take the files from under the running turn.
    let refused = restore(service, chat, 1, CheckpointMoment::Before).await;
    assert_eq!(refused, Err(CommandError::TurnRunning { chat }));
    assert!(setup.worktree.join("draft.txt").exists());

    gate.open()?;
    collect_turn(&mut events, chat).await?;
    let (_, turns) = checkpoints(service, chat).await?;
    assert!(turns[0].after.is_some());
    Ok(())
}

#[tokio::test]
async fn the_branch_diff_shows_committed_and_uncommitted_changes_against_the_base() -> TestResult {
    let script = Script::new().turn([write("feature.txt", "new feature\n")]);
    let setup = set_up(&script).await?;
    let (service, chat) = (&setup.service, setup.chat);

    run_turn(service, chat, "build the feature").await?;
    commit_all(&setup.worktree, "Add the feature")?;
    std::fs::write(
        setup.worktree.join("README.md"),
        "# test repository\nmore\n",
    )?;
    // The base branch moved on meanwhile; that is not the branch's change.
    setup
        .repo
        .push_to_remote(DEFAULT_BRANCH, "elsewhere.txt", "someone else\n")?;
    git(&setup.worktree, &["fetch", "--quiet", "origin"])?;

    let (branch_base, _) = checkpoints(service, chat).await?;
    assert_eq!(branch_base, format!("origin/{DEFAULT_BRANCH}"));
    let files = diff(service, chat, DiffRange::Branch).await?;
    assert_eq!(
        changes(&files),
        [
            ("README.md", &FileChange::Modified),
            ("feature.txt", &FileChange::Added)
        ]
    );
    assert_eq!(changed_lines(file(&files, "README.md")?)?, ["+more"]);
    Ok(())
}

#[tokio::test]
async fn restoring_a_checkpoint_returns_the_worktree_to_that_state() -> TestResult {
    let script = Script::new().turn([write("notes.txt", "one\n")]).turn([
        write("notes.txt", "one\ntwo\n"),
        write("todo.txt", "buy milk\n"),
    ]);
    let setup = set_up(&script).await?;
    let (service, chat) = (&setup.service, setup.chat);
    let dir = &setup.worktree;
    // Build output git ignores is not part of any checkpoint.
    std::fs::write(dir.join(".gitignore"), "target/\n")?;
    std::fs::create_dir(dir.join("target"))?;
    std::fs::write(dir.join("target").join("build.out"), "built\n")?;

    run_turn(service, chat, "write notes").await?;
    run_turn(service, chat, "more").await?;
    // The agent committed part of its work.
    commit_all(dir, "Notes")?;
    let head = git(dir, &["rev-parse", "HEAD"])?;
    let mut events = service.subscribe();

    restore(service, chat, 2, CheckpointMoment::Before).await?;

    assert_eq!(read(&dir.join("notes.txt"))?, "one\n");
    assert!(!dir.join("todo.txt").exists());
    assert_eq!(read(&dir.join("target").join("build.out"))?, "built\n");
    // The commits and the branch stay; the restore shows as changes.
    assert_eq!(git(dir, &["rev-parse", "HEAD"])?, head);
    assert_eq!(git(dir, &["diff", "--cached", "--name-only"])?, "");
    assert_eq!(
        git(dir, &["diff", "--name-status"])?,
        "M\tnotes.txt\nD\ttodo.txt"
    );
    assert_eq!(
        next_event(&mut events, chat).await?,
        Event::CheckpointRestored {
            chat,
            turn: 2,
            moment: CheckpointMoment::Before
        }
    );

    restore(service, chat, 2, CheckpointMoment::After).await?;
    assert_eq!(read(&dir.join("notes.txt"))?, "one\ntwo\n");
    assert_eq!(read(&dir.join("todo.txt"))?, "buy milk\n");
    assert_eq!(git(dir, &["status", "--porcelain"])?, "");

    restore(service, chat, 1, CheckpointMoment::Before).await?;
    assert!(!dir.join("notes.txt").exists());
    assert_eq!(read(&dir.join(".gitignore"))?, "target/\n");
    assert!(dir.join("target").join("build.out").exists());
    Ok(())
}

#[tokio::test]
async fn the_next_message_tells_the_agent_about_a_restore() -> TestResult {
    let script = Script::new()
        .turn([write("notes.txt", "one\n")])
        .turn([Step::EchoUserMessage])
        .turn([Step::EchoUserMessage]);
    let setup = set_up(&script).await?;
    let (service, chat) = (&setup.service, setup.chat);

    run_turn(service, chat, "write notes").await?;
    restore(service, chat, 1, CheckpointMoment::Before).await?;
    let told = run_turn(service, chat, "start again").await?;
    let untold = run_turn(service, chat, "go on").await?;

    let AgentEvent::Message { text } = &told[0] else {
        return Err(format!("expected a message, got {told:?}").into());
    };
    assert!(text.contains("restored the files"), "{text}");
    assert!(text.contains("before turn 1"), "{text}");
    assert!(text.ends_with("start again"), "{text}");
    assert_eq!(
        untold[0],
        AgentEvent::Message {
            text: "go on".to_owned()
        }
    );
    Ok(())
}

#[tokio::test]
async fn an_unknown_checkpoint_is_refused() -> TestResult {
    let setup = set_up(&Script::new()).await?;
    let (service, chat) = (&setup.service, setup.chat);

    assert_eq!(
        restore(service, chat, 1, CheckpointMoment::Before).await,
        Err(CommandError::UnknownCheckpoint {
            chat,
            turn: 1,
            moment: CheckpointMoment::Before
        })
    );
    assert!(matches!(
        service
            .execute(Command::ReadDiff {
                chat,
                range: DiffRange::Turn { turn: 1 }
            })
            .await,
        Err(CommandError::UnknownCheckpoint { turn: 1, .. })
    ));
    Ok(())
}

#[tokio::test]
async fn checkpoints_survive_a_restart_of_the_harness_service() -> TestResult {
    let script = Script::new().turn([write("notes.txt", "one\n")]);
    let setup = set_up(&script).await?;
    run_turn(&setup.service, setup.chat, "write notes").await?;
    let (_, before) = checkpoints(&setup.service, setup.chat).await?;

    let harness = setup.harness.restart().await?;
    let service = harness.connect();

    let (_, after) = checkpoints(&service, setup.chat).await?;
    assert_eq!(after, before);
    restore(&service, setup.chat, 1, CheckpointMoment::Before).await?;
    assert!(!setup.worktree.join("notes.txt").exists());
    Ok(())
}
