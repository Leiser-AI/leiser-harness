//! Permission modes, approval requests and agent questions, at the service
//! seam with the fake agent CLI.

use std::collections::BTreeMap;
use std::error::Error;
use std::path::Path;

use leiser_service::{
    AgentEvent, Answer, ChatId, ChatOverview, ChatSettings, ChatStatus, Checkout, Command,
    CommandError, Event, PermissionMode, Question, QuestionOption, RepositoryId, Response,
    ServiceHandle, TranscriptEntry,
};
use leiser_testkit::{Gate, Script, Step, TestRepo, TestService, collect_turn, next_event};
use tokio::sync::broadcast;

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

const FAKE_AGENT_CLI: &str = env!("CARGO_BIN_EXE_fake-agent-cli");

struct Setup {
    harness: TestService,
    service: ServiceHandle,
    repository: RepositoryId,
    events: broadcast::Receiver<Event>,
    repo: TestRepo,
}

/// A harness service on `script` with one repository. `config`, when
/// given, is the repository's `.leiser/config.toml`.
async fn setup(script: &Script, config: Option<&str>) -> TestResult<Setup> {
    let repo = TestRepo::new()?;
    if let Some(config) = config {
        std::fs::create_dir_all(repo.path().join(".leiser"))?;
        std::fs::write(repo.path().join(".leiser/config.toml"), config)?;
    }
    let harness = TestService::start(Path::new(FAKE_AGENT_CLI), script)?;
    let service = harness.connect();
    let response = service
        .execute(Command::AddRepository {
            path: repo.path().to_path_buf(),
        })
        .await?;
    let Response::RepositoryAdded { repository } = response else {
        return Err(format!("expected RepositoryAdded, got {response:?}").into());
    };
    let events = service.subscribe();
    Ok(Setup {
        harness,
        service,
        repository,
        events,
        repo,
    })
}

/// What `ReadChat` reports about permissions and open requests.
#[derive(Debug, PartialEq, Eq)]
struct ChatPermissions {
    status: ChatStatus,
    permission_mode: PermissionMode,
    applied_permission_mode: PermissionMode,
    open_requests: Vec<String>,
}

impl Setup {
    async fn start_chat(&self) -> TestResult<ChatId> {
        let response = self
            .service
            .execute(Command::StartChat {
                repository: self.repository,
                checkout: Checkout::Main,
                settings: ChatSettings::default(),
            })
            .await?;
        let Response::ChatStarted { chat } = response else {
            return Err(format!("expected ChatStarted, got {response:?}").into());
        };
        Ok(chat)
    }

    async fn send(&self, chat: ChatId, text: &str) -> TestResult {
        let response = self
            .service
            .execute(Command::SendMessage {
                chat,
                text: text.to_owned(),
            })
            .await?;
        assert_eq!(response, Response::MessageSent);
        Ok(())
    }

    async fn change_mode(&self, chat: ChatId, permission_mode: PermissionMode) -> TestResult {
        let response = self
            .service
            .execute(Command::ChangePermissionMode {
                chat,
                permission_mode,
            })
            .await?;
        assert_eq!(response, Response::PermissionModeChanged);
        Ok(())
    }

    async fn answer(
        &self,
        chat: ChatId,
        request: &str,
        answer: Answer,
    ) -> Result<Response, CommandError> {
        self.service
            .execute(Command::AnswerRequest {
                chat,
                request: request.to_owned(),
                answer,
            })
            .await
    }

    async fn turn(&mut self, chat: ChatId) -> TestResult<Vec<AgentEvent>> {
        Ok(collect_turn(&mut self.events, chat).await?)
    }

    /// Sends `text` and returns the agent's messages of the turn it starts.
    async fn messages(&mut self, chat: ChatId, text: &str) -> TestResult<Vec<String>> {
        self.send(chat, text).await?;
        Ok(messages(&self.turn(chat).await?))
    }

    /// Waits for the chat's next agent event.
    async fn next_agent_event(&mut self, chat: ChatId) -> TestResult<AgentEvent> {
        loop {
            if let Event::Agent { event, .. } = next_event(&mut self.events, chat).await? {
                return Ok(event);
            }
        }
    }

    /// Waits for the approval request the running turn sends, after its
    /// messages, and returns its id.
    async fn approval_request(&mut self, chat: ChatId, description: &str) -> TestResult<String> {
        loop {
            match self.next_agent_event(chat).await? {
                AgentEvent::Message { .. } => {}
                AgentEvent::ApprovalRequested {
                    request,
                    description: got,
                } if got == description => return Ok(request),
                other => return Err(format!("expected an approval request, got {other:?}").into()),
            }
        }
    }

