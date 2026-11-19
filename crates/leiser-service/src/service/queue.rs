//! Runs the task queue ([`crate::queue`]) on the service's chats: after
//! every step of the command loop it tells the queue what the chats of its
//! tasks do, starts the tasks the queue picks, each in a chat in a new
//! worktree, and reports changes as [`Event::QueueChanged`]. The queue's
//! rules live in `crate::queue`; this is only what carries them out.

use std::time::Duration;

use tokio::sync::oneshot;

use super::{Background, ChatPullRequest, Setup, Started, State, unix_now};
use crate::queue::{
    Activity, Cancelled, ChatSeen, LimitKey, LimitSeen, QueueAnswer, QueueCommand, QueueItemId,
    Start, TurnEnd,
};
use crate::repo_config::RepositoryConfig;
use crate::{
    AgentEvent, ChatId, ChatSettings, Checkout, CommandError, Event, Response, StartOn,
    TranscriptEntry,
};

/// How often the queue looks whether a usage limit it waits for has reset.
pub(super) const TICK_INTERVAL: Duration = Duration::from_secs(30);

/// Queue work done in the background, for the command loop.
pub(super) enum QueueWork {
    /// A task's chat exists, or could not be started.
    ChatStarted {
        item: QueueItemId,
        result: Result<ChatId, String>,
    },
}

impl State {
    pub(super) async fn execute_queue(
        &mut self,
        command: QueueCommand,
    ) -> Result<Response, CommandError> {
        let repository = match &command {
            QueueCommand::Move { item, .. }
            | QueueCommand::Cancel { item }
            | QueueCommand::Remove { item } => self.queue.repository_of(*item)?,
            QueueCommand::Read { repository }
            | QueueCommand::Enqueue { repository, .. }
            | QueueCommand::Pause { repository }
            | QueueCommand::Resume { repository }
            | QueueCommand::ChangeSettings { repository, .. } => {
                self.repository_path(*repository)?;
                *repository
            }
        };
        let read_only = matches!(command, QueueCommand::Read { .. });
        let mut enqueued = None;
        match command {
            QueueCommand::Read { .. } => {}
            QueueCommand::Enqueue {
                task,
                agent_cli,
                permission_mode,
                ..
            } => {
                if let Some(agent_cli) = agent_cli {
                    self.driver(agent_cli)?;
                }
                enqueued =
                    Some(
                        self.queue
                            .enqueue(repository, task, agent_cli, permission_mode)?,
                    );
            }
            QueueCommand::Move { item, to } => self.queue.move_item(item, to)?,
            QueueCommand::Cancel { item } => {
                if let Cancelled::Interrupt(chat) = self.queue.cancel(item)? {
                    // The turn may have ended meanwhile; the chat stays.
                    if let Err(error) = self.interrupt_turn(chat).await {
                        tracing::debug!(?chat, %error, "no turn to interrupt for a cancelled task");
                    }
                }
            }
            QueueCommand::Remove { item } => self.queue.remove(item)?,
            QueueCommand::Pause { .. } => self.queue.pause(repository),
            QueueCommand::Resume { .. } => self.queue.resume(repository),
            QueueCommand::ChangeSettings { settings, .. } => {
                if let Some(agent_cli) = settings.agent_cli {
                    self.driver(agent_cli)?;
                }
                self.queue.change_settings(repository, settings)?;
            }
        }
        if !read_only {
            // The developer learns when a change could not be stored.
            self.queue.save(&self.store)?;
        }
        // Starts what the change makes room for before answering, so the
        // answer shows it.
        self.run_queue().await;
        let queue = self.queue.view(repository, unix_now(), self.update_waiting);
        let answer = match enqueued {
            Some(item) => QueueAnswer::Enqueued { item, queue },
            None => QueueAnswer::Queue { queue },
        };
        Ok(Response::Queue { answer })
    }

