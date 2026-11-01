//! Sandbox configuration and the path/containment helpers that enforce it.
//!
//! The sandbox is *workspace confinement*: every tool-supplied path is resolved
//! against the workspace root and rejected if it escapes; every command runs
//! with the workspace as its working directory. (Full filesystem/network
//! isolation is a future, OS-level concern.)

use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use crate::error::ToolError;

/// Default timeout for a single command tool call.
pub const DEFAULT_COMMAND_TIMEOUT: Duration = Duration::from_secs(120);
/// Default cap on captured output bytes per tool call.
pub const DEFAULT_OUTPUT_CAP: u64 = 256 * 1024;

/// A workspace-confined sandbox for tool execution.
#[derive(Debug, Clone)]
pub struct SandboxConfig {
    pub workspace_root: PathBuf,
    pub command_timeout: Duration,
    pub output_cap_bytes: u64,
    /// Whether tools may initiate network I/O (informational in v1).
    pub allow_network: bool,
    /// Refs `git push` may target from a worker shell: branch, `remote/branch`, or `*`.
    /// Empty refuses every push. A guardrail on the command string, not a sandbox.
    pub allow_push: Vec<String>,
    pub docker_image: Option<String>,
}

impl SandboxConfig {
    pub fn new(workspace_root: impl Into<PathBuf>) -> Self {
        let root: PathBuf = workspace_root.into();
        // A relative root (`./work`) breaks as soon as a tool runs a child process
        // from inside it: a script path built from the root is then resolved
        // against the root again (`work/work/x.py`). Absolute from the start, so
        // every tool and every child sees the same path.
        let root = std::path::absolute(&root).unwrap_or(root);
        Self {
            workspace_root: root,
            command_timeout: DEFAULT_COMMAND_TIMEOUT,
            output_cap_bytes: DEFAULT_OUTPUT_CAP,
            allow_network: true,
            allow_push: Vec::new(),
            docker_image: None,
        }
    }

    pub fn command_timeout(mut self, d: Duration) -> Self {
        self.command_timeout = d;
        self
    }

    pub fn output_cap_bytes(mut self, n: u64) -> Self {
        self.output_cap_bytes = n;
        self
    }

    pub fn allow_network(mut self, b: bool) -> Self {
        self.allow_network = b;
        self
    }

    pub fn allow_push(mut self, refs: Vec<String>) -> Self {
        self.allow_push = refs;
        self
    }

    pub fn root(&self) -> &Path {
        &self.workspace_root
    }

    pub fn docker_image(mut self, image: Option<String>) -> Self {
        self.docker_image = image;
        self
    }

    /// Resolve a tool-supplied path to an absolute path inside the sandbox, or
    /// [`ToolError::PathEscape`] if it would land outside it.
    pub fn resolve(&self, p: &str) -> Result<PathBuf, ToolError> {
        resolve_within(&self.workspace_root, p)
    }
}

/// Lexically resolve `.` and `..` components without touching the filesystem.
fn normalize_lexical(path: &Path) -> PathBuf {
    let mut comps: Vec<Component<'_>> = Vec::new();
    for c in path.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                if comps
                    .last()
                    .copied()
                    .is_some_and(|c| matches!(c, Component::Normal(_)))
                {
                    comps.pop();
                }
            }
            other => comps.push(other),
        }
    }
    if comps.is_empty() {
        PathBuf::from(".")
    } else {
        comps.iter().cloned().collect()
    }
}

/// Resolve `p` against `root`, then verify the result stays under `root`.
pub fn resolve_within(root: &Path, p: &str) -> Result<PathBuf, ToolError> {
    let trimmed = p.trim();
    if trimmed.is_empty() {
        return Ok(normalize_lexical(root));
    }
    let candidate = if Path::new(trimmed).is_absolute() {
        PathBuf::from(trimmed)
    } else {
        root.join(trimmed)
    };
    let normalized = normalize_lexical(&candidate);
    let root_norm = normalize_lexical(root);
    if normalized == root_norm || normalized.starts_with(&root_norm) {
        if let Ok(actual_root) = std::fs::canonicalize(&root_norm) {
            let mut ancestor = normalized.as_path();
            while std::fs::symlink_metadata(ancestor).is_err() {
                let Some(parent) = ancestor.parent() else { break; }; ancestor = parent;
            }
            let actual = std::fs::canonicalize(ancestor).map_err(|_| ToolError::PathEscape { path:p.into() })?;
            if !actual.starts_with(actual_root) { return Err(ToolError::PathEscape { path:p.into() }); }
        }
        Ok(normalized)
    } else {
        Err(ToolError::PathEscape {
            path: p.to_string(),
        })
    }
}

/// Lossily convert captured bytes to text, capping at `cap` bytes. Returns the
/// text and whether it was truncated.
pub fn cap_output(bytes: &[u8], cap: u64) -> (String, bool) {
    let total = bytes.len() as u64;
    if total > cap {
        let n = cap as usize;
        (String::from_utf8_lossy(&bytes[..n]).to_string(), true)
    } else {
        (String::from_utf8_lossy(bytes).to_string(), false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_collapses_dots() {
        let n = normalize_lexical(Path::new("/a/b/../../c"));
        assert_eq!(n, Path::new("/c"));
    }

    #[test]
    fn resolve_within_allows_inside() {
        let root = Path::new("/work");
        let p = resolve_within(root, "src/main.rs").unwrap();
        assert!(p.starts_with(root));
        // Traversal that stays inside is fine.
        let p2 = resolve_within(root, "src/../ok.txt").unwrap();
        assert!(p2.starts_with(root));
    }

    #[test]
    fn resolve_within_rejects_escape() {
        let root = Path::new("/work");
        assert!(resolve_within(root, "../etc/passwd").is_err());
        // Repeated parent that climbs above the root.
        assert!(resolve_within(root, "a/../../x").is_err());
    }

    #[test]
    fn cap_output_truncates() {
        let (s, t) = cap_output(b"hello world", 5);
        assert_eq!(s, "hello");
        assert!(t);
        let (s2, t2) = cap_output(b"hi", 100);
        assert_eq!(s2, "hi");
        assert!(!t2);
    }
}
