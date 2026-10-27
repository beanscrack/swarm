//! `/rewind`: take back the agent's file edits.
//!
//! Every `write_file` / `edit_file` result carries what the file held before
//! (see `swarm_tools::FileChange`) and a fingerprint of what the call left
//! there. Replaying those newest first puts the files back as they were before
//! a request. A file the user (or a shell command) has touched since is left
//! alone and reported, so a rewind never destroys work it did not make.
//!
//! Shell commands are not tracked: whatever `bash` did stays done, and the
//! report says so.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::Result;
use swarm_core::RunId;
use swarm_core::event::{EventKind, RunEvent};
use swarm_tools::{FileChange, content_hash, resolve_within};

use super::session::{INTERNAL_PREFIX, clip};
use super::Chat;
use crate::style::role;
use crate::ui::DialogSpec;

/// One file edit a run made.
#[derive(Debug, Clone, PartialEq)]
pub struct Edit {
    /// The path as the tool call spelled it.
    pub path: String,
    pub change: FileChange,
}

/// The file edits in a run's events, oldest first.
pub fn edits_in(events: &[RunEvent]) -> Vec<Edit> {
    events
        .iter()
        .filter(|event| event.kind == EventKind::ToolResult)
        .filter(|event| event.payload.get("success").and_then(|value| value.as_bool()) == Some(true))
        .filter_map(|event| {
            let change = event.payload.get("change").filter(|value| !value.is_null())?;
            let change: FileChange = serde_json::from_value(change.clone()).ok()?;
            let path = event.payload.get("arguments")?.get("path")?.as_str()?.to_string();
            Some(Edit { path, change })
        })
        .collect()
}

/// What to do to one file.
#[derive(Debug, Clone, PartialEq)]
pub enum Op {
    /// Put these contents back.
    Write(String),
    /// The agent created it; remove it.
    Delete,
}

#[derive(Debug, Default, PartialEq)]
pub struct Plan {
    /// Workspace-relative path, absolute path and what to do, in path order.
    pub ops: Vec<(String, PathBuf, Op)>,
    /// Files left alone, and why.
    pub skipped: Vec<(String, String)>,
}

impl Plan {
    pub fn is_empty(&self) -> bool {
        self.ops.is_empty()
    }
}

/// Work out how to undo `edits` (oldest first) in `workspace`.
///
/// Each file is all-or-nothing: if any of its edits cannot be undone safely,
/// the file stays exactly as it is now.
pub fn plan(workspace: &Path, edits: &[Edit]) -> Plan {
    // Per file: what it holds on disk now, what it would hold after the undo
    // steps so far (`None` = absent), and the first reason it cannot be undone.
    struct File {
        label: String,
        now: Option<String>,
        then: Option<String>,
        blocked: Option<String>,
    }
    let mut files: HashMap<PathBuf, File> = HashMap::new();
    let mut order: Vec<PathBuf> = Vec::new();

    for edit in edits.iter().rev() {
        let (key, resolved) = match resolve_within(workspace, &edit.path) {
            Ok(abs) => (abs.clone(), Some(abs)),
            Err(_) => (PathBuf::from(&edit.path), None),
        };
        if !files.contains_key(&key) {
            let label = key.strip_prefix(workspace).map_or_else(|_| edit.path.clone(), |rel| rel.display().to_string());
            let mut file = File { label, now: None, then: None, blocked: None };
            match resolved.as_deref().map(read_text) {
                Some(Ok(text)) => {
                    file.now = text.clone();
                    file.then = text;
                }
                Some(Err(why)) => file.blocked = Some(why),
                None => file.blocked = Some("outside the workspace".to_string()),
            }
            order.push(key.clone());
            files.insert(key.clone(), file);
        }
        let file = files.get_mut(&key).expect("inserted above");
        if file.blocked.is_some() {
            continue;
        }
        // Calls recorded before fingerprints existed carry none; they are
        // restored without the check, which is all that can be done for them.
        if let Some(after) = edit.change.after {
            match &file.then {
                Some(text) if content_hash(text) == after => {}
                Some(_) => {
                    file.blocked = Some("changed since the agent wrote it".to_string());
                    continue;
                }
                None => {
                    file.blocked = Some("deleted since the agent wrote it".to_string());
                    continue;
                }
            }
        }
        if edit.change.created {
            file.then = None;
        } else if let Some(before) = &edit.change.before {
            file.then = Some(before.clone());
        } else {
            file.blocked = Some("its earlier version was too large to keep".to_string());
        }
    }

    let mut plan = Plan::default();
    for key in order {
        let file = files.remove(&key).expect("every key was inserted");
        if let Some(why) = file.blocked {
            plan.skipped.push((file.label, why));
        } else if file.now != file.then {
            let op = file.then.map_or(Op::Delete, Op::Write);
            plan.ops.push((file.label, key, op));
        }
    }
    plan.ops.sort_by(|a, b| a.0.cmp(&b.0));
    plan.skipped.sort();
    plan
}

