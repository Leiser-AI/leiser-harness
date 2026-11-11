//! Auto-fix: while a chat's pull request is open and auto-fix is on, the
//! harness service polls the git host every [`POLL_INTERVAL`] with the
//! entity tags of the last answer. A failing check, a conflict with the
//! base branch or a new review comment wakes the chat's agent in its own
//! agent session, worktree, subscription and permission mode; once the
//! turn has ended, the harness pushes the branch through the same host
//! account check and hard blocks as [`Command::Push`](crate::Command::Push).
//!
//! It stops and tells the developer when [`MAX_ATTEMPTS`] attempts in a
//! row leave the pull request's head commit as it was, when a check fails
//! again after an attempt to fix it without passing in between, when the
//! branch conflicts with the same base commit again after an attempt to
//! resolve it, and when a fix cannot be pushed. Watching ends when the
//! pull request is merged or closed. A poll waits while a turn runs.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::PathBuf;
use std::time::Duration;

use super::{Background, Opening, Place, Setup, State, verify_and_push};
use crate::git_host::{
    CheckOutcome, GitHostError, HostAccountAccess, Mergeable, PolledPullRequest,
    PullRequestActivity,
};
use crate::store::{AttemptsV3, AutoFixV3, ConflictV3};
use crate::{
    AutoFix, AutoFixStop, AutoFixTrigger, ChatId, CommandError, Event, HostAccountBinding,
    PullRequest, PullRequestState, Response, TranscriptEntry,
};

/// How often an open pull request is polled.
pub(super) const POLL_INTERVAL: Duration = Duration::from_secs(30);
/// How many attempts on the same head commit auto-fix makes.
const MAX_ATTEMPTS: u32 = 3;

/// A chat's auto-fix as the harness service keeps it.
pub(super) struct AutoFixWatch {
    pub(super) setting: AutoFix,
    progress: Progress,
    /// The last poll's answer, whose tags the next poll sends. Not
    /// stored: the first poll after a restart gets full answers.
    last: Option<PolledPullRequest>,
    phase: Phase,
}

/// What auto-fix is doing for the chat right now.
enum Phase {
    Idle,
    Polling,
    /// An auto-fix turn runs. `head_before` is the pull request's head
    /// commit when it started.
    Fixing {
        head_before: String,
    },
    /// The fix is being pushed.
    Pushing,
}

/// What auto-fix remembers between polls, also across restarts.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Progress {
    comments: Comments,
    /// The checks an attempt tried to fix, with the head commit then;
    /// forgotten once the check passes.
    attempted_checks: BTreeMap<String, String>,
    /// The conflict an attempt tried to resolve; forgotten once the branch
    /// merges cleanly.
    attempted_conflict: Option<ConflictV3>,
    /// The head commit of the last attempt and how many attempts it had.
    attempts: Option<AttemptsV3>,
    /// The head commit before an attempt whose fix was pushed: a poll that
    /// still reports it has not seen the push yet.
    pushed_over: Option<String>,
}

/// The comments auto-fix has handled.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Comments {
    /// The next poll's comments count as handled: those posted before
    /// auto-fix was switched on, or before it existed, are not acted on.
    TakeAsSeen,
    Seen(BTreeSet<String>),
}

/// What a poll means for the chat.
enum Decision {
    Wait,
    Fix(Vec<Found>),
    Stop(AutoFixStop),
}

/// A trigger with what the agent needs beyond it, such as a check's log.
struct Found {
    trigger: AutoFixTrigger,
    log: Option<String>,
}

/// Auto-fix work done in the background, for the command loop.
pub(super) enum AutoFixWork {
    Polled {
        chat: ChatId,
        number: u64,
        result: Result<PolledPullRequest, GitHostError>,
    },
    Pushed {
        chat: ChatId,
        head_before: String,
        /// The branch's head commit after the push.
        result: Result<String, CommandError>,
    },
}

impl AutoFixWatch {
    /// A new chat's auto-fix: on, with no comment handled yet.
    pub(super) fn new() -> Self {
        Self {
            setting: AutoFix::On,
            progress: Progress {
                comments: Comments::Seen(BTreeSet::new()),
                ..Progress::fresh()
            },
            last: None,
            phase: Phase::Idle,
        }
    }

    pub(super) fn restored(stored: AutoFixV3) -> Self {
        Self {
            setting: stored.setting,
            progress: Progress {
                comments: match stored.seen_comments {
                    None => Comments::TakeAsSeen,
                    Some(seen) => Comments::Seen(seen.into_iter().collect()),
                },
                attempted_checks: stored.attempted_checks,
                attempted_conflict: stored.attempted_conflict,
                attempts: stored.attempts,
                pushed_over: stored.pushed_over,
            },
            last: None,
            phase: Phase::Idle,
        }
    }

