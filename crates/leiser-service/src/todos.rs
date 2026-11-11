//! A chat's todo list: what the developer and, where switched on, agent
//! sessions add, check and remove.

use crate::store::{TodoListV1, TodoV1};
use crate::{Todo, TodoId};

/// One change to a todo list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TodoChange {
    Add { text: String },
    SetDone { todo: TodoId, done: bool },
    Remove { todo: TodoId },
}

/// Why a change was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TodoError {
    Empty,
    Unknown(TodoId),
}

/// A chat's todos in list order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct TodoList {
    /// The last id given out; ids are never used again.
    last_id: u64,
    todos: Vec<Todo>,
}

impl TodoList {
    pub fn todos(&self) -> &[Todo] {
        &self.todos
    }

    /// Applies a change; for [`TodoChange::Add`], returns the new todo's id.
    pub fn apply(&mut self, change: TodoChange) -> Result<Option<TodoId>, TodoError> {
        match change {
            TodoChange::Add { text } => {
                let text = text.trim();
                if text.is_empty() {
                    return Err(TodoError::Empty);
                }
                self.last_id += 1;
                let id = TodoId(self.last_id);
                self.todos.push(Todo {
                    id,
                    text: text.to_owned(),
                    done: false,
                });
                Ok(Some(id))
            }
            TodoChange::SetDone { todo, done } => {
                self.todos
                    .iter_mut()
                    .find(|entry| entry.id == todo)
                    .ok_or(TodoError::Unknown(todo))?
                    .done = done;
                Ok(None)
            }
            TodoChange::Remove { todo } => {
                let index = self
                    .todos
                    .iter()
                    .position(|entry| entry.id == todo)
                    .ok_or(TodoError::Unknown(todo))?;
                self.todos.remove(index);
                Ok(None)
            }
        }
    }

    pub fn from_stored(stored: TodoListV1) -> Self {
        let todos: Vec<Todo> = stored
            .todos
            .into_iter()
            .map(|todo| Todo {
                id: TodoId(todo.id),
                text: todo.text,
                done: todo.done,
            })
            .collect();
        // Guards against a hand-edited file whose ids outran `last_id`.
        let last_id = todos
            .iter()
            .map(|todo| todo.id.0)
            .fold(stored.last_id, u64::max);
        Self { last_id, todos }
    }

    pub fn to_stored(&self) -> TodoListV1 {
        TodoListV1 {
            last_id: self.last_id,
            todos: self
                .todos
                .iter()
                .map(|todo| TodoV1 {
                    id: todo.id.0,
                    text: todo.text.clone(),
                    done: todo.done,
                })
                .collect(),
        }
    }
}
