//! Claude Code-style live transcript for `swarm run`.
//!
//! The run is event-sourced, so this is a pure renderer over [`RunEvent`]s:
//! `⏺` marks an action (an assistant turn, a tool call), `⎿` marks its result,
//! and dim text carries meta (latency, tokens, state changes). Keeping the
//! renderer pure means the exact bytes a terminal user sees are unit-testable
//! with colors off, and `--quiet`/pipes get plain lines.

use std::collections::HashMap;
use std::env;
use std::fmt::Write as _;
use std::io::{self, IsTerminal};

use serde_json::Value;
use swarm_core::event::{EventKind, RunEvent};

use crate::style::{tokens, PREVIEW_LINES};

const BULLET: &str = crate::style::bullet(); // ● assistant / tool action
const BRANCH: char = '\u{23bf}'; // ⎿ result
const CROSS: char = '\u{2717}'; // ✗ failure
const SPARK: char = '\u{273b}'; // ✻ footer flourish
const ELLIPSIS: char = '\u{2026}'; // …

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Color {
    Plain,
    Ansi,
}

/// TTY detection, overridable with `SWARM_UI=always|never` so the live view can
/// be exercised in CI, pipes, and screenshot tooling.
pub fn ui_enabled(streams_tty: bool) -> bool {
    match env::var("SWARM_UI").ok().as_deref() {
        Some("always") => true,
        Some("never") => false,
        _ => streams_tty,
    }
}

impl Color {
    /// Colors only on a real terminal, never for pipes, `NO_COLOR`, or `dumb`.
    pub fn detect(stream_tty: bool) -> Self {
        let tty = ui_enabled(stream_tty) && io::stdout().is_terminal();
        let dumb = env::var("TERM").map(|term| term == "dumb").unwrap_or(false);
        if !tty || env::var_os("NO_COLOR").is_some() || dumb {
            Color::Plain
        } else {
            Color::Ansi
        }
    }
}

/// Paints a line. `role` is one of: action, bullet, result, text, ok, fail,
/// warn, meta, ask, brand, user, suggest. The palette itself lives in
/// [`crate::style`], transcribed from the Claude Code bundle.
pub fn paint(colors: Color, role: &str, text: &str) -> String {
    crate::style::role(
        match colors {
            Color::Plain => crate::style::Color::Plain,
            Color::Ansi => crate::style::Color::Ansi,
        },
        role,
        text,
    )
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
        Value::String(s) => s.replace('\n', " "),
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
        .unwrap_or("")
        .to_string()
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
    let flat = text
        .replace(['\r', '\n'], " ")
        .trim()
        .to_string();
    let count = flat.chars().count();
    if count <= limit {
        flat
    } else {
        format!(
            "{}{}",
            flat.chars().take(limit).collect::<String>(),
            ELLIPSIS
        )
    }
}