/// The file's text, `None` if it does not exist, or why it cannot be undone.
fn read_text(path: &Path) -> Result<Option<String>, String> {
    match std::fs::read(path) {
        Ok(bytes) => String::from_utf8(bytes).map(Some).map_err(|_| "not text".to_string()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.to_string()),
    }
}

/// What applying a plan did.
#[derive(Debug, Default, PartialEq)]
pub struct Report {
    pub restored: usize,
    pub deleted: usize,
    pub failed: Vec<(String, String)>,
}

/// Carry out `plan`. Removing a created file also removes the directories it
/// leaves empty, up to (not including) the workspace.
pub fn apply(workspace: &Path, plan: &Plan) -> Report {
    let mut report = Report::default();
    for (label, abs, op) in &plan.ops {
        let result = match op {
            Op::Write(text) => abs
                .parent()
                .map_or(Ok(()), std::fs::create_dir_all)
                .and_then(|()| std::fs::write(abs, text))
                .map(|()| report.restored += 1),
            Op::Delete => std::fs::remove_file(abs).map(|()| {
                report.deleted += 1;
                prune_empty_parents(workspace, abs);
            }),
        };
        if let Err(error) = result {
            report.failed.push((label.clone(), error.to_string()));
        }
    }
    report
}

fn prune_empty_parents(workspace: &Path, file: &Path) {
    let mut dir = file.parent();
    let mut seen = HashSet::new();
    while let Some(current) = dir {
        if current == workspace || !current.starts_with(workspace) || !seen.insert(current.to_path_buf()) {
            break;
        }
        if std::fs::remove_dir(current).is_err() {
            break;
        }
        dir = current.parent();
    }
}

/// What `/rewind` takes back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scope {
    Both,
    Conversation,
    Code,
}

/// How many requests back to go, and what to take back, from the words after
/// `/rewind`. `Err` is the thing that was not understood.
fn parse_args(args: &str) -> Result<(Option<usize>, Option<Scope>), String> {
    let (mut back, mut scope) = (None, None);
    for word in args.split_whitespace() {
        match word.to_ascii_lowercase().as_str() {
            "code" | "files" => scope = Some(Scope::Code),
            "chat" | "conversation" => scope = Some(Scope::Conversation),
            "both" | "all" => scope = Some(Scope::Both),
            number => match number.parse::<usize>() {
                Ok(n) if n >= 1 => back = Some(n),
                _ => return Err(word.to_string()),
            },
        }
    }
    Ok((back, scope))
}

impl Chat {
    /// The file edits each turn from `from` on made, one list per turn.
    async fn edits_by_turn(&self, from: usize) -> Vec<Vec<Edit>> {
        let mut out = Vec::new();
        for turn in &self.session.turns[from..] {
            let events = match RunId::parse(&turn.run_id) {
                Ok(run) => self.store.events_since(&run, 0, 1_000_000).await.unwrap_or_default(),
                Err(_) => Vec::new(),
            };
            out.push(edits_in(&events));
        }
        out
    }

