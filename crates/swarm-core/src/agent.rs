use crate::{
    AgentId, AgentRole, AgentState, CoreResult, RunId, StateMachine, TaskId,
    budget::BudgetUsage, provider::ChatMessage, tool::ToolCallRecord,
};
use serde::{Deserialize, Serialize};

/// A logical agent: an executor bound to exactly one task attempt.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Agent {
    pub id: AgentId,
    pub run_id: RunId,
    pub task_id: TaskId,
    pub parent_id: Option<AgentId>,
    pub role: AgentRole,
    pub depth: u32,
    pub state: AgentState,
    /// Model inference steps taken so far.
    pub steps: u32,
    pub created_at: i64,
    pub updated_at: i64,
}

impl Agent {
    /// Creates an agent in the `Created` state.
    pub fn new(
        run_id: RunId,
        task_id: TaskId,
        parent_id: Option<AgentId>,
        role: AgentRole,
        depth: u32,
        now: i64,
    ) -> Self {
        Self {
            id: AgentId::new(),
            run_id,
            task_id,
            parent_id,
            role,
            depth,
            state: AgentState::Created,
            steps: 0,
            created_at: now,
            updated_at: now,
        }
    }

    /// Applies a validated state transition.
    pub fn transition(&mut self, next: AgentState, now: i64) -> CoreResult<()> {
        self.state = self.state.transition_to(next)?;
        self.updated_at = now;
        Ok(())
    }
}

/// Durable snapshot of an agent's conversation and progress.
///
/// Saved after every step so a restarted process resumes from the last
/// completed step instead of repeating inference.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct AgentCheckpoint {
    /// Checkpoint schema version.
    pub version: u32,
    pub steps: u32,
    /// Consecutive malformed replies for the current step.
    pub format_retries: u32,
    pub messages: Vec<ChatMessage>,
    pub tool_calls: Vec<ToolCallRecord>,
    pub child_task_ids: Vec<TaskId>,
    pub usage: BudgetUsage,
}

impl AgentCheckpoint {
    /// Current checkpoint schema version.
    pub const VERSION: u32 = 1;

    /// Empty checkpoint at the current schema version.
    pub fn new() -> Self {
        Self {
            version: Self::VERSION,
            ..Self::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_agent_starts_created_and_transitions_are_validated() {
        let mut agent = Agent::new(
            RunId::new(),
            TaskId::new(),
            None,
            AgentRole::Worker,
            0,
            10,
        );
        assert_eq!(agent.state, AgentState::Created);
        assert!(agent.transition(AgentState::Completed, 11).is_err());
        agent.transition(AgentState::Ready, 11).unwrap();
        agent.transition(AgentState::Running, 12).unwrap();
        assert_eq!(agent.updated_at, 12);
    }

    #[test]
    fn checkpoint_defaults_to_current_version() {
        assert_eq!(AgentCheckpoint::new().version, AgentCheckpoint::VERSION);
    }
}
