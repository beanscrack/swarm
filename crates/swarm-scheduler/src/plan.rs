//! The admission planner: turns a ranked candidate list into a concrete batch.

use std::collections::BTreeMap;

use swarm_core::TaskId;

use crate::config::{Occupancy, SchedulerConfig};
use crate::task::{SchedulableTask, compare, effective_priority};

/// One task the planner has admitted into the dispatch batch.
#[derive(Debug, Clone, PartialEq)]
pub struct PlannedDispatch {
    pub id: TaskId,
    pub provider: String,
    pub group: String,
    /// Effective priority used to rank this task (base priority + research boost).
    pub priority: i32,
    pub depth: u32,
}

/// Why a candidate was not admitted this cycle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeferReason {
    /// Not in a dispatchable state, or a dependency is unmet.
    NotReady,
    /// The global ceiling on simultaneous inferences was reached.
    GlobalLanesFull,
    /// The candidate's provider already has its concurrency limit in flight.
    ProviderFull(String),
    /// The candidate's group hit its per-cycle fairness cap.
    GroupFull(String),
}

/// The outcome of one planning pass.
#[derive(Debug, Default)]
pub struct SchedulePlan {
    /// Tasks to dispatch now, in dispatch order.
    pub admitted: Vec<PlannedDispatch>,
    /// Every other candidate, with the reason it was deferred.
    pub deferred: Vec<(TaskId, DeferReason)>,
}

impl SchedulePlan {
    /// True when there was nothing to admit and nothing to defer.
    pub fn is_empty(&self) -> bool {
        self.admitted.is_empty() && self.deferred.is_empty()
    }

    /// The admitted tasks, in dispatch order.
    pub fn admitted_ids(&self) -> Vec<TaskId> {
        self.admitted.iter().map(|d| d.id.clone()).collect()
    }

    /// The reason a candidate was deferred, if it was deferred.
    pub fn reason_for(&self, id: &TaskId) -> Option<&DeferReason> {
        self.deferred.iter().find(|(t, _)| t == id).map(|(_, r)| r)
    }
}

/// Stateless scheduling policy over a [`SchedulerConfig`]. Cheap to build and
/// safe to share across the runtime's scheduler loop.
#[derive(Debug, Clone)]
pub struct Scheduler {
    config: SchedulerConfig,
}

impl Scheduler {
    pub fn new(config: SchedulerConfig) -> Self {
        Self { config }
    }

    /// The configuration driving this scheduler.
    pub fn config(&self) -> &SchedulerConfig {
        &self.config
    }

    /// Produce the dispatch batch for one scheduling pass.
    ///
    /// `candidates` need not be pre-ordered or pre-filtered. The planner ranks
    /// them (priority, then age, then id), admits a prefix subject to the
    /// global / provider / group budgets given `occupancy` for the lanes already
    /// in flight, and reports every other candidate with the reason it was
    /// deferred. The result is deterministic for a given input.
    pub fn plan(
        &self,
        candidates: &[SchedulableTask],
        occupancy: &Occupancy,
        boosts: &BTreeMap<String, i32>,
    ) -> SchedulePlan {
        let mut plan = SchedulePlan::default();

        // Report candidates that are not dispatchable, in stable input order.
        for t in candidates.iter().filter(|t| !t.ready) {
            plan.deferred.push((t.id.clone(), DeferReason::NotReady));
        }

        let mut ready: Vec<&SchedulableTask> = candidates.iter().filter(|t| t.ready).collect();
        ready.sort_by(|a, b| compare(a, b, boosts));

        // Lanes already in flight, cloned so we can simulate the batch filling
        // them without mutating the caller's snapshot.
        let mut occ = occupancy.clone();
        let mut group_admitted: BTreeMap<String, usize> = BTreeMap::new();

        for t in ready {
            let provider_now = occ.get(&t.provider);
            let group_now = group_admitted.get(&t.group).copied().unwrap_or(0);
            let limit = self.config.limit_for(&t.provider);

            let defer = if occ.total() >= self.config.max_active {
                Some(DeferReason::GlobalLanesFull)
            } else if provider_now >= limit {
                Some(DeferReason::ProviderFull(t.provider.clone()))
            } else if group_now >= self.config.max_per_group {
                Some(DeferReason::GroupFull(t.group.clone()))
            } else {
                occ.add(&t.provider, 1);
                *group_admitted.entry(t.group.clone()).or_insert(0) += 1;
                plan.admitted.push(PlannedDispatch {
                    id: t.id.clone(),
                    provider: t.provider.clone(),
                    group: t.group.clone(),
                    priority: effective_priority(t, boosts),
                    depth: t.depth,
                });
                None
            };
            if let Some(reason) = defer {
                plan.deferred.push((t.id.clone(), reason));
            }
        }

        plan
    }
}