    async fn permissions(&self, chat: ChatId) -> TestResult<ChatPermissions> {
        match self.service.execute(Command::ReadChat { chat }).await? {
            Response::Chat {
                status,
                permission_mode,
                applied_permission_mode,
                open_requests,
                ..
            } => Ok(ChatPermissions {
                status,
                permission_mode,
                applied_permission_mode,
                open_requests,
            }),
            other => Err(format!("expected Chat, got {other:?}").into()),
        }
    }

    async fn transcript(&self, chat: ChatId) -> TestResult<Vec<TranscriptEntry>> {
        match self.service.execute(Command::ReadChat { chat }).await? {
            Response::Chat { transcript, .. } => Ok(transcript),
            other => Err(format!("expected Chat, got {other:?}").into()),
        }
    }

    async fn status_in_list(&self, chat: ChatId) -> TestResult<ChatStatus> {
        let Response::Chats { chats } = self.service.execute(Command::ListChats).await? else {
            return Err("expected Chats".into());
        };
        chats
            .into_iter()
            .find(|overview: &ChatOverview| overview.chat == chat)
            .map(|overview| overview.status)
            .ok_or_else(|| "the chat is not listed".into())
    }
}

fn messages(turn: &[AgentEvent]) -> Vec<String> {
    turn.iter()
        .filter_map(|event| match event {
            AgentEvent::Message { text } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

/// A script whose every turn reports the permission mode it runs in.
fn reporting_mode(turns: usize) -> Script {
    (0..turns).fold(Script::new(), |script, _| {
        script.turn([Step::ReportPermissionMode])
    })
}

fn approval(description: &str) -> Script {
    Script::new().turn([
        Step::message("Let me run the tests."),
        Step::RequestApproval {
            description: description.to_owned(),
        },
    ])
}

fn question() -> Question {
    Question {
        id: "runner".to_owned(),
        header: "Test runner".to_owned(),
        question: "Which test runner should I use?".to_owned(),
        options: vec![
            QuestionOption {
                label: "cargo test".to_owned(),
                description: "The default runner".to_owned(),
            },
            QuestionOption {
                label: "cargo nextest".to_owned(),
                description: "Faster, needs an install".to_owned(),
            },
        ],
        multi_select: false,
    }
}

#[tokio::test]
async fn a_new_chat_runs_in_auto_by_default() -> TestResult {
    let mut setup = setup(&reporting_mode(1), None).await?;
    let chat = setup.start_chat().await?;

    assert_eq!(setup.messages(chat, "hi").await?, ["auto"]);
    let permissions = setup.permissions(chat).await?;
    assert_eq!(permissions.permission_mode, PermissionMode::Auto);
    assert_eq!(permissions.applied_permission_mode, PermissionMode::Auto);
    Ok(())
}

#[tokio::test]
async fn the_repository_default_applies_to_new_chats() -> TestResult {
    // Tables of other features are left to them.
    let config = "permission_mode = \"supervised\"\n\n[agent_access]\ntodo_list = true\n";
    let mut setup = setup(&reporting_mode(1), Some(config)).await?;
    let chat = setup.start_chat().await?;

    assert_eq!(setup.messages(chat, "hi").await?, ["supervised"]);
    assert_eq!(
        setup.permissions(chat).await?.permission_mode,
        PermissionMode::Supervised
    );
    Ok(())
}

#[tokio::test]
async fn a_repository_default_that_is_no_permission_mode_fails_the_new_chat() -> TestResult {
    let setup = setup(&reporting_mode(1), Some("permission_mode = \"yolo\"\n")).await?;

    let refused = setup
        .service
        .execute(Command::StartChat {
            repository: setup.repository,
            checkout: Checkout::Main,
            settings: ChatSettings::default(),
        })
        .await;

    assert!(
        matches!(refused, Err(CommandError::RepositoryConfig { .. })),
        "{refused:?}"
    );
    Ok(())
}

#[tokio::test]
async fn a_chat_changes_its_permission_mode_and_keeps_its_agent_session() -> TestResult {
    let script = Script::new()
        .turn([Step::ReportPermissionMode, Step::EchoUserMessage])
        .turn([Step::ReportPermissionMode, Step::message("turn 2")]);
    let mut setup = setup(&script, Some("permission_mode = \"supervised\"\n")).await?;
    let chat = setup.start_chat().await?;
    let other = setup.start_chat().await?;
    assert_eq!(
        setup.messages(chat, "first").await?,
        ["supervised", "first"]
    );

    setup.change_mode(chat, PermissionMode::FullAccess).await?;

    // The same agent session goes on, now in Full access: it plays its
    // second turn instead of starting over.
    assert_eq!(
        setup.messages(chat, "second").await?,
        ["full_access", "turn 2"]
    );
    // Other chats keep the repository default.
    assert_eq!(setup.messages(other, "hi").await?, ["supervised", "hi"]);
    Ok(())
}

#[tokio::test]
async fn a_mode_change_during_a_turn_applies_from_the_next_turn() -> TestResult {
    let gate = Gate::new()?;
    let script = Script::new()
        .turn([
            Step::ReportPermissionMode,
            gate.step(),
            Step::message("done"),
        ])
        .turn([Step::ReportPermissionMode]);
    let mut setup = setup(&script, None).await?;
    let chat = setup.start_chat().await?;
    setup.send(chat, "long").await?;
    assert_eq!(
        setup.next_agent_event(chat).await?,
        AgentEvent::Message {
            text: "auto".to_owned()
        }
    );

    setup.change_mode(chat, PermissionMode::Supervised).await?;
    gate.open()?;

    assert_eq!(messages(&setup.turn(chat).await?), ["done"]);
    assert_eq!(setup.messages(chat, "next").await?, ["supervised"]);
    Ok(())
}

#[tokio::test]
async fn a_queued_message_after_a_mode_change_runs_in_the_new_mode() -> TestResult {
    let gate = Gate::new()?;
    let script = Script::new()
        .turn([gate.step(), Step::ReportPermissionMode])
        .turn([Step::ReportPermissionMode, Step::EchoUserMessage]);
    let mut setup = setup(&script, None).await?;
    let chat = setup.start_chat().await?;
    setup.send(chat, "long").await?;
    let queued = setup
        .service
        .execute(Command::QueueMessage {
            chat,
            text: "queued".to_owned(),
        })
        .await?;
    assert!(
        matches!(queued, Response::MessageQueued { .. }),
        "{queued:?}"
    );

    setup.change_mode(chat, PermissionMode::FullAccess).await?;
    gate.open()?;

    assert_eq!(messages(&setup.turn(chat).await?), ["auto"]);
    // The queued message continues the same agent session, in Full access.
    assert_eq!(
        messages(&setup.turn(chat).await?),
        ["full_access", "queued"]
    );
    Ok(())
}

#[tokio::test]
async fn a_mode_the_agent_cli_lacks_falls_back_to_supervised() -> TestResult {
    let script = reporting_mode(1).without_permission_mode(PermissionMode::AutoAcceptEdits);
    let mut setup = setup(&script, Some("permission_mode = \"auto_accept_edits\"\n")).await?;
    let chat = setup.start_chat().await?;

    assert_eq!(setup.messages(chat, "hi").await?, ["supervised"]);
    let permissions = setup.permissions(chat).await?;
    assert_eq!(permissions.permission_mode, PermissionMode::AutoAcceptEdits);
    assert_eq!(
        permissions.applied_permission_mode,
        PermissionMode::Supervised
    );
    Ok(())
}

#[tokio::test]
async fn the_permission_mode_survives_a_service_restart() -> TestResult {
    let setup = setup(&reporting_mode(2), None).await?;
    let chat = setup.start_chat().await?;
    setup
        .change_mode(chat, PermissionMode::AutoAcceptEdits)
        .await?;
    let Setup {
        harness,
        repository,
        repo,
        ..
    } = setup;

    let harness = harness.restart().await?;
    let service = harness.connect();
    let mut setup = Setup {
        events: service.subscribe(),
        harness,
        service,
        repository,
        repo,
    };

    assert_eq!(setup.messages(chat, "hi").await?, ["auto_accept_edits"]);
    Ok(())
}

#[tokio::test]
async fn an_approval_request_makes_the_chat_wait() -> TestResult {
    let mut setup = setup(&approval("Run `cargo test`"), None).await?;
    let chat = setup.start_chat().await?;
    setup.send(chat, "test it").await?;
    assert_eq!(
        setup.next_agent_event(chat).await?,
        AgentEvent::Message {
            text: "Let me run the tests.".to_owned()
        }
    );

    let request = setup.approval_request(chat, "Run `cargo test`").await?;

    let permissions = setup.permissions(chat).await?;
    assert_eq!(permissions.status, ChatStatus::WaitingForYou);
    assert_eq!(permissions.open_requests, [request]);
    assert_eq!(setup.status_in_list(chat).await?, ChatStatus::WaitingForYou);
    Ok(())
}

#[tokio::test]
async fn approving_reaches_the_agent() -> TestResult {
    let mut setup = setup(&approval("Run `cargo test`"), None).await?;
    let chat = setup.start_chat().await?;
    setup.send(chat, "test it").await?;
    let request = setup.approval_request(chat, "Run `cargo test`").await?;

    let response = setup.answer(chat, &request, Answer::Approve).await?;

    assert_eq!(response, Response::RequestAnswered);
    assert_eq!(messages(&setup.turn(chat).await?), ["approved"]);
    let permissions = setup.permissions(chat).await?;
    assert_eq!(permissions.status, ChatStatus::WaitingForYou);
    assert_eq!(permissions.open_requests, Vec::<String>::new());
    assert!(
        setup
            .transcript(chat)
            .await?
            .contains(&TranscriptEntry::Answer {
                request,
                answer: Answer::Approve
            })
    );
    Ok(())
}

#[tokio::test]
async fn declining_reaches_the_agent() -> TestResult {
    let mut setup = setup(&approval("Run `rm -rf target`"), None).await?;
    let chat = setup.start_chat().await?;
    setup.send(chat, "clean up").await?;
    let request = setup.approval_request(chat, "Run `rm -rf target`").await?;

    setup
        .answer(chat, &request, Answer::Decline { feedback: None })
        .await?;

    assert_eq!(messages(&setup.turn(chat).await?), ["declined"]);
    Ok(())
}

#[tokio::test]
async fn a_text_reply_declines_and_passes_the_text_to_the_agent() -> TestResult {
    let mut setup = setup(&approval("Run `cargo test`"), None).await?;
    let chat = setup.start_chat().await?;
    setup.send(chat, "test it").await?;
    let request = setup.approval_request(chat, "Run `cargo test`").await?;
    let feedback = "Use cargo nextest instead.".to_owned();

    setup
        .answer(
            chat,
            &request,
            Answer::Decline {
                feedback: Some(feedback.clone()),
            },
        )
        .await?;

    assert_eq!(
        messages(&setup.turn(chat).await?),
        [format!("declined: {feedback}")]
    );
    Ok(())
}

#[tokio::test]
async fn an_answer_reaches_every_connection() -> TestResult {
    let mut setup = setup(&approval("Run `cargo test`"), None).await?;
    let chat = setup.start_chat().await?;
    setup.send(chat, "test it").await?;
    let request = setup.approval_request(chat, "Run `cargo test`").await?;
    let mut other_app = setup.service.subscribe();

    setup.answer(chat, &request, Answer::Approve).await?;

    assert_eq!(
        next_event(&mut other_app, chat).await?,
        Event::RequestAnswered {
            chat,
            request,
            answer: Answer::Approve
        }
    );
    Ok(())
}

#[tokio::test]
async fn agent_questions_wait_for_the_developers_answers() -> TestResult {
    let script = Script::new().turn([Step::AskQuestions {
        questions: vec![question()],
    }]);
    let mut setup = setup(&script, None).await?;
    let chat = setup.start_chat().await?;
    setup.send(chat, "set up tests").await?;
    let AgentEvent::QuestionAsked { request, questions } = setup.next_agent_event(chat).await?
    else {
        return Err("expected a question".into());
    };
    assert_eq!(questions, [question()]);
    assert_eq!(
        setup.permissions(chat).await?.status,
        ChatStatus::WaitingForYou
    );

    let answers = BTreeMap::from([("runner".to_owned(), vec!["cargo nextest".to_owned()])]);
    setup
        .answer(chat, &request, Answer::Answers { answers })
        .await?;

    assert_eq!(
        messages(&setup.turn(chat).await?),
        ["answered: runner=cargo nextest"]
    );
    Ok(())
}

#[tokio::test]
async fn a_text_reply_to_a_question_reaches_the_agent() -> TestResult {
    let script = Script::new().turn([Step::AskQuestions {
        questions: vec![question()],
    }]);
    let mut setup = setup(&script, None).await?;
    let chat = setup.start_chat().await?;
    setup.send(chat, "set up tests").await?;
    let AgentEvent::QuestionAsked { request, .. } = setup.next_agent_event(chat).await? else {
        return Err("expected a question".into());
    };

    setup
        .answer(
            chat,
            &request,
            Answer::Decline {
                feedback: Some("Neither, use the Makefile.".to_owned()),
            },
        )
        .await?;

    assert_eq!(
        messages(&setup.turn(chat).await?),
        ["declined: Neither, use the Makefile."]
    );
    Ok(())
}

#[tokio::test]
async fn an_answer_that_does_not_fit_the_request_is_refused() -> TestResult {
    let script = Script::new()
        .turn([Step::RequestApproval {
            description: "Run `ls`".to_owned(),
        }])
        .turn([Step::AskQuestions {
            questions: vec![question()],
        }]);
    let mut setup = setup(&script, None).await?;
    let chat = setup.start_chat().await?;
    setup.send(chat, "look").await?;
    let approval = setup.approval_request(chat, "Run `ls`").await?;

    let refused = setup
        .answer(
            chat,
            &approval,
            Answer::Answers {
                answers: BTreeMap::new(),
            },
        )
        .await;
    assert_eq!(
        refused,
        Err(CommandError::AnswerDoesNotFit {
            chat,
            request: approval.clone()
        })
    );
    setup.answer(chat, &approval, Answer::Approve).await?;
    setup.turn(chat).await?;

    setup.send(chat, "ask").await?;
    let AgentEvent::QuestionAsked { request, .. } = setup.next_agent_event(chat).await? else {
        return Err("expected a question".into());
    };
    let refused = setup.answer(chat, &request, Answer::Approve).await;
    assert_eq!(
        refused,
        Err(CommandError::AnswerDoesNotFit { chat, request })
    );
    Ok(())
}

#[tokio::test]
async fn a_request_is_answered_only_once() -> TestResult {
    let mut setup = setup(&approval("Run `cargo test`"), None).await?;
    let chat = setup.start_chat().await?;
    setup.send(chat, "test it").await?;
    let request = setup.approval_request(chat, "Run `cargo test`").await?;
    setup.answer(chat, &request, Answer::Approve).await?;

    let again = setup.answer(chat, &request, Answer::Approve).await;

    assert_eq!(again, Err(CommandError::UnknownRequest { chat, request }));
    Ok(())
}

#[tokio::test]
async fn an_interrupted_turn_no_longer_waits() -> TestResult {
    let mut setup = setup(&approval("Run `cargo test`"), None).await?;
    let chat = setup.start_chat().await?;
    setup.send(chat, "test it").await?;
    let request = setup.approval_request(chat, "Run `cargo test`").await?;

    setup
        .service
        .execute(Command::InterruptTurn { chat })
        .await?;

    assert_eq!(
        setup.turn(chat).await?.last(),
        Some(&AgentEvent::TurnInterrupted)
    );
    let permissions = setup.permissions(chat).await?;
    assert_eq!(permissions.status, ChatStatus::WaitingForYou);
    assert_eq!(permissions.open_requests, Vec::<String>::new());
    let late = setup.answer(chat, &request, Answer::Approve).await;
    assert_eq!(late, Err(CommandError::UnknownRequest { chat, request }));
    Ok(())
}

#[tokio::test]
async fn requests_of_different_agent_sessions_never_share_an_id() -> TestResult {
    // A new agent session numbers its requests from the start again; the
    // chat must still tell them apart.
    let script = Script::new()
        .turn([
            Step::RequestApproval {
                description: "first".to_owned(),
            },
            Step::ForgetSession,
        ])
        .turn([Step::RequestApproval {
            description: "second".to_owned(),
        }]);
    let mut setup = setup(&script, None).await?;
    let chat = setup.start_chat().await?;
    setup.send(chat, "one").await?;
    let first = setup.approval_request(chat, "first").await?;
    setup.answer(chat, &first, Answer::Approve).await?;
    setup.turn(chat).await?;
    // The forgotten agent session ends; the next message starts a new one.
    while setup.next_agent_event(chat).await? != AgentEvent::SessionEnded {}

    setup.send(chat, "two").await?;
    let second = loop {
        if let AgentEvent::ApprovalRequested { request, .. } = setup.next_agent_event(chat).await? {
            break request;
        }
    };

    assert_ne!(first, second);
    Ok(())
}
