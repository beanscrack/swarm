//! A text tool protocol for endpoints without native `tool_calls`.
//!
//! swarm's normal path sends the OpenAI `tools` array and reads `tool_calls` back, which is
//! the same mechanism any function-calling API uses. Small local models, quantized builds,
//! and servers started without tool support accept the request and answer in prose: the
//! agent loop sees no call, spends its `max_format_retries` nagging, and dies. That is a
//! repair layer standing in for a protocol.
//!
//! This module is the protocol: a compact instruction block for the prompt, and a tolerant
//! parser for the reply. It is deliberately forgiving, because the whole point is to work
//! with models that were not trained on a schema. Every rejection carries a reason, so the
//! retry prompt can name the defect ("no JSON object found", "arguments is a string, not an
//! object") instead of "malformed".
//!
//! Deliberate limits: one call per reply (a model that can emit two will emit one), and no
//! attempt to rescue a chat-UI bridge - see `docs/TEXT-TOOLS-PLAN.md` for why a web UI is a
//! negative control here, not a target.

use serde_json::{Map, Value};

/// The names a model may use for the tool, and for its arguments.
const NAME_KEYS: [&str; 3] = ["tool", "name", "function"];
const ARGS_KEYS: [&str; 4] = ["arguments", "params", "input", "args"];

/// One tool call recovered from prose, plus how confident we are that it is complete.
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedCall {
    pub name: String,
    pub arguments: Value,
    /// True when the JSON parsed exactly; false when recovery heuristics were used (an
    /// unterminated fence, a trailing comma). The loop can ask the model to confirm.
    pub exact: bool,
}

/// Why a reply produced no usable call.
#[derive(Debug, Clone, PartialEq)]
pub enum Rejection {
    NoJsonObject,
    MissingToolName,
    ArgumentsNotAnObject,
}

impl Rejection {
    /// Wording for the correction prompt: name the defect, do not say "malformed".
    pub fn prompt_text(&self) -> &'static str {
        match self {
            Rejection::NoJsonObject => {
                "no JSON object was found in your reply. Reply with only a fenced block: \
                 ```json\n{\"tool\": \"read_file\", \"arguments\": {\"path\": \"README.md\"}}\n```"
            }
            Rejection::MissingToolName => {
                "your JSON object had no \"tool\" key naming the tool to call. Keys accepted: \
                 tool, name, function."
            }
            Rejection::ArgumentsNotAnObject => {
                "\"arguments\" must be a JSON object of parameters, not a string or a list."
            }
        }
    }
}

/// Instructions appended to the system prompt when the endpoint has no native tool support.
pub fn tool_prompt(tools: &[Value]) -> String {
    let mut names = String::new();
    for t in tools {
        let name = t
            .get("function")
            .and_then(|f| f.get("name"))
            .and_then(Value::as_str)
            .or_else(|| t.get("name").and_then(Value::as_str));
        let desc = t
            .get("function")
            .and_then(|f| f.get("description"))
            .and_then(Value::as_str)
            .unwrap_or("");
        if let Some(n) = name {
            names.push_str(&format!("- {n}: {desc}\n"));
        }
    }
    format!(
        "TOOLS (text protocol). You are an agent; you act only by calling tools, and this \
         endpoint has no tool-call field, so you write the call as text. Reply with exactly \
         ONE fenced JSON block and nothing else:\n\
         ```json\n{{\"tool\": \"<tool name>\", \"arguments\": {{...}}}}\n```\n\
         One call per reply. Put no commentary outside the block; think first if you must, \
         then emit the block and stop. When the work is finished, call the `report_result` \
         tool with your status and summary.\n\nAvailable tools:\n{names}"
    )
}

/// Recover at most one tool call from a model reply.
///
/// Prefers fenced blocks (```json / ``` / ~~~), then falls back to the largest brace-balanced
/// span, so prose like "Sure! Here is the call: {...}" works. `Ok` on the first object that
/// yields a tool name and object arguments; `Err` with the reason for the last failure seen.
pub fn parse_tool_call(text: &str) -> Result<ParsedCall, Rejection> {
    let mut last = Rejection::NoJsonObject;
    for (candidate, exact) in candidates(text) {
        let value = match parse_json(&candidate) {
            Some(v) => v,
            None => {
                last = Rejection::NoJsonObject;
                continue;
            }
        };
        let obj = match value.as_object() {
            Some(o) => o,
            None => {
                last = Rejection::NoJsonObject;
                continue;
            }
        };
        let Some(name) = NAME_KEYS
            .iter()
            .filter_map(|k| obj.get(*k))
            .find_map(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
        else {
            last = Rejection::MissingToolName;
            continue;
        };
        let arguments = ARGS_KEYS
            .iter()
            .find_map(|k| obj.get(*k))
            .cloned()
            .unwrap_or(Value::Object(Map::new()));
        let arguments = match expand_string_arguments(&arguments) {
            Some(v) => v,
            None => {
                last = Rejection::ArgumentsNotAnObject;
                continue;
            }
        };
        return Ok(ParsedCall {
            name: name.to_string(),
            arguments,
            exact,
        });
    }
    Err(last)
}

/// Candidate JSON spans, most trustworthy first: fenced blocks, then brace-balanced spans.
fn candidates(text: &str) -> Vec<(String, bool)> {
    let mut out: Vec<(String, bool)> = Vec::new();
    let bytes = text.as_bytes();
    let mut rest = text;
    while let Some(open) = rest.find("```").or_else(|| rest.find("~~~")) {
        let fence = &rest[open..open + 3];
        let body = &rest[open + 3..];
        match body.find(fence) {
            Some(close) => {
                let inner = body[..close].trim();
                let inner = inner.strip_prefix("json").unwrap_or(inner).trim();
                if !inner.is_empty() {
                    out.push((inner.to_string(), true));
                }
                rest = &body[close + 3..];
            }
            // Unterminated fence: a truncated reply still deserves a chance.
            None => {
                let inner = body.trim().strip_prefix("json").unwrap_or(body).trim();
                if !inner.is_empty() {
                    out.push((inner.to_string(), false));
                }
                break;
            }
        }
        if bytes.len() < 3 {
            break;
        }
    }
    if let Some(span) = brace_span(text) {
        out.push((span.to_string(), false));
    }
    out
}

/// The largest brace-balanced span in the text, if any. Ignores braces inside strings.
fn brace_span(text: &str) -> Option<&str> {
    let start = text.find('{')?;
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (i, ch) in text[start..].char_indices() {
        if in_string {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }
        match ch {
            '"' => in_string = true,
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&text[start..start + i + 1]);
                }
            }
            _ => {}
        }
    }
    // Truncated mid-object: close it and let the caller mark the result inexact.
    Some(&text[start..])
}