    /// Tells the queue what its tasks' chats do now, starts the tasks it
    /// picks, and stores and reports what changed. While an update of the
    /// harness service waits, no task starts.
    pub(super) async fn run_queue(&mut self) {
        for (chat, needs_end) in self.queue.watched() {
            if let Some(seen) = self.chat_seen(chat, needs_end) {
                self.queue.observe(chat, seen);
            }
        }
        if !self.update_waiting {
            // The queue is taken out, so the candidates can read the rest
            // of the service.
            let mut queue = std::mem::take(&mut self.queue);
            let starts = queue.plan(unix_now(), |start| self.candidates(start));
            self.queue = queue;
            for start in starts {
                self.start_task(start).await;
            }
        }
        let changed = self.queue.take_changed();
        if changed.is_empty() {
            return;
        }
        if let Err(error) = self.queue.save(&self.store) {
            tracing::error!(%error, "could not store the task queue");
        }
        for repository in changed {
            self.emit(Event::QueueChanged { repository });
        }
    }

    pub(super) async fn queue_work_finished(&mut self, work: QueueWork) {
        match work {
            QueueWork::ChatStarted { item, result } => {
                let Some((chat, message)) = self.queue.chat_started(item, result) else {
                    return;
                };
                if let Err(error) = self.send_message(chat, message).await {
                    tracing::warn!(?item, ?chat, %error, "could not send a task's first message");
                    self.queue.message_failed(item, error.to_string());
                }
            }
        }
    }

    /// What the queue needs to know of a task's chat, if it still exists.
    fn chat_seen(&self, id: ChatId, needs_end: bool) -> Option<ChatSeen> {
        let chat = self.chats.get(&id)?;
        let provider = chat.agent_cli.provider();
        let working = matches!(chat.setup, Setup::Running { .. }) || chat.turn_running();
        let activity = if working {
            Activity::Working {
                waiting_for_you: chat
                    .session
                    .as_ref()
                    .is_some_and(|session| !session.open.is_empty()),
            }
        } else {
            Activity::Idle {
                ended: if needs_end {
                    self.last_turn_end(id)
                } else {
                    TurnEnd::Unknown
                },
                usage_limit: chat.usage_limit.as_ref().map(|limit| LimitSeen {
                    limit: limit.limit.clone(),
                    key: LimitKey {
                        provider,
                        subscription: limit.subscription,
                    },
                    resets_at: limit.resets_at,
                    others: self
                        .limited
                        .iter()
                        .filter(|(subscription, _)| {
                            self.subscriptions.provider(**subscription) == Some(provider)
                        })
                        .map(|(subscription, resets_at)| {
                            (
                                LimitKey {
                                    provider,
                                    subscription: Some(*subscription),
                                },
                                *resets_at,
                            )
                        })
                        .collect(),
                }),
                done: chat.done,
            }
        };
        Some(ChatSeen {
            activity,
            pull_request: chat
                .pull_request
                .as_ref()
                .and_then(ChatPullRequest::opened)
                .map(|pull_request| (pull_request.number, pull_request.url.clone())),
            on: LimitKey {
                provider,
                subscription: chat.subscription(),
            },
        })
    }

    /// How the chat's last turn ended, from its transcript.
    fn last_turn_end(&self, chat: ChatId) -> TurnEnd {
        let transcript = match self.store.transcript(chat.0) {
            Ok(transcript) => transcript,
            Err(error) => {
                tracing::warn!(?chat, %error, "could not read how a task's turn ended");
                return TurnEnd::Unknown;
            }
        };
        transcript
            .iter()
            .rev()
            .find_map(|entry| match entry {
                TranscriptEntry::Agent { event, .. } => match event {
                    AgentEvent::TurnEnded => Some(TurnEnd::Completed),
                    AgentEvent::TurnInterrupted => Some(TurnEnd::Interrupted),
                    AgentEvent::TurnFailed { reason } => Some(TurnEnd::Failed {
                        reason: reason.clone(),
                    }),
                    AgentEvent::SessionEnded => Some(TurnEnd::SessionEnded),
                    _ => None,
                },
                // A message after the last turn's end that started none.
                TranscriptEntry::UserMessage { .. } => Some(TurnEnd::Unknown),
                _ => None,
            })
            .unwrap_or(TurnEnd::Unknown)
    }

