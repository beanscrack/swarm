//! The built-in tool catalog: the standard tools an agent can request, each
//! with a JSON input schema the model sees.

use serde_json::{Value, json};

use swarm_core::tool::{ToolCatalog, ToolKind, ToolSchema, ToolSpec};

/// Build a [`ToolSpec`] with the given output cap.
fn spec(name: &str, kind: ToolKind, desc: &str, schema: Value, max_output: u64) -> ToolSpec {
    let process_tool = matches!(kind, ToolKind::Bash | ToolKind::Test | ToolKind::Python | ToolKind::Git);
    let mut s = ToolSpec::new(name, kind, desc, ToolSchema::new(schema));
    s.max_output_bytes = max_output;
    // A working directory is not OS-level confinement: these tools can still
    // address paths outside the workspace and must be explicitly approved.
    if process_tool {
        s.sandboxed = false;
        s.requires_approval = true;
    }
    s
}

/// The default tool set: file read/write/edit, listing, search, directory
/// creation, shell, tests, Python, and git.
pub fn builtin_catalog() -> ToolCatalog {
    let mut c = ToolCatalog::new();

    c.register(spec(
        "bash",
        ToolKind::Bash,
        "Run a shell command in the workspace and return stdout, stderr, and the exit code.",
        json!({
            "type": "object",
            "properties": {
                "command": {"type": "string"},
                "cwd": {"type": "string"},
                "timeout_ms": {"type": "integer"}
            },
            "required": ["command"]
        }),
        256 * 1024,
    ));

    c.register(spec(
        "read_file",
        ToolKind::Read,
        "Read a file from the workspace. Optionally paginate by 1-based start_line and limit.",
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "start_line": {"type": "integer"},
                "limit": {"type": "integer"}
            },
            "required": ["path"]
        }),
        256 * 1024,
    ));

    c.register(spec(
        "write_file",
        ToolKind::Write,
        "Create or overwrite a file in the workspace with the given text content.",
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "content": {"type": "string"}
            },
            "required": ["path", "content"]
        }),
        64 * 1024,
    ));

    c.register(spec(
        "edit_file",
        ToolKind::Edit,
        "Replace an exact `find` substring in a file. Must be unique unless replace_all is true.",
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "find": {"type": "string"},
                "replace": {"type": "string"},
                "replace_all": {"type": "boolean"}
            },
            "required": ["path", "find"]
        }),
        64 * 1024,
    ));

    c.register(spec(
        "list_files",
        ToolKind::Search,
        "Recursively list files under a workspace-relative directory (bounded).",
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "max_entries": {"type": "integer"}
            }
        }),
        128 * 1024,
    ));

    c.register(spec(
        "search_files",
        ToolKind::Search,
        "Search file contents for a substring and return path:line: text hits.",
        json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string"},
                "path": {"type": "string"},
                "case_insensitive": {"type": "boolean"},
                "max_matches": {"type": "integer"}
            },
            "required": ["pattern"]
        }),
        128 * 1024,
    ));

    c.register(spec(
        "create_directory",
        ToolKind::Write,
        "Create a directory (and parents) inside the workspace.",
        json!({
            "type": "object",
            "properties": {"path": {"type": "string"}},
            "required": ["path"]
        }),
        64 * 1024,
    ));

    c.register(spec(
        "run_tests",
        ToolKind::Test,
        "Run a test command in the workspace. Defaults to `cargo test`.",
        json!({
            "type": "object",
            "properties": {
                "command": {"type": "string"},
                "cwd": {"type": "string"},
                "timeout_ms": {"type": "integer"}
            }
        }),
        256 * 1024,
    ));

    c.register(spec(
        "python",
        ToolKind::Python,
        "Run a Python program in the workspace and return stdout/stderr/exit.",
        json!({
            "type": "object",
            "properties": {
                "code": {"type": "string"},
                "timeout_ms": {"type": "integer"}
            },
            "required": ["code"]
        }),
        256 * 1024,
    ));

    c.register(spec(
        "git",
        ToolKind::Git,
        "Run a git command in the workspace. `args` is the argv after `git`.",
        json!({
            "type": "object",
            "properties": {
                "args": {"type": "array", "items": {"type": "string"}},
                "timeout_ms": {"type": "integer"}
            },
            "required": ["args"]
        }),
        128 * 1024,
    ));

    c
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_catalog_is_complete() {
        let c = builtin_catalog();
        let expected = [
            "bash",
            "read_file",
            "write_file",
            "edit_file",
            "list_files",
            "search_files",
            "create_directory",
            "run_tests",
            "python",
            "git",
        ];
        for name in expected {
            let s = c.get(name).unwrap_or_else(|| panic!("missing tool {name}"));
            let process_tool = matches!(name, "bash" | "run_tests" | "python" | "git");
            assert_eq!(s.sandboxed, !process_tool);
            assert_eq!(s.requires_approval, process_tool);
            assert!(s.input_schema.as_value().is_object());
        }
        assert_eq!(c.len(), expected.len());
    }
}
