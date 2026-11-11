//! A fake git host: its login signs a host account in by writing a file
//! into the host account's config directory, and a push signs in as the
//! account written there. Pushes go to the test repository's local remote;
//! pull requests are kept in memory, shared by the fake's clones. A pull
//! request's head commit is whatever its branch points to on that remote;
//! tests fail and pass its checks, make it conflict and comment on it, and
//! see whether a poll got a full answer or `304 Not Modified`. Wayfinder
//! maps are kept in memory too: tests add maps and tickets, block, close
//! and assign tickets, and see how each poll of the maps was answered.

use std::collections::BTreeMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};

use leiser_service::{
    Blocker, CheckOutcome, CommentLocation, CommitId, GitHost, GitHostError, GitHostFuture,
    HostAccountAccess, IssueRef, LoginOutcome, LoginProcess, MapIssue, Mergeable, NewPullRequest,
    OpenedPullRequest, PollMemo, PolledPullRequest, PolledWayfinderMaps, PullRequestActivity,
    PullRequestCheck, PullRequestComment, PullRequestState, TicketIssue, TicketType,
};
use serde::Deserialize;
use tokio::sync::{mpsc, oneshot};

/// The file the fake git host's login writes into the host account's
/// config directory: the account it signed in as.
pub const FAKE_GIT_HOST_LOGIN_FILE: &str = "fake-git-host-login";
/// A file in the host account's config directory that lists pull request
/// numbers, one per line: the fake git host reports them merged. For
/// trying the desktop app, where the fake runs inside the service process.
pub const FAKE_GIT_HOST_MERGED_FILE: &str = "fake-git-host-merged";
/// A JSON file in the host account's config directory with activity on
/// pull requests, by number, which every poll adds: for trying auto-fix in
/// the desktop app. For example
/// `{"1": {"failed_checks": {"test": "assertion failed"}, "conflicting": false,
/// "comments": [{"author": "reviewer", "body": "Rename it",
/// "location": {"path": "src/lib.rs", "line": 3}}]}}`.
/// A check listed there fails on every head commit.
pub const FAKE_GIT_HOST_ACTIVITY_FILE: &str = "fake-git-host-activity.json";
/// A JSON file in the host account's config directory with wayfinder maps,
/// which every poll of the maps reads after those kept in memory: for
/// trying the wayfinder map in the desktop app. For example
/// `{"maps": [{"number": 1, "title": "Map: v2", "tickets": [
/// {"number": 2, "title": "Pick a store", "type": "research"},
/// {"number": 3, "title": "Sketch the UI", "type": "prototype", "open": false},
/// {"number": 4, "title": "Agree on the API", "type": "grilling",
/// "assignees": ["someone"], "blocked_by": [2]}]}]}`.
pub const FAKE_GIT_HOST_WAYFINDER_FILE: &str = "fake-git-host-wayfinder.json";
/// The memo key the fake git host keeps its one entity tag under.
const TAG: &str = "pull";
/// The memo key of the entity tag of the wayfinder maps.
const MAPS_TAG: &str = "maps";
/// The credential helper the fake git host gives bound repositories. A
/// local remote never asks for credentials.
pub const FAKE_CREDENTIAL_HELPER: &str = "!leiser-fake-git-host-credential";

/// A git host for tests and for trying the desktop app without a git host
/// account. Clones share what the next login signs in as.
#[derive(Debug, Clone, Default)]
pub struct FakeGitHost {
    next_login: Arc<Mutex<Option<String>>>,
    pull_requests: Arc<Mutex<Vec<FakePullRequest>>>,
    activity: Arc<Mutex<BTreeMap<u64, Activity>>>,
    polls_fail: Arc<Mutex<bool>>,
    maps: Arc<Mutex<FakeMaps>>,
    map_polls: Arc<Mutex<Vec<FakePoll>>>,
}

/// Wayfinder maps on the fake git host, as kept in memory and in
/// [`FAKE_GIT_HOST_WAYFINDER_FILE`].
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct FakeMaps {
    maps: Vec<FakeMap>,
}

#[derive(Debug, Deserialize)]
struct FakeMap {
    number: u64,
    title: String,
    #[serde(default)]
    tickets: Vec<FakeTicket>,
}

