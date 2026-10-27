//! The [`ToolExecutor`]: resolves, validates, and executes tool calls inside a
//! [`SandboxConfig`].

use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use serde_json::Value;

use swarm_core::tool::{ToolCatalog, ToolRequestValidation, ToolSpec};

use crate::catalog::builtin_catalog;
use crate::error::{FileChange, ToolResult};
use crate::sandbox::{SandboxConfig, cap_output};

/// A single model-requested tool call, decoupled from the provider types.
#[derive(Debug, Clone)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}

impl ToolCall {
    pub fn new(id: impl Into<String>, name: impl Into<String>, arguments: Value) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            arguments,
        }
    }
}

/// Executes validated tool calls inside a sandbox. Cheap to clone-free; the
/// executor is immutable after construction and safe to share.
pub struct ToolExecutor {
    pub catalog: ToolCatalog,
    pub config: SandboxConfig,
}

impl ToolExecutor {
    /// An executor with the built-in tool catalog and the given sandbox.
    pub fn new(config: SandboxConfig) -> Self {
        Self {
            catalog: builtin_catalog(),
            config,
        }
    }

    /// Replace the tool catalog (for custom/tool-extended runs).
    pub fn with_catalog(mut self, catalog: ToolCatalog) -> Self {
        self.catalog = catalog;
        self
    }

    pub fn catalog(&self) -> &ToolCatalog {
        &self.catalog
    }

    pub fn spec(&self, name: &str) -> Option<&ToolSpec> {
        self.catalog.get(name)
    }

    /// Names of the tools available to the agent.
    pub fn tool_names(&self) -> Vec<String> {
        self.catalog.specs.iter().map(|s| s.name.clone()).collect()
    }

    /// Policy check for a call: auto-approve (sandboxed and in-bounds),
    /// needs human approval, or deny (unknown tool, out-of-bounds path).
    pub fn validate(&self, name: &str, input: &Value) -> ToolRequestValidation {
        let spec = match self.catalog.get(name) {
            Some(s) => s,
            None => {
                return ToolRequestValidation::Deny {
                    reason: format!("unknown tool: {name}"),
                };
            }
        };
        if !input.is_object() {
            return ToolRequestValidation::Deny {
                reason: format!("tool `{name}` input must be a JSON object"),
            };
        }
        for field in ["path", "cwd"] {
            if let Some(p) = input.get(field).and_then(|v| v.as_str()) {
                if let Err(e) = self.config.resolve(p) {
                    return ToolRequestValidation::Deny {
                        reason: e.to_string(),
                    };
                }
            }
        }
        if spec.requires_approval || !spec.sandboxed {
            return ToolRequestValidation::NeedsApproval {
                risk: format!("tool `{name}` may act outside the sandbox"),
            };
        }
        ToolRequestValidation::AutoApprove {
            reason: format!("sandboxed `{name}` within workspace"),
        }
    }

    /// Execute a tool call, returning its result. Never panics; tool-level
    /// failures are carried in the returned result.
    pub async fn execute(&self, call: &ToolCall) -> ToolResult {
        let start = Instant::now();
        let mut r = match call.name.as_str() {
            "bash" => self.tool_bash(&call.arguments).await,
            "read_file" => self.tool_read_file(&call.arguments),
            "write_file" => self.tool_write_file(&call.arguments),
            "edit_file" => self.tool_edit_file(&call.arguments),
            "list_files" => self.tool_list_files(&call.arguments),
            "search_files" => self.tool_search_files(&call.arguments),
            "create_directory" => self.tool_create_directory(&call.arguments),
            "run_tests" => self.tool_run_tests(&call.arguments).await,
            "python" => self.tool_python(&call.arguments).await,
            "git" => self.tool_git(&call.arguments).await,
            other => ToolResult::failure(format!("unknown tool: {other}")),
        };
        r.call_id = call.id.clone();
        r.name = call.name.clone();
        r.duration_ms = start.elapsed().as_millis() as u64;
        r
    }

    // ---- input accessors -------------------------------------------------

