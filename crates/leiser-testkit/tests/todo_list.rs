//! A chat's todo list: the developer adds, checks and removes todos; with
//! agent access switched on in the repository's `.leiser/config.toml`, the
//! agent does the same through the harness MCP server.

use std::error::Error;
use std::path::Path;

use leiser_service::{
    AgentEvent, ChatId, Checkout, Command, CommandError, Event, Response, ServiceHandle, Todo,
    TodoId,
};
use leiser_testkit::{Script, Step, TestRepo, TestService, collect_turn, next_event};
use serde_json::{Value, json};
use tokio::sync::broadcast;

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

const FAKE_AGENT_CLI: &str = env!("CARGO_BIN_EXE_fake-agent-cli");

const AGENT_ACCESS_ON: &str = "[agent_access]\ntodo_list = true\n";
/// The harness MCP server's tools that every agent session gets.
const ALWAYS_ON_TOOLS: &str = "read_chat,name_branch";

fn start_service(script: &Script) -> TestResult<TestService> {
    Ok(TestService::start(Path::new(FAKE_AGENT_CLI), script)?)
}

/// A repository whose `.leiser/config.toml` has `config`.
fn repo_with_config(config: &str) -> TestResult<TestRepo> {
    let repo = TestRepo::new()?;
    std::fs::create_dir(repo.path().join(".leiser"))?;
    std::fs::write(repo.path().join(".leiser/config.toml"), config)?;
    Ok(repo)
}

async fn start_chat(service: &ServiceHandle, repo: &TestRepo) -> TestResult<ChatId> {
    let response = service
        .execute(Command::AddRepository {
            path: repo.path().to_path_buf(),
        })
        .await?;
    let Response::RepositoryAdded { repository } = response else {
        return Err(format!("expected RepositoryAdded, got {response:?}").into());
    };
    let response = service
        .execute(Command::StartChat {
            repository,
            checkout: Checkout::Main,
        })
        .await?;
    let Response::ChatStarted { chat } = response else {
        return Err(format!("expected ChatStarted, got {response:?}").into());
    };
    Ok(chat)
}

async fn add(service: &ServiceHandle, chat: ChatId, text: &str) -> TestResult<TodoId> {
    let response = service
        .execute(Command::AddTodo {
            chat,
            text: text.to_owned(),
        })
        .await?;
    let Response::TodoAdded { todo } = response else {
        return Err(format!("expected TodoAdded, got {response:?}").into());
    };
    Ok(todo)
}

async fn todos(service: &ServiceHandle, chat: ChatId) -> TestResult<Vec<Todo>> {
    let response = service.execute(Command::ListTodos { chat }).await?;
    let Response::Todos { todos } = response else {
        return Err(format!("expected Todos, got {response:?}").into());
    };
    Ok(todos)
}

/// The todos as (text, done) pairs, in list order.
fn texts(todos: &[Todo]) -> Vec<(&str, bool)> {
    todos
        .iter()
        .map(|todo| (todo.text.as_str(), todo.done))
        .collect()
}

/// Sends a message and returns the agent's messages of that turn.
async fn turn(
    service: &ServiceHandle,
    events: &mut broadcast::Receiver<Event>,
    chat: ChatId,
    text: &str,
) -> TestResult<Vec<String>> {
    let response = service
        .execute(Command::SendMessage {
            chat,
            text: text.to_owned(),
        })
        .await?;
    assert_eq!(response, Response::MessageSent);
    Ok(collect_turn(events, chat)
        .await?
        .into_iter()
        .filter_map(|event| match event {
            AgentEvent::Message { text } => Some(text),
            _ => None,
        })
        .collect())
}

fn call(name: &str, arguments: Value) -> Step {
    Step::CallHarnessTool {
        name: name.to_owned(),
        arguments,
    }
}

