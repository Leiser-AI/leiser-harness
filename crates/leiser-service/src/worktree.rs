//! Git worktrees for chats: each chat in a new worktree gets its own
//! directory and branch, so parallel chats never touch each other's files.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::git::{self, GitError};

/// How long a fetch of the base branch may take before the worktree
/// starts from what the repository already has.
const FETCH_TIMEOUT: Duration = Duration::from_secs(120);
/// The prefix of the branch a new worktree starts on, until the agent
/// names it.
const TEMPORARY_BRANCH_PREFIX: &str = "leiser/chat-";
/// Gives up finding a free branch name or directory after this many tries.
const MAX_NAME_TRIES: u32 = 100;

/// Why a worktree could not be created or its branch not be named.
#[derive(Debug, thiserror::Error)]
pub(crate) enum WorktreeError {
    #[error(transparent)]
    Git(#[from] GitError),
    #[error("the base branch {base} does not exist, or the repository has no commits yet")]
    NoBase { base: String },
    #[error("found no free branch name and directory for the worktree")]
    NoFreeName,
    #[error("could not create {}: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// A worktree just created.
#[derive(Debug)]
pub(crate) struct Created {
    pub path: PathBuf,
}

/// Creates a worktree of `repository` for chat `chat` in `parent`, on a
/// new temporary branch from `base`, fetched first when it is a remote's
/// branch. Without `base`, the remote `origin`'s default branch, else what
/// the main checkout has checked out.
pub(crate) async fn create(
    repository: &Path,
    parent: &Path,
    chat: u64,
    base: Option<&str>,
) -> Result<Created, WorktreeError> {
    let start = base_commit(repository, base).await?;
    let (branch, path) = free_name(repository, parent, chat).await?;
    std::fs::create_dir_all(parent).map_err(|source| WorktreeError::Io {
        path: parent.to_path_buf(),
        source,
    })?;
    let path_text = path.display().to_string();
    git::run(
        repository,
        &[
            "worktree",
            "add",
            "--quiet",
            "--no-track",
            "-b",
            &branch,
            &path_text,
            &start,
        ],
    )
    .await?;
    Ok(Created { path })
}

/// Whether `name` is the temporary branch a new worktree starts on, which
/// the agent has not named yet.
pub(crate) fn is_temporary_branch(name: &str) -> bool {
    name.starts_with(TEMPORARY_BRANCH_PREFIX)
}

/// Renames the branch checked out in `worktree` to `name`.
pub(crate) async fn rename_branch(worktree: &Path, name: &str) -> Result<(), WorktreeError> {
    git::run(worktree, &["check-ref-format", "--branch", name]).await?;
    git::run(worktree, &["branch", "--move", name]).await?;
    Ok(())
}

/// The commit a new worktree starts from.
async fn base_commit(repository: &Path, base: Option<&str>) -> Result<String, WorktreeError> {
    let remotes = git::run(repository, &["remote"]).await?;
    let is_remote = |name: &str| remotes.lines().any(|remote| remote == name);
    let (start, fetch) = match base {
        Some(base) => {
            let tracking = base.split_once('/').filter(|(remote, _)| is_remote(remote));
            match tracking {
                Some((remote, branch)) => (
                    format!("refs/remotes/{base}"),
                    Some((remote.to_owned(), branch.to_owned())),
                ),
                None => (base.to_owned(), None),
            }
        }
        None if is_remote("origin") => {
            match git::run(
                repository,
                &["symbolic-ref", "--quiet", "refs/remotes/origin/HEAD"],
            )
            .await
            {
                Ok(head) => {
                    let branch = head
                        .strip_prefix("refs/remotes/origin/")
                        .map(|branch| ("origin".to_owned(), branch.to_owned()));
                    (head, branch)
                }
                Err(_) => ("HEAD".to_owned(), None),
            }
        }
        None => ("HEAD".to_owned(), None),
    };
    if let Some((remote, branch)) = fetch {
        let fetched = tokio::time::timeout(
            FETCH_TIMEOUT,
            git::run(repository, &["fetch", "--quiet", &remote, &branch]),
        )
        .await;
        match fetched {
            Ok(Ok(_)) => {}
            // Offline, or the host account cannot read: the worktree starts
            // from what the repository has.
            Ok(Err(error)) => tracing::warn!(%error, "could not fetch the base branch"),
            Err(_) => tracing::warn!("fetching the base branch timed out"),
        }
    }
    git::run(
        repository,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{start}^{{commit}}"),
        ],
    )
    .await
    .map_err(|_| WorktreeError::NoBase {
        base: base.unwrap_or("HEAD").to_owned(),
    })
}

/// A branch name and directory no other worktree uses.
async fn free_name(
    repository: &Path,
    parent: &Path,
    chat: u64,
) -> Result<(String, PathBuf), WorktreeError> {
    for attempt in 0..MAX_NAME_TRIES {
        let suffix = if attempt == 0 {
            chat.to_string()
        } else {
            format!("{chat}-{attempt}")
        };
        let branch = format!("{TEMPORARY_BRANCH_PREFIX}{suffix}");
        let path = parent.join(format!("chat-{suffix}"));
        let taken = git::run(
            repository,
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("refs/heads/{branch}"),
            ],
        )
        .await
        .is_ok();
        if !taken && !path.exists() {
            return Ok((branch, path));
        }
    }
    Err(WorktreeError::NoFreeName)
}