impl Default for Scheduler {
    fn default() -> Self {
        Self::new(SchedulerConfig::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::SchedulableTask;
    use std::collections::BTreeMap;
    use swarm_core::TaskId;

    fn id(n: u8) -> TaskId {
        TaskId::parse(&format!("task_{n:02}")).unwrap()
    }

    fn cand(
        id: TaskId,
        group: &str,
        provider: &str,
        priority: i32,
        created: i64,
    ) -> SchedulableTask {
        SchedulableTask::new(
            id,
            group.to_string(),
            provider.to_string(),
            priority,
            created,
        )
    }

    /// A permissive base config; tests override only the knob under test.
    fn base() -> SchedulerConfig {
        SchedulerConfig {
            max_active: 8,
            provider_limits: BTreeMap::new(),
            default_provider_limit: 8,
            max_per_group: 4,
        }
    }

    fn no_boosts() -> BTreeMap<String, i32> {
        BTreeMap::new()
    }

    #[test]
    fn respects_global_lane_cap() {
        let mut cfg = base();
        cfg.max_active = 2;
        let s = Scheduler::new(cfg);
        let c = vec![
            cand(id(1), "g", "mock", 0, 1),
            cand(id(2), "g", "mock", 0, 2),
            cand(id(3), "g", "mock", 0, 3),
        ];
        let p = s.plan(&c, &Occupancy::new(), &no_boosts());
        assert_eq!(p.admitted_ids(), vec![id(1), id(2)]);
        assert_eq!(p.deferred.len(), 1);
        assert!(matches!(p.deferred[0].1, DeferReason::GlobalLanesFull));
    }

    #[test]
    fn respects_per_provider_limit() {
        let mut cfg = base();
        cfg.default_provider_limit = 1;
        let s = Scheduler::new(cfg);
        let c = vec![
            cand(id(1), "g1", "mock", 0, 1),
            cand(id(2), "g2", "mock", 0, 2),
            cand(id(3), "g3", "mock", 0, 3),
        ];
        let p = s.plan(&c, &Occupancy::new(), &no_boosts());
        assert_eq!(p.admitted_ids(), vec![id(1)]);
        assert!(
            p.deferred
                .iter()
                .all(|(_, r)| matches!(r, DeferReason::ProviderFull(_)))
        );
    }

    #[test]
    fn prioritizes_higher_priority_first() {
        let mut cfg = base();
        cfg.max_active = 1;
        let s = Scheduler::new(cfg);
        let c = vec![
            cand(id(1), "a", "mock", 10, 1),
            cand(id(2), "b", "mock", 1, 2),
        ];
        let p = s.plan(&c, &Occupancy::new(), &no_boosts());
        assert_eq!(p.admitted_ids(), vec![id(1)]);
        assert!(matches!(p.deferred[0].1, DeferReason::GlobalLanesFull));
    }

    #[test]
    fn fairness_gives_other_groups_a_slot() {
        let mut cfg = base();
        cfg.max_active = 3;
        cfg.max_per_group = 2;
        let s = Scheduler::new(cfg);
        // Group "x" has three ready tasks; group "y" has one.
        let c = vec![
            cand(id(1), "x", "mock", 0, 1),
            cand(id(2), "x", "mock", 0, 2),
            cand(id(3), "x", "mock", 0, 3),
            cand(id(4), "y", "mock", 0, 4),
        ];
        let p = s.plan(&c, &Occupancy::new(), &no_boosts());
        // "x" is capped at two; "y" still gets a slot; the third "x" is deferred.
        assert_eq!(p.admitted_ids(), vec![id(1), id(2), id(4)]);
        assert!(matches!(p.reason_for(&id(3)), Some(DeferReason::GroupFull(g)) if g == "x"));
    }

    #[test]
    fn occupancy_counts_existing_lanes() {
        let mut cfg = base();
        cfg.max_active = 2;
        let s = Scheduler::new(cfg);
        let mut occ = Occupancy::new();
        occ.set("mock", 1);
        let c = vec![
            cand(id(1), "g1", "mock", 0, 1),
            cand(id(2), "g2", "mock", 0, 2),
        ];
        let p = s.plan(&c, &occ, &no_boosts());
        assert_eq!(p.admitted_ids(), vec![id(1)]);
        assert!(matches!(p.deferred[0].1, DeferReason::GlobalLanesFull));
    }

    #[test]
    fn occupancy_counts_existing_provider_lanes() {
        let mut cfg = base();
        cfg.default_provider_limit = 1;
        let s = Scheduler::new(cfg);
        let mut occ = Occupancy::new();
        occ.set("mock", 1);
        let c = vec![cand(id(1), "g", "mock", 0, 1)];
        let p = s.plan(&c, &occ, &no_boosts());
        assert!(p.admitted.is_empty());
        assert!(matches!(p.deferred[0].1, DeferReason::ProviderFull(_)));
    }

    #[test]
    fn research_boost_reorders() {
        let mut cfg = base();
        cfg.max_active = 1;
        let s = Scheduler::new(cfg);
        let c = vec![
            cand(id(1), "a", "mock", 0, 1),
            cand(id(2), "b", "mock", 0, 2),
        ];
        let boosts = BTreeMap::from([("b".to_string(), 5i32)]);
        let p = s.plan(&c, &Occupancy::new(), &boosts);
        // The boosted (later-created) task jumps ahead.
        assert_eq!(p.admitted_ids(), vec![id(2)]);
    }

    #[test]
    fn not_ready_is_deferred_not_admitted() {
        let s = Scheduler::new(base());
        let ready = cand(id(1), "g", "mock", 0, 1);
        let mut blocked = cand(id(2), "g", "mock", 0, 2);
        blocked.ready = false;
        let c = vec![ready, blocked];
        let p = s.plan(&c, &Occupancy::new(), &no_boosts());
        assert_eq!(p.admitted_ids(), vec![id(1)]);
        assert!(matches!(p.reason_for(&id(2)), Some(DeferReason::NotReady)));
    }

    #[test]
    fn plan_is_deterministic_across_input_order() {
        let mut cfg = base();
        cfg.max_active = 2;
        let s = Scheduler::new(cfg);
        let a = vec![
            cand(id(1), "g1", "mock", 0, 1),
            cand(id(2), "g2", "mock", 0, 2),
            cand(id(3), "g3", "mock", 0, 3),
        ];
        let b = vec![a[2].clone(), a[0].clone(), a[1].clone()];
        let pa = s.plan(&a, &Occupancy::new(), &no_boosts());
        let pb = s.plan(&b, &Occupancy::new(), &no_boosts());
        assert_eq!(pa.admitted_ids(), pb.admitted_ids());
        // The two earliest-created tasks fill the lanes regardless of input order.
        assert_eq!(pa.admitted_ids(), vec![id(1), id(2)]);
    }
}