#[tokio::test]
async fn the_developer_adds_checks_and_removes_todos() -> TestResult {
    let harness = start_service(&Script::new())?;
    let service = harness.connect();
    let repo = TestRepo::new()?;
    let chat = start_chat(&service, &repo).await?;

    let tests = add(&service, chat, "Write the failing test").await?;
    let parse = add(&service, chat, "Parse the config").await?;
    add(&service, chat, "Update the docs").await?;
    service
        .execute(Command::SetTodoDone {
            chat,
            todo: tests,
            done: true,
        })
        .await?;
    service
        .execute(Command::RemoveTodo { chat, todo: parse })
        .await?;

    assert_eq!(
        texts(&todos(&service, chat).await?),
        [("Write the failing test", true), ("Update the docs", false)]
    );
    Ok(())
}

#[tokio::test]
async fn a_checked_todo_can_be_unchecked() -> TestResult {
    let harness = start_service(&Script::new())?;
    let service = harness.connect();
    let repo = TestRepo::new()?;
    let chat = start_chat(&service, &repo).await?;
    let todo = add(&service, chat, "Run clippy").await?;

    for done in [true, false] {
        service
            .execute(Command::SetTodoDone { chat, todo, done })
            .await?;
    }

    assert_eq!(
        texts(&todos(&service, chat).await?),
        [("Run clippy", false)]
    );
    Ok(())
}

#[tokio::test]
async fn each_chat_has_its_own_todo_list() -> TestResult {
    let harness = start_service(&Script::new())?;
    let service = harness.connect();
    let repo = TestRepo::new()?;
    let first = start_chat(&service, &repo).await?;
    let second = start_chat(&service, &repo).await?;

    add(&service, first, "Only in the first chat").await?;

    assert_eq!(todos(&service, second).await?, []);
    Ok(())
}

#[tokio::test]
async fn an_empty_todo_is_refused() -> TestResult {
    let harness = start_service(&Script::new())?;
    let service = harness.connect();
    let repo = TestRepo::new()?;
    let chat = start_chat(&service, &repo).await?;

    let result = service
        .execute(Command::AddTodo {
            chat,
            text: "  \n".to_owned(),
        })
        .await;

    assert_eq!(result, Err(CommandError::EmptyTodo));
    assert_eq!(todos(&service, chat).await?, []);
    Ok(())
}

#[tokio::test]
async fn changing_a_todo_that_does_not_exist_fails() -> TestResult {
    let harness = start_service(&Script::new())?;
    let service = harness.connect();
    let repo = TestRepo::new()?;
    let chat = start_chat(&service, &repo).await?;
    let todo = add(&service, chat, "Gone soon").await?;
    service.execute(Command::RemoveTodo { chat, todo }).await?;

    let checked = service
        .execute(Command::SetTodoDone {
            chat,
            todo,
            done: true,
        })
        .await;
    let removed = service.execute(Command::RemoveTodo { chat, todo }).await;

    assert_eq!(checked, Err(CommandError::UnknownTodo { chat, todo }));
    assert_eq!(removed, Err(CommandError::UnknownTodo { chat, todo }));
    Ok(())
}

#[tokio::test]
async fn every_change_reports_the_new_todo_list() -> TestResult {
    let harness = start_service(&Script::new())?;
    let service = harness.connect();
    let repo = TestRepo::new()?;
    let chat = start_chat(&service, &repo).await?;
    let mut events = service.subscribe();

    let todo = add(&service, chat, "Review the diff").await?;
    let added = next_event(&mut events, chat).await?;
    service
        .execute(Command::SetTodoDone {
            chat,
            todo,
            done: true,
        })
        .await?;
    let checked = next_event(&mut events, chat).await?;
    service.execute(Command::RemoveTodo { chat, todo }).await?;
    let removed = next_event(&mut events, chat).await?;

    let Event::TodosChanged {
        todos: after_add, ..
    } = added
    else {
        return Err(format!("expected TodosChanged, got {added:?}").into());
    };
    assert_eq!(texts(&after_add), [("Review the diff", false)]);
    let Event::TodosChanged {
        todos: after_check, ..
    } = checked
    else {
        return Err(format!("expected TodosChanged, got {checked:?}").into());
    };
    assert_eq!(texts(&after_check), [("Review the diff", true)]);
    assert_eq!(
        removed,
        Event::TodosChanged {
            chat,
            todos: vec![]
        }
    );
    Ok(())
}

