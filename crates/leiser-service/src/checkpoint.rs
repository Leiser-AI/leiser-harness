//! Checkpoints: the files of a chat's working directory, captured as a
//! commit under a hidden git ref before and after each turn, the diffs
//! between them, and putting the files back to one.
//!
//! A checkpoint holds every file git does not ignore, also files that are
//! not committed or not even added. Capturing it touches neither the
//! index nor `HEAD` nor the branch: the files go through an index of their
//! own, and the commit is reachable only from its ref under
//! [`REF_PREFIX`], which no push sends.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::git::{self, GitError};
use crate::{CheckpointMoment, DiffLine, FileChange, FileDiff, Hunk, Patch};

/// Where checkpoints' refs live: `<prefix>/<chat>/<turn>/before` and
/// `…/after`.
const REF_PREFIX: &str = "refs/leiser/checkpoints";
/// Who checkpoint commits are by. They are on no branch and never pushed.
const AUTHOR_NAME: &str = "leiser-harness";
const AUTHOR_EMAIL: &str = "checkpoint@leiser-harness.invalid";
/// A file with more diff lines than this shows as [`Patch::TooLarge`].
const MAX_PATCH_LINES: usize = 5_000;
/// Paths per `git checkout-index` call when restoring, to stay below the
/// command line limit.
const PATHS_PER_CALL: usize = 200;

/// Tells apart the temporary index files of captures that run at once.
static TEMPORARY_INDEXES: AtomicU64 = AtomicU64::new(0);

