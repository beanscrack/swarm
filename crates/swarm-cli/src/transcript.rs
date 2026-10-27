//! Claude Code-style transcript for a run.
//!
//! The run is event-sourced, so this is a pure renderer over [`RunEvent`]s:
//! `●` marks an action (a tool call, an assistant reply), `⎿` its result, and
//! dim text carries meta. Keeping the renderer pure means the exact bytes a
//! terminal user sees are unit-testable with colours off.
//!
//! Everything that came from a model or a tool is untrusted and goes through
//! [`sanitize`] before it can reach a terminal.

use std::collections::{BTreeSet, HashMap};
use std::fmt::Write as _;

use serde_json::Value;
use swarm_core::event::{EventKind, RunEvent};
use swarm_tools::todo::{self, Todo, TodoStatus};

pub use crate::style::{Color, ui_enabled};
use crate::style::{PREVIEW_LINES, tokens};
use crate::ui::diff::{self, DiffView};
use crate::ui::markdown;
use crate::ui::text::{sanitize, truncate};

const BULLET: &str = crate::style::bullet(); // ● assistant / tool action
const BRANCH: char = '\u{23bf}'; // ⎿ result
const CROSS: char = '\u{2717}'; // ✗ failure
const SPARK: char = '\u{273b}'; // ✻ footer flourish
const ELLIPSIS: char = '\u{2026}'; // …

/// `  ⎿  ` then the body: the result gutter.
const GUTTER: &str = "  \u{23bf}  ";
/// Continuation rows sit under the body, not under the branch.
const BELOW: &str = "     ";

/// Paints a line with a named role from [`crate::style`].
pub fn paint(colors: Color, role: &str, text: &str) -> String {
    crate::style::role(colors, role, text)
}

const MAX_ARGS_VALUE: usize = 56;
const MAX_ARGS_LINE: usize = 80;

/// One-line summary of tool arguments, e.g. `path=a.txt, content=3 lines`.
pub fn args_summary(arguments: &Value) -> String {
    let Some(map) = arguments.as_object() else {
        let text = compact(&arguments.to_string(), MAX_ARGS_VALUE);
        return if text.is_empty() { String::new() } else { text };
    };
    let mut pairs: Vec<String> = Vec::with_capacity(map.len());
    for (key, value) in map {
        let raw = raw_of(value);
        let capped = if raw.chars().count() > MAX_ARGS_VALUE {
            cap_char(&raw, MAX_ARGS_VALUE)
        } else {
            raw
        };
        pairs.push(format!("{key}={capped}"));
    }
    let mut out = pairs.join(", ");
    if out.chars().count() > MAX_ARGS_LINE {
        out = cap_char(&out, MAX_ARGS_LINE);
    }
    out
}

fn raw_of(value: &Value) -> String {
    match value {
        Value::String(s) => sanitize(s).replace('\n', " "),
        Value::Array(items) => {
            let shown = items
                .iter()
                .take(3)
                .map(|item| raw_of(item).replace('\n', " "))
                .collect::<Vec<_>>()
                .join(", ");
            if items.len() > 3 {
                format!("[{shown}{ELLIPSIS}]")
            } else {
                format!("[{shown}]")
            }
        }
        Value::Object(map) => format!("{{{}}} keys", map.len()),
        other => other.to_string(),
    }
}

/// First non-empty string among `keys` in an event payload.
fn first_of(payload: &Value, keys: &[&str]) -> String {
    keys.iter()
        .filter_map(|key| payload.get(*key).and_then(Value::as_str))
        .find(|value| !value.is_empty())
        .map(sanitize)
        .unwrap_or_default()
}

fn cap_char(text: &str, limit: usize) -> String {
    let count = text.chars().count();
    if count <= limit {
        return text.to_string();
    }
    let head: String = text.chars().take(limit).collect();
    let mut cut = String::with_capacity(limit + 3);
    let _ = write!(cut, "{}{ELLIPSIS}", head.trim_end());
    cut
}

fn compact(text: &str, limit: usize) -> String {
    let flat = sanitize(text).replace('\n', " ").trim().to_string();
    let count = flat.chars().count();
    if count <= limit {
        flat
    } else {
        format!("{}{}", flat.chars().take(limit).collect::<String>(), ELLIPSIS)
    }
}

/// Tool output as at most `limit` rows with Claude Code's `+N lines` marker
/// for what was cut. Rows are cut to `cols` cells.
fn preview(text: &str, limit: usize, cols: usize) -> Vec<String> {
    let clean = sanitize(text);
    let mut lines: Vec<&str> = clean.trim_end().lines().collect();
    while lines.last().is_some_and(|line| line.trim().is_empty()) {
        lines.pop();
    }
    let mut out = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        if index >= limit {
            out.push(format!("+{} more lines (ctrl+o to expand)", lines.len() - limit));
            break;
        }
        out.push(truncate(line, cols));
    }
    out
}

/// The header printed before a run starts.
#[derive(Debug, Clone)]
pub struct Header {
    pub run_id: String,
    pub goal: String,
    pub provider: String,
    pub model: String,
    pub workspace: String,
    pub approvals: String,
    pub agents: u32,
}

/// A file edit that did not apply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailedEdit {
    pub tool: String,
    pub path: String,
    pub error: String,
}

/// What a turn did, for its one-line footer and the honest-failure notice.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TurnStats {
    pub files_edited: BTreeSet<String>,
    pub added: usize,
    pub removed: usize,
    pub reads: usize,
    pub commands: usize,
    pub searches: usize,
    pub failed_edits: Vec<FailedEdit>,
}

impl TurnStats {
    /// `edited 2 files +18 -3`, `read 4 files`, `ran 3 commands`, at most four
    /// segments (then `+N more`), failed calls never counted.
    pub fn segments(&self) -> Vec<String> {
        let plural = |count: usize, one: &str| {
            if count == 1 { format!("{count} {one}") } else { format!("{count} {one}s") }
        };
        let mut all = Vec::new();
        if !self.files_edited.is_empty() {
            let mut text = format!("edited {}", plural(self.files_edited.len(), "file"));
            if self.added + self.removed > 0 {
                let _ = write!(text, " +{} -{}", self.added, self.removed);
            }
            all.push(text);
        }
        if self.reads > 0 {
            all.push(format!("read {}", plural(self.reads, "file")));
        }
        if self.searches > 0 {
            all.push(format!("searched {}", plural(self.searches, "time")));
        }
        if self.commands > 0 {
            all.push(format!("ran {}", plural(self.commands, "command")));
        }
        if all.len() > 4 {
            let extra = all.len() - 4;
            all.truncate(4);
            all.push(format!("+{extra} more"));
        }
        all
    }

