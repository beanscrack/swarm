//! Checks on a worker's plain-text final reply, before Swarm accepts it as done.
//!
//! A chat model can finish a pull-request review without ever running the commands
//! it was told to run (it reads the code through a connector or its own sandbox
//! instead), hand back a report that is one line long, or "save" the report as a
//! download Swarm cannot see. Each of those looks like a normal completed task, so
//! the only defence is to compare the reply with what Swarm actually recorded: the
//! tool calls the agent made. Everything here is a pure function of the task text,
//! the recorded calls and the reply, so it is testable without a provider.

use swarm_core::tool::ToolCallRecord;
use swarm_core::AgentRole;
use swarm_core::task::Task;

/// How many times one agent is sent back before its reply is accepted anyway,
/// marked with what is wrong with it.
pub const MAX_CORRECTIONS: u32 = 2;

/// Pull request numbers a task names as `#123`, in order and without repeats, when
/// the task is about pull requests at all. A task that is not (`#12` as an issue
/// number, a hash in prose) yields nothing, so nothing is demanded of it.
pub fn assigned_prs(title: &str, objective: &str) -> Vec<u64> {
    let text = format!("{title}\n{objective}");
    let lower = text.to_lowercase();
    let about_prs = lower.contains("pull request")
        || lower.contains("gh pr")
        || text.split(|c: char| !c.is_alphanumeric()).any(|word| word == "PR" || word == "PRs");
    if !about_prs {
        return Vec::new();
    }
    let mut found = Vec::new();
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'#' {
            let digits: String = text[i + 1..].chars().take_while(char::is_ascii_digit).collect();
            if !digits.is_empty() && digits.len() <= 6 {
                if let Ok(n) = digits.parse::<u64>() {
                    if !found.contains(&n) {
                        found.push(n);
                    }
                }
            }
            i += 1 + digits.len();
        } else {
            i += 1;
        }
    }
    found
}

/// Whether `text` has the number `n` as a whole number (`#12` and `PR 12` match,
/// `123` and `1234` do not).
fn has_number(text: &str, n: u64) -> bool {
    text.split(|c: char| !c.is_ascii_digit()).any(|token| token.parse::<u64>().ok() == Some(n))
}

/// The assigned pull requests whose diff no successful `bash` call fetched: neither
/// `gh pr diff` nor `gh api` on `pulls/<n>` names them.
pub fn prs_without_diff(calls: &[ToolCallRecord], prs: &[u64]) -> Vec<u64> {
    let commands: Vec<String> = calls
        .iter()
        .filter(|call| call.name == "bash" && call.success)
        .filter_map(|call| call.input.get("command").and_then(|c| c.as_str()))
        .map(str::to_lowercase)
        .collect();
    prs.iter()
        .copied()
        .filter(|&n| {
            !commands.iter().any(|command| {
                (command.contains("gh pr diff") || (command.contains("gh api") && command.contains("pulls/")))
                    && has_number(command, n)
            })
        })
        .collect()
}

/// A reply that offers a file or a download. Swarm only sees text: whatever the
/// model made in the chat site's own sandbox does not exist for it.
pub fn offers_attachment(reply: &str) -> bool {
    let lower = reply.to_lowercase();
    ["sandbox:/", "/mnt/data", "[attachment", "report saved", "download the report", "(.md)", "(.pdf)", "(.csv)", "(.docx)", "(.xlsx)"]
        .iter()
        .any(|marker| lower.contains(marker))
}

