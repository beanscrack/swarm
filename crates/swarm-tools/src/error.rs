//! Tool results and errors.
//!
//! A [`ToolResult`] is the outcome of executing a single tool call. It is
//! *data*, not a panic: a non-zero process exit or a file-not-found is a result
//! the model can read, while only tool-level failures (unknown tool, path
//! escape, spawn error, timeout) mark the call itself as failed.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use swarm_core::artifact::ArtifactRef;
use swarm_core::time::now_millis;
use swarm_core::tool::ToolCallRecord;

/// A tool-level error: something that stopped the call before it could produce
/// meaningful output. Converted into a failed [`ToolResult`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolError {
    UnknownTool { name: String },
    BadInput { field: String, problem: String },
    PathEscape { path: String },
    NotFound { path: String },
    Io { message: String },
    Timeout { millis: u64 },
}

impl std::fmt::Display for ToolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownTool { name } => write!(f, "unknown tool: {name}"),
            Self::BadInput { field, problem } => {
                write!(f, "bad input for field `{field}`: {problem}")
            }
            Self::PathEscape { path } => write!(f, "path escapes the sandbox: {path}"),
            Self::NotFound { path } => write!(f, "not found: {path}"),
            Self::Io { message } => write!(f, "io error: {message}"),
            Self::Timeout { millis } => write!(f, "timed out after {millis}ms"),
        }
    }
}

impl std::error::Error for ToolError {}

/// Largest file whose previous contents are kept for a diff.
const CHANGE_CAP_BYTES: usize = 128 * 1024;

/// What a file tool changed, recorded at the moment it ran so a diff can be
/// shown later without racing the file system.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct FileChange {
    /// The file's contents before the call; `None` for a new file, or one too
    /// large to keep.
    pub before: Option<String>,
    /// The call created the file.
    pub created: bool,
    /// [`content_hash`] of what the call left in the file, so a later rewind
    /// can tell the file still holds the agent's work and not a hand edit.
    #[serde(default)]
    pub after: Option<u64>,
}

impl FileChange {
    pub fn capture(before: Option<String>, created: bool) -> Self {
        Self {
            before: before.filter(|text| text.len() <= CHANGE_CAP_BYTES),
            created,
            after: None,
        }
    }

    /// Record what the call wrote.
    pub fn with_after(mut self, written: &str) -> Self {
        self.after = Some(content_hash(written));
        self
    }
}

/// FNV-1a over the text's bytes: stable across runs and releases (unlike
/// `DefaultHasher`), which matters because the value is stored in events.
pub fn content_hash(text: &str) -> u64 {
    text.bytes().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

/// The outcome of executing one tool call.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolResult {
    pub call_id: String,
    pub name: String,
    /// True if the tool itself ran. A non-zero exit code is still a successful
    /// *execution*; it is exposed via [`exit_code`](ToolResult::exit_code).
    pub success: bool,
    /// Primary output: command stdout, file contents, search hits, etc.
    pub content: String,
    /// stderr for command tools.
    pub stderr: String,
    /// Process exit code for command tools, when the process ran.
    pub exit_code: Option<i32>,
    /// True if output was truncated to fit the sandbox byte cap.
    pub truncated: bool,
    /// Set only on a tool-level failure.
    pub error: Option<String>,
    /// Wall-clock execution time.
    pub duration_ms: u64,
    /// A reference to a persisted artifact, when the runtime stored the output.
    pub output_ref: Option<ArtifactRef>,
    /// What a file tool changed (`write_file`, `edit_file`).
    #[serde(default)]
    pub change: Option<FileChange>,
}

impl ToolResult {
    /// A successful execution with plain text output.
    pub fn ok(content: impl Into<String>) -> Self {
        Self {
            call_id: String::new(),
            name: String::new(),
            success: true,
            content: content.into(),
            stderr: String::new(),
            exit_code: None,
            truncated: false,
            error: None,
            duration_ms: 0,
            output_ref: None,
            change: None,
        }
    }

    /// A successful command execution: the process ran (any exit code) and its
    /// stdout/stderr were captured.
    pub fn command(
        content: String,
        stderr: String,
        exit_code: Option<i32>,
        truncated: bool,
    ) -> Self {
        Self {
            call_id: String::new(),
            name: String::new(),
            success: true,
            content,
            stderr,
            exit_code,
            truncated,
            error: None,
            duration_ms: 0,
            output_ref: None,
            change: None,
        }
    }

    /// A tool-level failure (no meaningful output produced).
    pub fn failure(error: impl Into<String>) -> Self {
        Self {
            call_id: String::new(),
            name: String::new(),
            success: false,
            content: String::new(),
            stderr: String::new(),
            exit_code: None,
            truncated: false,
            error: Some(error.into()),
            duration_ms: 0,
            output_ref: None,
            change: None,
        }
    }

    /// A tool-level failure that also captured stderr and/or an exit code.
    pub fn failure_with(
        error: impl Into<String>,
        stderr: String,
        exit_code: Option<i32>,
        truncated: bool,
    ) -> Self {
        Self {
            call_id: String::new(),
            name: String::new(),
            success: false,
            content: String::new(),
            stderr,
            exit_code,
            truncated,
            error: Some(error.into()),
            duration_ms: 0,
            output_ref: None,
            change: None,
        }
    }

    /// Text to feed back to the model as the tool-result message.
    pub fn as_model_output(&self) -> String {
        if !self.success {
            let mut s = format!(
                "tool '{}' failed: {}",
                self.name,
                self.error.as_deref().unwrap_or("unknown error")
            );
            if let Some(code) = self.exit_code {
                s.push_str(&format!("\n(exit code {code})"));
            }
            if !self.stderr.trim().is_empty() {
                s.push_str("\n\n[stderr]\n");
                s.push_str(&self.stderr);
            }
            return s;
        }
        let mut s = self.content.clone();
        if let Some(code) = self.exit_code {
            if code != 0 {
                s.push_str(&format!("\n\n[exit code {code}]"));
            }
        }
        if !self.stderr.trim().is_empty() {
            s.push_str("\n\n[stderr]\n");
            s.push_str(&self.stderr);
        }
        if self.truncated {
            s.push_str("\n\n[output truncated to fit the sandbox cap]");
        }
        s
    }

    /// Convert to the durable [`ToolCallRecord`].
    pub fn to_record(&self, input: &Value) -> ToolCallRecord {
        ToolCallRecord {
            call_id: self.call_id.clone(),
            name: self.name.clone(),
            input: input.clone(),
            output_ref: self.output_ref.clone(),
            success: self.success,
            error: self.error.clone(),
            duration_ms: self.duration_ms,
            created_at: now_millis(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_output_shapes() {
        let ok = ToolResult::command("hi".into(), String::new(), Some(0), false);
        assert!(ok.as_model_output().contains("hi"));

        let fail = ToolResult::failure("boom");
        assert!(fail.as_model_output().contains("failed"));
        assert!(fail.as_model_output().contains("boom"));

        let exit3 = ToolResult::command("out".into(), "err".into(), Some(3), false);
        let s = exit3.as_model_output();
        assert!(s.contains("[exit code 3]"), "got: {s}");
        assert!(s.contains("err"), "got: {s}");
    }
}