#[derive(Debug, Clone, Deserialize)]
struct FakeTicket {
    number: u64,
    title: String,
    #[serde(default, rename = "type")]
    ticket_type: Option<TicketType>,
    #[serde(default = "open")]
    open: bool,
    #[serde(default)]
    assignees: Vec<String>,
    /// Issue numbers; one that is no ticket counts as an open issue.
    #[serde(default)]
    blocked_by: Vec<u64>,
}

fn open() -> bool {
    true
}

/// A map or ticket the fake git host does not have.
#[derive(Debug, thiserror::Error)]
#[error("no map or ticket #{0} on the fake git host")]
pub struct UnknownIssue(pub u64);

/// How the fake git host answered a poll.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FakePoll {
    /// With the whole pull request: it had changed, or the poll sent no
    /// entity tag.
    Full,
    /// `304 Not Modified`: nothing changed since the tag the poll sent.
    NotModified,
}

/// What happened on a pull request besides pushes.
#[derive(Debug, Default)]
struct Activity {
    /// Check outcomes by head commit, then check name.
    checks: BTreeMap<CommitId, BTreeMap<String, FakeCheck>>,
    conflicting: bool,
    comments: Vec<PullRequestComment>,
    polls: Vec<FakePoll>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum FakeCheck {
    Pending,
    Passed,
    Failed { log: String },
}

/// The contents of [`FAKE_GIT_HOST_ACTIVITY_FILE`] for one pull request.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct ActivityFile {
    failed_checks: BTreeMap<String, String>,
    conflicting: bool,
    comments: Vec<CommentFile>,
}

#[derive(Debug, Deserialize)]
struct CommentFile {
    author: String,
    body: String,
    location: Option<CommentLocation>,
}

/// A pull request opened on the fake git host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FakePullRequest {
    pub number: u64,
    pub url: String,
    /// The repository's remote URL.
    pub remote: String,
    pub head: String,
    pub base: String,
    pub title: String,
    pub body: String,
    /// The account the host account was signed in as when it opened it.
    pub opened_by: String,
    pub state: PullRequestState,
}

impl FakeGitHost {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Makes the next login sign in as `login` instead of the host
    /// account's own login, as a developer who picked the wrong account
    /// in the browser would.
    pub fn next_login_signs_in_as(&self, login: &str) {
        *self
            .next_login
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(login.to_owned());
    }

    /// Every pull request opened so far, oldest first.
    #[must_use]
    pub fn pull_requests(&self) -> Vec<FakePullRequest> {
        self.pull_requests
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Merges pull request `number`, as a reviewer on the git host would.
    pub fn merge(&self, number: u64) {
        self.set_state(number, PullRequestState::Merged);
    }

    /// Sets the state of pull request `number`.
    pub fn set_state(&self, number: u64, state: PullRequestState) {
        let mut pull_requests = self
            .pull_requests
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(pull_request) = pull_requests
            .iter_mut()
            .find(|pull_request| pull_request.number == number)
        {
            pull_request.state = state;
        }
    }

    /// Makes check `name` of pull request `number` fail on the pull
    /// request's head commit, with `log` as the end of its log.
    pub fn fail_check(&self, number: u64, name: &str, log: &str) {
        self.set_check(
            number,
            name,
            FakeCheck::Failed {
                log: log.to_owned(),
            },
        );
    }

    /// Makes check `name` of pull request `number` pass on the pull
    /// request's head commit.
    pub fn pass_check(&self, number: u64, name: &str) {
        self.set_check(number, name, FakeCheck::Passed);
    }

    /// Makes check `name` of pull request `number` run on the pull
    /// request's head commit.
    pub fn start_check(&self, number: u64, name: &str) {
        self.set_check(number, name, FakeCheck::Pending);
    }

    fn set_check(&self, number: u64, name: &str, check: FakeCheck) {
        let Some(head) = self.head_commit(number) else {
            return;
        };
        self.with_activity(number, |activity| {
            activity
                .checks
                .entry(head)
                .or_default()
                .insert(name.to_owned(), check);
        });
    }

    /// Makes pull request `number` conflict with its base branch, or not.
    pub fn set_conflicting(&self, number: u64, conflicting: bool) {
        self.with_activity(number, |activity| activity.conflicting = conflicting);
    }

    /// Posts a comment on pull request `number`, on `location` or on the
    /// whole pull request.
    pub fn comment(
        &self,
        number: u64,
        author: &str,
        body: &str,
        location: Option<CommentLocation>,
    ) {
        self.with_activity(number, |activity| {
            let id = format!("fake-comment-{}", activity.comments.len() + 1);
            activity.comments.push(PullRequestComment {
                id,
                author: author.to_owned(),
                body: body.to_owned(),
                location,
            });
        });
    }

    /// Makes polls fail, as a git host that cannot be reached would, or
    /// answer again.
    pub fn fail_polls(&self, fail: bool) {
        *self
            .polls_fail
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = fail;
    }

    /// How the fake git host answered each poll of pull request `number`,
    /// oldest first.
    #[must_use]
    pub fn polls(&self, number: u64) -> Vec<FakePoll> {
        self.activity
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&number)
            .map(|activity| activity.polls.clone())
            .unwrap_or_default()
    }