    /// `/rewind [n] [code|chat|both]`: go back to before a request you made,
    /// undoing the agent's file edits, the conversation, or both.
    pub(super) async fn cmd_rewind(&mut self, args: &str) -> Result<()> {
        let (back, scope) = match parse_args(args) {
            Ok(parsed) => parsed,
            Err(word) => {
                self.say(format!("unknown rewind option `{word}` \u{00b7} /rewind [n] [code|chat|both]"));
                return Ok(());
            }
        };
        let requests: Vec<usize> = self
            .session
            .turns
            .iter()
            .enumerate()
            .filter(|(_, turn)| !turn.goal.starts_with(INTERNAL_PREFIX))
            .map(|(index, _)| index)
            .collect();
        if requests.is_empty() {
            self.say("nothing to rewind to yet");
            return Ok(());
        }
        // Newest first: what the picker lists, and what `n` counts.
        let recent: Vec<usize> = requests.iter().rev().take(20).copied().collect();
        let oldest = *recent.last().expect("requests is not empty");
        let by_turn = self.edits_by_turn(oldest).await;
        let files_from = |index: usize| -> usize {
            let edits: Vec<Edit> = by_turn[index - oldest..].concat();
            plan(&self.workspace, &edits).ops.len()
        };

        let target = match back {
            Some(n) => match requests.iter().rev().nth(n - 1) {
                Some(index) if *index >= oldest => *index,
                _ => {
                    self.say(format!("only {} request(s) to rewind to", recent.len()));
                    return Ok(());
                }
            },
            None if self.ui.is_live() => {
                let options = recent
                    .iter()
                    .map(|&index| {
                        let files = files_from(index);
                        let note = match files {
                            0 => String::new(),
                            1 => "  (1 file)".to_string(),
                            n => format!("  ({n} files)"),
                        };
                        format!("{}{note}", clip(&self.session.turns[index].goal, 64))
                    })
                    .collect();
                let mut spec = DialogSpec::pick("Rewind to before which request?", options);
                spec.hint = "\u{2191}/\u{2193} to move \u{00b7} enter to select \u{00b7} esc to cancel".into();
                match self.ui.choose(spec).await {
                    Some(choice) => recent[choice],
                    None => return Ok(()),
                }
            }
            None => {
                let mut lines = vec!["/rewind <n> goes back n requests (1 is the latest):".to_string()];
                for (number, &index) in recent.iter().enumerate() {
                    lines.push(format!("{:>3}. {}", number + 1, clip(&self.session.turns[index].goal, 70)));
                }
                self.say_lines(lines);
                return Ok(());
            }
        };

        let edits: Vec<Edit> = by_turn[target - oldest..].concat();
        let plan = plan(&self.workspace, &edits);
        let scope = match scope {
            Some(scope) => scope,
            None if self.ui.is_live() => match self.ask_scope(target, &plan).await {
                Some(scope) => scope,
                None => return Ok(()),
            },
            None => Scope::Both,
        };

        let goal = self.session.turns[target].goal.clone();
        let mut lines = Vec::new();
        if scope != Scope::Conversation {
            let report = apply(&self.workspace, &plan);
            if plan.is_empty() && plan.skipped.is_empty() {
                lines.push("no file edits to undo".to_string());
            }
            let mut done = Vec::new();
            if report.restored > 0 {
                done.push(format!("restored {} file(s)", report.restored));
            }
            if report.deleted > 0 {
                done.push(format!("removed {} file(s) the agent created", report.deleted));
            }
            if !done.is_empty() {
                lines.push(done.join(", "));
            }
            for (path, why) in &plan.skipped {
                lines.push(format!("left {path} alone \u{2014} {why}"));
            }
            for (path, why) in &report.failed {
                lines.push(format!("could not restore {path}: {why}"));
            }
        }
        if scope != Scope::Code {
            let dropped = self.session.turns.len() - target;
            self.session.turns.truncate(target);
            if let Some(goal) = self.goal.as_mut().filter(|goal| goal.is_active()) {
                goal.pause("rewound");
                lines.push("goal paused \u{2014} /goal resume to keep going".to_string());
            }
            self.last_result = self.session.turns.last().map(|turn| turn.summary.clone()).unwrap_or_default();
            self.last_run = self.session.turns.last().and_then(|turn| RunId::parse(&turn.run_id).ok());
            lines.insert(0, format!("rewound {dropped} turn(s); your request is back in the prompt"));
            self.ui.prefill(goal);
        }
        if scope != Scope::Conversation {
            lines.push("shell commands the agent ran are not undone".to_string());
        }
        self.say_lines(lines);
        self.push_info();
        self.save().await;
        Ok(())
    }