    pub(super) fn stored(&self) -> AutoFixV3 {
        let progress = &self.progress;
        AutoFixV3 {
            setting: self.setting.clone(),
            seen_comments: match &progress.comments {
                Comments::TakeAsSeen => None,
                Comments::Seen(seen) => Some(seen.iter().cloned().collect()),
            },
            attempted_checks: progress.attempted_checks.clone(),
            attempted_conflict: progress.attempted_conflict.clone(),
            attempts: progress.attempts.clone(),
            pushed_over: progress.pushed_over.clone(),
        }
    }

    /// An agent session that ended mid-turn ends the attempt; the next
    /// poll decides again.
    pub(super) fn session_ended(&mut self) {
        if let Phase::Fixing { .. } = self.phase {
            self.phase = Phase::Idle;
        }
    }
}

impl Progress {
    fn fresh() -> Self {
        Self {
            comments: Comments::TakeAsSeen,
            attempted_checks: BTreeMap::new(),
            attempted_conflict: None,
            attempts: None,
            pushed_over: None,
        }
    }

    /// Decides what a poll of the open pull request means, and remembers
    /// the attempt it starts.
    fn decide(&mut self, activity: &PullRequestActivity, base: &str) -> Decision {
        let head = &activity.head_commit;
        if self.pushed_over.as_ref() == Some(head) {
            return Decision::Wait;
        }
        self.pushed_over = None;
        self.forget_fixed(activity);
        let Comments::Seen(seen) = &self.comments else {
            self.comments = Comments::Seen(
                activity
                    .comments
                    .iter()
                    .map(|comment| comment.id.clone())
                    .collect(),
            );
            return Decision::Wait;
        };
        let mut found = match self.problems(activity, base) {
            Ok(found) => found,
            Err(stop) => return Decision::Stop(stop),
        };
        let mut new_comments = Vec::new();
        for comment in &activity.comments {
            if seen.contains(&comment.id) {
                continue;
            }
            new_comments.push(comment.id.clone());
            found.push(Found {
                trigger: AutoFixTrigger::Comment {
                    author: comment.author.clone(),
                    body: comment.body.clone(),
                    location: comment.location.clone(),
                },
                log: None,
            });
        }
        if found.is_empty() {
            return Decision::Wait;
        }
        let count = match &self.attempts {
            Some(attempts) if attempts.head == *head => attempts.count + 1,
            _ => 1,
        };
        if count > MAX_ATTEMPTS {
            return Decision::Stop(AutoFixStop::NoProgress {
                attempts: MAX_ATTEMPTS,
            });
        }
        self.remember(activity, &found, new_comments, count);
        Decision::Fix(found)
    }

    /// Forgets the checks that pass now and a conflict that is gone.
    fn forget_fixed(&mut self, activity: &PullRequestActivity) {
        for check in &activity.checks {
            if check.outcome == CheckOutcome::Passed {
                self.attempted_checks.remove(&check.name);
            }
        }
        if activity.mergeable == Mergeable::Clean {
            self.attempted_conflict = None;
        }
    }

    /// The failing checks and the conflict to fix, or why auto-fix stops:
    /// one of them came back after an attempt on an older head commit.
    fn problems(
        &self,
        activity: &PullRequestActivity,
        base: &str,
    ) -> Result<Vec<Found>, AutoFixStop> {
        let head = &activity.head_commit;
        let mut found = Vec::new();
        for check in &activity.checks {
            let CheckOutcome::Failed { url, log } = &check.outcome else {
                continue;
            };
            // Not attempted yet, or attempted on this head commit without
            // a fix, is tried (again).
            if self
                .attempted_checks
                .get(&check.name)
                .is_some_and(|attempted| attempted != head)
            {
                return Err(AutoFixStop::CheckFailedAgain {
                    name: check.name.clone(),
                });
            }
            found.push(Found {
                trigger: AutoFixTrigger::CheckFailed {
                    name: check.name.clone(),
                    url: url.clone(),
                },
                log: Some(log.clone()),
            });
        }
        if activity.mergeable == Mergeable::Conflicting {
            if self.attempted_conflict.as_ref().is_some_and(|attempted| {
                attempted.base == activity.base_commit && attempted.head != *head
            }) {
                return Err(AutoFixStop::ConflictAgain);
            }
            found.push(Found {
                trigger: AutoFixTrigger::Conflict {
                    base: base.to_owned(),
                },
                log: None,
            });
        }
        Ok(found)
    }