    /// The subscriptions a task could run on: where its chat would start
    /// and, from an interchangeable subscription, the others it would move
    /// to at a usage limit.
    fn candidates(&self, start: &Start) -> Result<Vec<LimitKey>, String> {
        let (_, config) = self
            .repository_config(start.repository)
            .map_err(|error| error.to_string())?;
        let settings = self
            .task_settings(&config, start)
            .map_err(|error| error.to_string())?;
        let chat_start = self
            .chat_start(&config, settings)
            .map_err(|error| error.to_string())?;
        let provider = chat_start.agent_cli.provider();
        let first = chat_start.subscriptions.get(&provider).copied();
        let mut subscriptions = vec![first];
        if let Some(first) = first.filter(|first| self.subscriptions.interchangeable(*first)) {
            subscriptions.extend(
                self.subscriptions
                    .interchangeable_of(provider)
                    .filter(|other| *other != first)
                    .map(Some),
            );
        }
        Ok(subscriptions
            .into_iter()
            .map(|subscription| LimitKey {
                provider,
                subscription,
            })
            .collect())
    }

    /// The settings a task's chat starts with: its agent CLI on the
    /// repository's default subscription there, and its permission mode.
    fn task_settings(
        &self,
        config: &RepositoryConfig,
        start: &Start,
    ) -> Result<ChatSettings, CommandError> {
        let start_on = match start.agent_cli {
            Some(agent_cli) => {
                let provider = agent_cli.provider();
                Some(StartOn {
                    provider,
                    subscription: self.default_subscriptions(config)?.get(&provider).copied(),
                })
            }
            None => None,
        };
        Ok(ChatSettings {
            start_on,
            permission_mode: start.permission_mode,
        })
    }

    /// Starts a task's chat in a new worktree; its first message goes out
    /// once the worktree exists ([`QueueWork::ChatStarted`]).
    async fn start_task(&mut self, start: Start) {
        let item = start.item;
        if let Err(error) = self.start_task_chat(start).await {
            tracing::warn!(?item, %error, "could not start a task's chat");
            // Nothing to send: the chat does not exist.
            let _ = self.queue.chat_started(item, Err(error.to_string()));
        }
    }

    async fn start_task_chat(&mut self, start: Start) -> Result<(), CommandError> {
        let (_, config) = self.repository_config(start.repository)?;
        let settings = self.task_settings(&config, &start)?;
        self.bind_host_account(start.repository).await?;
        let item = start.item;
        match self.start_chat(start.repository, Checkout::NewWorktree, settings)? {
            // A new worktree never starts at once; handled all the same.
            Started::Now(chat) => {
                Box::pin(self.queue_work_finished(QueueWork::ChatStarted {
                    item,
                    result: Ok(chat),
                }))
                .await;
            }
            Started::AfterWorktree {
                pending,
                repository_path,
                base_branch,
            } => {
                let (reply, answer) = oneshot::channel();
                self.create_worktree(pending, repository_path, base_branch, reply);
                self.forward_started(item, answer);
            }
        }
        Ok(())
    }

    /// Passes the answer of a task's worktree on to the command loop.
    fn forward_started(
        &mut self,
        item: QueueItemId,
        answer: oneshot::Receiver<Result<Response, CommandError>>,
    ) {
        let background = self.background.clone();
        self.background_running += 1;
        tokio::spawn(async move {
            let result = match answer.await {
                Ok(Ok(Response::ChatStarted { chat })) => Ok(chat),
                Ok(Ok(response)) => Err(format!("unexpected answer {response:?}")),
                Ok(Err(error)) => Err(error.to_string()),
                Err(_) => Err(CommandError::ServiceStopped.to_string()),
            };
            // The service has stopped; the restart queues the task again.
            let _ = background
                .send(Background::Queue(QueueWork::ChatStarted { item, result }))
                .await;
        });
    }
}

impl From<crate::QueueError> for CommandError {
    fn from(error: crate::QueueError) -> Self {
        Self::Queue { error }
    }
}