    /// Rows warning that edits failed while the turn may still claim success
    /// (Hermes' file-mutation verifier). Empty when everything applied.
    pub fn failure_rows(&self, colors: Color) -> Vec<String> {
        if self.failed_edits.is_empty() {
            return Vec::new();
        }
        let count = self.failed_edits.len();
        let mut rows = vec![paint(
            colors,
            "warn",
            &format!(
                "  \u{26a0} {count} file edit{} did not apply this turn:",
                if count == 1 { "" } else { "s" }
            ),
        )];
        for edit in self.failed_edits.iter().take(5) {
            rows.push(paint(
                colors,
                "subtle",
                &format!("    \u{2022} {} \u{2014} {}: {}", edit.path, edit.tool, compact(&edit.error, 80)),
            ));
        }
        if count > 5 {
            rows.push(paint(colors, "subtle", &format!("    +{} more", count - 5)));
        }
        rows
    }
}

/// Renders run events as transcript lines.
pub struct Transcript {
    colors: Color,
    verbose: bool,
    /// Show only the assistant's words, failures and denials.
    focus: bool,
    width: usize,
    agents: HashMap<String, String>,
    tasks: HashMap<String, String>,
    /// The current step's usage row, printed after the step's tool rows.
    pending: Vec<String>,
    stats: TurnStats,
    /// The last assistant text shown, so the run's `result` is not a repeat.
    last_text: String,
}

impl Transcript {
    pub fn new(colors: Color, verbose: bool) -> Self {
        Self {
            colors,
            verbose,
            focus: false,
            width: 100,
            agents: HashMap::new(),
            tasks: HashMap::new(),
            pending: Vec::new(),
            stats: TurnStats::default(),
            last_text: String::new(),
        }
    }

    /// Focus view (`/focus`): the tool activity that went well is not printed.
    pub fn set_focus(&mut self, focus: bool) {
        self.focus = focus;
    }

    /// The terminal width rows are wrapped and diffs are cut to.
    pub fn set_width(&mut self, width: usize) {
        self.width = width.clamp(20, 200);
    }

    pub fn stats(&self) -> &TurnStats {
        &self.stats
    }

    fn meta(&self, text: String) -> String {
        paint(self.colors, "meta", &text)
    }

    /// Short, stable labels so multi-agent runs stay readable.
    fn label(names: &mut HashMap<String, String>, kind: &str, id: &str) -> String {
        if let Some(known) = names.get(id) {
            return known.clone();
        }
        let label = format!("{kind}{}", names.len() + 1);
        names.insert(id.to_string(), label.clone());
        label
    }

    /// ` [a1 t1]` for events from a named actor. Registered even when it is not
    /// shown, so labels stay stable once a second agent appears.
    fn actor(&mut self, event: &RunEvent) -> String {
        let mut parts = Vec::new();
        if let Some(agent) = &event.agent_id {
            parts.push(Self::label(&mut self.agents, "a", agent.as_str()));
        }
        if let Some(task) = &event.task_id {
            parts.push(Self::label(&mut self.tasks, "t", task.as_str()));
        }
        // A lone agent needs no name tag; once there are several they do.
        if parts.is_empty() || (self.agents.len() <= 1 && self.tasks.len() <= 1 && !self.verbose) {
            String::new()
        } else {
            format!(" [{}]", parts.join(" "))
        }
    }

    pub fn banner(&self, header: &Header) -> Vec<String> {
        let dim = |label: &str| self.meta(label.to_string());
        let agents = format!("{} agent{}", header.agents, if header.agents == 1 { "" } else { "s" });
        vec![
            self.paint_action(&format!("{BULLET} swarm")),
            format!(
                "  {} {} \u{00b7} model {} \u{00b7} {} \u{00b7} {} {}",
                dim("provider"),
                header.provider,
                header.model,
                agents,
                dim("approvals"),
                header.approvals,
            ),
            format!("  {} {}", dim("goal"), compact(&header.goal, 120)),
            format!("  {} {} \u{00b7} {} {}", dim("workspace"), header.workspace, dim("run"), header.run_id),
            String::new(),
        ]
    }

    fn paint_action(&self, text: &str) -> String {
        paint(self.colors, "action", text)
    }

    /// An action row: the bullet, the tool's name in bold, its target after.
    fn action(&self, actor: &str, name: &str, detail: &str) -> String {
        let dot = paint(self.colors, "bullet", BULLET);
        let actor = if actor.is_empty() { String::new() } else { paint(self.colors, "subtle", actor) };
        let name = paint(self.colors, "action", name);
        if detail.is_empty() {
            format!("{dot}{actor} {name}")
        } else {
            format!("{dot}{actor} {name}{}", paint(self.colors, "text", &format!("({detail})")))
        }
    }

    /// Transcript lines for one event; empty means "not shown at this level".
    ///
    /// The log records an inference before the tool calls it asked for, so the
    /// step's usage row (verbose only) is held back and printed after those rows.
    pub fn lines(&mut self, event: &RunEvent) -> Vec<String> {
        let mut out = match event.kind {
            EventKind::ToolRequested | EventKind::ToolResult | EventKind::Message => Vec::new(),
            _ => std::mem::take(&mut self.pending),
        };
        out.extend(self.event_lines(event));
        if event.kind == EventKind::Message {
            out.extend(std::mem::take(&mut self.pending));
        }
        out
    }

    /// Rows the renderer is still holding. Call once the stream is exhausted.
    pub fn drain(&mut self) -> Vec<String> {
        std::mem::take(&mut self.pending)
    }

    /// The gutter-prefixed result rows: first on the branch, the rest below it.
    fn result(&self, role: &str, rows: Vec<String>) -> Vec<String> {
        rows.into_iter()
            .enumerate()
            .map(|(index, row)| {
                if index == 0 {
                    format!("{}{}", paint(self.colors, "subtle", GUTTER), paint(self.colors, role, &row))
                } else {
                    format!("{BELOW}{}", paint(self.colors, role, &row))
                }
            })
            .collect()
    }

    fn diff_rows(&self, view: DiffView) -> Vec<String> {
        view.lines.into_iter().map(|line| format!("{BELOW}{line}")).collect()
    }

    fn body_width(&self) -> usize {
        self.width.saturating_sub(BELOW.len() + 1).max(20)
    }