    /// Show what would be undone and ask how far to go.
    async fn ask_scope(&self, target: usize, plan: &Plan) -> Option<Scope> {
        let mut spec = DialogSpec::pick(
            &format!("Rewind to before: {}", clip(&self.session.turns[target].goal, 50)),
            Vec::new(),
        );
        let mut options = Vec::new();
        let mut scopes = Vec::new();
        if !plan.is_empty() {
            for (path, _, op) in plan.ops.iter().take(8) {
                let mark = match op {
                    Op::Write(_) => "restore",
                    Op::Delete => "remove ",
                };
                spec.body.push(role(self.colors, "subtle", &format!("  {mark}  {path}")));
            }
            if plan.ops.len() > 8 {
                spec.body.push(role(self.colors, "subtle", &format!("  \u{2026} and {} more", plan.ops.len() - 8)));
            }
            options.push("Restore code and conversation".to_string());
            scopes.push(Scope::Both);
            options.push("Restore conversation only".to_string());
            scopes.push(Scope::Conversation);
            options.push("Restore code only".to_string());
            scopes.push(Scope::Code);
        } else {
            spec.body.push(role(self.colors, "subtle", "  no file edits to undo"));
            options.push("Restore conversation".to_string());
            scopes.push(Scope::Conversation);
        }
        for (path, why) in plan.skipped.iter().take(4) {
            spec.body.push(role(self.colors, "warn", &format!("  {path}: {why}, left alone")));
        }
        spec.body.push(role(self.colors, "subtle", "  shell commands the agent ran are not undone"));
        options.push("Never mind".to_string());
        spec.options = options;
        match self.ui.choose(spec).await {
            Some(choice) => scopes.get(choice).copied(),
            None => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn change(before: Option<&str>, created: bool, after: &str) -> FileChange {
        FileChange { before: before.map(str::to_string), created, after: Some(content_hash(after)) }
    }

    fn edit(path: &str, before: Option<&str>, created: bool, after: &str) -> Edit {
        Edit { path: path.to_string(), change: change(before, created, after) }
    }

    #[test]
    fn edits_come_out_of_tool_results_oldest_first() {
        let run = RunId::new();
        let result = |name: &str, path: &str, success: bool, change: serde_json::Value| {
            RunEvent::new(
                EventKind::ToolResult,
                run.clone(),
                json!({"name": name, "success": success, "arguments": {"path": path}, "change": change}),
            )
        };
        let events = vec![
            result("write_file", "a.txt", true, json!({"before": null, "created": true})),
            result("read_file", "a.txt", true, json!(null)),
            result("edit_file", "a.txt", false, json!({"before": "x", "created": false})),
            result("edit_file", "a.txt", true, json!({"before": "one", "created": false, "after": 7})),
        ];
        let edits = edits_in(&events);
        assert_eq!(edits.len(), 2, "reads and failures are not edits");
        assert!(edits[0].change.created);
        assert_eq!(edits[1].change.before.as_deref(), Some("one"));
        assert_eq!(edits[1].change.after, Some(7));
    }

    #[test]
    fn a_created_file_is_removed_with_the_directories_it_made() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("src/deep")).unwrap();
        std::fs::write(root.join("src/deep/new.rs"), "fn main() {}\n").unwrap();
        let edits = vec![edit("src/deep/new.rs", None, true, "fn main() {}\n")];
        let plan = plan(root, &edits);
        assert_eq!(plan.ops.len(), 1);
        let report = apply(root, &plan);
        assert_eq!(report.deleted, 1);
        assert!(!root.join("src").exists(), "empty directories go with it");
        assert!(root.exists());
    }

