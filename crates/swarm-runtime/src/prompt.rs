//! Prompt construction for the agent runtime.
//!
//! Pure, deterministic functions: the same `(role, mode)` or `(run, task)`
//! inputs always yield the same prompt text, so runs are reproducible.

use std::path::Path;

use swarm_core::message::ChatMessage;
use swarm_core::run::Run;
use swarm_core::task::Task;
use swarm_core::{AgentRole, RunMode};

/// Files read for project instructions, in priority order. `AGENTS.md` is the
/// cross-tool convention; `CLAUDE.md` is the filename Claude Code trained users
/// on, so a repo that already has one works without a copy.
pub const INSTRUCTION_FILES: [&str; 2] = ["AGENTS.md", "CLAUDE.md"];

/// Instructions longer than this are clipped: they ride along on every step of
/// every agent, so the file is context budget, not documentation.
pub const INSTRUCTION_CAP: usize = 8_000;

/// Reads project instructions for a workspace, if the repo ships any.
pub fn project_instructions(workspace: &Path) -> Option<String> {
    INSTRUCTION_FILES.iter().find_map(|name| {
        let text = std::fs::read_to_string(workspace.join(name)).ok()?;
        let text = text.trim();
        if text.is_empty() {
            return None;
        }
        let clipped = if text.chars().count() > INSTRUCTION_CAP {
            let head: String = text.chars().take(INSTRUCTION_CAP).collect();
            format!("{}\n\u{2026} (instructions clipped at {INSTRUCTION_CAP} chars)", head.trim_end())
        } else {
            text.to_string()
        };
        Some(format!("{name}\n{clipped}"))
    })
}

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

/// [`system_prompt`] plus the workspace's project instructions, appended as a
/// section the model is told to follow.
pub fn system_prompt_with(
    role: AgentRole,
    mode: RunMode,
    instructions: Option<&str>,
) -> String {
    let base = system_prompt(role, mode);
    match instructions {
        None | Some("") => base,
        Some(text) => format!(
            "{base}\n\nProject instructions (follow them):\n```md\n{text}\n```"
        ),
    }
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
    initial_messages_with(run, task, None)
}

/// [`initial_messages`] with the workspace's project instructions in the system
/// message (`AGENTS.md` / `CLAUDE.md`, see [`project_instructions`]).
pub fn initial_messages_with(
    run: &Run,
    task: &Task,
    instructions: Option<&str>,
) -> Vec<ChatMessage> {
    vec![
        ChatMessage::system(system_prompt_with(task.role, run.mode, instructions)),
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
    fn project_instructions_come_from_the_workspace() {
        let dir = std::env::temp_dir().join(format!("swarm-instr-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::remove_file(dir.join("AGENTS.md")).ok();
        std::fs::remove_file(dir.join("CLAUDE.md")).ok();
        assert!(project_instructions(&dir).is_none(), "no file, no instructions");

        std::fs::write(dir.join("CLAUDE.md"), "use pnpm\n").unwrap();
        let only_claude = project_instructions(&dir).unwrap();
        assert!(only_claude.starts_with("CLAUDE.md\n"), "{only_claude}");
        assert!(only_claude.contains("use pnpm"), "{only_claude}");

        std::fs::write(dir.join("AGENTS.md"), "use bun\n").unwrap();
        let both = project_instructions(&dir).unwrap();
        assert!(both.starts_with("AGENTS.md\n"), "AGENTS.md wins: {both}");

        std::fs::write(dir.join("AGENTS.md"), "x".repeat(INSTRUCTION_CAP + 500)).unwrap();
        let capped = project_instructions(&dir).unwrap();
        assert!(capped.contains("instructions clipped"), "cap missing");
        assert!(capped.chars().count() < INSTRUCTION_CAP + 200, "cap too loose");

        std::fs::write(dir.join("AGENTS.md"), "   \n").unwrap();
        std::fs::write(dir.join("CLAUDE.md"), "use pnpm\n").unwrap();
        assert!(
            project_instructions(&dir).unwrap().starts_with("CLAUDE.md"),
            "an empty AGENTS.md falls through"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn instructions_ride_in_the_system_message() {
        let (run, task) = fixture();
        let bare = initial_messages(&run, &task);
        let with = initial_messages_with(&run, &task, Some("AGENTS.md\nnever force push"));
        assert!(!bare[0].content.contains("Project instructions"));
        assert!(with[0].content.contains("Project instructions (follow them)"));
        assert!(with[0].content.contains("never force push"));
        assert_eq!(bare.len(), with.len(), "instructions are not a new turn");
        assert!(
            system_prompt_with(AgentRole::Worker, RunMode::Coding, Some("")).eq(&system_prompt(
                AgentRole::Worker,
                RunMode::Coding
            )),
            "empty instructions change nothing"
        );
    }

    #[test]
    fn system_prompt_is_deterministic_and_varies_by_role() {
        let w = system_prompt(AgentRole::Worker, RunMode::Coding);
        assert_eq!(w, system_prompt(AgentRole::Worker, RunMode::Coding));
        assert_ne!(w, system_prompt(AgentRole::Planner, RunMode::Coding));
    }
}