    fn event_lines(&mut self, event: &RunEvent) -> Vec<String> {
        let actor = self.actor(event);
        let payload = &event.payload;
        let text_of = |key: &str| sanitize(payload.get(key).and_then(Value::as_str).unwrap_or(""));
        match event.kind {
            EventKind::InferenceStarted => Vec::new(),
            EventKind::InferenceCompleted => {
                let mut out = Vec::new();
                let calls: Vec<String> = payload
                    .get("tool_calls")
                    .and_then(Value::as_array)
                    .map(|items| items.iter().filter_map(Value::as_str).map(sanitize).collect())
                    .unwrap_or_default();
                let text = text_of("text");
                if calls.is_empty() {
                    if !text.trim().is_empty() {
                        out.extend(self.assistant_text(&actor, &text));
                        self.last_text = text.trim().to_string();
                    }
                } else if self.verbose {
                    let verb = if calls.len() == 1 {
                        format!("asks for {}", calls[0])
                    } else {
                        format!("asks for {} tools", calls.len())
                    };
                    out.push(self.action(&actor, &verb, ""));
                    let summary = compact(&text, 100);
                    if !summary.is_empty() {
                        out.push(format!("  {}", paint(self.colors, "subtle", &summary)));
                    }
                } else {
                    // A short remark before a tool call is worth showing as the
                    // assistant's own words; the call itself follows.
                    let remark = text.trim();
                    if !self.focus && !remark.is_empty() && !remark.starts_with("```") {
                        out.extend(self.assistant_text(&actor, remark));
                    }
                }
                if self.verbose {
                    let step = payload.get("step").and_then(Value::as_u64).unwrap_or(0);
                    let latency = payload.get("latency_ms").and_then(Value::as_u64).unwrap_or(0);
                    let model = text_of("model");
                    let num = |key: &str| payload.get(key).and_then(Value::as_u64).unwrap_or(0);
                    let mut meta = format!(
                        "  {BRANCH} step {step} \u{00b7} {:.1}s \u{00b7} {} \u{2192} {} tokens",
                        latency as f64 / 1000.0,
                        tokens(num("input_tokens")),
                        tokens(num("output_tokens")),
                    );
                    if !model.is_empty() {
                        let _ = write!(meta, " \u{00b7} {model}");
                    }
                    self.pending = vec![self.meta(meta)];
                }
                out
            }
            EventKind::InferenceFailed => vec![paint(
                self.colors,
                "fail",
                &format!("{CROSS}{actor} {}", first_of(payload, &["error", "message"])),
            )],
            EventKind::ToolRequested if self.focus => Vec::new(),
            EventKind::ToolRequested => {
                let name = text_of("name");
                let args = payload.get("arguments").cloned().unwrap_or(Value::Null);
                let (display, detail) = tool_heading(&name, &args, self.verbose);
                vec![self.action(&actor, &display, &detail)]
            }
            EventKind::ToolResult if self.focus => {
                // Still rendered, so the turn's file and line counts stay right.
                let rows = self.tool_result(payload);
                if payload.get("success").and_then(Value::as_bool).unwrap_or(false) {
                    return Vec::new();
                }
                // A failure is shown with the call that failed, since the call
                // itself was held back.
                let name = text_of("name");
                let args = payload.get("arguments").cloned().unwrap_or(Value::Null);
                let (display, detail) = tool_heading(&name, &args, false);
                let mut out = vec![self.action(&actor, &display, &detail)];
                out.extend(rows);
                out
            }
            EventKind::ToolResult => self.tool_result(payload),
            // The permission dialog carries the request; the transcript only
            // records the outcome, below.
            EventKind::ApprovalRequested => Vec::new(),
            EventKind::ApprovalResolved => {
                let approved = payload.get("approved").and_then(Value::as_bool).unwrap_or(false);
                if self.focus && approved {
                    return Vec::new();
                }
                vec![format!(
                    "{}{}",
                    paint(self.colors, "subtle", GUTTER),
                    paint(
                        self.colors,
                        if approved { "ok" } else { "warn" },
                        if approved { "approved" } else { "denied" }
                    )
                )]
            }
            EventKind::Message => {
                let mut out = Vec::new();
                if let Some(summary) = payload.get("summary").and_then(Value::as_str) {
                    let summary = sanitize(summary);
                    // A single agent's final reply was already shown as it
                    // arrived; the run's `result` only adds something when it
                    // is a different text (a plan, several workers' output).
                    if !summary.trim().is_empty() && summary.trim() != self.last_text {
                        out.push(self.action(&actor, "Result", ""));
                        out.extend(self.markdown_rows(&summary, "  "));
                    }
                }
                if let Some(error) = payload.get("error").and_then(Value::as_str) {
                    out.push(paint(self.colors, "fail", &format!("{CROSS}{actor} {}", sanitize(error))));
                }
                out
            }
            EventKind::TaskStateChanged => {
                let state = text_of("state");
                if state == "failed" || (self.verbose && state != "ready") {
                    vec![self.meta(format!("  {BRANCH} task {state}"))]
                } else {
                    Vec::new()
                }
            }
            EventKind::RunStateChanged => {
                let state = first_of(payload, &["status", "state"]);
                if matches!(state.as_str(), "failed" | "cancelled") || (self.verbose && !state.is_empty()) {
                    vec![self.meta(format!("  {BRANCH} run {state}"))]
                } else {
                    Vec::new()
                }
            }
            EventKind::AgentStateChanged if self.verbose => {
                let state = first_of(payload, &["to", "state"]);
                if state.is_empty() {
                    Vec::new()
                } else {
                    vec![self.meta(format!("  {BRANCH}{actor} agent {state}"))]
                }
            }
            EventKind::CheckpointSaved if self.verbose => {
                let version = payload.get("version").and_then(Value::as_u64);
                let steps = payload.get("steps").and_then(Value::as_u64);
                match (version, steps) {
                    (Some(version), Some(steps)) => {
                        vec![self.meta(format!("  {BRANCH} checkpoint v{version} \u{00b7} {steps} steps"))]
                    }
                    _ => vec![self.meta("  \u{00b7} checkpoint".to_string())],
                }
            }
            _ => Vec::new(),
        }
    }

    /// `● ` then the reply as markdown: the first row shares the bullet's line,
    /// the rest hang under it.
    fn assistant_text(&self, actor: &str, text: &str) -> Vec<String> {
        let rows = self.markdown_rows(text, "  ");
        let dot = paint(self.colors, "bullet", BULLET);
        let actor = if actor.is_empty() { String::new() } else { paint(self.colors, "subtle", actor) };
        let mut out = Vec::with_capacity(rows.len());
        let mut rows = rows.into_iter();
        match rows.next() {
            Some(first) => out.push(format!("{dot}{actor} {}", first.trim_start_matches(' '))),
            None => out.push(format!("{dot}{actor}")),
        }
        out.extend(rows);
        out
    }

