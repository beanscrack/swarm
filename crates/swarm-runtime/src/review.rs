//! Provenance checks for reviewer reports across CLI and embedded runtimes.
use serde_json::{Value, json};
use std::path::Path;
use swarm_core::{
    event::{EventKind, RunEvent},
    review::{ReviewReport, ReviewVerdict, Verification},
};
fn results(events: &[RunEvent]) -> impl Iterator<Item = &Value> {
    events
        .iter()
        .filter(|e| e.kind == EventKind::ToolResult)
        .map(|e| &e.payload)
}

pub fn work_calls(events: &[RunEvent]) -> usize {
    results(events)
        .filter(|r| {
            r["success"] == true
                && !matches!(
                    r["name"].as_str(),
                    Some("todo_write" | "report_result" | "spawn_agents") | None
                )
        })
        .count()
}

fn command(result: &Value) -> Option<String> {
    match result["name"].as_str()? {
        "bash" | "run_tests" => result["arguments"]["command"].as_str().map(str::to_string),
        "git" => Some(format!(
            "git {}",
            result["arguments"]["args"]
                .as_array()?
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(" ")
        )),
        _ => None,
    }
}

fn observed(events: &[RunEvent], cmd: &str, output: &str) -> bool {
    !output.trim().is_empty()
        && results(events).any(|r| {
            command(r).as_deref() == Some(cmd)
                && r["output"].as_str().unwrap_or("").contains(output)
                && r["exit_code"].is_number()
        })
}

