//! Prompt construction for the agent runtime.
//!
//! Pure, deterministic functions: the same `(role, mode)` or `(run, task)`
//! inputs always yield the same prompt text, so runs are reproducible.

use swarm_core::message::ChatMessage;
use swarm_core::run::Run;
use swarm_core::task::Task;
use swarm_core::{AgentRole, RunMode};

/// The system prompt for an agent playing `role` under the run's `mode`.
pub fn system_prompt(role: AgentRole, mode: RunMode) -> String {
    let role_prompt: &'static str = match role {
        AgentRole::Worker => {
            "You are a coding agent. Use the provided tools to complete the task inside the workspace. Verify your work (run tests/read files back). When finished, reply with a brief summary and NO tool calls. You cannot create, edit, read, download, or test anything by yourself: a file exists only because a tool call in this conversation created it. Never say a file was written, read, verified, or attached unless you issued that tool call and saw its result."
        }
        AgentRole::Planner => {
            "You are a planner. Decompose the objective into worker tasks. Each worker must verify its own output. Prefer independent tasks that can run in parallel. If a task requires another task's output, list the earlier task's zero-based index in depends_on. Do not create a standalone verification task without that dependency. Reply with a fenced ```swarm JSON block: {\"plan\":{\"steps\":[{\"description\":\"specific worker objective\",\"kind\":\"implementation\",\"priority\":0,\"depends_on\":[]}],\"expected_artifacts\":[],\"success_criteria\":[]}}. Do not call tools."
        }
        AgentRole::Reviewer => {
            "You are a reviewer. Independently check the task result against its objective: read the artifacts, run tests where available, and say whether the result holds. When finished, reply with a brief verdict and NO tool calls."
        }
    };
    format!("{role_prompt}\n\nRun mode: {}.", mode_str(mode))
}

fn mode_str(mode: RunMode) -> &'static str {
    match mode {
        RunMode::General => "general",
        RunMode::Coding => "coding",
        RunMode::Research => "research",
        RunMode::Benchmark => "benchmark",
    }
}

/// Builds the initial message list for an agent starting on `task` in `run`.
pub fn initial_messages(run: &Run, task: &Task) -> Vec<ChatMessage> {
    vec![
        ChatMessage::system(system_prompt(task.role, run.mode)),
        ChatMessage::user(if task.role == AgentRole::Planner {
            format!("Objective: {}\nTask: {}\n{}\nWorker subtasks: {}",
                run.objective, task.title, task.objective, run.agent_target.saturating_sub(1))
        } else {
            format!("Objective: {}\nTask: {}\n{}", run.objective, task.title, task.objective)
        }),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use swarm_core::budget::ResourceBudget;
    use swarm_core::message::ChatRole;
    use swarm_core::{TaskId, TaskState};

    fn fixture() -> (Run, Task) {
        let run = Run::new("ship the thing", "mock", None, 1, true, ResourceBudget::default(), 1_000);
        let task = Task {
            id: TaskId::new(),
            run_id: run.id.clone(),
            parent_id: None,
            depth: 0,
            title: "do the thing".to_string(),
            objective: "make it work".to_string(),
            role: AgentRole::Worker,
            group: "main".to_string(),
            priority: 0,
            depends_on: vec![],
            state: TaskState::Ready,
            attempts: 0,
            created_at: 0,
            updated_at: 0,
        };
        (run, task)
    }

    #[test]
    fn initial_messages_contain_objective_and_title() {
        let (run, task) = fixture();
        let msgs = initial_messages(&run, &task);
        let joined = msgs.iter().map(|m| m.content.as_str()).collect::<String>();
        assert!(joined.contains("ship the thing"), "objective missing:\n{joined}");
        assert!(joined.contains("do the thing"), "title missing:\n{joined}");
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].role, ChatRole::System);
        assert_eq!(msgs[1].role, ChatRole::User);
    }

    #[test]
    fn initial_messages_are_deterministic() {
        let (run, task) = fixture();
        let a = initial_messages(&run, &task);
        let b = initial_messages(&run, &task);
        assert_eq!(a.len(), b.len());
        for (x, y) in a.iter().zip(b.iter()) {
            assert_eq!(x.content, y.content);
        }
    }

    #[test]
    fn system_prompt_is_deterministic_and_varies_by_role() {
        let w = system_prompt(AgentRole::Worker, RunMode::Coding);
        assert_eq!(w, system_prompt(AgentRole::Worker, RunMode::Coding));
        assert_ne!(w, system_prompt(AgentRole::Planner, RunMode::Coding));
    }
}
