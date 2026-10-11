//! Tools: static specs, JSON-schema inputs, the resolved catalog, validation
//! classification and durable per-call records.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::artifact::ArtifactRef;
use crate::time::now_millis;

string_enum! {
        /// The family a tool belongs to. Drives default sandbox rules.
        pub enum ToolKind {
        Bash => "bash",
        Read => "read",
        Write => "write",
        Edit => "edit",
        Search => "search",
        Git => "git",
        Python => "python",
        Test => "test",
        InspectArtifact => "inspect_artifact",
        RegisterArtifact => "register_artifact",
        Symbol => "symbol",
        Custom => "custom",
    }
}

/// A JSON-Schema description of a tool's input payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolSchema(pub Value);

impl ToolSchema {
    pub fn new(schema: Value) -> Self {
        Self(schema)
    }

    /// A permissive schema that accepts any JSON object.
    pub fn passthrough() -> Self {
        Self(Value::Object(Default::default()))
    }

    pub fn as_value(&self) -> &Value {
        &self.0
    }
}

impl From<Value> for ToolSchema {
    fn from(v: Value) -> Self {
        Self(v)
    }
}

/// Static description of a tool an agent may request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub kind: ToolKind,
    pub description: String,
    pub input_schema: ToolSchema,
    /// Whether execution is confined to the sandbox (almost always yes).
    pub sandboxed: bool,
    /// Maximum bytes of stdout/stderr returned before truncation.
    pub max_output_bytes: u64,
    /// Whether the tool must be approved before it runs.
    pub requires_approval: bool,
}

impl ToolSpec {
    pub fn new(
        name: impl Into<String>,
        kind: ToolKind,
        description: impl Into<String>,
        input_schema: ToolSchema,
    ) -> Self {
        Self {
            name: name.into(),
            kind,
            description: description.into(),
            input_schema,
            sandboxed: true,
            max_output_bytes: 64 * 1024,
            requires_approval: false,
        }
    }
}

/// The catalog of tools available to an agent, resolved for a run.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ToolCatalog {
    pub specs: Vec<ToolSpec>,
}

impl ToolCatalog {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, spec: ToolSpec) {
        self.specs.push(spec);
    }

    pub fn get(&self, name: &str) -> Option<&ToolSpec> {
        self.specs.iter().find(|s| s.name == name)
    }

    pub fn len(&self) -> usize {
        self.specs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.specs.is_empty()
    }
}

/// The classification of a tool request, checked before execution.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ToolRequestValidation {
    /// Safe to execute automatically under the sandbox.
    AutoApprove { reason: String },
    /// Requires explicit (or policy) approval before execution.
    NeedsApproval { risk: String },
    /// Refused by policy (for example, a path escapes the sandbox).
    Deny { reason: String },
}

impl ToolRequestValidation {
    pub fn is_auto(&self) -> bool {
        matches!(self, Self::AutoApprove { .. })
    }
}

/// A durable record of a single tool call an agent made (used in checkpoints).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCallRecord {
    pub call_id: String,
    pub name: String,
    pub input: Value,
    pub output_ref: Option<ArtifactRef>,
    pub success: bool,
    pub error: Option<String>,
    pub duration_ms: u64,
    pub created_at: i64,
}

impl ToolCallRecord {
    pub fn new(call_id: impl Into<String>, name: impl Into<String>, input: Value) -> Self {
        Self {
            call_id: call_id.into(),
            name: name.into(),
            input,
            output_ref: None,
            success: false,
            error: None,
            duration_ms: 0,
            created_at: now_millis(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_lookup() {
        let mut c = ToolCatalog::new();
        c.register(ToolSpec::new("bash", ToolKind::Bash, "run a command", ToolSchema::passthrough()));
        assert_eq!(c.len(), 1);
        assert!(c.get("bash").is_some());
        assert!(c.get("nope").is_none());
    }
}