/// Why a checkpoint could not be captured, read or restored.
#[derive(Debug, thiserror::Error)]
pub(crate) enum CheckpointError {
    #[error(transparent)]
    Git(#[from] GitError),
    #[error("could not change {}: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("could not read git's diff: {0}")]
    Diff(&'static str),
}

/// The ref of a turn's checkpoint.
pub(crate) fn ref_name(chat: u64, turn: u64, moment: CheckpointMoment) -> String {
    format!("{REF_PREFIX}/{chat}/{turn}/{moment}")
}

/// Captures the files of `dir` as a commit on top of `HEAD`, points
/// `reference` at it and returns the commit.
pub(crate) async fn capture(
    dir: &Path,
    reference: &str,
    message: &str,
) -> Result<String, CheckpointError> {
    let tree = files_now(dir).await?;
    let head = git::run(dir, &["rev-parse", "--verify", "--quiet", "HEAD^{commit}"])
        .await
        .ok();
    let mut args = vec!["commit-tree", "--no-gpg-sign", "-m", message];
    if let Some(head) = &head {
        args.extend(["-p", head]);
    }
    args.push(&tree);
    let author = [
        ("GIT_AUTHOR_NAME", OsStr::new(AUTHOR_NAME)),
        ("GIT_AUTHOR_EMAIL", OsStr::new(AUTHOR_EMAIL)),
        ("GIT_COMMITTER_NAME", OsStr::new(AUTHOR_NAME)),
        ("GIT_COMMITTER_EMAIL", OsStr::new(AUTHOR_EMAIL)),
    ];
    let commit = git::run_with(dir, &author, &args).await?;
    git::run(dir, &["update-ref", reference, &commit]).await?;
    Ok(commit)
}

/// What [`crate::DiffRange::Branch`] compares to: the base branch's name
/// and the commit where the chat's branch left it.
pub(crate) struct BranchBase {
    pub name: String,
    pub commit: String,
}

/// The base of the branch checked out in `dir`: `base_branch` from the
/// repository's config, else the remote `origin`'s default branch, else
/// `HEAD` itself, so only changes not committed yet show.
pub(crate) async fn branch_base(
    dir: &Path,
    base_branch: Option<&str>,
) -> Result<BranchBase, CheckpointError> {
    let remotes = git::run(dir, &["remote"]).await.unwrap_or_default();
    let is_remote = |name: &str| remotes.lines().any(|remote| remote == name);
    let (name, reference) = match base_branch {
        Some(base) => match base.split_once('/') {
            Some((remote, _)) if is_remote(remote) => {
                (base.to_owned(), format!("refs/remotes/{base}"))
            }
            _ => (base.to_owned(), base.to_owned()),
        },
        None => match git::run(
            dir,
            &["symbolic-ref", "--quiet", "refs/remotes/origin/HEAD"],
        )
        .await
        {
            Ok(head) => (
                head.strip_prefix("refs/remotes/")
                    .unwrap_or(&head)
                    .to_owned(),
                head,
            ),
            Err(_) => ("HEAD".to_owned(), "HEAD".to_owned()),
        },
    };
    let commit = match git::run(dir, &["merge-base", &reference, "HEAD"]).await {
        Ok(commit) => commit,
        // Unrelated histories, or no commit yet: everything is new.
        Err(_) => match git::run(
            dir,
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("{reference}^{{commit}}"),
            ],
        )
        .await
        {
            Ok(commit) if reference != "HEAD" => commit,
            _ => empty_tree(dir).await?,
        },
    };
    Ok(BranchBase { name, commit })
}

/// The changes from `from` to `to`, both commits or trees; to the files in
/// `dir` now without `to`.
pub(crate) async fn diff(
    dir: &Path,
    from: &str,
    to: Option<&str>,
) -> Result<Vec<FileDiff>, CheckpointError> {
    let to = match to {
        Some(to) => to.to_owned(),
        None => files_now(dir).await?,
    };
    // Both listings take the same options, so git puts the files in the
    // same order: the paths come from the first, safe for any file name,
    // the lines from the second.
    let options = [
        "--no-color",
        "--no-ext-diff",
        "--no-textconv",
        "--find-renames",
        "--src-prefix=a/",
        "--dst-prefix=b/",
    ];
    let mut listing = vec!["diff", "--name-status", "-z"];
    listing.extend(options);
    listing.extend([from, to.as_str()]);
    let files = parse_name_status(&git::run_raw(dir, &listing).await?)?;
    let mut patch = vec!["diff"];
    patch.extend(options);
    patch.extend([from, to.as_str()]);
    let mut sections = parse_patch(&git::run_raw(dir, &patch).await?).into_iter();
    files
        .into_iter()
        .map(|(path, change, parts)| {
            let mut hunks = Vec::new();
            let mut binary = false;
            for _ in 0..parts {
                let section = sections
                    .next()
                    .ok_or(CheckpointError::Diff("fewer patches than files"))?;
                binary |= section.binary;
                hunks.extend(section.hunks);
            }
            let lines: usize = hunks.iter().map(|hunk| hunk.lines.len()).sum();
            let patch = if binary {
                Patch::Binary
            } else if lines > MAX_PATCH_LINES {
                Patch::TooLarge { lines }
            } else {
                Patch::Text { hunks }
            };
            Ok(FileDiff {
                path,
                change,
                patch,
            })
        })
        .collect()
}

/// Puts the files of `dir` back to the checkpoint `commit`: files that
/// differ get its contents, files it does not have are removed. Ignored
/// files, `HEAD` and the branch stay; the index is reset to `HEAD`, so
/// what the restore changed shows as changes not staged yet.
pub(crate) async fn restore(dir: &Path, commit: &str) -> Result<(), CheckpointError> {
    let target = git::run(
        dir,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{commit}^{{tree}}"),
        ],
    )
    .await?;
    let now = files_now(dir).await?;
    let changed = git::run_raw(
        dir,
        &["diff", "--name-status", "-z", "--no-renames", &target, &now],
    )
    .await?;
    let mut remove = Vec::new();
    let mut write = Vec::new();
    for (path, change, _) in parse_name_status(&changed)? {
        match change {
            FileChange::Added => remove.push(path),
            _ => write.push(path),
        }
    }
    // Removed first: a file may come back where a directory is now.
    for path in &remove {
        remove_file(dir, path)?;
    }
    if !write.is_empty() {
        let index = TemporaryIndex::new(dir, false).await?;
        let env = [("GIT_INDEX_FILE", index.path.as_os_str())];
        git::run_with(dir, &env, &["read-tree", &target]).await?;
        for paths in write.chunks(PATHS_PER_CALL) {
            let mut args = vec!["checkout-index", "--force", "--quiet", "--"];
            args.extend(paths.iter().map(String::as_str));
            git::run_with(dir, &env, &args).await?;
        }
    }
    if git::run(dir, &["reset", "--quiet"]).await.is_err() {
        // No commit yet, so there is no `HEAD` to reset to.
        git::run(dir, &["read-tree", "--empty"]).await?;
    }
    Ok(())
}