    fn markdown_rows(&self, text: &str, indent: &str) -> Vec<String> {
        let limit = if self.verbose { 400 } else { 60 };
        let body_width = self.width.saturating_sub(indent.len()).max(10);
        let rendered = markdown::render(text, body_width, self.colors);
        let total = rendered.len();
        let mut rows: Vec<String> = rendered.into_iter().take(limit).map(|row| format!("{indent}{row}")).collect();
        if total > limit {
            rows.push(paint(
                self.colors,
                "subtle",
                &format!("{indent}+{} more lines (ctrl+o to expand)", total - limit),
            ));
        }
        rows
    }

    fn tool_result(&mut self, payload: &Value) -> Vec<String> {
        let name = sanitize(payload.get("name").and_then(Value::as_str).unwrap_or(""));
        let args = payload.get("arguments").cloned().unwrap_or(Value::Null);
        let success = payload.get("success").and_then(Value::as_bool).unwrap_or(false);
        let output = payload.get("output").and_then(Value::as_str).unwrap_or("");
        let truncated = payload.get("truncated").and_then(Value::as_bool).unwrap_or(false);
        let path = sanitize(args.get("path").and_then(Value::as_str).unwrap_or(""));
        let is_edit = matches!(name.as_str(), "write_file" | "edit_file");

        if !success {
            let first = output.lines().find(|line| !line.trim().is_empty()).unwrap_or("failed");
            if is_edit {
                self.stats.failed_edits.push(FailedEdit {
                    tool: name.clone(),
                    path: if path.is_empty() { "(no path)".into() } else { path.clone() },
                    error: first.to_string(),
                });
            }
            let mut rows = vec![format!("{CROSS} {}", truncate(&sanitize(first), self.body_width()))];
            rows.extend(preview(output, 6, self.body_width()).into_iter().skip(1));
            return self.result("fail", rows);
        }

        let cols = self.body_width();
        let limit = if self.verbose { 120 } else { PREVIEW_LINES };
        match name.as_str() {
            "write_file" | "edit_file" => {
                let change = payload.get("change").filter(|value| !value.is_null());
                let before = change.and_then(|change| change.get("before")).and_then(Value::as_str);
                let created = change
                    .and_then(|change| change.get("created"))
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let rows_cap = if self.verbose { 80 } else { 14 };
                let (view, after) = if name == "write_file" {
                    let content = args.get("content").and_then(Value::as_str).unwrap_or("");
                    (diff::file_diff(before, content, cols, self.colors, rows_cap), None::<String>)
                } else {
                    let find = args.get("find").and_then(Value::as_str).unwrap_or("");
                    let replace = args.get("replace").and_then(Value::as_str).unwrap_or("");
                    let line = before.and_then(|text| diff::line_of(text, find)).unwrap_or(0);
                    (diff::snippet_diff(find, replace, line, cols, self.colors, rows_cap), None)
                };
                let _ = after;
                self.stats.files_edited.insert(if path.is_empty() { "(file)".into() } else { path.clone() });
                self.stats.added += view.summary.added;
                self.stats.removed += view.summary.removed;
                let headline = if name == "write_file" && (created || before.is_none()) {
                    let lines = args.get("content").and_then(Value::as_str).unwrap_or("").lines().count();
                    format!("Wrote {lines} line{} to {path}", if lines == 1 { "" } else { "s" })
                } else if view.lines.is_empty() {
                    format!("{path}: no changes")
                } else {
                    format!("Updated {path} with {}", change_phrase(&view))
                };
                let mut out = self.result("result", vec![headline]);
                out.extend(self.diff_rows(view));
                out
            }
            "read_file" => {
                self.stats.reads += 1;
                let lines = output.lines().count();
                let capped = output.chars().count() >= 1190 || truncated;
                self.result(
                    "result",
                    vec![format!("Read {lines}{} line{}", if capped { "+" } else { "" }, if lines == 1 && !capped { "" } else { "s" })],
                )
            }
            "todo_write" => {
                let Ok(todos) = todo::parse(&args) else {
                    return self.result("result", vec!["Updated the todo list".to_string()]);
                };
                if todos.is_empty() {
                    return self.result("subtle", vec!["Todo list cleared".to_string()]);
                }
                todo_rows(self.colors, &todos)
                    .into_iter()
                    .enumerate()
                    .map(|(index, row)| {
                        let lead = if index == 0 { paint(self.colors, "subtle", GUTTER) } else { BELOW.to_string() };
                        format!("{lead}{row}")
                    })
                    .collect()
            }
            "list_files" => {
                self.stats.searches += 1;
                let entries = output.lines().filter(|line| !line.trim().is_empty()).count();
                self.result("result", vec![format!("Listed {entries}{} path{}", if truncated { "+" } else { "" }, if entries == 1 { "" } else { "s" })])
            }
            "search_files" => {
                self.stats.searches += 1;
                let hits = output.lines().filter(|line| !line.trim().is_empty()).count();
                let mut rows = vec![if hits == 0 {
                    "No matches".to_string()
                } else {
                    format!("Found {hits}{} match{}", if truncated { "+" } else { "" }, if hits == 1 { "" } else { "es" })
                }];
                if self.verbose {
                    rows.extend(preview(output, limit, cols));
                }
                self.result("result", rows)
            }
            "bash" | "run_tests" | "python" | "git" => {
                self.stats.commands += 1;
                let mut rows = preview(output, limit, cols);
                // A non-zero exit is a successful *execution* but the user
                // needs to see it failed.
                let failed_exit = output
                    .lines()
                    .rev()
                    .find_map(|line| line.trim().strip_prefix("[exit code ").and_then(|rest| rest.strip_suffix(']')))
                    .and_then(|code| code.parse::<i32>().ok())
                    .filter(|code| *code != 0);
                if rows.is_empty() {
                    rows.push("(no output)".to_string());
                }
                match failed_exit {
                    Some(code) => {
                        let mut all = vec![format!("{CROSS} exit code {code}")];
                        all.extend(rows.into_iter().filter(|row| !row.trim_start().starts_with("[exit code")));
                        self.result("fail", all)
                    }
                    None => self.result("result", rows),
                }
            }
            _ => {
                let mut rows = preview(output, limit, cols);
                if rows.is_empty() {
                    rows.push("(empty)".to_string());
                }
                if truncated {
                    let last = rows.len() - 1;
                    rows[last] = format!("{} {ELLIPSIS}", rows[last]);
                }
                self.result("result", rows)
            }
        }
    }
}