/// What is wrong with `reply` as the final answer of `task`, one sentence each;
/// empty when it is acceptable. `calls` are the agent's recorded tool calls.
pub fn problems(task: &Task, calls: &[ToolCallRecord], reply: &str) -> Vec<String> {
    let mut found = Vec::new();
    if offers_attachment(reply) {
        found.push(
            "Your reply offers a file or download. Swarm cannot see anything made in your own sandbox, so the report does not exist for it. Put the whole report in the reply as plain text."
                .to_string(),
        );
    }
    if task.role != AgentRole::Worker {
        return found;
    }
    let prs = assigned_prs(&task.title, &task.objective);
    if prs.is_empty() {
        return found;
    }
    let undiffed = prs_without_diff(calls, &prs);
    if !undiffed.is_empty() {
        found.push(format!(
            "You have not run `gh pr diff` through the bash tool for {}. Do that now, one ```swarm block, `gh pr diff <number> --repo <owner/repo from your task>`, and base your findings on what it returns, not on anything you read through a connector or your own sandbox.",
            list(&undiffed)
        ));
    }
    let unmentioned: Vec<u64> = prs.iter().copied().filter(|&n| !has_number(reply, n)).collect();
    let verdicts = reply.to_lowercase().matches("verdict:").count();
    if !unmentioned.is_empty() {
        found.push(format!(
            "Your report covers {} of {} pull requests: add the block for {}. Each block starts with the PR number and has a `Verdict:` line; if you could not examine one, its Verdict is `could not review` with the reason.",
            prs.len() - unmentioned.len(),
            prs.len(),
            list(&unmentioned)
        ));
    } else if verdicts < prs.len() {
        found.push(format!(
            "Your report has {verdicts} `Verdict:` lines for {} pull requests ({}). Give every PR its own block with a `Verdict:` line: no issues, issues found, or could not review with the reason.",
            prs.len(),
            list(&prs)
        ));
    }
    found
}

fn list(prs: &[u64]) -> String {
    prs.iter().map(|n| format!("#{n}")).collect::<Vec<_>>().join(", ")
}

/// The message sent back to an agent whose reply has `problems`.
pub fn correction(problems: &[String]) -> String {
    format!(
        "Swarm did not accept that as your final reply:\n- {}\nFix this, then reply again.",
        problems.join("\n- ")
    )
}