/// Indented continuation lines, so a 40-line reply stays readable in a column.
/// Overflow is marked Claude Code's way: `+N` for N more lines, not an ellipsis.
fn indent(text: &str, prefix: &str, limit: usize) -> Vec<String> {
    let mut lines: Vec<String> = text.trim_end().lines().map(str::to_string).collect();
    while lines.last().is_some_and(|line| line.trim().is_empty()) {
        lines.pop();
    }
    let mut out = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        if index >= limit {
            out.push(format!("{prefix}+{}", lines.len() - limit));
            break;
        }
        out.push(format!("{prefix}{}", compact(line, 200)));
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

/// Renders run events as transcript lines.
pub struct Transcript {
    colors: Color,
    verbose: bool,
    agents: HashMap<String, String>,
    tasks: HashMap<String, String>,
    /// The current step's usage row, printed after the step's tool rows.
    pending: Vec<String>,
}

impl Transcript {
    pub fn new(colors: Color, verbose: bool) -> Self {
        Self {
            colors,
            verbose,
            agents: HashMap::new(),
            tasks: HashMap::new(),
            pending: Vec::new(),
        }
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

    fn actor(&mut self, event: &RunEvent) -> String {
        let mut parts = Vec::new();
        if let Some(agent) = &event.agent_id {
            parts.push(Self::label(&mut self.agents, "a", agent.as_str()));
        }
        if let Some(task) = &event.task_id {
            parts.push(Self::label(&mut self.tasks, "t", task.as_str()));
        }
        if parts.is_empty() {
            String::new()
        } else {
            format!(" [{}]", parts.join(" "))
        }
    }

    pub fn banner(&self, header: &Header) -> Vec<String> {
        let dim = |label: &str| self.meta(label.to_string());
        let agents = format!(
            "{} agent{}",
            header.agents,
            if header.agents == 1 { "" } else { "s" }
        );
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
            format!(
                "  {} {} \u{00b7} {} {}",
                dim("workspace"),
                header.workspace,
                dim("run"),
                header.run_id
            ),
            String::new(),
        ]
    }

    fn paint_action(&self, text: &str) -> String {
        paint(self.colors, "action", text)
    }

    /// Claude Code's action row: the bullet cell, then the action's own text in
    /// grey-200, then its arguments dimmer still.
    fn action(&self, actor: &str, head: &str, args: &str) -> String {
        let dot = paint(self.colors, "bullet", BULLET);
        // The actor label and the action verb need a separator: `[a1 t1]bash(...)`
        // reads as one token, and in a plain (uncoloured) transcript the space is
        // the only thing between them.
        let label = if actor.is_empty() || head.is_empty() {
            format!("{actor}{head}")
        } else {
            format!("{actor} {head}")
        };
        let label = paint(self.colors, "action", &label);
        let detail = paint(self.colors, "meta", args);
        if head.is_empty() && args.is_empty() {
            format!("{dot}{label}")
        } else {
            format!("{dot} {label}{detail}")
        }
    }

    /// Transcript lines for one event; empty means "not shown at this level".
    /// Render one event.
    ///
    /// The log records an inference before the tool calls it asked for, so the
    /// step's usage row is held back and printed after those rows: action,
    /// result, usage.
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

    fn event_lines(&mut self, event: &RunEvent) -> Vec<String> {
        let actor = self.actor(event);
        let payload = &event.payload;
        let text_of = |key: &str| {
            payload
                .get(key)
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string()
        };
        match event.kind {
            EventKind::InferenceStarted => Vec::new(),
            EventKind::InferenceCompleted => {
                let mut out = Vec::new();
                let calls: Vec<String> = payload
                    .get("tool_calls")
                    .and_then(Value::as_array)
                    .map(|items| {
                        items
                            .iter()
                            .filter_map(Value::as_str)
                            .map(str::to_string)
                            .collect()
                    })
                    .unwrap_or_default();
                let text = text_of("text");
                if calls.is_empty() {
                    if !text.trim().is_empty() {
                        out.push(self.action(&actor, "", ""));
                        out.extend(indent(&text, "  ", if self.verbose { 120 } else { 24 }));
                    }
                } else {
                    let summary = compact(&text, 100);
                    let verb = if calls.len() == 1 {
                        format!("asks for {}", calls[0])
                    } else {
                        format!("asks for {} tools", calls.len())
                    };
                    out.push(self.action(&actor, &verb, ""));
                    if self.verbose && !summary.is_empty() {
                        out.extend(indent(&summary, "  ", 4));
                    }
                }
                let step = payload.get("step").and_then(Value::as_u64).unwrap_or(0);
                let latency = payload
                    .get("latency_ms")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let model = text_of("model");
                let mut meta = format!(
                    "  {BRANCH} step {step} \u{00b7} {:.1}s \u{00b7} {} \u{2192} {} tokens",
                    latency as f64 / 1000.0,
                    tokens(
                        payload
                            .get("input_tokens")
                            .and_then(Value::as_u64)
                            .unwrap_or(0)
                    ),
                    tokens(
                        payload
                            .get("output_tokens")
                            .and_then(Value::as_u64)
                            .unwrap_or(0)
                    )
                );
                if !model.is_empty() {
                    let _ = write!(meta, " \u{00b7} {model}");
                }
                self.pending = vec![self.meta(meta)];
                out
            }
            EventKind::InferenceFailed => vec![paint(
                self.colors,
                "fail",
                &format!("{CROSS}{actor} {}", first_of(payload, &["error", "message"])),
            )],
            EventKind::ToolRequested => {
                let name = text_of("name");
                let args = payload
                    .get("arguments")
                    .map(args_summary)
                    .unwrap_or_default();
                vec![self.action(&actor, &name, &format!("({args})"))]
            }
            EventKind::ToolResult => {
                let success = payload
                    .get("success")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let output = text_of("output");
                let mark = if success { BRANCH } else { CROSS };
                let role = if success { "result" } else { "fail" };
                let truncated = payload
                    .get("truncated")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                // Claude Code keeps a short preview of the output under the
                // branch, at 4-space indent, and marks the cut with `+N`.
                let budget = if self.verbose { 120 } else { PREVIEW_LINES };
                let mut body = indent(&output, "    ", budget);
                if truncated && !body.is_empty() {
                    let last = body.len() - 1;
                    body[last] = format!("{} {ELLIPSIS}", body[last]);
                }
                let mut out = Vec::new();
                match body.first().cloned() {
                    Some(head) => {
                        out.push(paint(self.colors, role, &format!("  {mark} {}", head.trim())));
                        out.extend(
                            body[1..]
                                .iter()
                                .map(|line| paint(self.colors, role, line)),
                        );
                    }
                    None => out.push(paint(self.colors, role, &format!("  {mark} (empty)"))),
                }
                out
            }
            // The interactive card (ConsoleApproval) carries the request; the
            // transcript only records the outcome, below.
            EventKind::ApprovalRequested => Vec::new(),
            EventKind::ApprovalResolved => {
                let approved = payload
                    .get("approved")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                vec![paint(
                    self.colors,
                    if approved { "ok" } else { "warn" },
                    &format!(
                        "  {BRANCH} {}",
                        if approved { "approved" } else { "denied" }
                    ),
                )]
            }
            EventKind::Message => {
                let mut out = Vec::new();
                if let Some(summary) = payload.get("summary").and_then(Value::as_str) {
                    out.push(self.paint_action(&format!("{BULLET}{actor} result")));
                    out.extend(indent(summary, "  ", if self.verbose { 120 } else { 12 }));
                }
                if let Some(error) = payload.get("error").and_then(Value::as_str) {
                    out.push(paint(
                        self.colors,
                        "fail",
                        &format!("{CROSS}{actor} {error}"),
                    ));
                }
                out
            }
            EventKind::TaskStateChanged => {
                let state = text_of("state");
                if self.verbose || matches!(state.as_str(), "completed" | "failed") {
                    vec![self.meta(format!("  {BRANCH} task {state}"))]
                } else {
                    Vec::new()
                }
            }
            EventKind::RunStateChanged => {
                let state = first_of(payload, &["status", "state"]);
                if state.is_empty() {
                    Vec::new()
                } else {
                    vec![self.meta(format!("  {BRANCH} run {state}"))]
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
                        vec![self.meta(format!("  {BRANCH} checkpoint v{version} · {steps} steps"))]
                    }
                    _ => vec![self.meta("  \u{00b7} checkpoint".to_string())],
                }
            }
            _ => Vec::new(),
        }
    }
}