    fn str<'a>(input: &'a Value, key: &str) -> Option<&'a str> {
        input.get(key).and_then(|v| v.as_str())
    }

    fn uint(input: &Value, key: &str) -> Option<u64> {
        input.get(key).and_then(|v| v.as_u64())
    }

    fn bool(input: &Value, key: &str) -> Option<bool> {
        input.get(key).and_then(|v| v.as_bool())
    }

    fn timeout_for(input: &Value, config: &SandboxConfig) -> Duration {
        match Self::uint(input, "timeout_ms") {
            Some(ms) if ms > 0 => Duration::from_millis(ms),
            _ => config.command_timeout,
        }
    }

    // ---- command tools ----------------------------------------------------

    async fn tool_bash(&self, input: &Value) -> ToolResult {
        let command = match Self::str(input, "command") {
            Some(c) if !c.trim().is_empty() => c.to_string(),
            _ => return ToolResult::failure("bash: 'command' is required"),
        };
        let timeout = Self::timeout_for(input, &self.config);
        self.run_shell(&command, timeout).await
    }

    async fn tool_run_tests(&self, input: &Value) -> ToolResult {
        let command = Self::str(input, "command")
            .unwrap_or("cargo test")
            .to_string();
        let timeout = Self::timeout_for(input, &self.config);
        self.run_shell(&command, timeout).await
    }

    async fn run_shell(&self, command: &str, timeout: Duration) -> ToolResult {
        let (prog, args): (&str, Vec<String>) = if cfg!(windows) {
            ("cmd", vec!["/C".to_string(), command.to_string()])
        } else {
            ("/bin/sh", vec!["-c".to_string(), command.to_string()])
        };
        let mut cmd = tokio::process::Command::new(prog);
        cmd.args(args);
        cmd.current_dir(&self.config.workspace_root);
        // A tool must never read the user's keyboard, and an abandoned call (Ctrl+C,
        // a timeout) must not leave its process running.
        cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
        let child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => return ToolResult::failure(format!("failed to spawn command: {e}")),
        };
        match tokio::time::timeout(timeout, child.wait_with_output()).await {
            Ok(Ok(out)) => {
                let (content, t1) = cap_output(&out.stdout, self.config.output_cap_bytes);
                let (stderr, t2) = cap_output(&out.stderr, self.config.output_cap_bytes);
                ToolResult::command(content, stderr, out.status.code(), t1 || t2)
            }
            Ok(Err(e)) => ToolResult::failure(format!("command process error: {e}")),
            Err(_) => ToolResult::failure_with(
                format!("command timed out after {}ms", timeout.as_millis()),
                String::new(),
                None,
                false,
            ),
        }
    }

    // ---- file tools -------------------------------------------------------

    fn tool_read_file(&self, input: &Value) -> ToolResult {
        let path = match Self::str(input, "path") {
            Some(p) if !p.trim().is_empty() => p.to_string(),
            _ => return ToolResult::failure("read_file: 'path' is required"),
        };
        let abs = match self.config.resolve(&path) {
            Ok(p) => p,
            Err(e) => return ToolResult::failure(e.to_string()),
        };
        let bytes = match std::fs::read(&abs) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return ToolResult::failure(format!("not found: {path}"));
            }
            Err(e) => return ToolResult::failure(format!("read_file: {e}")),
        };
        let text = String::from_utf8_lossy(&bytes).to_string();
        let lines: Vec<&str> = text.lines().collect();
        let start = Self::uint(input, "start_line").unwrap_or(1).max(1) as usize;
        let mut chosen = String::new();
        if start - 1 <= lines.len() {
            let rest = &lines[(start - 1)..];
            let take = Self::uint(input, "limit")
                .map(|l| l as usize)
                .unwrap_or(usize::MAX)
                .min(rest.len());
            chosen = rest[..take].join("\n");
        }
        let (content, truncated) = cap_output(chosen.as_bytes(), self.config.output_cap_bytes);
        let mut r = ToolResult::ok(content);
        r.truncated = truncated;
        r
    }

    fn tool_write_file(&self, input: &Value) -> ToolResult {
        let path = match Self::str(input, "path") {
            Some(p) if !p.trim().is_empty() => p.to_string(),
            _ => return ToolResult::failure("write_file: 'path' is required"),
        };
        let content = match Self::str(input, "content") {
            Some(c) => c.to_string(),
            None => return ToolResult::failure("write_file: 'content' is required"),
        };
        let abs = match self.config.resolve(&path) {
            Ok(p) => p,
            Err(e) => return ToolResult::failure(e.to_string()),
        };
        if let Some(parent) = abs.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                return ToolResult::failure(format!("write_file: cannot create directory: {e}"));
            }
        }
        let before = std::fs::read_to_string(&abs).ok();
        let created = !abs.exists();
        match std::fs::write(&abs, &content) {
            Ok(()) => {
                let mut result = ToolResult::ok(format!("wrote {} byte(s) to {path}", content.len()));
                result.change = Some(FileChange::capture(before, created));
                result
            }
            Err(e) => ToolResult::failure(format!("write_file: {e}")),
        }
    }

    fn tool_edit_file(&self, input: &Value) -> ToolResult {
        let path = match Self::str(input, "path") {
            Some(p) if !p.trim().is_empty() => p.to_string(),
            _ => return ToolResult::failure("edit_file: 'path' is required"),
        };
        let find = match Self::str(input, "find") {
            Some(f) if !f.is_empty() => f.to_string(),
            _ => return ToolResult::failure("edit_file: 'find' is required"),
        };
        let replace = Self::str(input, "replace").unwrap_or("").to_string();
        let replace_all = Self::bool(input, "replace_all").unwrap_or(false);
        let abs = match self.config.resolve(&path) {
            Ok(p) => p,
            Err(e) => return ToolResult::failure(e.to_string()),
        };
        let original = match std::fs::read_to_string(&abs) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return ToolResult::failure(format!("not found: {path}"));
            }
            Err(e) => return ToolResult::failure(format!("edit_file: {e}")),
        };
        let count = original.matches(&find).count();
        if count == 0 {
            return ToolResult::failure(format!("edit_file: 'find' not found in {path}"));
        }
        if !replace_all && count > 1 {
            return ToolResult::failure(format!(
                "edit_file: 'find' matched {count} times; make it unique or pass replace_all=true"
            ));
        }
        let updated = if replace_all {
            original.replace(&find, &replace)
        } else {
            original.replacen(&find, &replace, 1)
        };
        match std::fs::write(&abs, &updated) {
            Ok(()) => {
                let mut result = ToolResult::ok(format!(
                    "edited {path} ({} replacement(s))",
                    if replace_all { count } else { 1 }
                ));
                result.change = Some(FileChange::capture(Some(original), false));
                result
            }
            Err(e) => ToolResult::failure(format!("edit_file: {e}")),
        }
    }

    fn tool_create_directory(&self, input: &Value) -> ToolResult {
        let path = match Self::str(input, "path") {
            Some(p) if !p.trim().is_empty() => p.to_string(),
            _ => return ToolResult::failure("create_directory: 'path' is required"),
        };
        let abs = match self.config.resolve(&path) {
            Ok(p) => p,
            Err(e) => return ToolResult::failure(e.to_string()),
        };
        match std::fs::create_dir_all(&abs) {
            Ok(()) => ToolResult::ok(format!("created directory: {path}")),
            Err(e) => ToolResult::failure(format!("create_directory: {e}")),
        }
    }

    fn tool_list_files(&self, input: &Value) -> ToolResult {
        let root_rel = Self::str(input, "path").unwrap_or(".");
        let base = match self.config.resolve(root_rel) {
            Ok(p) => p,
            Err(e) => return ToolResult::failure(e.to_string()),
        };
        if !base.is_dir() {
            return ToolResult::failure(format!("list_files: not a directory: {}", base.display()));
        }
        let max = Self::uint(input, "max_entries").unwrap_or(200) as usize;
        let mut entries: Vec<String> = Vec::new();
        collect_paths(&base, &base, &mut entries, max);
        let text = if entries.is_empty() {
            String::from("(empty directory)")
        } else {
            entries.join("\n")
        };
        let (content, truncated) = cap_output(text.as_bytes(), self.config.output_cap_bytes);
        let mut r = ToolResult::ok(content);
        r.truncated = truncated;
        r
    }

    fn tool_search_files(&self, input: &Value) -> ToolResult {
        let pattern = match Self::str(input, "pattern") {
            Some(p) if !p.trim().is_empty() => p.to_string(),
            _ => return ToolResult::failure("search_files: 'pattern' is required"),
        };
        let root_rel = Self::str(input, "path").unwrap_or(".");
        let base = match self.config.resolve(root_rel) {
            Ok(p) => p,
            Err(e) => return ToolResult::failure(e.to_string()),
        };
        let max = Self::uint(input, "max_matches").unwrap_or(100) as usize;
        let case_insensitive = Self::bool(input, "case_insensitive").unwrap_or(false);
        let needle = if case_insensitive {
            pattern.to_lowercase()
        } else {
            pattern
        };
        let mut hits: Vec<String> = Vec::new();
        let mut stack = vec![base.clone()];
        'walk: while let Some(dir) = stack.pop() {
            let read = match std::fs::read_dir(&dir) {
                Ok(r) => r,
                Err(_) => continue,
            };
            for entry in read.flatten() {
                let p = entry.path();
                let rels = rel_str(&p, &base);
                if p.is_dir() {
                    stack.push(p);
                } else if let Ok(bytes) = std::fs::read(&p) {
                    if bytes.contains(&0u8) {
                        continue; // skip binary files
                    }
                    let content = String::from_utf8_lossy(&bytes).to_string();
                    for (i, line) in content.lines().enumerate() {
                        let hay = if case_insensitive {
                            line.to_lowercase()
                        } else {
                            line.to_string()
                        };
                        if hay.contains(&needle) {
                            let trimmed: String = line.trim_end().chars().take(200).collect();
                            hits.push(format!("{}:{}: {}", rels, i + 1, trimmed));
                            if hits.len() >= max {
                                break 'walk;
                            }
                        }
                    }
                }
            }
        }
        let text = if hits.is_empty() {
            String::from("(no matches)")
        } else {
            hits.join("\n")
        };
        let (content, truncated) = cap_output(text.as_bytes(), self.config.output_cap_bytes);
        let mut r = ToolResult::ok(content);
        r.truncated = truncated;
        r
    }

    // ---- interpreter / vcs tools -----------------------------------------

    async fn tool_python(&self, input: &Value) -> ToolResult {
        let code = match Self::str(input, "code") {
            Some(c) if !c.trim().is_empty() => c.to_string(),
            _ => return ToolResult::failure("python: 'code' is required"),
        };
        let tmp = self.config.workspace_root.join(".swarm_tmp_run.py");
        if let Some(parent) = tmp.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Err(e) = std::fs::write(&tmp, &code) {
            return ToolResult::failure(format!("python: cannot write temp script: {e}"));
        }
        let timeout = Self::timeout_for(input, &self.config);
        let interpreter = if cfg!(windows) { "python" } else { "python3" };
        let mut cmd = tokio::process::Command::new(interpreter);
        cmd.arg(tmp.as_os_str());
        cmd.current_dir(&self.config.workspace_root);
        // A tool must never read the user's keyboard, and an abandoned call (Ctrl+C,
        // a timeout) must not leave its process running.
        cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
        let child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
                return ToolResult::failure(format!("failed to spawn python: {e}"));
            }
        };
        let res = match tokio::time::timeout(timeout, child.wait_with_output()).await {
            Ok(Ok(out)) => {
                let (content, t1) = cap_output(&out.stdout, self.config.output_cap_bytes);
                let (stderr, t2) = cap_output(&out.stderr, self.config.output_cap_bytes);
                ToolResult::command(content, stderr, out.status.code(), t1 || t2)
            }
            Ok(Err(e)) => ToolResult::failure(format!("python process error: {e}")),
            Err(_) => ToolResult::failure_with(
                format!("python timed out after {}ms", timeout.as_millis()),
                String::new(),
                None,
                false,
            ),
        };
        let _ = std::fs::remove_file(&tmp);
        res
    }

    async fn tool_git(&self, input: &Value) -> ToolResult {
        let args_val = match input.get("args") {
            Some(v) if v.is_array() => v,
            _ => return ToolResult::failure("git: 'args' must be an array of strings"),
        };
        let args: Vec<String> = args_val
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|a| a.as_str().map(|s| s.to_string()))
            .collect();
        if args.is_empty() {
            return ToolResult::failure("git: 'args' must be a non-empty array");
        }
        let timeout = Self::timeout_for(input, &self.config);
        let mut cmd = tokio::process::Command::new("git");
        cmd.args(&args);
        cmd.current_dir(&self.config.workspace_root);
        // A tool must never read the user's keyboard, and an abandoned call (Ctrl+C,
        // a timeout) must not leave its process running.
        cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
        let child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => return ToolResult::failure(format!("failed to spawn git: {e}")),
        };
        match tokio::time::timeout(timeout, child.wait_with_output()).await {
            Ok(Ok(out)) => {
                let (content, t1) = cap_output(&out.stdout, self.config.output_cap_bytes);
                let (stderr, t2) = cap_output(&out.stderr, self.config.output_cap_bytes);
                ToolResult::command(content, stderr, out.status.code(), t1 || t2)
            }
            Ok(Err(e)) => ToolResult::failure(format!("git process error: {e}")),
            Err(_) => ToolResult::failure_with(
                format!("git timed out after {}ms", timeout.as_millis()),
                String::new(),
                None,
                false,
            ),
        }
    }
}