    /// The commit pull request `number`'s branch points to on the remote.
    #[must_use]
    pub fn head_commit(&self, number: u64) -> Option<CommitId> {
        let (remote, head) = self.pull_requests().into_iter().find_map(|pull_request| {
            (pull_request.number == number).then_some((pull_request.remote, pull_request.head))
        })?;
        let output = std::process::Command::new("git")
            .args(["ls-remote", &remote, &format!("refs/heads/{head}")])
            .output()
            .ok()?;
        commit_of(&String::from_utf8_lossy(&output.stdout))
    }

    /// Opens a wayfinder map: an issue labelled `wayfinder:map`.
    pub fn add_map(&self, number: u64, title: &str) {
        self.lock_maps().maps.push(FakeMap {
            number,
            title: title.to_owned(),
            tickets: Vec::new(),
        });
    }

    /// Adds an open, unassigned ticket at the end of map `map`.
    ///
    /// # Errors
    ///
    /// Returns [`UnknownIssue`] when there is no map `map`.
    pub fn add_ticket(
        &self,
        map: u64,
        number: u64,
        title: &str,
        ticket_type: Option<TicketType>,
    ) -> Result<(), UnknownIssue> {
        let mut maps = self.lock_maps();
        let found = maps
            .maps
            .iter_mut()
            .find(|candidate| candidate.number == map)
            .ok_or(UnknownIssue(map))?;
        found.tickets.push(FakeTicket {
            number,
            title: title.to_owned(),
            ticket_type,
            open: true,
            assignees: Vec::new(),
            blocked_by: Vec::new(),
        });
        Ok(())
    }

    /// Makes issue `blocker` block ticket `ticket`.
    ///
    /// # Errors
    ///
    /// Returns [`UnknownIssue`] when there is no ticket `ticket`.
    pub fn block(&self, ticket: u64, blocker: u64) -> Result<(), UnknownIssue> {
        self.with_ticket(ticket, |ticket| ticket.blocked_by.push(blocker))
    }

    /// Closes ticket `ticket`, as resolving it on the git host would.
    ///
    /// # Errors
    ///
    /// Returns [`UnknownIssue`] when there is no ticket `ticket`.
    pub fn close_ticket(&self, ticket: u64) -> Result<(), UnknownIssue> {
        self.with_ticket(ticket, |ticket| ticket.open = false)
    }

    /// Assigns ticket `ticket` to `login`, as claiming it would.
    ///
    /// # Errors
    ///
    /// Returns [`UnknownIssue`] when there is no ticket `ticket`.
    pub fn assign(&self, ticket: u64, login: &str) -> Result<(), UnknownIssue> {
        self.with_ticket(ticket, |ticket| ticket.assignees.push(login.to_owned()))
    }