#[tokio::test]
async fn todos_survive_a_service_restart() -> TestResult {
    let harness = start_service(&Script::new())?;
    let repo = TestRepo::new()?;
    let (chat, todo) = {
        let service = harness.connect();
        let chat = start_chat(&service, &repo).await?;
        let todo = add(&service, chat, "Checked before the restart").await?;
        service
            .execute(Command::SetTodoDone {
                chat,
                todo,
                done: true,
            })
            .await?;
        add(&service, chat, "Open before the restart").await?;
        (chat, todo)
    };

    let harness = harness.restart().await?;
    let service = harness.connect();
    let after = todos(&service, chat).await?;
    let added = add(&service, chat, "Added after the restart").await?;

    assert_eq!(
        texts(&after),
        [
            ("Checked before the restart", true),
            ("Open before the restart", false)
        ]
    );
    assert_ne!(added, todo, "a new todo never reuses an id");
    Ok(())
}

#[tokio::test]
async fn without_agent_access_the_agent_gets_no_todo_tools() -> TestResult {
    let script = Script::new().turn([
        Step::ListHarnessTools,
        call("add_todo", json!({ "text": "Sneaked in" })),
    ]);
    let harness = start_service(&script)?;
    let service = harness.connect();
    let repo = TestRepo::new()?;
    let chat = start_chat(&service, &repo).await?;
    let mut events = service.subscribe();

    let messages = turn(&service, &mut events, chat, "Plan the work.").await?;

    let [tools, added] = messages.as_slice() else {
        return Err(format!("expected two messages, got {messages:?}").into());
    };
    assert_eq!(tools, ALWAYS_ON_TOOLS);
    assert!(added.starts_with("rpc error"), "{added}");
    assert_eq!(todos(&service, chat).await?, []);
    Ok(())
}

#[tokio::test]
async fn agent_access_switched_off_in_the_config_gives_no_todo_tools() -> TestResult {
    let script = Script::new().turn([Step::ListHarnessTools]);
    let harness = start_service(&script)?;
    let service = harness.connect();
    let repo = repo_with_config("[agent_access]\ntodo_list = false\n")?;
    let chat = start_chat(&service, &repo).await?;
    let mut events = service.subscribe();

    let messages = turn(&service, &mut events, chat, "Plan the work.").await?;

    assert_eq!(messages, [ALWAYS_ON_TOOLS]);
    Ok(())
}

#[tokio::test]
async fn with_agent_access_the_agent_gets_the_todo_tools() -> TestResult {
    let script = Script::new().turn([Step::ListHarnessTools]);
    let harness = start_service(&script)?;
    let service = harness.connect();
    let repo = repo_with_config(AGENT_ACCESS_ON)?;
    let chat = start_chat(&service, &repo).await?;
    let mut events = service.subscribe();

    let messages = turn(&service, &mut events, chat, "Plan the work.").await?;

    let [tools] = messages.as_slice() else {
        return Err(format!("expected one message, got {messages:?}").into());
    };
    let mut tools: Vec<&str> = tools.split(',').collect();
    tools.sort_unstable();
    assert_eq!(
        tools,
        [
            "add_todo",
            "list_todos",
            "name_branch",
            "read_chat",
            "remove_todo",
            "set_todo_done"
        ]
    );
    Ok(())
}

#[tokio::test]
async fn the_agent_reads_the_todos_the_developer_added() -> TestResult {
    let script = Script::new().turn([call("list_todos", json!({}))]);
    let harness = start_service(&script)?;
    let service = harness.connect();
    let repo = repo_with_config(AGENT_ACCESS_ON)?;
    let chat = start_chat(&service, &repo).await?;
    let done = add(&service, chat, "Write the failing test").await?;
    add(&service, chat, "Make it pass").await?;
    service
        .execute(Command::SetTodoDone {
            chat,
            todo: done,
            done: true,
        })
        .await?;
    let mut events = service.subscribe();

    let messages = turn(&service, &mut events, chat, "What is left?").await?;

    let [list] = messages.as_slice() else {
        return Err(format!("expected one message, got {messages:?}").into());
    };
    let lines: Vec<&str> = list.lines().collect();
    assert!(
        lines
            .iter()
            .any(|line| line.contains("[x]") && line.contains("Write the failing test")),
        "{list}"
    );
    assert!(
        lines
            .iter()
            .any(|line| line.contains("[ ]") && line.contains("Make it pass")),
        "{list}"
    );
    Ok(())
}