/// Recursively collect workspace-relative paths (forward-slash separated),
/// bounded by `max` total entries.
fn collect_paths(dir: &Path, base: &Path, out: &mut Vec<String>, max: usize) {
    let read = match std::fs::read_dir(dir) {
        Ok(r) => r,
        Err(_) => return,
    };
    for entry in read.flatten() {
        let p = entry.path();
        out.push(rel_str(&p, base));
        if out.len() >= max {
            return;
        }
        if p.is_dir() {
            collect_paths(&p, base, out, max);
        }
    }
}

/// A workspace-relative path string using forward slashes, for stable model
/// output regardless of host OS.
fn rel_str(p: &Path, base: &Path) -> String {
    let rel = p.strip_prefix(base).unwrap_or(p);
    rel.to_string_lossy().replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::tempdir;

    fn executor_in(dir: &tempfile::TempDir) -> ToolExecutor {
        ToolExecutor::new(SandboxConfig::new(dir.path().to_path_buf()))
    }

    fn call(id: &str, name: &str, args: Value) -> ToolCall {
        ToolCall::new(id.to_string(), name.to_string(), args)
    }

    #[tokio::test]
    async fn write_then_read() {
        let dir = tempdir().unwrap();
        let ex = executor_in(&dir);
        let w = ex
            .execute(&call(
                "1",
                "write_file",
                json!({"path": "a.txt", "content": "hello swarm"}),
            ))
            .await;
        assert!(w.success, "write failed: {:?}", w.error);
        let r = ex
            .execute(&call("2", "read_file", json!({"path": "a.txt"})))
            .await;
        assert!(r.success, "read failed: {:?}", r.error);
        assert_eq!(r.content, "hello swarm");
    }

    #[tokio::test]
    async fn edit_replace_unique() {
        let dir = tempdir().unwrap();
        let ex = executor_in(&dir);
        ex.execute(&call(
            "1",
            "write_file",
            json!({"path": "f.txt", "content": "foo bar foo"}),
        ))
        .await;
        let e = ex
            .execute(&call(
                "2",
                "edit_file",
                json!({"path": "f.txt", "find": "foo bar", "replace": "baz"}),
            ))
            .await;
        assert!(e.success, "edit failed: {:?}", e.error);
        let r = ex
            .execute(&call("3", "read_file", json!({"path": "f.txt"})))
            .await;
        assert_eq!(r.content, "baz foo");
    }

    /// The diff the UI shows is built from what the tool captured as it ran,
    /// not from re-reading the file later.
    #[tokio::test]
    async fn file_tools_record_what_they_changed() {
        let dir = tempdir().unwrap();
        let ex = executor_in(&dir);
        let created = ex
            .execute(&call("1", "write_file", json!({"path": "n.txt", "content": "one\n"})))
            .await;
        let change = created.change.expect("write_file records a change");
        assert!(change.created, "a new file is flagged as created");
        assert_eq!(change.before, None);

        let overwritten = ex
            .execute(&call("2", "write_file", json!({"path": "n.txt", "content": "two\n"})))
            .await;
        let change = overwritten.change.expect("change");
        assert!(!change.created);
        assert_eq!(change.before.as_deref(), Some("one\n"));

        let edited = ex
            .execute(&call(
                "3",
                "edit_file",
                json!({"path": "n.txt", "find": "two", "replace": "three"}),
            ))
            .await;
        assert_eq!(edited.change.expect("change").before.as_deref(), Some("two\n"));

        let failed = ex
            .execute(&call("4", "edit_file", json!({"path": "n.txt", "find": "absent", "replace": "x"})))
            .await;
        assert!(!failed.success && failed.change.is_none(), "a failed edit changed nothing");
        let read = ex.execute(&call("5", "read_file", json!({"path": "n.txt"}))).await;
        assert!(read.change.is_none(), "reads change nothing");
    }

    #[tokio::test]
    async fn a_huge_previous_version_is_not_kept() {
        let dir = tempdir().unwrap();
        let ex = executor_in(&dir);
        std::fs::write(dir.path().join("big.txt"), "x".repeat(300 * 1024)).unwrap();
        let result = ex
            .execute(&call("1", "write_file", json!({"path": "big.txt", "content": "small"})))
            .await;
        let change = result.change.expect("change");
        assert!(!change.created);
        assert_eq!(change.before, None, "over the cap: no copy is stored");
    }

    /// A tool that reads stdin must not be able to take the user's keystrokes
    /// away from the prompt: it sees end-of-file at once. The test points fd 0
    /// at a pipe that never closes, which is what a live terminal looks like to
    /// a child that inherits it.
    #[cfg(unix)]
    #[tokio::test]
    async fn tool_processes_do_not_inherit_the_keyboard() {
        let dir = tempdir().unwrap();
        let ex = executor_in(&dir);
        let mut fds = [0i32; 2];
        // SAFETY: plain fd plumbing on descriptors this test creates; fd 0 is
        // restored before any assertion runs.
        let saved = unsafe {
            assert_eq!(libc::pipe(fds.as_mut_ptr()), 0);
            let saved = libc::dup(0);
            libc::dup2(fds[0], 0);
            saved
        };
        let started = Instant::now();
        let result = ex
            .execute(&call("1", "bash", json!({"command": "cat; echo finished", "timeout_ms": 4000})))
            .await;
        let elapsed = started.elapsed();
        unsafe {
            libc::dup2(saved, 0);
            libc::close(saved);
            libc::close(fds[0]);
            libc::close(fds[1]);
        }
        assert!(result.success, "{:?}", result.error);
        assert!(result.content.contains("finished"), "{:?}", result.content);
        assert!(elapsed < Duration::from_secs(3), "cat waited on the terminal for {elapsed:?}");
    }

    /// Interrupting a turn drops the tool future; the process it started must
    /// not outlive it.
    #[cfg(unix)]
    #[tokio::test]
    async fn dropping_a_running_tool_kills_its_process() {
        let dir = tempdir().unwrap();
        let ex = executor_in(&dir);
        let pidfile = dir.path().join("pid");
        let command = format!("echo $$ > {}; exec sleep 60", pidfile.display());
        let tool_call = call("1", "bash", json!({"command": command, "timeout_ms": 120000}));
        let running = ex.execute(&tool_call);
        let _ = tokio::time::timeout(Duration::from_millis(600), running).await;
        let pid: i32 = std::fs::read_to_string(&pidfile).unwrap().trim().parse().unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        // SAFETY: signal 0 only probes whether the process exists.
        let alive = unsafe { libc::kill(pid, 0) } == 0;
        assert!(!alive, "process {pid} survived its cancelled tool call");
    }

    #[tokio::test]
    async fn edit_ambiguous_requires_replace_all() {
        let dir = tempdir().unwrap();
        let ex = executor_in(&dir);
        ex.execute(&call(
            "1",
            "write_file",
            json!({"path": "f.txt", "content": "x x x"}),
        ))
        .await;
        let e = ex
            .execute(&call(
                "2",
                "edit_file",
                json!({"path": "f.txt", "find": "x", "replace": "y"}),
            ))
            .await;
        assert!(!e.success);
        let e2 = ex
            .execute(&call(
                "3",
                "edit_file",
                json!({"path": "f.txt", "find": "x", "replace": "y", "replace_all": true}),
            ))
            .await;
        assert!(e2.success, "edit_all failed: {:?}", e2.error);
        let r = ex
            .execute(&call("4", "read_file", json!({"path": "f.txt"})))
            .await;
        assert_eq!(r.content, "y y y");
    }

    #[tokio::test]
    async fn list_and_search() {
        let dir = tempdir().unwrap();
        let ex = executor_in(&dir);
        ex.execute(&call(
            "1",
            "write_file",
            json!({"path": "src/mod.rs", "content": "fn target() {}\nfn other() {}"}),
        ))
        .await;
        let l = ex.execute(&call("2", "list_files", json!({}))).await;
        assert!(
            l.content.contains("src/mod.rs"),
            "list should include src/mod.rs: {}",
            l.content
        );
        let s = ex
            .execute(&call("3", "search_files", json!({"pattern": "target"})))
            .await;
        assert!(s.success, "search failed: {:?}", s.error);
        assert!(
            s.content.contains("fn target()"),
            "search should find target: {}",
            s.content
        );
    }

    #[tokio::test]
    async fn bash_captures_output_and_exit() {
        let dir = tempdir().unwrap();
        let ex = executor_in(&dir);
        let ok = ex
            .execute(&call("1", "bash", json!({"command": "echo hi"})))
            .await;
        assert!(ok.success, "bash failed: {:?}", ok.error);
        assert!(ok.content.contains("hi"), "bash output: {}", ok.content);
        assert_eq!(ok.exit_code, Some(0));
        let nz = ex
            .execute(&call("2", "bash", json!({"command": "exit 3"})))
            .await;
        assert!(nz.success, "bash tool ran: {:?}", nz.error);
        assert_eq!(nz.exit_code, Some(3));
    }

    #[tokio::test]
    async fn unknown_tool_is_failure() {
        let dir = tempdir().unwrap();
        let ex = executor_in(&dir);
        let r = ex.execute(&call("1", "nope", json!({}))).await;
        assert!(!r.success);
        assert!(r.error.as_deref().unwrap().contains("unknown tool"));
    }

    fn describe(v: &ToolRequestValidation) -> String {
        match v {
            ToolRequestValidation::AutoApprove { .. } => "AutoApprove".into(),
            ToolRequestValidation::NeedsApproval { .. } => "NeedsApproval".into(),
            ToolRequestValidation::Deny { .. } => "Deny".into(),
        }
    }

    #[test]
    fn validate_denies_path_escape() {
        let dir = tempdir().unwrap();
        let ex = executor_in(&dir);
        let bad = if cfg!(windows) {
            json!({"path": "C:\\Windows\\System32"})
        } else {
            json!({"path": "/etc/passwd"})
        };
        let v = ex.validate("read_file", &bad);
        assert!(
            matches!(v, ToolRequestValidation::Deny { .. }),
            "expected Deny, got {}",
            describe(&v)
        );
        let ok = ex.validate("read_file", &json!({"path": "a.txt"}));
        assert!(
            matches!(ok, ToolRequestValidation::AutoApprove { .. }),
            "expected AutoApprove: {}",
            describe(&ok)
        );
        let unk = ex.validate("nope", &json!({}));
        assert!(
            matches!(unk, ToolRequestValidation::Deny { .. }),
            "expected Deny for unknown tool"
        );
    }
}