/// The tree of the files in `dir` now, without ignored files, written to
/// the repository's objects.
async fn files_now(dir: &Path) -> Result<String, CheckpointError> {
    let index = TemporaryIndex::new(dir, true).await?;
    let env = [("GIT_INDEX_FILE", index.path.as_os_str())];
    git::run_with(dir, &env, &["add", "--all", "--", "."]).await?;
    Ok(git::run_with(dir, &env, &["write-tree"]).await?)
}

/// The id of the empty tree in the repository's hash.
async fn empty_tree(dir: &Path) -> Result<String, CheckpointError> {
    // `mktree` reads the entries from standard input, which is empty.
    Ok(git::run(dir, &["mktree"]).await?)
}

/// Removes a file that the checkpoint does not have, and the directories
/// that are empty after it.
fn remove_file(dir: &Path, path: &str) -> Result<(), CheckpointError> {
    let full = dir.join(path);
    let io_error = |source| CheckpointError::Io {
        path: full.clone(),
        source,
    };
    match std::fs::symlink_metadata(&full) {
        // A nested repository: leave it alone rather than delete its history.
        Ok(metadata) if metadata.is_dir() => {
            tracing::warn!(path, "not removing a nested repository on restore");
            return Ok(());
        }
        Ok(_) => std::fs::remove_file(&full).map_err(io_error)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(io_error(error)),
    }
    let mut parent = full.parent();
    while let Some(directory) = parent.filter(|directory| *directory != dir) {
        // Not empty, or gone: either way the removal is done.
        if std::fs::remove_dir(directory).is_err() {
            break;
        }
        parent = directory.parent();
    }
    Ok(())
}

/// An index file of its own in the worktree's git directory, removed on
/// drop, so capturing and restoring leave the developer's index alone.
struct TemporaryIndex {
    path: PathBuf,
}

impl TemporaryIndex {
    /// Starts as a copy of the worktree's index when `copy` is set, which
    /// lets git skip files whose contents it already knows.
    async fn new(dir: &Path, copy: bool) -> Result<Self, CheckpointError> {
        let git_dir = PathBuf::from(git::run(dir, &["rev-parse", "--absolute-git-dir"]).await?);
        let number = TEMPORARY_INDEXES.fetch_add(1, Ordering::Relaxed);
        let path = git_dir.join(format!(
            "leiser-checkpoint-index-{}-{number}",
            std::process::id()
        ));
        let index = Self { path };
        let real = git_dir.join("index");
        if copy && real.exists() {
            std::fs::copy(&real, &index.path).map_err(|source| CheckpointError::Io {
                path: index.path.clone(),
                source,
            })?;
        }
        Ok(index)
    }
}

