//! A chat's pull request: opened on the developer's command after the host
//! account check and a push, at most one per chat, and, once it is merged,
//! the removal of the chat's worktree and local branch.

use std::path::{Path, PathBuf};

use super::{Background, Place, Reply, State, verify_and_push};
use crate::git_host::{HostAccountAccess, NewPullRequest};
use crate::repo_config::RepositoryConfig;
use crate::store::PullRequestV3;
use crate::{
    Branch, ChatId, ChatPlace, ChatStatus, Command, CommandError, Event, PullRequest,
    PullRequestState, Response,
};

/// A chat's pull request as the harness service keeps it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ChatPullRequest {
    /// The push and the git host's answer are still on their way.
    Opening,
    Opened(PullRequest),
}

impl ChatPullRequest {
    pub(super) fn opened(&self) -> Option<&PullRequest> {
        match self {
            Self::Opening => None,
            Self::Opened(pull_request) => Some(pull_request),
        }
    }
}

/// Pull request work done in the background, for the command loop.
pub(super) enum PullRequestWork {
    Opened {
        chat: ChatId,
        result: Result<PullRequest, CommandError>,
        reply: Reply,
    },
    Checked {
        chat: ChatId,
        number: u64,
        result: Result<PullRequestState, CommandError>,
        reply: Reply,
    },
}

pub(super) fn stored(pull_request: &PullRequest) -> PullRequestV3 {
    PullRequestV3 {
        number: pull_request.number,
        url: pull_request.url.clone(),
        head: pull_request.head.clone(),
        base: pull_request.base.clone(),
        state: pull_request.state,
    }
}

pub(super) fn restored(pull_request: PullRequestV3) -> ChatPullRequest {
    ChatPullRequest::Opened(PullRequest {
        number: pull_request.number,
        url: pull_request.url,
        head: pull_request.head,
        base: pull_request.base,
        state: pull_request.state,
    })
}

impl Place {
    pub(super) fn shown(self) -> ChatPlace {
        match self {
            Self::MainCheckout => ChatPlace::MainCheckout,
            Self::Worktree => ChatPlace::Worktree,
            Self::RemovedWorktree => ChatPlace::RemovedWorktree,
        }
    }
}

impl State {
    /// Runs a command on the chat's branch, pull request or worktree;
    /// `reply` gets the answer once the git host has answered.
    pub(super) async fn execute_pull_request(&mut self, command: Command, reply: Reply) {
        match command {
            Command::Push { chat } => self.push(chat, reply).await,
            Command::OpenPullRequest { chat, title, body } => {
                self.open_pull_request(chat, title, body, reply).await;
            }
            Command::CheckPullRequest { chat } => self.check_pull_request(chat, reply).await,
            Command::RemoveWorktree { chat } => {
                let _ = reply.send(self.remove_worktree(chat).await);
            }
            // `execute` sends only the commands above here.
            _ => {
                let _ = reply.send(Err(CommandError::ServiceStopped));
            }
        }
    }

    /// Fails for a chat whose worktree was removed: it takes no more turns.
    pub(super) fn check_not_removed(&self, id: ChatId) -> Result<(), CommandError> {
        match self.chats.get(&id) {
            Some(chat) if chat.place == Place::RemovedWorktree => {
                Err(CommandError::WorktreeRemoved { chat: id })
            }
            _ => Ok(()),
        }
    }

    /// Checks the host account, pushes the chat's branch and opens its
    /// pull request, in the background; `reply` gets the answer.
    pub(super) async fn open_pull_request(
        &mut self,
        id: ChatId,
        title: String,
        body: String,
        reply: Reply,
    ) {
        let prepared = self.prepare_pull_request(id, title, body).await;
        let (working_directory, name, access, request) = match prepared {
            Ok(prepared) => prepared,
            Err(error) => {
                tracing::debug!(chat = ?id, %error, "pull request refused");
                // The caller may have stopped waiting; nothing to do then.
                let _ = reply.send(Err(error));
                return;
            }
        };
        if let Some(chat) = self.chats.get_mut(&id) {
            chat.pull_request = Some(ChatPullRequest::Opening);
        }
        let git_host = self.git_host.clone();
        let background = self.background.clone();
        self.background_running += 1;
        tokio::spawn(async move {
            let result = async {
                let remote = verify_and_push(
                    git_host.as_ref(),
                    &working_directory,
                    &request.head,
                    &name,
                    &access,
                )
                .await?;
                let opened = git_host
                    .open_pull_request(&access, &remote, &request)
                    .await
                    .map_err(|error| CommandError::PullRequestFailed {
                        reason: error.to_string(),
                    })?;
                Ok(PullRequest {
                    number: opened.number,
                    url: opened.url,
                    head: request.head,
                    base: request.base,
                    state: PullRequestState::Open,
                })
            }
            .await;
            // The service has stopped; the caller learns that from `reply`.
            let _ = background
                .send(Background::PullRequest(PullRequestWork::Opened {
                    chat: id,
                    result,
                    reply,
                }))
                .await;
        });
    }