fn parse_json(candidate: &str) -> Option<Value> {
    let trimmed = candidate.trim().trim_end_matches([',', ';']);
    serde_json::from_str(trimmed).ok().or_else(|| {
        // Repair the common truncations: a missing closing brace, a trailing comma.
        let mut s = trimmed.to_string();
        let opens = s.matches('{').count();
        let closes = s.matches('}').count();
        if opens > closes {
            s.push_str(&"}".repeat(opens - closes));
        }
        let cleaned = s.replace(",}", "}").replace(",]", "]");
        serde_json::from_str(&cleaned)
            .ok()
            .or_else(|| serde_json::from_str(&s).ok())
    })
}

/// Models frequently write arguments as a JSON string, or as a stringified object. Accept
/// both; reject a bare scalar, which is almost never what a tool wants.
fn expand_string_arguments(value: &Value) -> Option<Value> {
    match value {
        Value::Null => Some(Value::Object(Map::new())),
        Value::Object(_) => Some(value.clone()),
        Value::String(s) => {
            let t = s.trim();
            parse_json(t).filter(Value::is_object).or_else(|| {
                let mut map = Map::new();
                if !t.is_empty() {
                    map.insert("text".to_string(), Value::String(s.clone()));
                }
                Some(Value::Object(map))
            })
        }
        _ => None,
    }
}

#[cfg(test)]
mod text_protocol_tests {
    use super::*;

    #[test]
    fn a_fenced_block_is_the_clean_case() {
        let reply =
            "```json\n{\"tool\": \"bash\", \"arguments\": {\"command\": \"go build ./...\"}}\n```";
        let call = parse_tool_call(reply).expect("parses");
        assert_eq!(call.name, "bash");
        assert_eq!(call.arguments["command"], "go build ./...");
        assert!(call.exact, "a closed fence is an exact parse");
    }

    #[test]
    fn prose_around_the_call_is_tolerated() {
        let reply = "Sure! I will look at that.\n{\"name\": \"read_file\", \"params\": {\"path\": \"a.go\"}}\nHope that helps.";
        let call = parse_tool_call(reply).expect("recovers from prose");
        assert_eq!(call.name, "read_file");
        assert_eq!(call.arguments["path"], "a.go");
    }

    #[test]
    fn string_arguments_are_parsed_when_they_are_json() {
        let reply = "{\"function\": \"write_file\", \"input\": \"{\\\"path\\\":\\\"x\\\",\\\"content\\\":\\\"y\\\"}\"}";
        let call = parse_tool_call(reply).expect("stringified arguments are accepted");
        assert_eq!(call.name, "write_file");
        assert_eq!(call.arguments["path"], "x");
    }

    #[test]
    fn a_truncated_reply_still_counts_and_says_so() {
        let reply = "```json\n{\"tool\": \"search\", \"arguments\": {\"pattern\": \"budget\"}";
        let call = parse_tool_call(reply).expect("unterminated fence recovers");
        assert_eq!(call.name, "search");
        assert!(!call.exact, "recovery is not an exact parse");
    }

    #[test]
    fn rejections_name_the_defect() {
        assert_eq!(
            parse_tool_call("I cannot help with that."),
            Err(Rejection::NoJsonObject)
        );
        assert_eq!(
            parse_tool_call("{\"arguments\": {\"path\": \"a\"}}"),
            Err(Rejection::MissingToolName)
        );
        assert_eq!(
            parse_tool_call("{\"tool\": \"bash\", \"arguments\": 42}"),
            Err(Rejection::ArgumentsNotAnObject)
        );
        assert!(Rejection::MissingToolName.prompt_text().contains("tool"));
    }

    #[test]
    fn the_prompt_names_every_available_tool() {
        let tools = serde_json::json!([
            {"function": {"name": "bash", "description": "run a shell command"}},
            {"function": {"name": "report_result", "description": "report the outcome"}}
        ]);
        let prompt = tool_prompt(tools.as_array().unwrap());
        assert!(prompt.contains("- bash: run a shell command"));
        assert!(prompt.contains("report_result"));
        assert!(prompt.contains("ONE fenced JSON block"));
    }
}
