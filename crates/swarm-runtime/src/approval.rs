//! Approval requests emitted before a tool call that needs human consent.

use async_trait::async_trait;
use serde_json::Value;
use swarm_core::{AgentId, RunId, TaskId};

#[derive(Debug, Clone)]
pub struct ApprovalRequest {
    pub run_id: RunId,
    pub task_id: TaskId,
    pub agent_id: AgentId,
    pub call_id: String,
    pub name: String,
    pub arguments: Value,
    pub risk: String,
}

#[async_trait]
pub trait ApprovalHandler: Send + Sync {
    /// Return true only after the user explicitly approves this exact call.
    async fn approve(&self, request: &ApprovalRequest) -> bool;
}