    /// Remembers the attempt on `found`, the `count`th on this head commit.
    fn remember(
        &mut self,
        activity: &PullRequestActivity,
        found: &[Found],
        new_comments: Vec<String>,
        count: u32,
    ) {
        let head = &activity.head_commit;
        if let Comments::Seen(seen) = &mut self.comments {
            seen.extend(new_comments);
        }
        for found in found {
            match &found.trigger {
                AutoFixTrigger::CheckFailed { name, .. } => {
                    self.attempted_checks.insert(name.clone(), head.clone());
                }
                AutoFixTrigger::Conflict { .. } => {
                    self.attempted_conflict = Some(ConflictV3 {
                        base: activity.base_commit.clone(),
                        head: head.clone(),
                    });
                }
                AutoFixTrigger::Comment { .. } => {}
            }
        }
        self.attempts = Some(AttemptsV3 {
            head: head.clone(),
            count,
        });
    }
}

impl State {
    /// Switches the chat's auto-fix on or off. On again after a stop, it
    /// starts over; comments posted meanwhile are not acted on.
    pub(super) fn set_auto_fix(&mut self, id: ChatId, on: bool) -> Result<Response, CommandError> {
        let chat = self
            .chats
            .get_mut(&id)
            .ok_or(CommandError::UnknownChat { chat: id })?;
        let watch = &mut chat.auto_fix;
        let setting = if on { AutoFix::On } else { AutoFix::Off };
        if watch.setting != setting {
            if on {
                watch.progress = Progress::fresh();
            }
            watch.setting = setting.clone();
            self.save()?;
            self.emit(Event::AutoFixChanged {
                chat: id,
                auto_fix: setting.clone(),
            });
        }
        Ok(Response::AutoFixSet { auto_fix: setting })
    }

    /// Polls the open pull request of every chat whose auto-fix is on and
    /// that runs no turn, in the background.
    pub(super) fn poll_pull_requests(&mut self) {
        // A new turn would keep the update waiting.
        if self.update_waiting {
            return;
        }
        let due: Vec<(ChatId, u64)> = self
            .chats
            .iter()
            .filter(|(_, chat)| {
                chat.auto_fix.setting == AutoFix::On
                    && matches!(chat.auto_fix.phase, Phase::Idle)
                    && chat.place != Place::RemovedWorktree
                    && matches!(chat.setup, Setup::Done)
                    && !chat.turn_running()
                    && chat.usage_limit.is_none()
            })
            .filter_map(|(id, chat)| {
                let pull_request = chat.pull_request.as_ref()?.opened()?;
                (pull_request.state == PullRequestState::Open).then_some((*id, pull_request.number))
            })
            .collect();
        for (id, number) in due {
            let Some((repository, access)) = self.poll_access(id) else {
                continue;
            };
            let Some(chat) = self.chats.get_mut(&id) else {
                continue;
            };
            chat.auto_fix.phase = Phase::Polling;
            let last = chat.auto_fix.last.clone();
            let git_host = self.git_host.clone();
            let background = self.background.clone();
            self.background_running += 1;
            tokio::spawn(async move {
                let result = match crate::git::origin_url(&repository).await {
                    Some(remote) => {
                        git_host
                            .poll_pull_request(&access, &remote, number, last.as_ref())
                            .await
                    }
                    None => Err(GitHostError::Refused {
                        reason: "the repository has no remote `origin`".to_owned(),
                    }),
                };
                // The service has stopped; the next start polls again.
                let _ = background
                    .send(Background::AutoFix(AutoFixWork::Polled {
                        chat: id,
                        number,
                        result,
                    }))
                    .await;
            });
        }
    }

    /// The repository's path and its host account's access, for a poll.
    /// Unlike a push, a poll leaves the repository's git config alone.
    fn poll_access(&self, id: ChatId) -> Option<(PathBuf, HostAccountAccess)> {
        let chat = self.chats.get(&id)?;
        let repository = self.repositories.get(&chat.repository)?.clone();
        let HostAccountBinding::Bound { host_account, .. } = self.host_account_binding(&repository)
        else {
            return None;
        };
        Some((repository, self.host_accounts.access(host_account)?))
    }