/// The checklist, one row per item: done items dim, the one in progress
/// highlighted, the rest plain.
pub fn todo_rows(colors: Color, todos: &[Todo]) -> Vec<String> {
    todos
        .iter()
        .map(|item| match item.status {
            TodoStatus::Completed => paint(colors, "subtle", &format!("\u{2612} {}", item.content)),
            TodoStatus::InProgress => paint(colors, "action", &format!("\u{25d0} {}", item.doing())),
            TodoStatus::Pending => format!("\u{2610} {}", item.content),
        })
        .collect()
}

/// `3 additions and 1 removal`, as Claude Code words an update.
fn change_phrase(view: &DiffView) -> String {
    let count = |n: usize, one: &str, many: &str| {
        if n == 1 { format!("{n} {one}") } else { format!("{n} {many}") }
    };
    match (view.summary.added, view.summary.removed) {
        (0, 0) => "no changes".to_string(),
        (a, 0) => count(a, "addition", "additions"),
        (0, r) => count(r, "removal", "removals"),
        (a, r) => format!("{} and {}", count(a, "addition", "additions"), count(r, "removal", "removals")),
    }
}

/// How a tool call is headed: Claude Code's names (`Bash`, `Update`, `Read`),
/// and the one argument that says what it is about. Verbose keeps swarm's own
/// tool name and every argument.
fn tool_heading(name: &str, args: &Value, verbose: bool) -> (String, String) {
    let text = |key: &str| sanitize(args.get(key).and_then(Value::as_str).unwrap_or(""));
    let first_line = |value: String| {
        let line = value.lines().next().unwrap_or("").to_string();
        compact(&line, 90)
    };
    if verbose {
        return (name.to_string(), args_summary(args));
    }
    match name {
        "bash" => ("Bash".into(), first_line(text("command"))),
        "run_tests" => {
            let command = text("command");
            ("Test".into(), if command.is_empty() { "cargo test".into() } else { first_line(command) })
        }
        "python" => ("Python".into(), first_line(text("code"))),
        "git" => {
            let parts: Vec<String> = args
                .get("args")
                .and_then(Value::as_array)
                .map(|items| items.iter().filter_map(Value::as_str).map(sanitize).collect())
                .unwrap_or_default();
            ("Git".into(), compact(&parts.join(" "), 90))
        }
        "read_file" => ("Read".into(), text("path")),
        "write_file" => ("Write".into(), text("path")),
        "edit_file" => ("Update".into(), text("path")),
        "list_files" => ("List".into(), {
            let path = text("path");
            if path.is_empty() { ".".into() } else { path }
        }),
        "search_files" => ("Search".into(), format!("\"{}\"", compact(&text("pattern"), 60))),
        "create_directory" => ("Mkdir".into(), text("path")),
        "todo_write" => ("Update Todos".into(), String::new()),
        other => (other.to_string(), args_summary(args)),
    }
}

/// The one-line summary `run` prints after the transcript.
pub fn footer(status: &str, seconds: f64, tasks: u64, inferences: u64, tokens: u64, tools: u64) -> String {
    format!(
        "{SPARK} {status} in {seconds:.1}s \u{00b7} {tasks} task{} \u{00b7} {inferences} inference{} \u{00b7} {tools} tool call{} \u{00b7} {tokens} tokens",
        plural(tasks),
        plural(inferences),
        plural(tools)
    )
}

