//! Persistent, append-only run events.
//!
//! The runtime appends a `RunEvent` for every significant transition. These are
//! written to the store (durable) and mirrored to a bounded in-memory stream
//! for the TUI and live status. The store keeps the authoritative, ordered log;
//! this module only holds the shape.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::ids::{AgentId, RunId, TaskId};
use crate::time::now_millis;

string_enum! {
        /// The category of a run event.
        pub enum EventKind {
        AgentStateChanged => "agent_state_changed",
        TaskStateChanged => "task_state_changed",
        RunStateChanged => "run_state_changed",
        InferenceStarted => "inference_started",
        InferenceCompleted => "inference_completed",
        InferenceFailed => "inference_failed",
        ToolRequested => "tool_requested",
        ApprovalRequested => "approval_requested",
        ApprovalResolved => "approval_resolved",
        ToolResult => "tool_result",
        FindingSubmitted => "finding_submitted",
        ArtifactCreated => "artifact_created",
        ResearchUpdated => "research_updated",
        CheckpointSaved => "checkpoint_saved",
        Message => "message",
        Log => "log",
    }
}

/// One entry in a run's event log.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunEvent {
    /// Monotonic per-run sequence number, assigned by the store.
    pub seq: u64,
    pub run_id: RunId,
    pub kind: EventKind,
    /// Structured payload; the schema depends on `kind`.
    pub payload: Value,
    pub agent_id: Option<AgentId>,
    pub task_id: Option<TaskId>,
    pub created_at: i64,
}

impl RunEvent {
    pub fn new(kind: EventKind, run_id: RunId, payload: Value) -> Self {
        Self {
            seq: 0,
            run_id,
            kind,
            payload,
            agent_id: None,
            task_id: None,
            created_at: now_millis(),
        }
    }

    pub fn with_agent(mut self, id: AgentId) -> Self {
        self.agent_id = Some(id);
        self
    }

    pub fn with_task(mut self, id: TaskId) -> Self {
        self.task_id = Some(id);
        self
    }

    pub fn with_seq(mut self, seq: u64) -> Self {
        self.seq = seq;
        self
    }
}

/// A bounded, in-memory view of a run's event stream (for the TUI and status).
/// The durable, process-level log is the store's append-only event table.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EventStream {
    pub events: Vec<RunEvent>,
}

impl EventStream {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, event: RunEvent) {
        self.events.push(event);
    }

    pub fn len(&self) -> usize {
        self.events.len()
    }

    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    pub fn drain(&mut self) -> std::vec::Drain<'_, RunEvent> {
        self.events.drain(..)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_round_trips() {
        let e = RunEvent::new(EventKind::Log, RunId::new(), serde_json::json!({"msg":"hi"}));
        let back: RunEvent = serde_json::from_str(&serde_json::to_string(&e).unwrap()).unwrap();
        assert_eq!(back.kind, EventKind::Log);
        assert_eq!(back.payload, serde_json::json!({"msg":"hi"}));
    }
}
