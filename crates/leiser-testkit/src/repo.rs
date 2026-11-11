//! A temporary git repository, optionally cloned from a local remote.

use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

use tempfile::TempDir;

/// The default branch of [`TestRepo::with_remote`]'s remote.
pub const DEFAULT_BRANCH: &str = "main";

/// A git repository in a temporary directory, removed on drop.
#[derive(Debug)]
pub struct TestRepo {
    path: PathBuf,
    /// The bare repository `origin` points to, for [`TestRepo::with_remote`].
    remote: Option<PathBuf>,
    _dir: TempDir,
}

impl TestRepo {
    /// An empty repository without commits or remote.
    ///
    /// # Errors
    ///
    /// Returns an error when the directory cannot be created or `git init`
    /// fails.
    pub fn new() -> io::Result<Self> {
        let dir = tempfile::tempdir()?;
        git(dir.path(), &["init", "--quiet"])?;
        Ok(Self {
            path: dir.path().to_path_buf(),
            remote: None,
            _dir: dir,
        })
    }

    /// A clone of a local bare remote `origin` whose default branch
    /// [`DEFAULT_BRANCH`] has one commit with a `README.md`, as a clone
    /// from a git host would be.
    ///
    /// # Errors
    ///
    /// Returns an error when a directory cannot be created or git fails.
    pub fn with_remote() -> io::Result<Self> {
        let dir = tempfile::tempdir()?;
        let seed = dir.path().join("seed");
        let remote = dir.path().join("origin.git");
        let path = dir.path().join("repo");
        git(
            dir.path(),
            &[
                "init",
                "--quiet",
                "--initial-branch",
                DEFAULT_BRANCH,
                "seed",
            ],
        )?;
        std::fs::write(seed.join("README.md"), "# test repository\n")?;
        commit_all(&seed, "Initial commit")?;
        git(
            dir.path(),
            &["clone", "--quiet", "--bare", "seed", "origin.git"],
        )?;
        git(dir.path(), &["clone", "--quiet", "origin.git", "repo"])?;
        std::fs::remove_dir_all(seed)?;
        Ok(Self {
            path,
            remote: Some(remote),
            _dir: dir,
        })
    }

    /// The repository's main checkout.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Commits `contents` to `file` on the remote's `branch`, as someone
    /// else pushing would: the repository's own refs do not see it until
    /// it fetches. The branch starts from the default branch if it is new.
    ///
    /// # Errors
    ///
    /// Returns an error when the repository has no remote or git fails.
    pub fn push_to_remote(&self, branch: &str, file: &str, contents: &str) -> io::Result<()> {
        let remote = self
            .remote
            .as_deref()
            .ok_or_else(|| io::Error::other("the test repository has no remote"))?;
        let elsewhere = tempfile::tempdir()?;
        let clone = elsewhere.path().join("clone");
        git(
            elsewhere.path(),
            &["clone", "--quiet", &remote.display().to_string(), "clone"],
        )?;
        let exists = git(
            &clone,
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("origin/{branch}"),
            ],
        )
        .is_ok();
        let start = if exists {
            format!("origin/{branch}")
        } else {
            format!("origin/{DEFAULT_BRANCH}")
        };
        git(&clone, &["checkout", "--quiet", "-B", branch, &start])?;
        std::fs::write(clone.join(file), contents)?;
        commit_all(&clone, &format!("Change {file}"))?;
        git(&clone, &["push", "--quiet", "origin", branch])?;
        Ok(())
    }

    /// The commit the remote's `branch` points to, `None` when the remote
    /// has no such branch.
    ///
    /// # Errors
    ///
    /// Returns an error when the repository has no remote or git fails.
    pub fn remote_branch(&self, branch: &str) -> io::Result<Option<String>> {
        let remote = self
            .remote
            .as_deref()
            .ok_or_else(|| io::Error::other("the test repository has no remote"))?;
        let output = Command::new("git")
            .arg("--git-dir")
            .arg(remote)
            .args(["rev-parse", "--verify", "--quiet"])
            .arg(format!("refs/heads/{branch}"))
            .output()?;
        Ok(output
            .status
            .success()
            .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned()))
    }

    /// Runs git in the main checkout and returns its trimmed output.
    ///
    /// # Errors
    ///
    /// Returns an error when git cannot run or fails.
    pub fn git(&self, args: &[&str]) -> io::Result<String> {
        git(&self.path, args)
    }
}

/// Commits everything in `dir`, independent of the machine's git config.
fn commit_all(dir: &Path, message: &str) -> io::Result<()> {
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
            "-c",
            "core.hooksPath=/dev/null",
            "commit",
            "--quiet",
            "--message",
            message,
        ],
    )?;
    Ok(())
}

/// Runs git in `dir` and returns its trimmed standard output.
fn git(dir: &Path, args: &[&str]) -> io::Result<String> {
    let output = Command::new("git").arg("-C").arg(dir).args(args).output()?;
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}