    /// How the fake git host answered each poll of the wayfinder maps,
    /// oldest first.
    #[must_use]
    pub fn map_polls(&self) -> Vec<FakePoll> {
        self.map_polls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn lock_maps(&self) -> std::sync::MutexGuard<'_, FakeMaps> {
        self.maps.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn with_ticket(
        &self,
        number: u64,
        change: impl FnOnce(&mut FakeTicket),
    ) -> Result<(), UnknownIssue> {
        let mut maps = self.lock_maps();
        let ticket = maps
            .maps
            .iter_mut()
            .flat_map(|map| map.tickets.iter_mut())
            .find(|ticket| ticket.number == number)
            .ok_or(UnknownIssue(number))?;
        change(ticket);
        Ok(())
    }

    /// The maps as a poll finds them now: those in memory, then those in
    /// the host account's file, newest first.
    fn maps_now(&self, access: &HostAccountAccess) -> Vec<MapIssue> {
        let from_file =
            std::fs::read_to_string(access.config_dir.join(FAKE_GIT_HOST_WAYFINDER_FILE))
                .ok()
                .and_then(|text| serde_json::from_str::<FakeMaps>(&text).ok())
                .unwrap_or_default();
        let kept = self.lock_maps();
        let maps: Vec<&FakeMap> = kept.maps.iter().chain(&from_file.maps).collect();
        let tickets: BTreeMap<u64, &FakeTicket> = maps
            .iter()
            .flat_map(|map| &map.tickets)
            .map(|ticket| (ticket.number, ticket))
            .collect();
        let url = |number: u64| format!("https://{}/fake/issues/{number}", access.git_host);
        let mut issues: Vec<MapIssue> = maps
            .iter()
            .map(|map| MapIssue {
                number: map.number,
                title: map.title.clone(),
                url: url(map.number),
                tickets: map
                    .tickets
                    .iter()
                    .map(|ticket| TicketIssue {
                        number: ticket.number,
                        title: ticket.title.clone(),
                        url: url(ticket.number),
                        ticket_type: ticket.ticket_type,
                        open: ticket.open,
                        assignees: ticket.assignees.clone(),
                        blocked_by: ticket
                            .blocked_by
                            .iter()
                            .map(|&number| Blocker {
                                issue: IssueRef::Here { number },
                                open: tickets.get(&number).is_none_or(|blocker| blocker.open),
                            })
                            .collect(),
                    })
                    .collect(),
            })
            .collect();
        issues.sort_by_key(|map| std::cmp::Reverse(map.number));
        issues
    }

    fn with_activity(&self, number: u64, change: impl FnOnce(&mut Activity)) {
        change(
            self.activity
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .entry(number)
                .or_default(),
        );
    }

    /// The pull request as a poll finds it now, without its tag.
    async fn activity_now(
        &self,
        access: &HostAccountAccess,
        pull_request: &FakePullRequest,
    ) -> Result<PullRequestActivity, GitHostError> {
        let head_commit = remote_commit(&pull_request.remote, &pull_request.head).await?;
        let base_commit = remote_commit(&pull_request.remote, &pull_request.base).await?;
        let from_file =
            std::fs::read_to_string(access.config_dir.join(FAKE_GIT_HOST_ACTIVITY_FILE))
                .ok()
                .and_then(|text| serde_json::from_str::<BTreeMap<String, ActivityFile>>(&text).ok())
                .and_then(|mut file| file.remove(&pull_request.number.to_string()))
                .unwrap_or_default();
        let activity = self.activity.lock().unwrap_or_else(PoisonError::into_inner);
        let known = activity.get(&pull_request.number);
        let mut checks: BTreeMap<String, FakeCheck> = known
            .and_then(|known| known.checks.get(&head_commit))
            .cloned()
            .unwrap_or_default();
        for (name, log) in from_file.failed_checks {
            checks.insert(name, FakeCheck::Failed { log });
        }
        let mut comments = known
            .map(|known| known.comments.clone())
            .unwrap_or_default();
        comments.extend(
            from_file
                .comments
                .into_iter()
                .enumerate()
                .map(|(index, comment)| PullRequestComment {
                    id: format!("fake-file-comment-{}", index + 1),
                    author: comment.author,
                    body: comment.body,
                    location: comment.location,
                }),
        );
        let conflicting = from_file.conflicting || known.is_some_and(|known| known.conflicting);
        Ok(PullRequestActivity {
            state: pull_request.state,
            checks: checks
                .into_iter()
                .map(|(name, check)| PullRequestCheck {
                    id: format!("{head_commit}-{name}"),
                    outcome: match check {
                        FakeCheck::Pending => CheckOutcome::Pending,
                        FakeCheck::Passed => CheckOutcome::Passed,
                        FakeCheck::Failed { log } => CheckOutcome::Failed {
                            url: format!("{}/checks/{name}", pull_request.url),
                            log,
                        },
                    },
                    name,
                })
                .collect(),
            head_commit,
            base_commit,
            mergeable: if conflicting {
                Mergeable::Conflicting
            } else {
                Mergeable::Clean
            },
            comments,
        })
    }

    /// The account the host account whose config directory is
    /// `config_dir` is signed in as, if any.
    #[must_use]
    pub fn signed_in_as(config_dir: &Path) -> Option<String> {
        std::fs::read_to_string(config_dir.join(FAKE_GIT_HOST_LOGIN_FILE))
            .ok()
            .map(|login| login.trim().to_owned())
    }
}

impl GitHost for FakeGitHost {
    fn login(&self, access: &HostAccountAccess) -> Result<LoginProcess, GitHostError> {
        let login = self
            .next_login
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
            .unwrap_or_else(|| access.login.clone());
        let (lines, output) = mpsc::channel(4);
        let (input, _answers) = mpsc::channel(1);
        let (finished, outcome) = oneshot::channel();
        let written = std::fs::create_dir_all(&access.config_dir).and_then(|()| {
            std::fs::write(access.config_dir.join(FAKE_GIT_HOST_LOGIN_FILE), &login)
        });
        let (line, result) = match written {
            Ok(()) => (
                format!(
                    "Logged in to {} as {login} (fake git host)",
                    access.git_host
                ),
                LoginOutcome::SignedIn,
            ),
            Err(error) => (
                format!("could not sign in: {error}"),
                LoginOutcome::Failed {
                    reason: error.to_string(),
                },
            ),
        };
        // Both channels have room; the receivers are returned below.
        let _ = lines.try_send(line);
        let _ = finished.send(result);
        Ok(LoginProcess {
            output,
            input,
            outcome,
        })
    }

