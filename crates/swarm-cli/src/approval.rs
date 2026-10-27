//! Tool approvals: the permission dialog and the session's allow list.
//!
//! The dialog is an ordinary [`DialogSpec`] shown by the UI task, so approving
//! never touches the keyboard or the screen from the engine's side; the
//! engine just awaits the answer.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;
use swarm_core::config::ApprovalPolicy;
use swarm_runtime::{ApprovalHandler, ApprovalRequest};
use tokio::sync::Mutex as AsyncMutex;

use crate::style::{Color, role};
use crate::ui::diff;
use crate::ui::text::{sanitize, truncate};
use crate::ui::{Accent, DialogSpec, Mode, UiHandle};

/// How long a prompt waits before it counts as a refusal.
const PROMPT_TIMEOUT: Duration = Duration::from_secs(600);
/// Rows of command or diff shown before the dialog collapses the rest.
const BODY_ROWS: usize = 16;

/// Tools the user approved for the rest of the session ("yes, and don't ask
/// again for this tool").
#[derive(Default)]
pub struct AllowList(Mutex<HashSet<String>>);

impl AllowList {
    pub fn new() -> Arc<Self> {
        Arc::default()
    }

    pub fn allow(&self, name: &str) {
        if let Ok(mut set) = self.0.lock() {
            set.insert(name.to_string());
        }
    }

    pub fn contains(&self, name: &str) -> bool {
        self.0.lock().map(|set| set.contains(name)).unwrap_or(false)
    }

    pub fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .0
            .lock()
            .map(|set| set.iter().cloned().collect())
            .unwrap_or_default();
        names.sort();
        names
    }

    pub fn clear(&self) {
        if let Ok(mut set) = self.0.lock() {
            set.clear();
        }
    }
}

/// The current permission mode, shared between the session loop (Shift+Tab)
/// and the approval handler, so switching to "auto" takes effect on a prompt
/// that has not been shown yet.
#[derive(Debug, Default)]
pub struct SharedMode(AtomicU8);

impl SharedMode {
    pub fn new(mode: Mode) -> Arc<Self> {
        let shared = Arc::new(Self::default());
        shared.set(mode);
        shared
    }

    pub fn set(&self, mode: Mode) {
        self.0.store(
            match mode {
                Mode::Default => 0,
                Mode::AskAll => 1,
                Mode::Auto => 2,
                Mode::Plan => 3,
            },
            Ordering::SeqCst,
        );
    }

    pub fn get(&self) -> Mode {
        match self.0.load(Ordering::SeqCst) {
            1 => Mode::AskAll,
            2 => Mode::Auto,
            3 => Mode::Plan,
            _ => Mode::Default,
        }
    }
}

/// The answer to one approval prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Deny,
    AllowOnce,
    AllowAlways,
}

impl Decision {
    pub fn approved(self) -> bool {
        self != Decision::Deny
    }

    /// The dialog's option index to a decision (`None` is a dismissal).
    pub fn from_choice(choice: Option<usize>) -> Self {
        match choice {
            Some(0) => Self::AllowOnce,
            Some(1) => Self::AllowAlways,
            _ => Self::Deny,
        }
    }
}

/// The footer mode a run's approval policy corresponds to.
pub fn mode_of(policy: ApprovalPolicy) -> Mode {
    match policy {
        ApprovalPolicy::AskDangerous => Mode::Default,
        ApprovalPolicy::AskAll => Mode::AskAll,
        ApprovalPolicy::Never => Mode::Auto,
    }
}

/// The runtime policy a mode needs. Plan mode asks about *every* tool so the
/// handler can refuse everything but reading.
pub fn policy_of(mode: Mode) -> ApprovalPolicy {
    match mode {
        Mode::Default => ApprovalPolicy::AskDangerous,
        Mode::AskAll | Mode::Plan => ApprovalPolicy::AskAll,
        Mode::Auto => ApprovalPolicy::Never,
    }
}

/// Tools that only look; plan mode lets them through.
pub fn is_read_only(tool: &str) -> bool {
    matches!(tool, "read_file" | "list_files" | "search_files")
}

pub struct UiApproval {
    ui: UiHandle,
    allow: Arc<AllowList>,
    mode: Arc<SharedMode>,
    workspace: PathBuf,
    /// Parallel agents can ask at once; the user answers one prompt at a time.
    gate: AsyncMutex<()>,
}