/// Evidence audit. This proves provenance, not the semantic truth of a claim.
pub fn audit_review(
    workspace: &Path,
    events: &[RunEvent],
    report: Option<&Value>,
    floor: usize,
) -> Value {
    let mut issues = Vec::new();
    let count = work_calls(events);
    if count < floor.max(2) {
        issues.push(format!(
            "only {count} substantive tool calls; need {}",
            floor.max(2)
        ));
    }
    let inspected = results(events).any(|r| {
        r["success"] == true
            && (matches!(r["name"].as_str(), Some("read_file" | "search_files"))
                || command(r).is_some_and(|c| {
                    r["exit_code"] == 0
                        && (c.contains("git diff")
                            || c.contains("git show")
                            || c.contains("gh pr diff"))
                }))
    });
    if !inspected {
        issues.push("no successful file or diff inspection recorded".into());
    }
    match report
        .cloned()
        .and_then(|r| serde_json::from_value::<ReviewReport>(r).ok())
    {
        None => issues.push("missing structured review (verdict, blockers, checks)".into()),
        Some(review) => {
            if let Err(error) = review.validate() {
                issues.push(error);
            }
            if review.verdict == ReviewVerdict::Inconclusive {
                issues.push("review is inconclusive".into());
            }
            if review.checks.is_empty() {
                issues.push("no checks attempted".into());
            }
            for check in &review.checks {
                if !observed(events, &check.command, &check.observed_output) {
                    issues.push(format!("check lacks recorded evidence: {}", check.command));
                }
            }
            for finding in &review.blockers {
                let root = std::fs::canonicalize(workspace).ok();
                let file = std::fs::canonicalize(workspace.join(&finding.file)).ok();
                let valid = root
                    .zip(file)
                    .filter(|(root, file)| file.starts_with(root))
                    .and_then(|(_, file)| std::fs::read_to_string(file).ok())
                    .is_some_and(|s| {
                        finding.line > 0 && s.lines().count() >= finding.line as usize
                    });
                if !valid {
                    issues.push(format!(
                        "invalid file location: {}:{}",
                        finding.file, finding.line
                    ));
                }
                let read = results(events).any(|r| {
                    r["success"] == true
                        && ((r["name"] == "read_file"
                            && r["arguments"]["path"].as_str().is_some_and(|path| {
                                std::fs::canonicalize(workspace.join(path)).ok()
                                    == std::fs::canonicalize(workspace.join(&finding.file)).ok()
                            })
                            && u64::from(finding.line)
                                >= r["arguments"]["start_line"].as_u64().unwrap_or(1)
                            && u64::from(finding.line).saturating_sub(
                                r["arguments"]["start_line"].as_u64().unwrap_or(1),
                            ) < r["arguments"]["limit"].as_u64().unwrap_or(u64::MAX)
                            && u64::from(finding.line).saturating_sub(
                                r["arguments"]["start_line"].as_u64().unwrap_or(1),
                            ) < r["output"].as_str().unwrap_or("").lines().count() as u64)
                            || command(r).is_some_and(|c| {
                                r["exit_code"] == 0
                                    && (c.contains("diff") || c.contains("show"))
                                    && r["output"].as_str().unwrap_or("").contains(&finding.file)
                            }))
                });
                if !read {
                    issues.push(format!("blocker file was not inspected: {}", finding.file));
                }
                match finding.verification {
                    Verification::Unverified => {
                        issues.push(format!("unverified blocker: {}", finding.claim))
                    }
                    Verification::Executed => {
                        if !observed(
                            events,
                            finding.repro_command.as_deref().unwrap_or(""),
                            finding.observed_output.as_deref().unwrap_or(""),
                        ) {
                            issues.push(format!(
                                "reproduction lacks recorded evidence: {}",
                                finding.claim
                            ));
                        }
                    }
                    Verification::CodeInspection => {}
                }
            }
        }
    }
    json!({"trusted":issues.is_empty(), "worker_tool_calls":count, "inspected":inspected,
        "issues":issues, "scope":"recorded evidence provenance; claims still require independent judgment"})
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn a_failed_diff_is_not_successful_inspection() {
        let dir = tempfile::tempdir().unwrap();
        let report = json!({"verdict":"APPROVE","blockers":[],"checks":[{"command":"cargo test","observed_output":"passed"}]});
        let mut events = vec![
            result(
                "bash",
                json!({"command":"git diff"}),
                "fatal: no repository",
                json!(128),
            ),
            result(
                "run_tests",
                json!({"command":"cargo test"}),
                "passed",
                json!(0),
            ),
        ];
        assert_eq!(
            audit_review(dir.path(), &events, Some(&report), 2)["trusted"],
            false
        );
        events[0].payload["exit_code"] = json!(0);
        events[0].payload["output"] = json!("diff --git a/a.rs b/a.rs");
        assert_eq!(
            audit_review(dir.path(), &events, Some(&report), 2)["trusted"],
            true
        );
    }
    fn result(name: &str, args: Value, output: &str, code: Value) -> RunEvent {
        RunEvent::new(
            EventKind::ToolResult,
            swarm_core::RunId::new(),
            json!({
                "name":name, "arguments":args, "output":output, "exit_code":code, "success":true
            }),
        )
    }
    #[test]
    fn empty_or_fabricated_review_cannot_be_trusted() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "fn broken() {}\n").unwrap();
        let mut report = json!({"verdict":"REQUEST_CHANGES", "blockers":[{
            "file":"a.rs", "line":1, "claim":"broken", "verification":"executed",
            "repro_command":"cargo test", "observed_output":"FAIL broken"
        }], "checks":[{"command":"cargo test", "observed_output":"FAIL broken"}]});
        assert_eq!(
            audit_review(dir.path(), &[], Some(&report), 2)["trusted"],
            false
        );
        let events = vec![
            result(
                "read_file",
                json!({"path":"a.rs"}),
                "fn broken() {}",
                Value::Null,
            ),
            result(
                "run_tests",
                json!({"command":"cargo test"}),
                "FAIL broken",
                json!(1),
            ),
        ];
        assert_eq!(
            audit_review(dir.path(), &events, Some(&report), 2)["trusted"],
            true
        );
        report["blockers"][0]["observed_output"] = json!("invented failure");
        assert_eq!(
            audit_review(dir.path(), &events, Some(&report), 2)["trusted"],
            false
        );
        report["blockers"][0]["verification"] = json!("code_inspection");
        assert_eq!(
            audit_review(dir.path(), &events, Some(&report), 2)["trusted"],
            true
        );
        report["blockers"][0]["line"] = json!(100);
        assert_eq!(
            audit_review(dir.path(), &events, Some(&report), 2)["trusted"],
            false
        );
    }
}
