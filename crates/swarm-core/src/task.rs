use crate::{CoreError, CoreResult, RunId, StateMachine, TaskId, TaskState};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};

string_enum! {
    /// Role an agent plays for a task.
    pub enum AgentRole {
        /// Decomposes an objective into subtasks and merges child results.
        Planner => "planner",
        /// Executes a leaf objective with tools.
        Worker => "worker",
        /// Independently checks another agent's artifact.
        Reviewer => "reviewer",
    }
}

/// A node in the hierarchical task graph.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Task {
    pub id: TaskId,
    pub run_id: RunId,
    pub parent_id: Option<TaskId>,
    /// Distance from the root task (the root has depth 0).
    pub depth: u32,
    pub title: String,
    pub objective: String,
    pub role: AgentRole,
    /// Fairness group: the scheduler round-robins across groups.
    pub group: String,
    /// Higher values are dispatched first within the same group.
    pub priority: i32,
    pub depends_on: Vec<TaskId>,
    pub state: TaskState,
    /// Number of times an agent has started this task.
    pub attempts: u32,
    pub created_at: i64,
    pub updated_at: i64,
}

impl Task {
    /// Applies a validated state transition.
    pub fn transition(&mut self, next: TaskState, now: i64) -> CoreResult<()> {
        self.state = self.state.transition_to(next)?;
        self.updated_at = now;
        Ok(())
    }
}

/// Caller-supplied description of a task before it is persisted.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskSpec {
    pub title: String,
    pub objective: String,
    pub role: AgentRole,
    pub group: String,
    pub priority: i32,
    pub depends_on: Vec<TaskId>,
}

/// Validates that `edges` (task, dependencies) form a DAG and returns a
/// topological order, dependencies first.
///
/// Rejects duplicate tasks, dependencies on unknown tasks and cycles. Each
/// error names the offending task so it can be reported to the user or model.
pub fn topological_order(edges: &[(TaskId, Vec<TaskId>)]) -> CoreResult<Vec<TaskId>> {
    let mut known: HashSet<&TaskId> = HashSet::with_capacity(edges.len());
    for (task, _) in edges {
        if !known.insert(task) {
            return Err(CoreError::DuplicateTask(task.to_string()));
        }
    }

    let mut indegree: HashMap<&TaskId, usize> = HashMap::with_capacity(edges.len());
    let mut dependents: HashMap<&TaskId, Vec<&TaskId>> = HashMap::with_capacity(edges.len());
    for (task, deps) in edges {
        indegree.entry(task).or_insert(0);
        for dep in deps {
            if !known.contains(dep) {
                return Err(CoreError::UnknownDependency {
                    task: task.to_string(),
                    dependency: dep.to_string(),
                });
            }
            *indegree.entry(task).or_insert(0) += 1;
            dependents.entry(dep).or_default().push(task);
        }
    }

    // Kahn's algorithm over the input order, so the result is deterministic.
    let mut queue: VecDeque<&TaskId> = edges
        .iter()
        .map(|(task, _)| task)
        .filter(|task| indegree.get(task).copied().unwrap_or(0) == 0)
        .collect();
    let mut order = Vec::with_capacity(edges.len());
    while let Some(task) = queue.pop_front() {
        order.push(task.clone());
        if let Some(children) = dependents.get(task) {
            for child in children {
                let degree = indegree.get_mut(child).expect("child tracked in indegree");
                *degree -= 1;
                if *degree == 0 {
                    queue.push_back(child);
                }
            }
        }
    }

    if order.len() != edges.len() {
        let stuck = edges
            .iter()
            .map(|(task, _)| task)
            .find(|task| indegree.get(task).copied().unwrap_or(0) > 0)
            .expect("unresolved task exists when order is short");
        return Err(CoreError::DependencyCycle(stuck.to_string()));
    }
    Ok(order)
}

/// Returns true when every dependency is in `completed`.
pub fn dependencies_satisfied(depends_on: &[TaskId], completed: &HashSet<TaskId>) -> bool {
    depends_on.iter().all(|dep| completed.contains(dep))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: u8) -> TaskId {
        TaskId::parse(&format!("task_{n:02}")).unwrap()
    }

    #[test]
    fn orders_dependencies_first() {
        let edges = vec![
            (id(3), vec![id(1), id(2)]),
            (id(1), vec![]),
            (id(2), vec![id(1)]),
        ];
        let order = topological_order(&edges).unwrap();
        let pos = |t: &TaskId| order.iter().position(|x| x == t).unwrap();
        assert!(pos(&id(1)) < pos(&id(2)));
        assert!(pos(&id(2)) < pos(&id(3)));
        assert_eq!(order.len(), 3);
    }

    #[test]
    fn rejects_cycles() {
        let edges = vec![(id(1), vec![id(2)]), (id(2), vec![id(3)]), (id(3), vec![id(1)])];
        assert!(matches!(
            topological_order(&edges),
            Err(CoreError::DependencyCycle(_))
        ));
    }

    #[test]
    fn rejects_self_dependency() {
        let edges = vec![(id(1), vec![id(1)])];
        assert!(matches!(
            topological_order(&edges),
            Err(CoreError::DependencyCycle(_))
        ));
    }

    #[test]
    fn rejects_unknown_and_duplicate_tasks() {
        let unknown = vec![(id(1), vec![id(9)])];
        assert!(matches!(
            topological_order(&unknown),
            Err(CoreError::UnknownDependency { .. })
        ));
        let duplicate = vec![(id(1), vec![]), (id(1), vec![])];
        assert!(matches!(
            topological_order(&duplicate),
            Err(CoreError::DuplicateTask(_))
        ));
    }

    #[test]
    fn dependency_satisfaction() {
        let completed: HashSet<TaskId> = [id(1), id(2)].into_iter().collect();
        assert!(dependencies_satisfied(&[id(1)], &completed));
        assert!(dependencies_satisfied(&[], &completed));
        assert!(!dependencies_satisfied(&[id(1), id(3)], &completed));
    }

    #[test]
    fn task_transition_is_validated() {
        let mut task = Task {
            id: id(1),
            run_id: RunId::new(),
            parent_id: None,
            depth: 0,
            title: "t".into(),
            objective: "o".into(),
            role: AgentRole::Worker,
            group: "g".into(),
            priority: 0,
            depends_on: vec![],
            state: TaskState::Pending,
            attempts: 0,
            created_at: 0,
            updated_at: 0,
        };
        assert!(task.transition(TaskState::Completed, 5).is_err());
        assert_eq!(task.state, TaskState::Pending);
        task.transition(TaskState::Ready, 5).unwrap();
        assert_eq!(task.updated_at, 5);
    }
}
