use crate::{CoreError, CoreResult};

/// A lifecycle state machine with an explicit, validated transition table.
pub trait StateMachine: Copy + Eq + std::fmt::Debug + std::fmt::Display {
    /// Human-readable entity name used in error messages.
    const ENTITY: &'static str;

    /// Whether the transition `self -> next` is part of the state machine.
    fn can_transition_to(self, next: Self) -> bool;

    /// Terminal states never leave.
    fn is_terminal(self) -> bool;

    /// Validated transition: returns `next` or an [`CoreError::InvalidTransition`].
    fn transition_to(self, next: Self) -> CoreResult<Self> {
        if self.can_transition_to(next) {
            Ok(next)
        } else {
            Err(CoreError::InvalidTransition {
                entity: Self::ENTITY,
                from: self.to_string(),
                to: next.to_string(),
            })
        }
    }
}

string_enum! {
    /// Lifecycle of a logical agent.
    pub enum AgentState {
        Created => "created",
        Ready => "ready",
        Running => "running",
        Waiting => "waiting",
        Paused => "paused",
        Completed => "completed",
        Failed => "failed",
        Cancelled => "cancelled",
    }
}

impl StateMachine for AgentState {
    const ENTITY: &'static str = "agent";

    fn can_transition_to(self, next: Self) -> bool {
        use AgentState::*;
        matches!(
            (self, next),
            (Created, Ready | Failed | Cancelled)
                | (Ready, Running | Paused | Cancelled | Failed)
                // Running -> Ready is crash recovery: the lease expired and the agent is requeued.
                | (Running, Waiting | Ready | Paused | Completed | Failed | Cancelled)
                | (Waiting, Ready | Paused | Failed | Cancelled)
                | (Paused, Ready | Cancelled | Failed)
        )
    }

    fn is_terminal(self) -> bool {
        matches!(
            self,
            AgentState::Completed | AgentState::Failed | AgentState::Cancelled
        )
    }
}

string_enum! {
    /// Lifecycle of a task node in the task graph.
    pub enum TaskState {
        Pending => "pending",
        Ready => "ready",
        Running => "running",
        Waiting => "waiting",
        Completed => "completed",
        Failed => "failed",
        Cancelled => "cancelled",
    }
}

impl StateMachine for TaskState {
    const ENTITY: &'static str = "task";

    fn can_transition_to(self, next: Self) -> bool {
        use TaskState::*;
        matches!(
            (self, next),
            // Pending: dependencies not yet satisfied.
            (Pending, Ready | Failed | Cancelled)
                | (Ready, Running | Failed | Cancelled)
                // Running -> Ready is lease recovery; Running -> Waiting is decomposition.
                | (Running, Waiting | Ready | Completed | Failed | Cancelled)
                | (Waiting, Ready | Failed | Cancelled)
        )
    }

    fn is_terminal(self) -> bool {
        matches!(
            self,
            TaskState::Completed | TaskState::Failed | TaskState::Cancelled
        )
    }
}

string_enum! {
    /// Lifecycle of a run (one user objective).
    pub enum RunStatus {
        Created => "created",
        Running => "running",
        Paused => "paused",
        Completed => "completed",
        Failed => "failed",
        Cancelled => "cancelled",
    }
}

impl StateMachine for RunStatus {
    const ENTITY: &'static str = "run";

    fn can_transition_to(self, next: Self) -> bool {
        use RunStatus::*;
        matches!(
            (self, next),
            (Created, Running | Cancelled)
                | (Running, Paused | Completed | Failed | Cancelled)
                | (Paused, Running | Cancelled | Failed)
        )
    }

    fn is_terminal(self) -> bool {
        matches!(
            self,
            RunStatus::Completed | RunStatus::Failed | RunStatus::Cancelled
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Exhaustively checks every (from, to) pair against the expected table.
    fn check_table<S: StateMachine>(all: &[S], allowed: &[(S, S)]) {
        for &from in all {
            for &to in all {
                let expected = allowed.contains(&(from, to));
                assert_eq!(
                    from.can_transition_to(to),
                    expected,
                    "{} transition {from} -> {to}",
                    S::ENTITY
                );
                assert_eq!(from.transition_to(to).is_ok(), expected);
            }
        }
    }

    #[test]
    fn agent_transition_table() {
        use AgentState::*;
        let allowed = [
            (Created, Ready),
            (Created, Failed),
            (Created, Cancelled),
            (Ready, Running),
            (Ready, Paused),
            (Ready, Cancelled),
            (Ready, Failed),
            (Running, Waiting),
            (Running, Ready),
            (Running, Paused),
            (Running, Completed),
            (Running, Failed),
            (Running, Cancelled),
            (Waiting, Ready),
            (Waiting, Paused),
            (Waiting, Failed),
            (Waiting, Cancelled),
            (Paused, Ready),
            (Paused, Cancelled),
            (Paused, Failed),
        ];
        check_table(AgentState::ALL, &allowed);
    }

    #[test]
    fn task_transition_table() {
        use TaskState::*;
        let allowed = [
            (Pending, Ready),
            (Pending, Failed),
            (Pending, Cancelled),
            (Ready, Running),
            (Ready, Failed),
            (Ready, Cancelled),
            (Running, Waiting),
            (Running, Ready),
            (Running, Completed),
            (Running, Failed),
            (Running, Cancelled),
            (Waiting, Ready),
            (Waiting, Failed),
            (Waiting, Cancelled),
        ];
        check_table(TaskState::ALL, &allowed);
    }

    #[test]
    fn run_transition_table() {
        use RunStatus::*;
        let allowed = [
            (Created, Running),
            (Created, Cancelled),
            (Running, Paused),
            (Running, Completed),
            (Running, Failed),
            (Running, Cancelled),
            (Paused, Running),
            (Paused, Cancelled),
            (Paused, Failed),
        ];
        check_table(RunStatus::ALL, &allowed);
    }

    #[test]
    fn terminal_states_have_no_exits() {
        for &state in AgentState::ALL.iter().filter(|s| s.is_terminal()) {
            assert!(AgentState::ALL.iter().all(|&to| !state.can_transition_to(to)));
        }
        for &state in TaskState::ALL.iter().filter(|s| s.is_terminal()) {
            assert!(TaskState::ALL.iter().all(|&to| !state.can_transition_to(to)));
        }
        for &state in RunStatus::ALL.iter().filter(|s| s.is_terminal()) {
            assert!(RunStatus::ALL.iter().all(|&to| !state.can_transition_to(to)));
        }
    }

    #[test]
    fn serde_and_display_agree_for_every_variant() {
        for &state in AgentState::ALL {
            assert_eq!(
                serde_json::to_string(&state).unwrap(),
                format!("\"{}\"", state.as_str())
            );
            assert_eq!(state.as_str().parse::<AgentState>().unwrap(), state);
        }
        for &state in TaskState::ALL {
            assert_eq!(
                serde_json::to_string(&state).unwrap(),
                format!("\"{}\"", state.as_str())
            );
        }
        for &state in RunStatus::ALL {
            assert_eq!(
                serde_json::to_string(&state).unwrap(),
                format!("\"{}\"", state.as_str())
            );
        }
    }
}