#[tokio::test]
async fn changes_by_the_agent_appear_in_the_todo_list() -> TestResult {
    // The developer's todos get ids 1 and 2; the agent's first gets 3.
    let script = Script::new().turn([
        call("add_todo", json!({ "text": "Added by the agent" })),
        call("set_todo_done", json!({ "id": 1, "done": true })),
        call("remove_todo", json!({ "id": 2 })),
    ]);
    let harness = start_service(&script)?;
    let service = harness.connect();
    let repo = repo_with_config(AGENT_ACCESS_ON)?;
    let chat = start_chat(&service, &repo).await?;
    add(&service, chat, "Checked by the agent").await?;
    add(&service, chat, "Removed by the agent").await?;
    let mut events = service.subscribe();

    let response = service
        .execute(Command::SendMessage {
            chat,
            text: "Update the todo list.".to_owned(),
        })
        .await?;
    assert_eq!(response, Response::MessageSent);
    let mut reported = Vec::new();
    loop {
        match next_event(&mut events, chat).await? {
            Event::TodosChanged { todos, .. } => reported.push(todos),
            Event::Agent {
                event: AgentEvent::TurnEnded,
                ..
            } => break,
            Event::Agent {
                event: AgentEvent::Message { text },
                ..
            } => assert!(!text.starts_with("error"), "{text}"),
            _ => {}
        }
    }

    let expected = [
        ("Checked by the agent", true),
        ("Added by the agent", false),
    ];
    assert_eq!(reported.len(), 3, "one event per change: {reported:?}");
    assert_eq!(
        reported.last().map(|todos| texts(todos)),
        Some(expected.to_vec())
    );
    assert_eq!(texts(&todos(&service, chat).await?), expected);
    Ok(())
}

#[tokio::test]
async fn the_agent_is_told_when_a_todo_does_not_exist() -> TestResult {
    let script = Script::new().turn([
        call("set_todo_done", json!({ "id": 42, "done": true })),
        call("add_todo", json!({ "text": " " })),
    ]);
    let harness = start_service(&script)?;
    let service = harness.connect();
    let repo = repo_with_config(AGENT_ACCESS_ON)?;
    let chat = start_chat(&service, &repo).await?;
    let mut events = service.subscribe();

    let messages = turn(&service, &mut events, chat, "Check todo 42.").await?;

    let [unknown, empty] = messages.as_slice() else {
        return Err(format!("expected two messages, got {messages:?}").into());
    };
    assert!(unknown.starts_with("error: "), "{unknown}");
    assert!(empty.starts_with("error: "), "{empty}");
    assert_eq!(todos(&service, chat).await?, []);
    Ok(())
}

#[tokio::test]
async fn an_invalid_repository_config_is_reported() -> TestResult {
    let harness = start_service(&Script::new())?;
    let service = harness.connect();
    let repo = repo_with_config("[agent_access]\ntodo_list = \"yes\"\n")?;
    let Response::RepositoryAdded { repository } = service
        .execute(Command::AddRepository {
            path: repo.path().to_path_buf(),
        })
        .await?
    else {
        return Err("expected RepositoryAdded".into());
    };

    // The config is read when the chat starts, for its default
    // subscriptions, so the chat does not start.
    let result = service
        .execute(Command::StartChat {
            repository,
            checkout: Checkout::Main,
        })
        .await;

    assert!(
        matches!(result, Err(CommandError::RepositoryConfig { .. })),
        "{result:?}"
    );
    Ok(())
}
