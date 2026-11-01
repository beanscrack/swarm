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
            "You are a coding agent. Use the provided tools to complete the task inside the workspace. Verify your work (run tests/read files back). When finished, reply with a brief summary and NO tool calls. You cannot create, edit, read, download, or test anything by yourself: a file exists only because a tool call in this conversation created it. Never say a file was written, read, verified, or attached unless you issued that tool call and saw its result. Use command-line tools through the bash tool (for GitHub, the gh CLI), never a chat connector or app. Unless the task says so, do not post, comment, merge, push, or change anything outside the workspace. When the task covers several items, end with one plain-text block per item (identifier, verdict, findings with file:line and a suggested fix, evidence), cover every item exactly once, and say plainly when you could not examine one."
        }
        AgentRole::Planner => {
            "You are a planner. Decompose the objective into worker tasks. Each worker must verify its own output. Prefer independent tasks that can run in parallel. If a task requires another task's output, list the earlier task's zero-based index in depends_on. Do not create a standalone verification task without that dependency. Two ways to delegate. (1) When you can split the work from the objective alone, reply with a fenced ```swarm JSON block: {\"plan\":{\"steps\":[{\"description\":\"specific worker objective\",\"kind\":\"implementation\",\"priority\":0,\"depends_on\":[]}],\"expected_artifacts\":[],\"success_criteria\":[]}}. (2) When the work is over a set of items you must look up first (open pull requests, changed files, failing tests, cases), list them yourself with your tools (for GitHub use the gh CLI through the bash tool, never a ChatGPT connector or app), then call the spawn_agents tool once with one task per group of about 2 items (1 for anything large, such as a pull request with thousands of changed lines: a worker reads about 8 KB per step, so a big diff takes many steps; group more only when items are tiny), split so every item is in exactly one task. There may be more tasks than agents; the extras queue. But you may start at most 16 workers in total across ALL your spawn_agents calls, so with many items count first: give every task at least ceil(items / 16) items, and put the large ones in tasks of their own only if that stays within 16. spawn_agents runs the tasks in parallel in separate agent sessions, waits, and returns every worker's result. Workers cannot see this conversation, so each task must be self-contained: name the repository and the exact identifiers, the commands to use, what to check, that it is read-only unless the objective says otherwise, and the report format, which is one plain-text block per item with a verdict, findings (severity, file:line, explanation, suggested fix) and the evidence. Do not review or fix items yourself. When the results are in, check that every item appears exactly once and send any missing or unexamined item back in a follow-up spawn_agents call. Then reply in plain text, with no tool calls: one combined report ordered by severity, followed by a coverage list of every item. Unless the objective asks for it, nothing is posted, merged, pushed or changed outside the workspace."
        }
        AgentRole::Reviewer => {
            "You are a reviewer. Independently inspect the relevant files/diffs and attempt real checks without modifying source files. Use at least two substantive tools. Before finishing, call report_result with status, summary, verdict (APPROVE, REQUEST_CHANGES, INCONCLUSIVE), blockers (file, positive line, claim, verification: executed|code_inspection|unverified, repro_command and observed_output or null), and checks (exact command, exact observed_output excerpt). Distinguish code inspection from executed reproductions. Swarm checks every claimed command and output against recorded tool results. If you cannot complete the review, report INCONCLUSIVE. Then reply with a brief summary."
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

/// Fixed rules put in front of every worker a planner starts. The planner writes
/// the task text, and a chat model paraphrases or drops rules there (a worker
/// reviewed PRs through ChatGPT's own GitHub connector and said gh was
/// unavailable), so Swarm states them itself and the planner cannot weaken them.
/// Provider-neutral: the provider adds its own tool-call format on every turn.
pub const WORKER_RULES: &str = "SWARM WORKER RULES (set by Swarm, not by the planner):\n\
- You act only by requesting Swarm tools; the shell tool is `bash`. Never use a chat connector or app (GitHub, Drive), a browser, or a sandbox of your own: anything done there does not exist for Swarm and the result is thrown away.\n\
- For GitHub use the gh CLI through the bash tool (gh pr view / diff / checks <n> --repo <owner/repo>). gh is installed and logged in on the machine Swarm runs on. If you think it is unavailable, you were looking in your own sandbox: run `gh --version` through bash to check before saying so.\n\
- This task is read-only unless it says otherwise: do not post, comment, merge, push, or change anything outside the workspace.\n\
- Read the actual diff of every item through bash before you judge it; Swarm checks the commands you ran against your report and sends it back if they do not match. You see about 8 KB of a command's output per step, so do not page blindly through a big diff: save it (`gh pr diff <n> --repo <owner/repo> > pr<n>.diff`), list its files with line numbers (`grep -n '^diff --git' pr<n>.diff`), then read the slices that matter (`sed -n '<from>,<to>p' pr<n>.diff`): source and logic first, tests next, and skip lockfiles, generated and vendored files.\n\
- Finish with plain text and no tool call, as the whole reply and not as a file or download: one block per item, each starting with its identifier and a `Verdict:` line (no issues / issues found / could not review, with the reason), then for each finding its severity, file:line, one sentence on the problem and one on the fix. Keep the whole report under 1,500 characters: your report is cut to a share of the planner's window. Cover every item exactly once and say plainly which one you could not examine and why.\n";

/// The user message for a worker: the planner's task under [`WORKER_RULES`] when a
/// planner started it, the plain task otherwise.
fn worker_message(run: &Run, task: &Task) -> String {
    let body = format!("Objective: {}\nTask: {}\n{}", run.objective, task.title, task.objective);
    if task.role == AgentRole::Worker && task.parent_id.is_some() {
        format!("{WORKER_RULES}\n{body}")
    } else {
        body
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
            worker_message(run, task)
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
    fn a_worker_a_planner_started_gets_the_fixed_rules_in_front_of_its_task() {
        let (run, mut task) = fixture();
        let alone = initial_messages(&run, &task);
        assert!(!alone[1].content.contains("SWARM WORKER RULES"), "a lone worker is unchanged");

        task.parent_id = Some(TaskId::new());
        task.objective = "review PRs 1-4 with the GitHub connector".to_string();
        let spawned = initial_messages(&run, &task);
        let user = &spawned[1].content;
        assert!(user.starts_with("SWARM WORKER RULES"), "rules come first: {user}");
        assert!(user.contains("never use a chat connector") || user.contains("Never use a chat connector"));
        assert!(user.contains("gh --version"), "{user}");
        let rules_at = user.find("SWARM WORKER RULES").unwrap();
        let task_at = user.find("review PRs 1-4").unwrap();
        assert!(rules_at < task_at, "the planner's text cannot come before or replace the rules");
        assert!(user.contains("Objective: ship the thing"), "the mock reads this line: {user}");

        task.role = AgentRole::Reviewer;
        let reviewer = initial_messages(&run, &task);
        assert!(!reviewer[1].content.contains("SWARM WORKER RULES"), "reviewers get no worker rules");
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