    async fn prepare_pull_request(
        &self,
        id: ChatId,
        title: String,
        body: String,
    ) -> Result<(PathBuf, String, HostAccountAccess, NewPullRequest), CommandError> {
        let chat = self
            .chats
            .get(&id)
            .ok_or(CommandError::UnknownChat { chat: id })?;
        let title = title.trim().to_owned();
        if title.is_empty() {
            return Err(CommandError::EmptyPullRequestTitle);
        }
        match &chat.pull_request {
            Some(ChatPullRequest::Opening) => {
                return Err(CommandError::PullRequestOpening { chat: id });
            }
            Some(ChatPullRequest::Opened(pull_request)) => {
                return Err(CommandError::PullRequestExists {
                    chat: id,
                    number: pull_request.number,
                });
            }
            None => {}
        }
        let repository = self.repository_path(chat.repository)?;
        let (working_directory, branch, name, access) = self.prepare_push(id).await?;
        // Another chat in the same worktree works on the same branch.
        let other = self.chats.iter().find_map(|(other, chat)| {
            let same_worktree = *other != id
                && chat.place == Place::Worktree
                && chat.working_directory == working_directory;
            match &chat.pull_request {
                Some(ChatPullRequest::Opened(pull_request))
                    if same_worktree && pull_request.head == branch =>
                {
                    Some(pull_request.number)
                }
                _ => None,
            }
        });
        if let Some(number) = other {
            return Err(CommandError::PullRequestExists { chat: id, number });
        }
        let base = base_branch(&repository).await?;
        Ok((
            working_directory,
            name,
            access,
            NewPullRequest {
                head: branch,
                base,
                title,
                body,
            },
        ))
    }

    /// Asks the git host for the state of the chat's pull request, in the
    /// background; `reply` gets the answer.
    pub(super) async fn check_pull_request(&mut self, id: ChatId, reply: Reply) {
        let prepared = self.prepare_check(id).await;
        let (repository, number, access) = match prepared {
            Ok(prepared) => prepared,
            Err(error) => {
                let _ = reply.send(Err(error));
                return;
            }
        };
        let git_host = self.git_host.clone();
        let background = self.background.clone();
        self.background_running += 1;
        tokio::spawn(async move {
            let result = async {
                let remote = crate::git::origin_url(&repository).await.ok_or_else(|| {
                    CommandError::PullRequestFailed {
                        reason: "the repository has no remote `origin`".to_owned(),
                    }
                })?;
                git_host
                    .pull_request_state(&access, &remote, number)
                    .await
                    .map_err(|error| CommandError::PullRequestFailed {
                        reason: error.to_string(),
                    })
            }
            .await;
            let _ = background
                .send(Background::PullRequest(PullRequestWork::Checked {
                    chat: id,
                    number,
                    result,
                    reply,
                }))
                .await;
        });
    }

    async fn prepare_check(
        &self,
        id: ChatId,
    ) -> Result<(PathBuf, u64, HostAccountAccess), CommandError> {
        let chat = self
            .chats
            .get(&id)
            .ok_or(CommandError::UnknownChat { chat: id })?;
        let number = match &chat.pull_request {
            Some(ChatPullRequest::Opened(pull_request)) => pull_request.number,
            Some(ChatPullRequest::Opening) | None => {
                return Err(CommandError::NoPullRequest { chat: id });
            }
        };
        let repository = chat.repository;
        let (_, binding) = self
            .bind_host_account(repository)
            .await?
            .ok_or(CommandError::NoHostAccount { repository })?;
        Ok((self.repository_path(repository)?, number, binding.access))
    }

    pub(super) fn pull_request_finished(&mut self, work: PullRequestWork) {
        match work {
            PullRequestWork::Opened {
                chat: id,
                result,
                reply,
            } => {
                let Some(chat) = self.chats.get_mut(&id) else {
                    let _ = reply.send(
                        result.map(|pull_request| Response::PullRequestOpened { pull_request }),
                    );
                    return;
                };
                let answer = match result {
                    Ok(pull_request) => {
                        chat.pull_request = Some(ChatPullRequest::Opened(pull_request.clone()));
                        self.keep_pull_request(id, &pull_request);
                        Ok(Response::PullRequestOpened { pull_request })
                    }
                    Err(error) => {
                        chat.pull_request = None;
                        Err(error)
                    }
                };
                let _ = reply.send(answer);
            }
            PullRequestWork::Checked {
                chat: id,
                number,
                result,
                reply,
            } => {
                let answer = result.and_then(|state| {
                    let pull_request = self
                        .chats
                        .get_mut(&id)
                        .and_then(|chat| match &mut chat.pull_request {
                            Some(ChatPullRequest::Opened(pull_request))
                                if pull_request.number == number =>
                            {
                                Some(pull_request)
                            }
                            _ => None,
                        })
                        .ok_or(CommandError::NoPullRequest { chat: id })?;
                    let changed = pull_request.state != state;
                    pull_request.state = state;
                    let pull_request = pull_request.clone();
                    if changed {
                        self.keep_pull_request(id, &pull_request);
                    }
                    Ok(Response::PullRequest { pull_request })
                });
                let _ = reply.send(answer);
            }
        }
    }