/// `reply` with its unresolved `problems` stated in front, for the agent that was
/// sent back [`MAX_CORRECTIONS`] times and still did not fix them: whoever reads the
/// report (the planner, the user) then knows what it is not.
pub fn annotate(reply: &str, problems: &[String]) -> String {
    format!("[UNVERIFIED REPORT - Swarm checked it against the tool calls it recorded: {}]\n{reply}", problems.join(" "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use swarm_core::{TaskId, TaskState};

    fn task(title: &str, objective: &str, role: AgentRole) -> Task {
        Task {
            id: TaskId::new(),
            run_id: swarm_core::RunId::new(),
            parent_id: Some(TaskId::new()),
            depth: 1,
            title: title.to_string(),
            objective: objective.to_string(),
            role,
            group: "g".to_string(),
            priority: 0,
            depends_on: vec![],
            state: TaskState::Ready,
            attempts: 0,
            created_at: 0,
            updated_at: 0,
        }
    }

    fn bash(command: &str, success: bool) -> ToolCallRecord {
        let mut record = ToolCallRecord::new("c", "bash", json!({ "command": command }));
        record.success = success;
        record
    }

    const OBJECTIVE: &str = "Review exactly PRs #306, #313, #322 and #327 in Coalign/platform with gh pr diff.";

    #[test]
    fn pr_numbers_come_only_from_a_task_about_pull_requests() {
        assert_eq!(assigned_prs("Review PRs #306, #313", "also #306 again and #1234567"), vec![306, 313]);
        assert_eq!(assigned_prs("x", OBJECTIVE), vec![306, 313, 322, 327]);
        assert!(assigned_prs("Fix the parser", "see issue #12 and PROJECT notes").is_empty(), "an issue number is not a PR");
    }

    #[test]
    fn a_diff_is_only_evidence_for_the_numbers_it_names_and_only_when_it_succeeded() {
        let calls = [
            bash("gh pr view 306 --repo Coalign/platform", true),
            bash("for n in 313 322; do gh pr diff $n --repo Coalign/platform; done", true),
            bash("gh pr diff 327 --repo Coalign/platform", false),
            bash("gh pr diff 3060 --repo Coalign/platform", true),
        ];
        assert_eq!(prs_without_diff(&calls, &[306, 313, 322, 327]), vec![306, 327]);
        let api = [bash("gh api repos/Coalign/platform/pulls/306 -H 'Accept: application/vnd.github.diff'", true)];
        assert!(prs_without_diff(&api, &[306]).is_empty(), "gh api on pulls/<n> counts");
        let not_bash = [ToolCallRecord::new("c", "read_file", json!({ "command": "gh pr diff 306" }))];
        assert_eq!(prs_without_diff(&not_bash, &[306]), vec![306], "only bash calls count");
    }

    #[test]
    fn a_review_that_skipped_the_diffs_is_sent_back_naming_them() {
        let t = task("Review PRs", OBJECTIVE, AgentRole::Worker);
        let reply = "PR #306 Verdict: no issues\nPR #313 Verdict: no issues\nPR #322 Verdict: no issues\nPR #327 Verdict: no issues";
        let metadata_only = [bash("gh pr view 306 --repo Coalign/platform --json files", true)];
        let found = problems(&t, &metadata_only, reply);
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].contains("#306, #313, #322, #327"), "{found:?}");
        assert!(found[0].contains("gh pr diff"));

        let all = [bash("for n in 306 313 322 327; do gh pr diff $n --repo Coalign/platform; done", true)];
        assert!(problems(&t, &all, reply).is_empty(), "diffs run and every PR answered");
    }

    #[test]
    fn a_one_line_give_up_does_not_pass_as_a_report() {
        let t = task("Review PRs", OBJECTIVE, AgentRole::Worker);
        let all = [bash("gh pr diff 306 && gh pr diff 313 && gh pr diff 322 && gh pr diff 327", true)];
        let found = problems(&t, &all, "Reviews for PRs #306, #313, #322 and #327 remain unverified.");
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].contains("0 `Verdict:` lines for 4"), "{found:?}");

        let partial = problems(&t, &all, "PR #306 Verdict: no issues\nPR #313 Verdict: issues found");
        assert!(partial[0].contains("#322, #327"), "{partial:?}");
        assert!(partial[0].contains("covers 2 of 4"), "{partial:?}");
    }

    #[test]
    fn an_offered_download_is_refused_for_any_role() {
        let planner = task("p", "plan", AgentRole::Planner);
        let found = problems(&planner, &[], "Status: Partial - report saved. Download Coalign PR Review Report (.md)");
        assert_eq!(found.len(), 1);
        assert!(found[0].contains("plain text"));
        assert!(problems(&planner, &[], "One combined report, plain text, nothing attached.").is_empty());
        assert!(offers_attachment("[the file](sandbox:/mnt/data/report.md)"));
    }

    #[test]
    fn a_task_with_no_pull_requests_is_not_held_to_a_diff() {
        let t = task("Fix the parser", "make the tests pass", AgentRole::Worker);
        assert!(problems(&t, &[], "done").is_empty());
        let reviewer = task("Review PRs", OBJECTIVE, AgentRole::Reviewer);
        assert!(problems(&reviewer, &[], "looks fine").is_empty(), "only workers owe diffs");
    }

    #[test]
    fn correction_and_annotation_carry_the_problems() {
        let p = vec!["no diff for #1".to_string(), "no verdict for #1".to_string()];
        let c = correction(&p);
        assert!(c.contains("no diff for #1") && c.contains("no verdict for #1") && c.contains("Fix this"));
        let a = annotate("PR #1 Verdict: no issues", &p);
        assert!(a.starts_with("[UNVERIFIED REPORT"));
        assert!(a.ends_with("PR #1 Verdict: no issues"));
    }
}
