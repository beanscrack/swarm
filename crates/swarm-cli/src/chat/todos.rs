//! The agent's checklist across a session: read back from the run that wrote
//! it, kept on the turn, shown by `/todos`, and told to the next request.

use anyhow::Result;
use swarm_core::RunId;
use swarm_core::event::{EventKind, RunEvent};
use swarm_tools::todo::{self, Todo};

use super::Chat;
use crate::transcript::todo_rows;

/// The list as the run's last `todo_write` left it, if there was one.
pub fn latest(events: &[RunEvent]) -> Option<Vec<Todo>> {
    events
        .iter()
        .rev()
        .filter(|event| event.kind == EventKind::ToolResult)
        .filter(|event| event.payload.get("name").and_then(|name| name.as_str()) == Some("todo_write"))
        .filter(|event| event.payload.get("success").and_then(|ok| ok.as_bool()) == Some(true))
        .find_map(|event| todo::parse(event.payload.get("arguments")?).ok())
}

impl Chat {
    /// The checklist the run `run_id` ended with.
    pub(super) async fn todos_of(&self, run_id: &str) -> Option<Vec<Todo>> {
        let run = RunId::parse(run_id).ok()?;
        let events = self.store.events_since(&run, 0, 1_000_000).await.ok()?;
        latest(&events)
    }

    /// `/todos`: the checklist the agent is keeping.
    pub(super) fn cmd_todos(&self) -> Result<()> {
        let todos = self.session.todos();
        if todos.is_empty() {
            self.say("no todo list \u{00b7} the agent starts one when a task has several steps");
            return Ok(());
        }
        let mut rows = vec![crate::style::role(self.colors, "action", &format!("  Todos ({})", todo::progress(todos)))];
        rows.extend(todo_rows(self.colors, todos).into_iter().map(|row| format!("    {row}")));
        self.ui.print(rows);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn result(name: &str, success: bool, arguments: Value) -> RunEvent {
        RunEvent::new(
            EventKind::ToolResult,
            RunId::new(),
            json!({"name": name, "success": success, "arguments": arguments}),
        )
    }

    #[test]
    fn the_last_good_update_wins() {
        let list = |status: &str| json!({"todos": [{"content": "ship", "status": status}]});
        let events = vec![
            result("todo_write", true, list("pending")),
            result("bash", true, json!({"command": "ls"})),
            result("todo_write", true, list("completed")),
            result("todo_write", false, list("pending")),
            result("todo_write", true, json!({"todos": "garbled"})),
        ];
        let todos = latest(&events).expect("a list");
        assert_eq!(todos[0].status, todo::TodoStatus::Completed, "a failed or garbled update is not the list");
        assert!(latest(&[result("bash", true, json!({}))]).is_none());
        assert!(latest(&[]).is_none());
    }
}
