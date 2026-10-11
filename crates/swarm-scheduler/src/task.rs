//! The unit the scheduler ranks: a schedulable projection of a task.

use std::cmp::Ordering;
use std::collections::{BTreeMap, HashSet};

use swarm_core::task::{Task, dependencies_satisfied};
use swarm_core::{TaskId, TaskState};

/// A task as the scheduler sees it.
///
/// This is a projection of [`swarm_core::task::Task`] carrying exactly the
/// fields a scheduling decision needs. Keeping it decoupled from the full domain
/// type lets the policy be reasoned about and tested without a database, and
/// lets the runtime add context a bare task row does not carry — most
/// importantly the provider the run will dispatch to.
#[derive(Debug, Clone, PartialEq)]
pub struct SchedulableTask {
    /// The task's id.
    pub id: TaskId,
    /// Research group; the scheduler round-robins across groups.
    pub group: String,
    /// Provider the task will be dispatched to (drawn from the run).
    pub provider: String,
    /// Base priority; higher values are dispatched first.
    pub priority: i32,
    /// Depth in the task tree (root is 0).
    pub depth: u32,
    /// Creation time (millis); earlier tasks win ties within a priority.
    pub created_at: i64,
    /// True when the task is in a dispatchable state with dependencies met.
    pub ready: bool,
}

impl SchedulableTask {
    /// A ready candidate with default depth — convenient for tests and for
    /// runtimes that already track tasks in memory.
    pub fn new(
        id: TaskId,
        group: String,
        provider: String,
        priority: i32,
        created_at: i64,
    ) -> Self {
        Self {
            id,
            group,
            provider,
            priority,
            depth: 0,
            created_at,
            ready: true,
        }
    }

    /// Project a domain task into a candidate.
    ///
    /// `completed` is the set of task ids that have finished successfully. A
    /// candidate is *ready* when its task is in the [`TaskState::Ready`] state
    /// and every dependency is in `completed`.
    pub fn from_task(task: &Task, provider: &str, completed: &HashSet<TaskId>) -> Self {
        let ready =
            task.state == TaskState::Ready && dependencies_satisfied(&task.depends_on, completed);
        Self {
            id: task.id.clone(),
            group: task.group.clone(),
            provider: provider.to_string(),
            priority: task.priority,
            depth: task.depth,
            created_at: task.created_at,
            ready,
        }
    }
}

/// A candidate's effective priority after its group's research boost is added.
///
/// The runtime derives boosts from [`swarm_core::research`] (hypothesis
/// confidence and evidence weight) keyed by a task's group; the scheduler only
/// applies them, which keeps the policy independent of how scores are computed.
pub fn effective_priority(task: &SchedulableTask, boosts: &BTreeMap<String, i32>) -> i32 {
    task.priority + boosts.get(&task.group).copied().unwrap_or(0)
}

/// Strict total ordering used to rank candidates: higher effective priority
/// first, then earlier-created first, then by id — so a plan is deterministic
/// for a given input.
pub fn compare(
    a: &SchedulableTask,
    b: &SchedulableTask,
    boosts: &BTreeMap<String, i32>,
) -> Ordering {
    effective_priority(b, boosts)
        .cmp(&effective_priority(a, boosts))
        .then_with(|| a.created_at.cmp(&b.created_at))
        .then_with(|| a.id.cmp(&b.id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use swarm_core::AgentRole;
    use swarm_core::RunId;

    fn mk_task(id: &str, state: TaskState, deps: Vec<TaskId>) -> Task {
        Task {
            id: TaskId::parse(id).unwrap(),
            run_id: RunId::new(),
            parent_id: None,
            depth: 0,
            title: "t".into(),
            objective: "o".into(),
            role: AgentRole::Worker,
            group: "g".into(),
            priority: 0,
            depends_on: deps,
            state,
            attempts: 0,
            created_at: 0,
            updated_at: 0,
        }
    }

    #[test]
    fn ready_requires_ready_state_and_met_deps() {
        let empty: HashSet<TaskId> = HashSet::new();
        let dep = TaskId::parse("task_dep").unwrap();
        let done: HashSet<TaskId> = HashSet::from([dep.clone()]);

        // Ready, no dependencies -> ready.
        let a = SchedulableTask::from_task(
            &mk_task("task_01", TaskState::Ready, vec![]),
            "mock",
            &empty,
        );
        assert!(a.ready);

        // Ready, dependency not completed -> not ready.
        let b = SchedulableTask::from_task(
            &mk_task("task_02", TaskState::Ready, vec![dep.clone()]),
            "mock",
            &empty,
        );
        assert!(!b.ready);

        // Ready, dependency completed -> ready.
        let c = SchedulableTask::from_task(
            &mk_task("task_03", TaskState::Ready, vec![dep.clone()]),
            "mock",
            &done,
        );
        assert!(c.ready);

        // Not in the Ready state -> not ready even with no dependencies.
        let d = SchedulableTask::from_task(
            &mk_task("task_04", TaskState::Pending, vec![]),
            "mock",
            &empty,
        );
        assert!(!d.ready);
    }

    #[test]
    fn compare_orders_by_priority_then_time_then_id() {
        let boosts = BTreeMap::new();
        let a = SchedulableTask::new(
            TaskId::parse("task_01").unwrap(),
            "g".into(),
            "mock".into(),
            5,
            10,
        );
        let b = SchedulableTask::new(
            TaskId::parse("task_02").unwrap(),
            "g".into(),
            "mock".into(),
            5,
            20,
        );
        let hi = SchedulableTask::new(
            TaskId::parse("task_03").unwrap(),
            "g".into(),
            "mock".into(),
            9,
            30,
        );

        // Higher effective priority first.
        assert!(compare(&hi, &a, &boosts) == Ordering::Less);
        // Equal priority, earlier created first.
        assert!(compare(&a, &b, &boosts) == Ordering::Less);
        // Symmetry of ordering.
        assert!(compare(&b, &a, &boosts) == Ordering::Greater);
        assert!(compare(&a, &a, &boosts) == Ordering::Equal);
    }

    #[test]
    fn effective_priority_applies_group_boost() {
        let t = SchedulableTask::new(
            TaskId::parse("task_01").unwrap(),
            "g".into(),
            "mock".into(),
            3,
            1,
        );
        let boosts = BTreeMap::from([("g".to_string(), 4i32)]);
        assert_eq!(effective_priority(&t, &BTreeMap::new()), 3);
        assert_eq!(effective_priority(&t, &boosts), 7);
    }
}
