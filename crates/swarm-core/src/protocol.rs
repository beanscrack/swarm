//! The structured protocol an agent speaks to Swarm: tool requests, findings,
//! plans and the parsed form of a model reply.
//!
//! A model reply is free text; the harness parses it into an [`AgentReply`] by
//! looking for a fenced ```swarm block. This module is the contract between the
//! provider (reasoning) and the runtime (execution).

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::artifact::{Artifact, ArtifactRef};
use crate::ids::MessageId;
use crate::time::now_millis;

/// One tool invocation an agent is requesting.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolRequest {
    pub id: String,
    pub name: String,
    pub input: Value,
    /// Artifact refs the model wants passed in (instead of large inline data).
    pub artifact_refs: Vec<ArtifactRef>,
    pub created_at: i64,
}

impl ToolRequest {
    pub fn new(id: impl Into<String>, name: impl Into<String>, input: Value) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            input,
            artifact_refs: Vec::new(),
            created_at: now_millis(),
        }
    }
}

/// A single step in a proposed plan.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanStep {
    pub description: String,
    /// Optional coarse category ("recon", "implementation", "test", ...).
    pub kind: Option<String>,
    /// Higher runs first when scheduling subtasks.
    pub priority: i32,
}

/// A plan an agent proposes for its task or for its children.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentPlan {
    pub steps: Vec<PlanStep>,
    pub expected_artifacts: Vec<String>,
    pub success_criteria: Vec<String>,
}

/// A durable finding an agent submits to shared memory.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FindingSubmission {
    pub title: String,
    pub content: String,
    pub confidence: f64,
    pub tags: Vec<String>,
    pub artifact_refs: Vec<ArtifactRef>,
    /// When the finding is attached to code, the artifact that holds it.
    pub artifact: Option<Artifact>,
    pub hypothesis_ref: Option<String>,
    pub created_at: i64,
}

impl FindingSubmission {
    pub fn new(title: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            content: content.into(),
            confidence: 0.0,
            tags: Vec::new(),
            artifact_refs: Vec::new(),
            artifact: None,
            hypothesis_ref: None,
            created_at: now_millis(),
        }
    }

    /// Validate the invariant that the title is non-empty and bounded.
    pub fn validate(&self) -> Result<(), crate::error::CoreError> {
        let t = self.title.trim();
        if t.is_empty() {
            return Err(crate::error::CoreError::InvalidFinding("empty finding title".into()));
        }
        if t.len() > 200 {
            return Err(crate::error::CoreError::InvalidFinding(
                "finding title too long (max 200 chars)".into(),
            ));
        }
        Ok(())
    }
}

/// The full parsed reply from one model inference step.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentReply {
    pub id: MessageId,
    pub text: String,
    pub tool_requests: Vec<ToolRequest>,
    pub finding: Option<FindingSubmission>,
    pub plan: Option<AgentPlan>,
    pub done: bool,
    pub done_reason: Option<String>,
    /// The raw model output, kept for auditing and offline replay.
    pub raw: String,
}

impl AgentReply {
    pub fn new(id: MessageId, text: impl Into<String>) -> Self {
        let text = text.into();
        let raw = text.clone();
        Self {
            id,
            text,
            tool_requests: Vec::new(),
            finding: None,
            plan: None,
            done: false,
            done_reason: None,
            raw,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finding_validation() {
        assert!(FindingSubmission::new("ok", "body").validate().is_ok());
        assert!(FindingSubmission::new("   ", "body").validate().is_err());
    }

    #[test]
    fn reply_round_trips() {
        let r = AgentReply::new(MessageId::new(), "hello");
        let json = serde_json::to_string(&r).unwrap();
        assert!(serde_json::from_str::<AgentReply>(&json).is_ok());
    }
}