fn plural(count: u64) -> &'static str {
    if count == 1 { "" } else { "s" }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use swarm_core::RunId;
    use swarm_core::event::RunEvent;

    fn event(kind: EventKind, payload: Value) -> RunEvent {
        RunEvent::new(kind, RunId::new(), payload)
    }

    fn render(kind: EventKind, payload: Value) -> Vec<String> {
        let mut transcript = Transcript::new(Color::Plain, false);
        let mut lines = transcript.lines(&event(kind, payload));
        lines.extend(transcript.drain());
        lines
    }

    fn verbose(kind: EventKind, payload: Value) -> Vec<String> {
        let mut transcript = Transcript::new(Color::Plain, true);
        let mut lines = transcript.lines(&event(kind, payload));
        lines.extend(transcript.drain());
        lines
    }

    #[test]
    fn a_reply_is_a_bullet_with_markdown_hanging_under_it() {
        let lines = render(
            EventKind::InferenceCompleted,
            json!({"step": 1, "model": "m", "text": "I will create the file.\n\nSecond **paragraph**.", "tool_calls": []}),
        );
        assert_eq!(lines[0], format!("{BULLET} I will create the file."));
        assert!(lines.iter().any(|line| line == "  Second paragraph."), "{lines:?}");
        assert!(!lines.iter().any(|line| line.contains("step 1")), "usage rows are verbose-only: {lines:?}");
    }

    #[test]
    fn verbose_adds_the_step_usage_row() {
        let lines = verbose(
            EventKind::InferenceCompleted,
            json!({"step": 1, "model": "gpt-5.1", "input_tokens": 120, "output_tokens": 30,
                   "latency_ms": 1234, "text": "hi", "tool_calls": []}),
        );
        let usage = lines.last().unwrap();
        assert!(usage.contains("step 1") && usage.contains("1.2s"), "{usage}");
        assert!(usage.contains("120 \u{2192} 30 tokens") && usage.contains("gpt-5.1"), "{usage}");
    }

    #[test]
    fn tool_calls_use_claude_codes_names() {
        let mut transcript = Transcript::new(Color::Plain, false);
        let mut head = |name: &str, args: Value| {
            transcript
                .lines(&event(EventKind::ToolRequested, json!({"call_id": "c", "name": name, "arguments": args})))
                .remove(0)
        };
        assert_eq!(head("bash", json!({"command": "cargo test\nsecond line"})), format!("{BULLET} Bash(cargo test)"));
        assert_eq!(head("read_file", json!({"path": "src/lib.rs"})), format!("{BULLET} Read(src/lib.rs)"));
        assert_eq!(head("write_file", json!({"path": "a.txt", "content": "x"})), format!("{BULLET} Write(a.txt)"));
        assert_eq!(head("edit_file", json!({"path": "a.txt", "find": "a"})), format!("{BULLET} Update(a.txt)"));
        assert_eq!(head("list_files", json!({})), format!("{BULLET} List(.)"));
        assert_eq!(head("search_files", json!({"pattern": "TODO"})), format!("{BULLET} Search(\"TODO\")"));
        assert_eq!(head("git", json!({"args": ["status", "-s"]})), format!("{BULLET} Git(status -s)"));
        assert_eq!(head("mystery", json!({"x": 1})), format!("{BULLET} mystery(x=1)"));
    }

    #[test]
    fn a_todo_update_draws_the_checklist() {
        let args = json!({"todos": [
            {"content": "Read the code", "status": "completed"},
            {"content": "Fix the parser", "status": "in_progress", "activeForm": "Fixing the parser"},
            {"content": "Run the tests", "status": "pending"},
        ]});
        let heading = render(EventKind::ToolRequested, json!({"name": "todo_write", "arguments": args}));
        assert_eq!(heading[0], format!("{BULLET} Update Todos"));
        let rows = render(EventKind::ToolResult, json!({"name": "todo_write", "success": true, "arguments": args, "output": "ok"}));
        assert_eq!(rows.len(), 3);
        assert!(rows[0].starts_with(GUTTER) && rows[0].contains("\u{2612} Read the code"), "{rows:?}");
        assert!(rows[1].starts_with(BELOW) && rows[1].contains("\u{25d0} Fixing the parser"), "in progress shows its active form: {rows:?}");
        assert!(rows[2].contains("\u{2610} Run the tests"), "{rows:?}");
        let cleared = render(
            EventKind::ToolResult,
            json!({"name": "todo_write", "success": true, "arguments": {"todos": []}, "output": "ok"}),
        );
        assert!(cleared[0].contains("Todo list cleared"), "{cleared:?}");
        let bad = render(
            EventKind::ToolResult,
            json!({"name": "todo_write", "success": false, "arguments": {"todos": "x"}, "output": "todo_write: 'todos' must be an array"}),
        );
        assert!(bad[0].contains("must be an array"), "a rejected update says why: {bad:?}");
    }

    #[test]
    fn focus_hides_tool_activity_that_went_well_but_not_what_failed() {
        let mut transcript = Transcript::new(Color::Plain, false);
        transcript.set_focus(true);
        let mut feed = |kind: EventKind, payload: Value| transcript.lines(&event(kind, payload)).join("\n");
        assert_eq!(feed(EventKind::ToolRequested, json!({"name": "bash", "arguments": {"command": "ls"}})), "");
        assert_eq!(
            feed(EventKind::ToolResult, json!({"name": "bash", "success": true, "arguments": {"command": "ls"}, "output": "a.txt"})),
            "",
            "a call that worked leaves no row",
        );
        assert_eq!(feed(EventKind::ApprovalResolved, json!({"approved": true})), "");
        let denied = feed(EventKind::ApprovalResolved, json!({"approved": false}));
        assert!(denied.contains("denied"), "a denial is always shown: {denied}");
        let failed = feed(
            EventKind::ToolResult,
            json!({"name": "bash", "success": false, "arguments": {"command": "make"}, "output": "make: *** no rule"}),
        );
        assert!(failed.contains("Bash(make)") && failed.contains("no rule"), "a failure names the call: {failed}");
        let said = feed(EventKind::InferenceCompleted, json!({"text": "All done.", "tool_calls": []}));
        assert!(said.contains("All done."), "the answer is the point: {said}");
        let remark = feed(EventKind::InferenceCompleted, json!({"text": "Let me look around.", "tool_calls": ["bash"]}));
        assert_eq!(remark, "", "chatter before a tool call is hidden too");
    }

    #[test]
    fn focus_still_counts_the_edits_it_does_not_show() {
        let mut transcript = Transcript::new(Color::Plain, false);
        transcript.set_focus(true);
        let rows = transcript.lines(&event(
            EventKind::ToolResult,
            json!({"name": "write_file", "success": true, "arguments": {"path": "a.txt", "content": "x\n"},
                   "output": "ok", "change": {"before": null, "created": true}}),
        ));
        assert!(rows.is_empty());
        assert_eq!(transcript.stats().files_edited.len(), 1, "the footer's file count is unaffected");
    }

    #[test]
    fn verbose_keeps_the_raw_tool_name_and_arguments() {
        let lines = verbose(
            EventKind::ToolRequested,
            json!({"call_id":"c1","name":"write_file","arguments":{"path":"hello.txt","content":"a\nb\nc"}}),
        );
        assert_eq!(lines[0], format!("{BULLET} write_file(content=a b c, path=hello.txt)"));
    }

    #[test]
    fn an_edit_result_shows_the_update_summary_and_a_numbered_diff() {
        let mut transcript = Transcript::new(Color::Plain, false);
        let lines = transcript.lines(&event(
            EventKind::ToolResult,
            json!({"call_id":"c","name":"edit_file","success":true,"truncated":false,
                   "arguments":{"path":"app.py","find":"    return 1","replace":"    return 2"},
                   "output":"edited app.py (1 replacement(s))",
                   "change":{"before":"import os\n\ndef main():\n    return 1\n","created":false}}),
        ));
        assert_eq!(lines[0], format!("  {BRANCH}  Updated app.py with 1 addition and 1 removal"));
        assert_eq!(lines[1], "      4 -     return 1");
        assert_eq!(lines[2], "      4 +     return 2");
        let stats = transcript.stats();
        assert_eq!((stats.added, stats.removed), (1, 1));
        assert!(stats.files_edited.contains("app.py"));
    }

    #[test]
    fn a_new_file_says_wrote_n_lines() {
        let lines = render(
            EventKind::ToolResult,
            json!({"call_id":"c","name":"write_file","success":true,"truncated":false,
                   "arguments":{"path":"hello.txt","content":"a\nb\nc\n"},"output":"wrote 6 byte(s)",
                   "change":{"before":null,"created":true}}),
        );
        assert_eq!(lines[0], format!("  {BRANCH}  Wrote 3 lines to hello.txt"));
        assert!(lines[1].contains("+ a"), "{lines:?}");
    }

    #[test]
    fn an_overwrite_is_an_update_with_a_diff() {
        let lines = render(
            EventKind::ToolResult,
            json!({"call_id":"c","name":"write_file","success":true,"truncated":false,
                   "arguments":{"path":"f.txt","content":"one\nTWO\nthree\n"},"output":"wrote",
                   "change":{"before":"one\ntwo\nthree\n","created":false}}),
        );
        assert_eq!(lines[0], format!("  {BRANCH}  Updated f.txt with 1 addition and 1 removal"));
    }

    #[test]
    fn an_edit_without_a_recorded_change_still_renders() {
        let lines = render(
            EventKind::ToolResult,
            json!({"call_id":"c","name":"edit_file","success":true,"truncated":false,
                   "arguments":{"path":"a.txt","find":"x","replace":"y"},"output":"edited"}),
        );
        assert!(lines[0].contains("Updated a.txt"), "{lines:?}");
    }

    #[test]
    fn command_output_is_previewed_with_a_more_marker() {
        let output: String = (0..30).map(|n| format!("line {n}\n")).collect();
        let lines = render(
            EventKind::ToolResult,
            json!({"call_id":"c","name":"bash","success":true,"truncated":false,"arguments":{"command":"seq"},"output":output}),
        );
        assert_eq!(lines[0], format!("  {BRANCH}  line 0"));
        assert_eq!(lines[1], "     line 1");
        assert_eq!(lines.len(), PREVIEW_LINES + 1);
        assert_eq!(lines.last().unwrap(), "     +22 more lines (ctrl+o to expand)");
    }

    #[test]
    fn a_nonzero_exit_reads_as_a_failure() {
        let lines = render(
            EventKind::ToolResult,
            json!({"call_id":"c","name":"bash","success":true,"truncated":false,"arguments":{"command":"false"},
                   "output":"boom\n\n[exit code 2]"}),
        );
        assert_eq!(lines[0], format!("  {BRANCH}  {CROSS} exit code 2"));
        assert!(lines.iter().any(|line| line.contains("boom")), "{lines:?}");
    }

    #[test]
    fn read_list_and_search_results_are_summarised() {
        let read = render(
            EventKind::ToolResult,
            json!({"call_id":"c","name":"read_file","success":true,"truncated":false,"arguments":{"path":"a"},"output":"a\nb\nc"}),
        );
        assert_eq!(read[0], format!("  {BRANCH}  Read 3 lines"));
        let list = render(
            EventKind::ToolResult,
            json!({"call_id":"c","name":"list_files","success":true,"truncated":false,"arguments":{},"output":"a\nb"}),
        );
        assert_eq!(list[0], format!("  {BRANCH}  Listed 2 paths"));
        let none = render(
            EventKind::ToolResult,
            json!({"call_id":"c","name":"search_files","success":true,"truncated":false,"arguments":{},"output":""}),
        );
        assert_eq!(none[0], format!("  {BRANCH}  No matches"));
        let hits = render(
            EventKind::ToolResult,
            json!({"call_id":"c","name":"search_files","success":true,"truncated":false,"arguments":{},"output":"a:1: x\nb:2: y"}),
        );
        assert_eq!(hits[0], format!("  {BRANCH}  Found 2 matches"));
    }

    #[test]
    fn failed_tools_show_the_error_and_failed_edits_are_remembered() {
        let mut transcript = Transcript::new(Color::Plain, false);
        let lines = transcript.lines(&event(
            EventKind::ToolResult,
            json!({"call_id":"c","name":"edit_file","success":false,"truncated":false,
                   "arguments":{"path":"a.py","find":"zzz","replace":"y"},
                   "output":"tool 'edit_file' failed: edit_file: 'find' not found in a.py"}),
        ));
        assert!(lines[0].starts_with(&format!("  {BRANCH}  {CROSS} tool 'edit_file' failed")), "{lines:?}");
        assert_eq!(transcript.stats().failed_edits.len(), 1);
        assert!(transcript.stats().files_edited.is_empty(), "a failed edit edited nothing");
        let rows = transcript.stats().failure_rows(Color::Plain);
        assert!(rows[0].contains("1 file edit did not apply"), "{rows:?}");
        assert!(rows[1].contains("a.py") && rows[1].contains("edit_file"), "{rows:?}");
    }

    #[test]
    fn turn_segments_summarise_the_work() {
        let mut stats = TurnStats::default();
        assert!(stats.segments().is_empty());
        stats.files_edited.extend(["a".to_string(), "b".to_string()]);
        stats.added = 18;
        stats.removed = 3;
        stats.reads = 4;
        stats.commands = 1;
        stats.searches = 2;
        assert_eq!(
            stats.segments(),
            vec!["edited 2 files +18 -3", "read 4 files", "searched 2 times", "ran 1 command"]
        );
        stats.reads = 1;
        assert!(stats.segments().contains(&"read 1 file".to_string()));
    }

    #[test]
    fn at_most_four_segments_then_a_remainder() {
        let mut stats = TurnStats::default();
        stats.files_edited.insert("a".into());
        stats.reads = 1;
        stats.searches = 1;
        stats.commands = 1;
        assert_eq!(stats.segments().len(), 4);
    }

    #[test]
    fn the_run_result_is_not_a_repeat_of_the_last_reply() {
        let mut transcript = Transcript::new(Color::Plain, false);
        let reply = transcript.lines(&event(
            EventKind::InferenceCompleted,
            json!({"step": 1, "text": "All done.", "tool_calls": []}),
        ));
        assert_eq!(reply[0], format!("{BULLET} All done."));
        let result = transcript.lines(&event(EventKind::Message, json!({"summary": "All done."})));
        assert!(result.is_empty(), "the same text twice is noise: {result:?}");
        let other = transcript.lines(&event(EventKind::Message, json!({"summary": "Plan: 1. a 2. b"})));
        assert!(other[0].contains("Result"), "{other:?}");
    }

    #[test]
    fn a_lone_agent_needs_no_label_but_several_do() {
        let mut transcript = Transcript::new(Color::Plain, false);
        let call = |agent| {
            event(EventKind::ToolRequested, json!({"name":"bash","arguments":{"command":"ls"}}))
                .with_agent(agent)
                .with_task(swarm_core::TaskId::new())
        };
        let a = swarm_core::AgentId::new();
        let b = swarm_core::AgentId::new();
        let solo = transcript.lines(&call(a.clone())).remove(0);
        assert_eq!(solo, format!("{BULLET} Bash(ls)"));
        // A second task appears: now the rows say who is acting.
        let first = transcript.lines(&call(b)).remove(0);
        assert!(first.contains("[a2 t2]"), "{first}");
        let again = transcript.lines(&call(a)).remove(0);
        assert!(again.contains("[a1 t3]"), "labels stay stable per agent: {again}");
    }

    #[test]
    fn actor_and_verb_are_separated_when_labelled() {
        let mut transcript = Transcript::new(Color::Plain, true);
        let request = event(EventKind::ToolRequested, json!({"name":"bash","arguments":{"command":"echo hi"}}))
            .with_agent(swarm_core::AgentId::new())
            .with_task(swarm_core::TaskId::new());
        let line = transcript.lines(&request).remove(0);
        assert!(line.contains("[a1 t1] bash(command=echo hi"), "{line}");
    }

    #[test]
    fn approvals_failures_and_state_changes() {
        assert!(render(EventKind::ApprovalRequested, json!({"name":"bash","risk":"runs a command"})).is_empty());
        let denied = render(EventKind::ApprovalResolved, json!({"approved": false}));
        assert_eq!(denied[0], format!("  {BRANCH}  denied"));
        let approved = render(EventKind::ApprovalResolved, json!({"approved": true}));
        assert_eq!(approved[0], format!("  {BRANCH}  approved"));
        let failure = render(EventKind::InferenceFailed, json!({"error":"429 quota"}));
        assert!(failure[0].contains("429 quota"));
        let live = render(
            EventKind::InferenceFailed,
            json!({"attempt":1,"class":"Transient","message":"provider HTTP error 504"}),
        );
        assert!(live[0].contains("provider HTTP error 504"), "{live:?}");
        let message = render(EventKind::Message, json!({"error":"bridge unreachable","retryable":true}));
        assert!(message[0].contains("bridge unreachable"));
        // Routine state changes stay quiet; failures do not.
        assert!(render(EventKind::TaskStateChanged, json!({"state":"ready"})).is_empty());
        assert!(render(EventKind::TaskStateChanged, json!({"state":"completed"})).is_empty());
        assert!(render(EventKind::TaskStateChanged, json!({"state":"failed"}))[0].contains("failed"));
        assert!(render(EventKind::RunStateChanged, json!({"status": "running"})).is_empty());
        assert!(render(EventKind::RunStateChanged, json!({"status": "failed"}))[0].contains("run failed"));
        assert!(render(EventKind::RunStateChanged, json!({})).is_empty());
        assert!(render(EventKind::InferenceStarted, json!({"step":1})).is_empty());
        // Verbose shows the routine ones too.
        assert!(verbose(EventKind::RunStateChanged, json!({"status": "running"}))[0].contains("run running"));
    }

    #[test]
    fn model_output_cannot_inject_terminal_escapes() {
        let reply = render(
            EventKind::InferenceCompleted,
            json!({"step":1,"text":"hi \u{1b}[2J\u{1b}]0;owned\u{7} there","tool_calls":[]}),
        );
        let tool = render(
            EventKind::ToolResult,
            json!({"call_id":"c","name":"bash","success":true,"truncated":false,"arguments":{"command":"x"},
                   "output":"\u{1b}[31mred\u{1b}[0m\r\nnext\u{7}"}),
        );
        let request = render(
            EventKind::ToolRequested,
            json!({"name":"bash","arguments":{"command":"echo \u{1b}[2Jgone"}}),
        );
        for row in reply.iter().chain(&tool).chain(&request) {
            assert!(!row.contains('\u{1b}') && !row.contains('\u{7}') && !row.contains('\r'), "{row:?}");
        }
        assert!(tool[0].ends_with("red"), "{:?}", tool[0]);
    }

    #[test]
    fn long_replies_are_capped_and_counted() {
        let text = (0..100).map(|n| format!("para {n}")).collect::<Vec<_>>().join("\n\n");
        let lines = render(EventKind::InferenceCompleted, json!({"step": 2, "text": text, "tool_calls": []}));
        assert!(lines.last().unwrap().contains("more lines (ctrl+o to expand)"), "{:?}", lines.last());
        assert!(lines.len() <= 62, "{} rows", lines.len());
    }

    #[test]
    fn a_remark_before_a_tool_call_is_shown_but_raw_tool_blocks_are_not() {
        let remark = render(
            EventKind::InferenceCompleted,
            json!({"step": 3, "text": "Let me look at the tests first.", "tool_calls": ["read_file"]}),
        );
        assert_eq!(remark[0], format!("{BULLET} Let me look at the tests first."));
        let raw = render(
            EventKind::InferenceCompleted,
            json!({"step": 3, "text": "```swarm\n{\"tool\":{\"name\":\"write_file\"}}\n```", "tool_calls": ["write_file"]}),
        );
        assert!(raw.is_empty(), "{raw:?}");
    }

    #[test]
    fn rows_stay_within_the_terminal_width() {
        let mut transcript = Transcript::new(Color::Plain, false);
        transcript.set_width(40);
        let long = "word ".repeat(60);
        let rows = transcript.lines(&event(
            EventKind::InferenceCompleted,
            json!({"step": 1, "text": long, "tool_calls": []}),
        ));
        assert!(rows.iter().all(|row| crate::ui::text::width(row) <= 40), "{rows:?}");
        let tool = transcript.lines(&event(
            EventKind::ToolResult,
            json!({"call_id":"c","name":"bash","success":true,"truncated":false,"arguments":{"command":"x"},"output":"y".repeat(300)}),
        ));
        assert!(tool.iter().all(|row| crate::ui::text::width(row) <= 40), "{tool:?}");
    }

    #[test]
    fn args_summary_flattens_and_caps() {
        assert_eq!(args_summary(&json!({"path":"a.txt"})), "path=a.txt");
        assert_eq!(args_summary(&json!({"command":"echo hi\nls"})), "command=echo hi ls");
        assert_eq!(args_summary(&json!({"limit":10,"flag":true})), "flag=true, limit=10");
        assert_eq!(args_summary(&json!({"paths":["a","b","c","d"]})), "paths=[a, b, c…]");
        let long = args_summary(&json!({"q":"x".repeat(400)}));
        assert!(long.chars().count() < 80, "{long}");
        assert!(long.ends_with(ELLIPSIS));
    }

    #[test]
    fn colors_wrap_only_when_enabled() {
        assert_eq!(paint(Color::Plain, "fail", "x"), "x");
        let colored = paint(Color::Ansi, "fail", "x");
        assert!(colored.starts_with("\x1b[38;2;255;107;107m") && colored.ends_with("\x1b[0m"), "{colored}");
        let bullet = paint(Color::Ansi, "bullet", crate::style::bullet());
        assert!(bullet.contains("38;2;215;119;87") && !bullet.contains("48;2;"), "{bullet}");
    }

    #[test]
    fn footer_counts_everything_the_user_asks_about() {
        let line = footer("completed", 8.42, 1, 2, 300, 1);
        assert!(line.contains("completed in 8.4s") && line.contains("1 task "));
        assert!(line.contains("2 inferences") && line.contains("1 tool call "));
        assert!(!line.contains("1 tool calls"), "one tool call is singular");
        assert!(line.contains("300 tokens"));
    }
}