    pub(super) async fn auto_fix_finished(&mut self, work: AutoFixWork) {
        match work {
            AutoFixWork::Polled {
                chat,
                number,
                result,
            } => self.polled(chat, number, result).await,
            AutoFixWork::Pushed {
                chat,
                head_before,
                result,
            } => self.fix_pushed(chat, &head_before, result),
        }
    }

    async fn polled(
        &mut self,
        id: ChatId,
        number: u64,
        result: Result<PolledPullRequest, GitHostError>,
    ) {
        let Some(chat) = self.chats.get_mut(&id) else {
            return;
        };
        if let Phase::Polling = chat.auto_fix.phase {
            chat.auto_fix.phase = Phase::Idle;
        }
        let polled = match result {
            Ok(polled) => polled,
            Err(error) => {
                // The next poll tries again.
                tracing::warn!(chat = ?id, %error, "could not poll the pull request");
                return;
            }
        };
        let Some(pull_request) =
            chat.pull_request
                .as_mut()
                .and_then(|pull_request| match pull_request {
                    super::ChatPullRequest::Opened(opened) if opened.number == number => {
                        Some(opened)
                    }
                    _ => None,
                })
        else {
            return;
        };
        let state = polled.activity.state;
        let changed = pull_request.state != state;
        pull_request.state = state;
        let pull_request = pull_request.clone();
        chat.auto_fix.last = Some(polled);
        if changed {
            self.keep_pull_request(id, &pull_request);
        }
        if state != PullRequestState::Open {
            return;
        }
        let Some(chat) = self.chats.get_mut(&id) else {
            return;
        };
        // The developer may have started a turn or switched auto-fix off
        // while the poll ran.
        if chat.auto_fix.setting != AutoFix::On
            || chat.turn_running()
            || !matches!(chat.auto_fix.phase, Phase::Idle)
        {
            return;
        }
        let Some(polled) = &chat.auto_fix.last else {
            return;
        };
        let head = polled.activity.head_commit.clone();
        let before = chat.auto_fix.progress.clone();
        let decision = chat
            .auto_fix
            .progress
            .decide(&polled.activity, &pull_request.base);
        let remembered = chat.auto_fix.progress != before;
        match decision {
            Decision::Wait => {
                if remembered && let Err(error) = self.save() {
                    tracing::error!(chat = ?id, %error, "could not store auto-fix's progress");
                }
            }
            Decision::Stop(reason) => self.stop_auto_fix(id, reason),
            Decision::Fix(found) => self.start_fix(id, &pull_request, head, found).await,
        }
    }

    /// Wakes the chat's agent with what it is to fix.
    async fn start_fix(
        &mut self,
        id: ChatId,
        pull_request: &PullRequest,
        head_before: String,
        found: Vec<Found>,
    ) {
        let triggers: Vec<AutoFixTrigger> =
            found.iter().map(|found| found.trigger.clone()).collect();
        if let Err(error) = self.save() {
            tracing::error!(chat = ?id, %error, "could not store auto-fix's attempt");
        }
        let entry = TranscriptEntry::AutoFixTriggered {
            pull_request: pull_request.number,
            triggers: triggers.clone(),
        };
        if let Err(error) = self.store.append(id.0, &entry) {
            tracing::error!(chat = ?id, %error, "could not store an auto-fix trigger");
        }
        self.emit(Event::AutoFixTriggered {
            chat: id,
            pull_request: pull_request.number,
            triggers,
        });
        let opening = Opening::AutoFix {
            pull_request: pull_request.number,
            message: message(pull_request, &found),
        };
        if let Some(chat) = self.chats.get_mut(&id) {
            chat.auto_fix.phase = Phase::Fixing { head_before };
        }
        if let Err(error) = self.start_turn(id, opening).await {
            // The attempt counts; the next poll tries again.
            tracing::warn!(chat = ?id, %error, "could not start an auto-fix turn");
            if let Some(chat) = self.chats.get_mut(&id) {
                chat.auto_fix.phase = Phase::Idle;
            }
        }
    }

