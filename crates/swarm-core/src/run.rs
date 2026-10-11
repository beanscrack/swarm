use crate::{
    CoreResult, RunId, RunStatus, StateMachine, TaskId, budget::ResourceBudget,
};
use serde::{Deserialize, Serialize};

string_enum! {
    /// Operating mode of a run. Selects the agent prompt set, tool policy, and the
    /// default decomposition strategy.
    /// General-purpose objective execution.
    pub enum RunMode {
        General => "general",
        Coding => "coding",
        Research => "research",
        Benchmark => "benchmark",
    }
}

/// One user objective executed to completion, possibly across process restarts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Run {
    pub id: RunId,
    pub objective: String,
    pub status: RunStatus,
    /// Operating mode (general, coding, research, benchmark).
    pub mode: RunMode,
    /// Configured provider name, such as `mock`.
    pub provider: String,
    pub model: Option<String>,
    pub root_task_id: Option<TaskId>,
    /// Logical agent target requested by the user (`--agents`). This is a pool
    /// size, not a promise of that many simultaneous inference requests.
    pub agent_target: u32,
    /// Whether paid inference was forbidden when the run was created.
    pub zero_cost: bool,
    pub budget: ResourceBudget,
    pub created_at: i64,
    pub updated_at: i64,
}

impl Run {
    /// Creates a run in the `Created` state.
    pub fn new(
        objective: impl Into<String>,
        provider: impl Into<String>,
        model: Option<String>,
        agent_target: u32,
        zero_cost: bool,
        budget: ResourceBudget,
        now: i64,
    ) -> Self {
        Self {
            id: RunId::new(),
            objective: objective.into(),
            status: RunStatus::Created,
            mode: RunMode::General,
            provider: provider.into(),
            model,
            root_task_id: None,
            agent_target,
            zero_cost,
            budget,
            created_at: now,
            updated_at: now,
        }
    }

    /// Applies a validated state transition.
    pub fn transition(&mut self, next: RunStatus, now: i64) -> CoreResult<()> {
        self.status = self.status.transition_to(next)?;
        self.updated_at = now;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_lifecycle() {
        let mut run = Run::new("obj", "mock", None, 4, true, ResourceBudget::default(), 1);
        assert!(run.transition(RunStatus::Completed, 2).is_err());
        run.transition(RunStatus::Running, 2).unwrap();
        run.transition(RunStatus::Paused, 3).unwrap();
        run.transition(RunStatus::Running, 4).unwrap();
        run.transition(RunStatus::Completed, 5).unwrap();
        assert!(run.transition(RunStatus::Running, 6).is_err());
    }
}