/// The one-line summary `run` prints after the transcript.
pub fn footer(
    status: &str,
    seconds: f64,
    tasks: u64,
    inferences: u64,
    tokens: u64,
    tools: u64,
) -> String {
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

    /// The log records an inference before its tool calls; the transcript prints
    /// the ask, the tool rows, then that step's usage row.
    #[test]
    fn step_usage_prints_after_the_tools_it_asked_for() {
        let mut transcript = Transcript::new(Color::Plain, false);
        let inference = transcript.lines(&event(
            EventKind::InferenceCompleted,
            json!({"step": 1, "text": "writing it", "tool_calls": ["write_file"],
                   "latency_ms": 900}),
        ));
        assert_eq!(inference.len(), 1, "the usage row waits for the tools");
        let tool = transcript
            .lines(&event(
                EventKind::ToolRequested,
                json!({"call_id": "c1", "name": "write_file", "arguments": {}}),
            ))
            .remove(0);
        assert!(tool.contains("write_file("));
        let next = transcript.lines(&event(
            EventKind::InferenceCompleted,
            json!({"step": 2, "text": "done", "tool_calls": [], "latency_ms": 300}),
        ));
        assert!(next[0].contains("step 1"), "step 1 prints before step 2");
        assert!(transcript.drain()[0].contains("step 2"));
    }

    #[test]
    fn assistant_reply_renders_as_a_bulleted_block() {
        let lines = render(
            EventKind::InferenceCompleted,
            json!({"step": 1, "model": "gpt-5.1", "input_tokens": 120, "output_tokens": 30,
                   "latency_ms": 1234, "text": "I will create the file.\nSecond line.", "tool_calls": []}),
        );
        assert_eq!(lines[0], format!("{BULLET}"));
        assert_eq!(lines[1], "  I will create the file.");
        assert_eq!(lines[2], "  Second line.");
        assert!(lines[3].contains("step 1"));
        assert!(lines[3].contains("1.2s"));
        assert!(lines[3].contains("120 \u{2192} 30 tokens"));
        assert!(lines[3].contains("gpt-5.1"));
    }

    #[test]
    fn tool_request_and_result_pair_up() {
        let mut transcript = Transcript::new(Color::Plain, false);
        let request = transcript
            .lines(&event(
                EventKind::ToolRequested,
                json!({"call_id":"c1","name":"write_file",
                "arguments":{"path":"hello.txt","content":"a\nb\nc"}}),
            ))
            .remove(0);
        assert_eq!(
            request,
            format!("{BULLET} write_file(content=a b c, path=hello.txt)"),
            "serde_json map iteration is alphabetical"
        );
        let result = transcript
            .lines(&event(
                EventKind::ToolResult,
                json!({"call_id":"c1","name":"write_file",
                "success":true,"truncated":false,"output":"File written\nignored"}),
            ))
            .remove(0);
        assert_eq!(result, format!("  {BRANCH} File written"));
        let failed = transcript
            .lines(&event(
                EventKind::ToolResult,
                json!({"call_id":"c2","name":"bash",
                "success":false,"truncated":false,"output":"exit 1: nope"}),
            ))
            .remove(0);
        assert!(failed.starts_with(&format!("  {CROSS} exit 1: nope")));
    }

    #[test]
    fn long_replies_are_capped_and_counted() {
        let text = "line\n".repeat(40);
        let lines = render(
            EventKind::InferenceCompleted,
            json!({"step": 2, "latency_ms": 10, "input_tokens": 1, "output_tokens": 1,
                   "text": text, "tool_calls": []}),
        );
        assert_eq!(lines.len(), 24 + 3); // bullet + 24 lines + overflow marker + meta
        assert!(lines.last().unwrap().contains("step 2"));
        assert_eq!(lines[25], "  +16");
    }

    #[test]
    fn tool_request_replaces_the_reply_block() {
        let lines = render(
            EventKind::InferenceCompleted,
            json!({"step": 3, "latency_ms": 900, "input_tokens": 5, "output_tokens": 6,
                   "text": "```swarm\n{\"tool\":{\"name\":\"write_file\"}}\n```",
                   "tool_calls": ["write_file"]}),
        );
        assert!(lines[0].contains("asks for write_file"));
        assert!(!lines.iter().any(|line| line.contains("```")));
    }

    #[test]
    fn actor_and_verb_are_separated() {
        // The live transcript showed `[a1 t1]asks for bash` and
        // `[a1 t1]bash(...)`; in a plain transcript the space is the separator.
        let mut transcript = Transcript::new(Color::Plain, false);
        let request = event(
            EventKind::ToolRequested,
            json!({"name":"bash","arguments":{"command":"echo hi"}}),
        )
        .with_agent(swarm_core::AgentId::new())
        .with_task(swarm_core::TaskId::new());
        let line = transcript.lines(&request).remove(0);
        assert!(line.contains("[a1 t1] bash(command=echo hi"), "{line}");
    }

    #[test]
    fn approvals_failures_and_state_changes_are_visible() {
        // The prompt card carries the request; the transcript records the outcome.
        assert!(
            render(
                EventKind::ApprovalRequested,
                json!({"name":"bash","risk":"runs a command"})
            )
            .is_empty()
        );
        let denied = render(EventKind::ApprovalResolved, json!({"approved": false}));
        assert!(denied[0].contains("denied"));
        let failure = render(EventKind::InferenceFailed, json!({"error":"429 quota"}));
        assert!(failure[0].contains("429 quota"));
        // The runtime's real payload uses `message`, not `error`.
        let live = render(EventKind::InferenceFailed,
            json!({"attempt":1,"class":"Transient","message":"provider HTTP error 504"}));
        assert!(live[0].contains("provider HTTP error 504"), "{live:?}");
        let message = render(
            EventKind::Message,
            json!({"error":"bridge unreachable","retryable":true}),
        );
        assert!(message[0].contains("bridge unreachable"));
        // Quiet events stay quiet unless verbose.
        assert!(render(EventKind::TaskStateChanged, json!({"state":"ready"})).is_empty());
        assert!(
            render(EventKind::TaskStateChanged, json!({"state":"completed"}))[0]
                .contains("completed")
        );
        // Run/agent state rows read the runtime's payload keys, never a blank.
        let running = render(EventKind::RunStateChanged, json!({"status": "running"}));
        assert_eq!(running[0].trim_start(), format!("{BRANCH} run running"));
        assert!(
            render(EventKind::RunStateChanged, json!({})).is_empty(),
            "a state event with no state must render nothing"
        );
        assert!(render(EventKind::InferenceStarted, json!({"step":1})).is_empty());
    }

    #[test]
    fn actors_get_short_stable_labels() {
        let mut transcript = Transcript::new(Color::Plain, false);
        let first = event(
            EventKind::ToolRequested,
            json!({"name":"read_file","arguments":{}}),
        )
        .with_agent(swarm_core::AgentId::new())
        .with_task(swarm_core::TaskId::new());
        let second = event(
            EventKind::ToolRequested,
            json!({"name":"list_files","arguments":{}}),
        )
        .with_agent(swarm_core::AgentId::new())
        .with_task(swarm_core::TaskId::new());
        let one = transcript.lines(&first).remove(0);
        let two = transcript.lines(&second).remove(0);
        let three = transcript.lines(&first).remove(0);
        assert!(one.contains("[a1 t1]"), "{one}");
        assert!(two.contains("[a2 t2]"), "{two}");
        assert_eq!(three, one, "labels must be stable per id");
    }

    #[test]
    fn args_summary_flattens_and_caps() {
        assert_eq!(args_summary(&json!({"path":"a.txt"})), "path=a.txt");
        assert_eq!(
            args_summary(&json!({"command":"echo hi\nls"})),
            "command=echo hi ls"
        );
        assert_eq!(
            args_summary(&json!({"limit":10,"flag":true})),
            "flag=true, limit=10",
            "keys are rendered in map (alphabetical) order"
        );
        assert_eq!(
            args_summary(&json!({"paths":["a","b","c","d"]})),
            "paths=[a, b, c…]"
        );
        let long = args_summary(&json!({"q":"x".repeat(400)}));
        assert!(long.chars().count() < 80, "{long}");
        assert!(long.ends_with(ELLIPSIS));
    }

    #[test]
    fn colors_wrap_only_when_enabled() {
        assert_eq!(paint(Color::Plain, "fail", "x"), "x");
        // Claude Code's error red, #FF6B6B, as truecolor.
        let colored = paint(Color::Ansi, "fail", "x");
        assert!(
            colored.starts_with("\x1b[38;2;255;107;107m") && colored.ends_with("\x1b[0m"),
            "{colored}"
        );
        // The action bullet is brand terracotta, and carries no background.
        let bullet = paint(Color::Ansi, "bullet", crate::style::bullet());
        assert!(bullet.contains("38;2;215;119;87"), "{bullet}");
        assert!(!bullet.contains("48;2;"), "{bullet}");
    }

    #[test]
    fn footer_counts_everything_the_user_asks_about() {
        let line = footer("completed", 8.42, 1, 2, 300, 1);
        assert!(line.contains("completed in 8.4s"));
        assert!(line.contains("1 task "));
        assert!(line.contains("2 inferences"));
        assert!(line.contains("1 tool call "));
        assert!(!line.contains("1 tool calls"), "one tool call is singular");
        assert!(line.contains("300 tokens"));
    }
}
