//! The agent's checklist (`todo_write`).
//!
//! The model sends the whole list each time it changes. The tool only checks
//! its shape; the list itself lives in the run's events, which is where a UI
//! reads it back to draw the checklist and where a session carries it from one
//! request to the next.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Most items a list may have.
pub const MAX_TODOS: usize = 50;
const MAX_CHARS: usize = 300;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    Pending,
    InProgress,
    Completed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Todo {
    pub content: String,
    pub status: TodoStatus,
    /// What to call the item while it is being done ("Running the tests").
    #[serde(default, rename = "activeForm", skip_serializing_if = "String::is_empty")]
    pub active_form: String,
}

impl Todo {
    /// The wording for work in progress.
    pub fn doing(&self) -> &str {
        if self.active_form.is_empty() { &self.content } else { &self.active_form }
    }
}

/// Read and check the tool's `todos` argument.
pub fn parse(input: &Value) -> Result<Vec<Todo>, String> {
    let items = input
        .get("todos")
        .and_then(Value::as_array)
        .ok_or("todo_write: 'todos' must be an array of {content, status}")?;
    if items.len() > MAX_TODOS {
        return Err(format!("todo_write: at most {MAX_TODOS} items"));
    }
    let mut out = Vec::with_capacity(items.len());
    for (index, item) in items.iter().enumerate() {
        let number = index + 1;
        let content = item.get("content").and_then(Value::as_str).map(str::trim).unwrap_or("");
        if content.is_empty() {
            return Err(format!("todo_write: item {number} needs 'content'"));
        }
        let status = match item.get("status").and_then(Value::as_str) {
            Some("pending") => TodoStatus::Pending,
            Some("in_progress") => TodoStatus::InProgress,
            Some("completed") => TodoStatus::Completed,
            _ => return Err(format!("todo_write: item {number} 'status' must be pending, in_progress or completed")),
        };
        let active_form = item.get("activeForm").and_then(Value::as_str).map(str::trim).unwrap_or("");
        out.push(Todo {
            content: content.chars().take(MAX_CHARS).collect(),
            status,
            active_form: active_form.chars().take(MAX_CHARS).collect(),
        });
    }
    Ok(out)
}

/// "2 of 5 done".
pub fn progress(todos: &[Todo]) -> String {
    let done = todos.iter().filter(|todo| todo.status == TodoStatus::Completed).count();
    format!("{done} of {} done", todos.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_good_list_parses() {
        let todos = parse(&json!({"todos": [
            {"content": "Read the code", "status": "completed", "activeForm": "Reading the code"},
            {"content": " Fix it ", "status": "in_progress"},
            {"content": "Test it", "status": "pending"},
        ]}))
        .unwrap();
        assert_eq!(todos.len(), 3);
        assert_eq!(todos[1].content, "Fix it", "trimmed");
        assert_eq!(todos[0].doing(), "Reading the code");
        assert_eq!(todos[1].doing(), "Fix it", "no activeForm falls back to the content");
        assert_eq!(progress(&todos), "1 of 3 done");
        assert_eq!(progress(&[]), "0 of 0 done");
    }

    #[test]
    fn a_bad_list_says_which_item_and_why() {
        assert!(parse(&json!({})).unwrap_err().contains("must be an array"));
        assert!(parse(&json!({"todos": "do it"})).unwrap_err().contains("must be an array"));
        let missing = parse(&json!({"todos": [{"content": "a", "status": "pending"}, {"status": "pending"}]}));
        assert!(missing.unwrap_err().contains("item 2 needs 'content'"));
        let status = parse(&json!({"todos": [{"content": "a", "status": "done"}]}));
        assert!(status.unwrap_err().contains("item 1 'status'"));
        let many: Vec<Value> = (0..51).map(|n| json!({"content": format!("t{n}"), "status": "pending"})).collect();
        assert!(parse(&json!({"todos": many})).unwrap_err().contains("at most 50"));
        assert_eq!(parse(&json!({"todos": []})).unwrap(), Vec::new(), "an empty list clears it");
    }

    #[test]
    fn a_list_round_trips_through_json() {
        let todos = parse(&json!({"todos": [{"content": "a", "status": "in_progress", "activeForm": "Doing a"}]})).unwrap();
        let back: Vec<Todo> = serde_json::from_value(serde_json::to_value(&todos).unwrap()).unwrap();
        assert_eq!(todos, back);
    }
}
