//! Hooks around tool calls: a place for the host (the CLI) to look at a call
//! before it runs, and at what came back.
//!
//! The runtime does not know what a hook is. It asks, and acts on the answer:
//! a block turns the call into a denial the model is told the reason for, and a
//! note is shown to the model next to the tool's own output.

use async_trait::async_trait;
use serde_json::Value;
use swarm_core::RunId;

/// A tool call, as hooks see it.
#[derive(Debug, Clone)]
pub struct HookCall {
    pub run_id: RunId,
    pub name: String,
    pub arguments: Value,
}

#[async_trait]
pub trait ToolHooks: Send + Sync {
    /// Before the call, and before anyone is asked to approve it. `Some(reason)`
    /// stops the call; the reason goes back to the model.
    async fn before(&self, call: &HookCall) -> Option<String>;

    /// After the call ran, with what the model is about to be shown. `Some(note)`
    /// is added to that output.
    async fn after(&self, call: &HookCall, success: bool, output: &str) -> Option<String>;
}