    /// Stores the chat's pull request and tells the desktop app.
    fn keep_pull_request(&self, id: ChatId, pull_request: &PullRequest) {
        if let Err(error) = self.save() {
            tracing::error!(chat = ?id, %error, "could not store the pull request");
        }
        self.emit(Event::PullRequestChanged {
            chat: id,
            pull_request: pull_request.clone(),
        });
    }

    /// Removes the chat's worktree and its local branch, once its pull
    /// request is merged and nothing would be lost. Every chat in the
    /// worktree is done and read-only afterwards.
    pub(super) async fn remove_worktree(&mut self, id: ChatId) -> Result<Response, CommandError> {
        let chat = self
            .chats
            .get(&id)
            .ok_or(CommandError::UnknownChat { chat: id })?;
        match chat.place {
            Place::MainCheckout => return Err(CommandError::NotInAWorktree { chat: id }),
            Place::RemovedWorktree => return Ok(Response::WorktreeRemoved),
            Place::Worktree => {}
        }
        match &chat.pull_request {
            Some(ChatPullRequest::Opened(pull_request)) => {
                if pull_request.state != PullRequestState::Merged {
                    return Err(CommandError::PullRequestNotMerged {
                        chat: id,
                        number: pull_request.number,
                    });
                }
            }
            Some(ChatPullRequest::Opening) | None => {
                return Err(CommandError::NoPullRequest { chat: id });
            }
        }
        let working_directory = chat.working_directory.clone();
        let repository = self.repository_path(chat.repository)?;
        let sharing: Vec<ChatId> = self
            .chats
            .iter()
            .filter(|(_, other)| {
                other.place == Place::Worktree && other.working_directory == working_directory
            })
            .map(|(other, _)| *other)
            .collect();
        for other in &sharing {
            if let Some(chat) = self.chats.get(other)
                && (chat.status() == ChatStatus::Working || chat.turn_running())
            {
                return Err(CommandError::TurnRunning { chat: *other });
            }
        }
        remove(&repository, &working_directory, id).await?;
        for other in &sharing {
            self.end_session(*other);
            if let Some(chat) = self.chats.get_mut(other) {
                chat.place = Place::RemovedWorktree;
                chat.done = true;
            }
        }
        if let Err(error) = self.save() {
            tracing::error!(%error, "could not store the removed worktree");
        }
        for other in sharing {
            self.emit(Event::StatusChanged {
                chat: other,
                status: ChatStatus::Done,
            });
            self.emit(Event::WorktreeRemoved { chat: other });
        }
        Ok(Response::WorktreeRemoved)
    }
}

/// The branch a pull request from the repository goes into: its
/// `base_branch` without the remote, else `origin`'s default branch.
async fn base_branch(repository: &Path) -> Result<String, CommandError> {
    let config =
        RepositoryConfig::load(repository).map_err(|error| CommandError::RepositoryConfig {
            reason: error.to_string(),
        })?;
    if let Some(base) = config.worktree.base_branch {
        let remotes = crate::git::run(repository, &["remote"])
            .await
            .unwrap_or_default();
        let branch = base
            .split_once('/')
            .filter(|(remote, _)| remotes.lines().any(|line| line == *remote))
            .map_or(base.as_str(), |(_, branch)| branch);
        return Ok(branch.to_owned());
    }
    let head = crate::git::run(
        repository,
        &["symbolic-ref", "--quiet", "refs/remotes/origin/HEAD"],
    )
    .await
    .map_err(|_| CommandError::NoBaseBranch)?;
    head.strip_prefix("refs/remotes/origin/")
        .map(str::to_owned)
        .ok_or(CommandError::NoBaseBranch)
}

/// Removes the worktree at `working_directory` and its branch, unless it
/// has uncommitted changes or commits on no remote.
async fn remove(
    repository: &Path,
    working_directory: &Path,
    id: ChatId,
) -> Result<(), CommandError> {
    let failed = |error: crate::git::GitError| CommandError::Worktree {
        reason: error.to_string(),
    };
    let changes = crate::git::run(working_directory, &["status", "--porcelain"])
        .await
        .map_err(failed)?;
    if !changes.is_empty() {
        return Err(CommandError::WorktreeHasChanges { chat: id });
    }
    let unpushed = crate::git::run(
        working_directory,
        &["rev-list", "--count", "HEAD", "--not", "--remotes"],
    )
    .await
    .map_err(failed)?;
    if unpushed != "0" {
        return Err(CommandError::WorktreeHasUnpushedCommits { chat: id });
    }
    let branch = crate::git::current_branch(working_directory).await;
    let path = working_directory.display().to_string();
    // `--force` only for ignored files such as build output: the checks
    // above found nothing else.
    crate::git::run(repository, &["worktree", "remove", "--force", &path])
        .await
        .map_err(failed)?;
    if let Branch::Named { name } = branch {
        crate::git::run(repository, &["branch", "-D", &name])
            .await
            .map_err(failed)?;
    }
    Ok(())
}