impl UiApproval {
    pub fn new(ui: UiHandle, allow: Arc<AllowList>, mode: Arc<SharedMode>, workspace: PathBuf) -> Self {
        Self { ui, allow, mode, workspace, gate: AsyncMutex::new(()) }
    }
}

#[async_trait]
impl ApprovalHandler for UiApproval {
    async fn approve(&self, request: &ApprovalRequest) -> bool {
        let _turn = self.gate.lock().await;
        // Plan mode is read-only no matter what was allowed earlier.
        if self.mode.get() == Mode::Plan {
            return is_read_only(&request.name);
        }
        if self.mode.get() == Mode::Auto || self.allow.contains(&request.name) {
            return true;
        }
        if !self.ui.is_live() {
            eprintln!(
                "tool {} requires approval, but no interactive terminal is available; denied",
                request.name
            );
            return false;
        }
        let spec = dialog_for(request, &self.workspace, crate::style::term_width(), self.ui.colors());
        let choice = tokio::time::timeout(PROMPT_TIMEOUT, self.ui.choose(spec)).await.ok().flatten();
        let decision = Decision::from_choice(choice);
        if decision == Decision::AllowAlways {
            self.allow.allow(&request.name);
        }
        decision.approved()
    }
}

/// What the dialog is called, by tool.
fn title_for(name: &str, creates: bool) -> String {
    match name {
        "bash" => "Bash command".into(),
        "run_tests" => "Run tests".into(),
        "python" => "Python script".into(),
        "git" => "Git command".into(),
        "write_file" if creates => "Create file".into(),
        "write_file" => "Overwrite file".into(),
        "edit_file" => "Edit file".into(),
        other => format!("Use tool: {other}"),
    }
}

fn str_arg<'a>(arguments: &'a Value, key: &str) -> &'a str {
    arguments.get(key).and_then(Value::as_str).unwrap_or("")
}