impl Drop for TemporaryIndex {
    fn drop(&mut self) {
        // Git may not have written it; then there is nothing to remove.
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Parses `git diff --name-status -z`: per file its path, its change and
/// how many sections the patch has for it. A type change, such as a file
/// that became a symlink, has two: one removes the old, one adds the new.
fn parse_name_status(output: &str) -> Result<Vec<(String, FileChange, usize)>, CheckpointError> {
    let mut fields = output.split('\0').filter(|field| !field.is_empty());
    let mut files = Vec::new();
    while let Some(status) = fields.next() {
        let mut path = || {
            fields
                .next()
                .map(str::to_owned)
                .ok_or(CheckpointError::Diff("a status without a path"))
        };
        let file = match status.chars().next() {
            Some('A') => (path()?, FileChange::Added, 1),
            Some('D') => (path()?, FileChange::Deleted, 1),
            Some('T') => (path()?, FileChange::Modified, 2),
            Some('R' | 'C') => {
                let from = path()?;
                (path()?, FileChange::Renamed { from }, 1)
            }
            Some(_) => (path()?, FileChange::Modified, 1),
            None => return Err(CheckpointError::Diff("an empty status")),
        };
        files.push(file);
    }
    Ok(files)
}

/// One file's section of a patch.
struct Section {
    binary: bool,
    hunks: Vec<Hunk>,
}

/// Splits a patch into its files' sections and their hunks.
fn parse_patch(patch: &str) -> Vec<Section> {
    let mut sections: Vec<Section> = Vec::new();
    let mut lines = patch.lines().peekable();
    while let Some(line) = lines.next() {
        if line.starts_with("diff --git ") {
            sections.push(Section {
                binary: false,
                hunks: Vec::new(),
            });
            continue;
        }
        let Some(section) = sections.last_mut() else {
            continue;
        };
        if line.starts_with("Binary files ") || line == "GIT binary patch" {
            section.binary = true;
            continue;
        }
        let Some((mut old, mut old_left, mut new, mut new_left, heading)) = parse_hunk_header(line)
        else {
            continue;
        };
        let mut hunk = Hunk {
            section: heading.to_owned(),
            lines: Vec::new(),
        };
        while old_left > 0 || new_left > 0 {
            let Some(line) = lines.next() else {
                break;
            };
            let (kind, text) = line.split_at(line.len().min(1));
            match kind {
                "+" => {
                    hunk.lines.push(DiffLine::Added {
                        new,
                        text: text.to_owned(),
                    });
                    new += 1;
                    new_left = new_left.saturating_sub(1);
                }
                "-" => {
                    hunk.lines.push(DiffLine::Removed {
                        old,
                        text: text.to_owned(),
                    });
                    old += 1;
                    old_left = old_left.saturating_sub(1);
                }
                // "\ No newline at end of file"
                "\\" => {}
                // A context line; empty with `diff.suppressBlankEmpty`.
                _ => {
                    hunk.lines.push(DiffLine::Context {
                        old,
                        new,
                        text: text.to_owned(),
                    });
                    old += 1;
                    new += 1;
                    old_left = old_left.saturating_sub(1);
                    new_left = new_left.saturating_sub(1);
                }
            }
        }
        // A last "\ No newline at end of file" belongs to this hunk.
        while lines.peek().is_some_and(|line| line.starts_with('\\')) {
            lines.next();
        }
        section.hunks.push(hunk);
    }
    sections
}

/// Parses `@@ -old,count +new,count @@ heading`.
fn parse_hunk_header(line: &str) -> Option<(u32, u32, u32, u32, &str)> {
    let rest = line.strip_prefix("@@ -")?;
    let (ranges, heading) = rest.split_once(" @@")?;
    let (old, new) = ranges.split_once(" +")?;
    let range = |range: &str| -> Option<(u32, u32)> {
        match range.split_once(',') {
            Some((start, count)) => Some((start.parse().ok()?, count.parse().ok()?)),
            None => Some((range.parse().ok()?, 1)),
        }
    };
    let (old_start, old_count) = range(old)?;
    let (new_start, new_count) = range(new)?;
    Some((
        old_start,
        old_count,
        new_start,
        new_count,
        heading.strip_prefix(' ').unwrap_or(heading),
    ))
}