    fn credential_helper(&self, _access: &HostAccountAccess) -> Result<String, GitHostError> {
        Ok(FAKE_CREDENTIAL_HELPER.to_owned())
    }

    fn account_for_push<'a>(
        &'a self,
        access: &'a HostAccountAccess,
        _remote: &'a str,
    ) -> GitHostFuture<'a, Result<String, GitHostError>> {
        Box::pin(async move {
            Self::signed_in_as(&access.config_dir).ok_or_else(|| GitHostError::NotSignedIn {
                reason: format!("no login in {}", access.config_dir.display()),
            })
        })
    }

    fn open_pull_request<'a>(
        &'a self,
        access: &'a HostAccountAccess,
        remote: &'a str,
        request: &'a NewPullRequest,
    ) -> GitHostFuture<'a, Result<OpenedPullRequest, GitHostError>> {
        Box::pin(async move {
            let opened_by = Self::signed_in_as(&access.config_dir).ok_or_else(|| {
                GitHostError::NotSignedIn {
                    reason: format!("no login in {}", access.config_dir.display()),
                }
            })?;
            let mut pull_requests = self
                .pull_requests
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            // As GitHub does: one open pull request per head branch.
            if pull_requests.iter().any(|pull_request| {
                pull_request.remote == remote
                    && pull_request.head == request.head
                    && pull_request.state == PullRequestState::Open
            }) {
                return Err(GitHostError::Refused {
                    reason: format!("a pull request for {} already exists", request.head),
                });
            }
            let number = pull_requests.len() as u64 + 1;
            let url = format!("https://{}/fake/pull/{number}", access.git_host);
            pull_requests.push(FakePullRequest {
                number,
                url: url.clone(),
                remote: remote.to_owned(),
                head: request.head.clone(),
                base: request.base.clone(),
                title: request.title.clone(),
                body: request.body.clone(),
                opened_by,
                state: PullRequestState::Open,
            });
            Ok(OpenedPullRequest { number, url })
        })
    }

    fn pull_request_state<'a>(
        &'a self,
        access: &'a HostAccountAccess,
        remote: &'a str,
        number: u64,
    ) -> GitHostFuture<'a, Result<PullRequestState, GitHostError>> {
        Box::pin(async move {
            let merged_by_file =
                std::fs::read_to_string(access.config_dir.join(FAKE_GIT_HOST_MERGED_FILE))
                    .is_ok_and(|merged| {
                        merged.lines().any(|line| line.trim() == number.to_string())
                    });
            if merged_by_file {
                self.set_state(number, PullRequestState::Merged);
            }
            self.pull_requests
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .iter()
                .find(|pull_request| pull_request.number == number && pull_request.remote == remote)
                .map(|pull_request| pull_request.state)
                .ok_or_else(|| GitHostError::Refused {
                    reason: format!("no pull request #{number}"),
                })
        })
    }

    fn poll_pull_request<'a>(
        &'a self,
        access: &'a HostAccountAccess,
        remote: &'a str,
        number: u64,
        last: Option<&'a PolledPullRequest>,
    ) -> GitHostFuture<'a, Result<PolledPullRequest, GitHostError>> {
        Box::pin(async move {
            if *self
                .polls_fail
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
            {
                return Err(GitHostError::Refused {
                    reason: "the fake git host cannot be reached".to_owned(),
                });
            }
            // Sets the state the merged file asks for.
            self.pull_request_state(access, remote, number).await?;
            let pull_request = self
                .pull_requests()
                .into_iter()
                .find(|pull_request| pull_request.number == number && pull_request.remote == remote)
                .ok_or_else(|| GitHostError::Refused {
                    reason: format!("no pull request #{number}"),
                })?;
            let activity = self.activity_now(access, &pull_request).await?;
            let mut hasher = DefaultHasher::new();
            format!("{activity:?}").hash(&mut hasher);
            let tag = format!("\"{:x}\"", hasher.finish());
            let unchanged = last.filter(|last| last.memo.get(TAG) == Some(tag.as_str()));
            self.with_activity(number, |known| {
                known.polls.push(if unchanged.is_some() {
                    FakePoll::NotModified
                } else {
                    FakePoll::Full
                });
            });
            if let Some(last) = unchanged {
                return Ok(last.clone());
            }
            let mut memo = PollMemo::default();
            memo.insert(TAG, &tag);
            Ok(PolledPullRequest { activity, memo })
        })
    }

    fn poll_wayfinder_maps<'a>(
        &'a self,
        access: &'a HostAccountAccess,
        _remote: &'a str,
        last: Option<&'a PolledWayfinderMaps>,
    ) -> GitHostFuture<'a, Result<PolledWayfinderMaps, GitHostError>> {
        Box::pin(async move {
            if *self
                .polls_fail
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
            {
                return Err(GitHostError::Refused {
                    reason: "the fake git host cannot be reached".to_owned(),
                });
            }
            let maps = self.maps_now(access);
            let mut hasher = DefaultHasher::new();
            format!("{maps:?}").hash(&mut hasher);
            let tag = format!("\"{:x}\"", hasher.finish());
            let unchanged = last.filter(|last| last.memo.get(MAPS_TAG) == Some(tag.as_str()));
            self.map_polls
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(if unchanged.is_some() {
                    FakePoll::NotModified
                } else {
                    FakePoll::Full
                });
            if let Some(last) = unchanged {
                return Ok(last.clone());
            }
            let mut memo = PollMemo::default();
            memo.insert(MAPS_TAG, &tag);
            Ok(PolledWayfinderMaps { maps, memo })
        })
    }
}

/// The commit `branch` points to on `remote`.
async fn remote_commit(remote: &str, branch: &str) -> Result<CommitId, GitHostError> {
    let output = tokio::process::Command::new("git")
        .args(["ls-remote", remote, &format!("refs/heads/{branch}")])
        .output()
        .await
        .map_err(|source| GitHostError::Spawn {
            program: "git".to_owned(),
            source,
        })?;
    commit_of(&String::from_utf8_lossy(&output.stdout)).ok_or_else(|| GitHostError::Refused {
        reason: format!("no branch {branch} on {remote}"),
    })
}

/// The commit of the first line of `git ls-remote`'s output.
fn commit_of(ls_remote: &str) -> Option<CommitId> {
    ls_remote.split_whitespace().next().map(CommitId::new)
}