/// Rows describing the call: the command it will run, or the diff it will make.
fn body_for(request: &ApprovalRequest, workspace: &Path, inner: usize, colors: Color) -> (Vec<String>, bool) {
    let args = &request.arguments;
    let dim = |text: &str| role(colors, "subtle", text);
    let mut rows: Vec<String> = Vec::new();
    let mut creates = false;
    let code = |text: &str| -> Vec<String> {
        sanitize(text).lines().map(|line| format!("  {line}")).collect()
    };
    match request.name.as_str() {
        "bash" => rows.extend(code(str_arg(args, "command"))),
        "run_tests" => {
            let command = str_arg(args, "command");
            rows.extend(code(if command.is_empty() { "cargo test" } else { command }));
        }
        "python" => rows.extend(code(str_arg(args, "code"))),
        "git" => {
            let parts: Vec<&str> = args
                .get("args")
                .and_then(Value::as_array)
                .map(|items| items.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            rows.push(format!("  git {}", sanitize(&parts.join(" "))));
        }
        "edit_file" | "write_file" => {
            let path = str_arg(args, "path");
            let on_disk = swarm_tools::resolve_within(workspace, path)
                .ok()
                .and_then(|abs| std::fs::read_to_string(abs).ok());
            let view = if request.name == "edit_file" {
                let find = str_arg(args, "find");
                let line = on_disk.as_deref().and_then(|text| diff::line_of(text, find)).unwrap_or(0);
                diff::snippet_diff(find, str_arg(args, "replace"), line, inner, colors, BODY_ROWS)
            } else {
                creates = on_disk.is_none();
                diff::file_diff(on_disk.as_deref(), str_arg(args, "content"), inner, colors, BODY_ROWS)
            };
            rows.push(format!("  {}", truncate(&sanitize(path), inner.saturating_sub(2))));
            rows.push(format!("  {}", dim(&diff::summary_text(&view.summary))));
            rows.extend(view.lines);
        }
        _ => rows.push(format!("  {}", crate::transcript::args_summary(args))),
    }
    if !request.risk.is_empty() {
        rows.push(String::new());
        rows.push(format!("  {}", dim(&sanitize(&request.risk))));
    }
    (rows, creates)
}

/// The permission dialog for one request.
pub fn dialog_for(request: &ApprovalRequest, workspace: &Path, cols: usize, colors: Color) -> DialogSpec {
    let inner = cols.clamp(24, 110).saturating_sub(4);
    let (body, creates) = body_for(request, workspace, inner, colors);
    DialogSpec {
        title: title_for(&request.name, creates),
        body,
        question: "Do you want to proceed?".to_string(),
        options: vec![
            "Yes".to_string(),
            format!("Yes, and don't ask again for {} this session", request.name),
            "No, deny this call (esc)".to_string(),
        ],
        shortcuts: vec![('y', 0), ('a', 1), ('n', 2)],
        default: 0,
        hint: "enter confirms \u{00b7} 1/2/3 or y/a/n \u{00b7} esc denies".to_string(),
        accent: Accent::Permission,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::text::strip_ansi;
    use serde_json::json;
    use swarm_core::{AgentId, RunId, TaskId};

    fn request(name: &str, arguments: Value, risk: &str) -> ApprovalRequest {
        ApprovalRequest {
            run_id: RunId::new(),
            task_id: TaskId::new(),
            agent_id: AgentId::new(),
            call_id: "call_1".to_string(),
            name: name.to_string(),
            arguments,
            risk: risk.to_string(),
        }
    }

    fn text(spec: &DialogSpec) -> String {
        spec.body.iter().map(|row| strip_ansi(row)).collect::<Vec<_>>().join("\n")
    }

    #[test]
    fn choices_map_to_decisions() {
        assert_eq!(Decision::from_choice(Some(0)), Decision::AllowOnce);
        assert_eq!(Decision::from_choice(Some(1)), Decision::AllowAlways);
        assert_eq!(Decision::from_choice(Some(2)), Decision::Deny);
        assert_eq!(Decision::from_choice(None), Decision::Deny, "dismissing denies");
        assert_eq!(Decision::from_choice(Some(9)), Decision::Deny);
        assert!(Decision::AllowAlways.approved() && !Decision::Deny.approved());
    }

    #[test]
    fn a_bash_call_shows_the_exact_command_and_why() {
        let spec = dialog_for(
            &request("bash", json!({"command": "rm -rf build\necho done"}), "runs a shell command"),
            Path::new("."),
            80,
            Color::Plain,
        );
        assert_eq!(spec.title, "Bash command");
        let body = text(&spec);
        assert!(body.contains("  rm -rf build\n  echo done"), "{body}");
        assert!(body.contains("runs a shell command"), "{body}");
        assert_eq!(spec.options.len(), 3);
        assert!(spec.options[1].contains("bash"));
        assert_eq!(spec.shortcuts, vec![('y', 0), ('a', 1), ('n', 2)]);
        assert_eq!(spec.accent, Accent::Permission);
    }

    #[test]
    fn untrusted_command_text_cannot_inject_escapes() {
        let spec = dialog_for(
            &request("bash", json!({"command": "echo \u{1b}[2J\u{1b}]0;pwned\u{7}hi"}), "r\u{1b}[31m"),
            Path::new("."),
            80,
            Color::Plain,
        );
        for row in &spec.body {
            assert!(!row.contains('\u{1b}') && !row.contains('\u{7}'), "{row:?}");
        }
    }

    #[test]
    fn git_and_python_calls_are_readable() {
        let git = dialog_for(&request("git", json!({"args": ["push", "--force"]}), ""), Path::new("."), 80, Color::Plain);
        assert!(text(&git).contains("git push --force"));
        let py = dialog_for(&request("python", json!({"code": "print(1)\nprint(2)"}), ""), Path::new("."), 80, Color::Plain);
        assert!(text(&py).contains("  print(1)\n  print(2)"));
        assert_eq!(py.title, "Python script");
    }

    #[test]
    fn an_edit_shows_the_diff_with_real_line_numbers() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("app.py"), "import os\n\ndef main():\n    return 1\n").unwrap();
        let spec = dialog_for(
            &request(
                "edit_file",
                json!({"path": "app.py", "find": "    return 1", "replace": "    return 2"}),
                "",
            ),
            dir.path(),
            80,
            Color::Plain,
        );
        assert_eq!(spec.title, "Edit file");
        let body = text(&spec);
        assert!(body.contains("app.py"), "{body}");
        assert!(body.contains("Added 1 line, removed 1 line"), "{body}");
        assert!(body.contains(" 4 -     return 1"), "line 4 is where the text really is: {body}");
        assert!(body.contains(" 4 +     return 2"), "{body}");
    }

    #[test]
    fn writing_a_new_file_says_create_and_an_existing_one_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let new = dialog_for(
            &request("write_file", json!({"path": "new.txt", "content": "hello\n"}), ""),
            dir.path(),
            80,
            Color::Plain,
        );
        assert_eq!(new.title, "Create file");
        assert!(text(&new).contains("Added 1 line"), "{}", text(&new));
        std::fs::write(dir.path().join("old.txt"), "before\n").unwrap();
        let over = dialog_for(
            &request("write_file", json!({"path": "old.txt", "content": "after\n"}), ""),
            dir.path(),
            80,
            Color::Plain,
        );
        assert_eq!(over.title, "Overwrite file");
        assert!(text(&over).contains("- before") && text(&over).contains("+ after"), "{}", text(&over));
    }

    #[test]
    fn a_path_outside_the_workspace_is_not_read() {
        let dir = tempfile::tempdir().unwrap();
        let spec = dialog_for(
            &request("write_file", json!({"path": "../../etc/hostname", "content": "x\n"}), ""),
            dir.path(),
            80,
            Color::Plain,
        );
        assert_eq!(spec.title, "Create file", "an escaping path is treated as unreadable");
    }

    #[test]
    fn a_huge_diff_is_collapsed_in_the_dialog() {
        let dir = tempfile::tempdir().unwrap();
        let content: String = (0..500).map(|n| format!("line {n}\n")).collect();
        let spec = dialog_for(
            &request("write_file", json!({"path": "big.txt", "content": content}), ""),
            dir.path(),
            80,
            Color::Plain,
        );
        assert!(spec.body.len() < 30, "{} rows", spec.body.len());
        assert!(text(&spec).contains("more lines"), "{}", text(&spec));
    }

    #[test]
    fn unknown_tools_fall_back_to_their_arguments() {
        let spec = dialog_for(&request("mystery", json!({"x": 1}), "unknown"), Path::new("."), 80, Color::Plain);
        assert_eq!(spec.title, "Use tool: mystery");
        assert!(text(&spec).contains("x=1"));
    }

    #[test]
    fn the_allow_list_tracks_tools() {
        let allow = AllowList::new();
        assert!(!allow.contains("write_file"));
        allow.allow("write_file");
        allow.allow("bash");
        assert!(allow.contains("write_file"));
        assert_eq!(allow.names(), vec!["bash".to_string(), "write_file".to_string()]);
        allow.clear();
        assert!(!allow.contains("write_file"));
    }

    #[test]
    fn modes_and_policies_map_both_ways() {
        for policy in [ApprovalPolicy::AskDangerous, ApprovalPolicy::AskAll, ApprovalPolicy::Never] {
            assert_eq!(policy_of(mode_of(policy)), policy);
        }
        assert_eq!(policy_of(Mode::Plan), ApprovalPolicy::AskAll, "plan mode must see every call");
    }

    #[test]
    fn the_shared_mode_round_trips() {
        let mode = SharedMode::new(Mode::Default);
        for next in [Mode::AskAll, Mode::Auto, Mode::Plan, Mode::Default] {
            mode.set(next);
            assert_eq!(mode.get(), next);
        }
    }

    #[tokio::test]
    async fn auto_mode_and_the_allow_list_skip_the_dialog() {
        let allow = AllowList::new();
        let mode = SharedMode::new(Mode::Auto);
        let approval =
            UiApproval::new(UiHandle::plain(Color::Plain), Arc::clone(&allow), Arc::clone(&mode), PathBuf::from("."));
        let call = request("bash", json!({"command": "ls"}), "runs");
        assert!(approval.approve(&call).await, "auto mode approves without a terminal");
        mode.set(Mode::Default);
        assert!(!approval.approve(&call).await, "no terminal and not allowed: denied");
        allow.allow("bash");
        assert!(approval.approve(&call).await, "an allowed tool needs no dialog");
    }

    #[tokio::test]
    async fn plan_mode_allows_only_reading() {
        let mode = SharedMode::new(Mode::Plan);
        let allow = AllowList::new();
        allow.allow("bash");
        let approval = UiApproval::new(UiHandle::plain(Color::Plain), allow, mode, PathBuf::from("."));
        assert!(approval.approve(&request("read_file", json!({"path": "a"}), "")).await);
        assert!(!approval.approve(&request("bash", json!({"command": "ls"}), "")).await, "even an allowed tool");
        assert!(!approval.approve(&request("write_file", json!({}), "")).await);
    }
}