    #[test]
    fn several_edits_to_one_file_unwind_to_the_oldest_version() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("f.txt"), "three\n").unwrap();
        let edits = vec![
            edit("f.txt", Some("one\n"), false, "two\n"),
            edit("f.txt", Some("two\n"), false, "three\n"),
        ];
        let plan = plan(root, &edits);
        assert_eq!(plan.ops.len(), 1, "one write, not one per edit");
        apply(root, &plan);
        assert_eq!(std::fs::read_to_string(root.join("f.txt")).unwrap(), "one\n");
    }

    #[test]
    fn a_hand_edited_file_is_left_alone_and_reported() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("mine.txt"), "I changed this myself\n").unwrap();
        std::fs::write(root.join("theirs.txt"), "agent wrote\n").unwrap();
        let edits = vec![
            edit("mine.txt", Some("old\n"), false, "agent wrote\n"),
            edit("theirs.txt", Some("old\n"), false, "agent wrote\n"),
        ];
        let plan = plan(root, &edits);
        assert_eq!(plan.ops.len(), 1);
        assert_eq!(plan.skipped, vec![("mine.txt".to_string(), "changed since the agent wrote it".to_string())]);
        apply(root, &plan);
        assert_eq!(std::fs::read_to_string(root.join("mine.txt")).unwrap(), "I changed this myself\n");
        assert_eq!(std::fs::read_to_string(root.join("theirs.txt")).unwrap(), "old\n");
    }

    #[test]
    fn a_file_blocked_partway_is_not_half_restored() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        // The agent wrote "two" then "three", but someone changed "two" to "TWO"
        // in between; undoing the newest edit is fine, the older one is not.
        std::fs::write(root.join("f.txt"), "three\n").unwrap();
        let edits = vec![
            edit("f.txt", Some("one\n"), false, "two\n"),
            edit("f.txt", Some("TWO\n"), false, "three\n"),
        ];
        let plan = plan(root, &edits);
        assert!(plan.is_empty(), "left exactly as it is");
        assert_eq!(plan.skipped.len(), 1);
        assert_eq!(std::fs::read_to_string(root.join("f.txt")).unwrap(), "three\n");
    }

    #[test]
    fn a_deleted_file_and_an_oversized_original_are_explained() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("big.txt"), "now\n").unwrap();
        let big = Edit {
            path: "big.txt".into(),
            change: FileChange { before: None, created: false, after: Some(content_hash("now\n")) },
        };
        let gone = edit("gone.txt", Some("x\n"), false, "y\n");
        let plan = plan(root, &[big, gone]);
        assert!(plan.is_empty());
        let reasons: Vec<&str> = plan.skipped.iter().map(|(_, why)| why.as_str()).collect();
        assert!(reasons.iter().any(|why| why.contains("too large")), "{reasons:?}");
        assert!(reasons.iter().any(|why| why.contains("deleted")), "{reasons:?}");
    }

    #[test]
    fn an_edit_with_no_fingerprint_is_restored_unchecked() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("old.txt"), "whatever\n").unwrap();
        let legacy = Edit {
            path: "old.txt".into(),
            change: FileChange { before: Some("before\n".into()), created: false, after: None },
        };
        let plan = plan(root, &[legacy]);
        apply(root, &plan);
        assert_eq!(std::fs::read_to_string(root.join("old.txt")).unwrap(), "before\n");
    }

    #[test]
    fn paths_outside_the_workspace_are_never_touched() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("x.txt"), "keep\n").unwrap();
        let hostile = format!("{}/x.txt", outside.path().display());
        let plan = plan(dir.path(), &[edit(&hostile, None, true, "keep\n")]);
        assert!(plan.is_empty());
        assert_eq!(plan.skipped.len(), 1);
        assert!(outside.path().join("x.txt").exists());
    }
}