    /// After a turn of the chat ended: pushes an auto-fix turn's fix in
    /// the background, unless the turn was cut short or the developer
    /// switched auto-fix off meanwhile.
    pub(super) async fn fix_turn_ended(&mut self, id: ChatId, completed: bool) {
        let Some(chat) = self.chats.get_mut(&id) else {
            return;
        };
        let Phase::Fixing { head_before } =
            std::mem::replace(&mut chat.auto_fix.phase, Phase::Idle)
        else {
            return;
        };
        if !completed || chat.auto_fix.setting != AutoFix::On {
            return;
        }
        let (working_directory, branch, name, access) = match self.prepare_push(id).await {
            Ok(prepared) => prepared,
            Err(error) => {
                self.stop_auto_fix(
                    id,
                    AutoFixStop::PushFailed {
                        reason: error.to_string(),
                    },
                );
                return;
            }
        };
        if let Some(chat) = self.chats.get_mut(&id) {
            chat.auto_fix.phase = Phase::Pushing;
        }
        let git_host = self.git_host.clone();
        let background = self.background.clone();
        self.background_running += 1;
        tokio::spawn(async move {
            let result = async {
                verify_and_push(
                    git_host.as_ref(),
                    &working_directory,
                    &branch,
                    &name,
                    &access,
                )
                .await?;
                crate::git::run(&working_directory, &["rev-parse", "HEAD"])
                    .await
                    .map_err(|error| CommandError::PushFailed {
                        reason: error.to_string(),
                    })
            }
            .await;
            // The service has stopped; the next poll sees what was pushed.
            let _ = background
                .send(Background::AutoFix(AutoFixWork::Pushed {
                    chat: id,
                    head_before,
                    result,
                }))
                .await;
        });
    }

    fn fix_pushed(&mut self, id: ChatId, head_before: &str, result: Result<String, CommandError>) {
        let Some(chat) = self.chats.get_mut(&id) else {
            return;
        };
        chat.auto_fix.phase = Phase::Idle;
        match result {
            Ok(head) if head != head_before => {
                chat.auto_fix.progress.pushed_over = Some(head_before.to_owned());
                if let Err(error) = self.save() {
                    tracing::error!(chat = ?id, %error, "could not store auto-fix's progress");
                }
            }
            // Nothing new: the next poll counts it as an attempt without
            // progress.
            Ok(_) => {}
            Err(error) => self.stop_auto_fix(
                id,
                AutoFixStop::PushFailed {
                    reason: error.to_string(),
                },
            ),
        }
    }

    /// Stops the chat's auto-fix and tells the developer why.
    fn stop_auto_fix(&mut self, id: ChatId, reason: AutoFixStop) {
        let Some(chat) = self.chats.get_mut(&id) else {
            return;
        };
        tracing::info!(chat = ?id, ?reason, "auto-fix stopped");
        let auto_fix = AutoFix::Stopped {
            reason: reason.clone(),
        };
        chat.auto_fix.setting = auto_fix.clone();
        if let Err(error) = self.save() {
            tracing::error!(chat = ?id, %error, "could not store that auto-fix stopped");
        }
        if let Err(error) = self
            .store
            .append(id.0, &TranscriptEntry::AutoFixStopped { reason })
        {
            tracing::error!(chat = ?id, %error, "could not store that auto-fix stopped");
        }
        self.emit(Event::AutoFixChanged { chat: id, auto_fix });
    }
}

/// What the agent gets for an attempt.
fn message(pull_request: &PullRequest, found: &[Found]) -> String {
    let mut text = format!(
        "[leiser harness] Auto-fix: pull request #{number} ({head} into {base}) needs \
         attention.\n",
        number = pull_request.number,
        head = pull_request.head,
        base = pull_request.base,
    );
    for found in found {
        text.push('\n');
        // Writing to a `String` cannot fail.
        let _ = match &found.trigger {
            AutoFixTrigger::CheckFailed { name, url } => {
                match found.log.as_deref().filter(|log| !log.trim().is_empty()) {
                    Some(log) => writeln!(
                        text,
                        "- The check `{name}` failed ({url}). The end of its log:\n\n```\n{log}\n```"
                    ),
                    None => writeln!(text, "- The check `{name}` failed ({url})."),
                }
            }
            AutoFixTrigger::Conflict { base } => writeln!(
                text,
                "- The branch conflicts with `{base}`. Run `git fetch origin`, merge \
                 `origin/{base}` into the branch, resolve the conflicts and commit the merge. \
                 Do not rebase."
            ),
            AutoFixTrigger::Comment {
                author,
                body,
                location,
            } => {
                let place = location
                    .as_deref()
                    .map(|location| format!(" on {location}"))
                    .unwrap_or_default();
                let _ = writeln!(text, "- A comment by {author}{place}:");
                body.lines()
                    .try_for_each(|line| writeln!(text, "  > {line}"))
            }
        };
    }
    text.push_str(
        "\nFix the cause on this branch and commit. The harness pushes the branch after your \
         turn: do not push yourself, never force-push, and do not merge the pull request. If a \
         comment needs no change, say why in your answer.",
    );
    text
}
