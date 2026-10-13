//! The runtime driver: owns a whole run — scheduling, leases, agent spawning.
//!
//! [`Runtime::create_run`] persists a run plus its single root task and hands
//! it off to the worker loop. [`Runtime::drive`] recovers the run, promotes
//! and fails tasks by their dependencies, admits tasks to the scheduler,
//! claims them under leases, and spawns one [`AgentRunner`] per task — keeping
//! at most `provider_limits` agents in flight per provider.
//!
//! Everything goes through the store: run/task/agent rows and state changes,
//! leases with TTLs, and an event log for observability.

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::json;
use tokio::task::JoinSet;
use uuid::Uuid;

use swarm_core::agent::Agent;
use swarm_core::config::{ApprovalPolicy, RunConfig};
use swarm_core::event::{EventKind, RunEvent};
use swarm_core::run::{Run, RunMode};
use swarm_core::task::Task;
use swarm_core::{AgentRole, AgentState, CoreError, ProviderId, RunId, RunStatus, StateMachine, TaskId, TaskState};
use swarm_providers::ProviderRegistry;
use swarm_scheduler::{Occupancy, SchedulableTask, Scheduler};
use swarm_store::{Store, DEFAULT_LEASE_TTL_MS};
use swarm_tools::ToolExecutor;

use crate::agent::{AgentOutcome, AgentRunner};
use crate::error::RuntimeError;

/// Lease-renewal tick: `DEFAULT_LEASE_TTL_MS / 3`, so in-flight leases are
/// renewed well before they would expire.
const RENEW_TICK_MS: i64 = DEFAULT_LEASE_TTL_MS / 3;

/// Current wall-clock time as epoch milliseconds (the store's time base).
fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

/// Drives a whole run to completion (or failure), one lease-claimed task at a
/// time per the provider limits.
pub struct Runtime {
    store: Arc<Store>,
    registry: Arc<ProviderRegistry>,
    executor: Arc<ToolExecutor>,
    scheduler: Scheduler,
    approval: ApprovalPolicy,
}

impl Runtime {
    pub fn new(
        store: Arc<Store>,
        registry: Arc<ProviderRegistry>,
        executor: Arc<ToolExecutor>,
        scheduler: Scheduler,
    ) -> Self {
        Self { store, registry, executor, scheduler, approval: ApprovalPolicy::AskDangerous }
    }

    /// Sets the tool approval policy used by agents in this runtime.
    pub fn with_approval(mut self, approval: ApprovalPolicy) -> Self {
        self.approval = approval;
        self
    }

    /// Persists the run plus its single root task (depth 0, Worker, group
    /// "main", priority 0, Ready) and marks the run Running.
    pub async fn create_run(&self, cfg: &RunConfig, mode: RunMode) -> Result<Run, RuntimeError> {
        let now = now_ms();
        let mut run = Run::new(
            cfg.objective.clone(),
            cfg.provider.as_str().to_string(),
            Some(cfg.model.clone()),
            cfg.agents,
            cfg.zero_cost,
            cfg.budget.clone(),
            now,
        );
        run.mode = mode;
        run.transition(RunStatus::Running, now)?;

        let root = Task {
            id: TaskId::new(),
            run_id: run.id.clone(),
            parent_id: None,
            depth: 0,
            title: cfg.objective.clone(),
            objective: cfg.objective.clone(),
            role: if cfg.agents > 1 { AgentRole::Planner } else { AgentRole::Worker },
            group: "main".to_string(),
            priority: 0,
            depends_on: Vec::new(),
            state: TaskState::Ready,
            attempts: 0,
            created_at: now,
            updated_at: now,
        };

        self.store.create_run(&run).await?;
        self.store.create_task(&root).await?;
        self.store.set_run_root_task(&run.id, &root.id, now).await?;
        run.root_task_id = Some(root.id.clone());

        self.emit_run(&run.id, RunStatus::Running).await?;
        Ok(run)
    }

    /// Drives the run to a terminal state, returning the final [`RunStatus`].
    pub async fn drive(&self, run_id: &RunId) -> Result<RunStatus, RuntimeError> {
        // (1) Recover: reclaim leases held by a crashed process.
        let now = now_ms();
        self.store.recover_run(run_id, now).await?;

        let run = self.store.load_run(run_id).await?;
        let Some(run) = run else {
            return Err(RuntimeError::Config(format!("run {run_id} not found")));
        };
        let provider = run.provider.clone();

        let mut in_flight: JoinSet<(TaskId, String, AgentOutcome)> = JoinSet::new();
        let mut lease_tokens: Vec<(TaskId, String)> = Vec::new();

        loop {
            let now = now_ms();
            let budget = &run.budget;

            // (6) Wall-clock budget.
            let wall_ms = budget.max_wall_clock_secs.saturating_mul(1000).min(i64::MAX as u64) as i64;
            let deadline = run.created_at.saturating_add(wall_ms);
            if now >= deadline {
                in_flight.abort_all();
                while in_flight.join_next().await.is_some() {}
                for task in self.store.list_tasks(run_id).await? {
                    if task.state.is_terminal() {
                        continue;
                    }
                    if let Some((_, token)) = lease_tokens.iter().find(|(id, _)| id == &task.id) {
                        self.store.fail_task(&task.id, token, now, false).await?;
                    } else {
                        self.store.set_task_state(&task.id, TaskState::Failed, now).await?;
                    }
                    self.emit_task(run_id, &task.id, TaskState::Failed).await?;
                }
                for mut agent in self.store.agents_for_run(run_id).await? {
                    if agent.state.is_terminal() {
                        continue;
                    }
                    let from = agent.state;
                    agent.transition(AgentState::Failed, now)?;
                    self.store.update_agent(&agent).await?;
                    self.store.append_event(&RunEvent::new(
                        EventKind::AgentStateChanged,
                        run_id.clone(),
                        json!({ "from": from.as_str(), "to": "failed", "reason": "wall-clock budget exceeded" }),
                    ).with_agent(agent.id).with_task(agent.task_id)).await?;
                }
                return self.finalize(run_id, RunStatus::Failed).await;
            }

            // (1) Promote satisfied Pending tasks; fail tasks with doomed deps.
            let tasks = self.store.list_tasks(run_id).await?;
            self.promote_and_fail(&tasks, &run, now).await?;
            let tasks = self.store.list_tasks(run_id).await?;
            let completed: HashSet<TaskId> = tasks
                .iter()
                .filter(|t| t.state == TaskState::Completed)
                .map(|t| t.id.clone())
                .collect();

            // (2) Plan: admit Ready tasks against current occupancy.
            let mut occupancy = Occupancy::new();
            occupancy.add(&provider, in_flight.len());
            let candidates: Vec<SchedulableTask> = tasks
                .iter()
                .filter(|t| t.state == TaskState::Ready)
                .map(|t| SchedulableTask::from_task(t, &provider, &completed))
                .collect();
            let boosts = BTreeMap::<String, i32>::new();
            let plan = self.scheduler.plan(&candidates, &occupancy, &boosts);

            // (3) Claim and spawn each admitted task.
            for dispatch in &plan.admitted {
                let Some(task) = tasks.iter().find(|t| t.id == dispatch.id) else {
                    continue;
                };
                let token = Uuid::new_v4().to_string();
                let claimed =
                    self.store.claim_task(&task.id, &token, now, DEFAULT_LEASE_TTL_MS).await?;
                if !claimed {
                    continue;
                }
                let agent = Agent::new(
                    run.id.clone(),
                    task.id.clone(),
                    None,
                    task.role,
                    task.depth,
                    now,
                );
                self.store.create_agent(&agent).await?;
                lease_tokens.push((task.id.clone(), token.clone()));
                self.emit_task(&run.id, &task.id, TaskState::Running).await?;

                // Spawn the agent (one persistent provider session per task).
                let run_c = run.clone();
                let task_c = task.clone();
                let store = self.store.clone();
                let registry = self.registry.clone();
                let executor = self.executor.clone();
                let provider_c = provider.clone();
                let token_c = token.clone();
                let approval = self.approval;
                in_flight.spawn(async move {
                    let prov = ProviderId::new(provider_c).expect("provider id");
                    let runner = AgentRunner {
                        store,
                        registry,
                        executor,
                        approval,
                    };
                    // Same `Agent` value persisted above, so `update_agent` finds the row.
                    let mut agent = agent;
                    let outcome = runner
                        .run(&run_c, &task_c, &mut agent, &prov)
                        .await
                        .unwrap_or_else(|e| {
                            AgentOutcome::Failed { reason: e.to_string(), retryable: false }
                        });
                    (task_c.id.clone(), token_c, outcome)
                });
            }

            // (4) Termination: nothing in flight and nothing to admit.
            if in_flight.is_empty() && plan.admitted.is_empty() {
                break;
            }

            // (5) Wait for the next finished agent or a renewal tick.
            let wait_ms = (deadline - now_ms()).clamp(1, RENEW_TICK_MS) as u64;
            let woke = tokio::select! {
                r = in_flight.join_next() => r,
                _ = tokio::time::sleep(Duration::from_millis(wait_ms)) => None,
            };
            match woke {
                Some(Ok((task_id, token, outcome))) => {
                    lease_tokens.retain(|(id, _)| id != &task_id);
                    self.finish(task_id, &token, outcome, &run, now_ms()).await?;
                }
                Some(Err(e)) => {
                    return Err(RuntimeError::Core(CoreError::Protocol(format!(
                        "agent task panicked: {e}"
                    ))));
                }
                None => {
                    // Renewal tick: keep in-flight leases alive.
                    let now = now_ms();
                    for (id, token) in &lease_tokens {
                        let _ = self.store.renew_lease(id, token, now, DEFAULT_LEASE_TTL_MS).await;
                    }
                }
            }
        }

        // End state: all tasks Completed -> Completed, otherwise Failed.
        let tasks = self.store.list_tasks(run_id).await?;
        let all_completed =
            !tasks.is_empty() && tasks.iter().all(|t| t.state == TaskState::Completed);
        let status = if all_completed {
            RunStatus::Completed
        } else {
            RunStatus::Failed
        };
        self.finalize(run_id, status).await
    }

    /// Promotes `Pending` tasks whose dependencies are all completed to `Ready`,
    /// and fails any non-terminal task that (transitively) depends on a
    /// `Failed` or `Cancelled` task.
    async fn promote_and_fail(&self, tasks: &[Task], run: &Run, now: i64) -> Result<(), RuntimeError> {
        let completed: HashSet<TaskId> = tasks
            .iter()
            .filter(|t| t.state == TaskState::Completed)
            .map(|t| t.id.clone())
            .collect();
        let failed: HashSet<TaskId> = tasks
            .iter()
            .filter(|t| matches!(t.state, TaskState::Failed | TaskState::Cancelled))
            .map(|t| t.id.clone())
            .collect();
        // Transitive dependents of a failed/cancelled task.
        let mut doomed: HashSet<TaskId> = failed.iter().cloned().collect();
        let mut worklist: Vec<TaskId> = failed.iter().cloned().collect();
        while let Some(dep) = worklist.pop() {
            for t in tasks {
                if t.depends_on.contains(&dep) && doomed.insert(t.id.clone()) {
                    worklist.push(t.id.clone());
                }
            }
        }
        for t in tasks {
            let is_doomed = doomed.contains(&t.id) && !failed.contains(&t.id);
            match t.state {
                TaskState::Pending => {
                    if is_doomed {
                        self.set_state(t, run, TaskState::Failed, now).await?;
                    } else if t.depends_on.iter().all(|d| completed.contains(d)) {
                        self.set_state(t, run, TaskState::Ready, now).await?;
                    }
                }
                TaskState::Ready => {
                    if is_doomed {
                        self.set_state(t, run, TaskState::Failed, now).await?;
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    async fn set_state(&self, t: &Task, run: &Run, state: TaskState, now: i64) -> Result<(), RuntimeError> {
        self.store.set_task_state(&t.id, state, now).await?;
        self.emit_task(&run.id, &t.id, state).await?;
        Ok(())
    }

    /// Records an agent's terminal outcome for its task: `Completed` completes
    /// the task; `Failed` re-queues it (`retry`) or fails it, depending on the
    /// retry flag and the run's attempt budget.
    async fn finish(
        &self,
        task_id: TaskId,
        token: &str,
        outcome: AgentOutcome,
        run: &Run,
        now: i64,
    ) -> Result<(), RuntimeError> {
        match outcome {
            AgentOutcome::Completed { summary } => {
                self.store.complete_task(&task_id, token, now).await?;
                self.emit_task(&run.id, &task_id, TaskState::Completed).await?;
                self.store.append_event(&RunEvent::new(
                    EventKind::Message,
                    run.id.clone(),
                    json!({ "summary": summary }),
                ).with_task(task_id)).await?;
            }
            AgentOutcome::Failed { reason: _, retryable } => {
                let task = self.store.load_task(&task_id).await?;
                let attempts = task.as_ref().map(|t| t.attempts).unwrap_or(0);
                let retry = retryable && attempts < run.budget.max_task_attempts;
                self.store.fail_task(&task_id, token, now, retry).await?;
                let state = if retry {
                    TaskState::Ready
                } else {
                    TaskState::Failed
                };
                self.emit_task(&run.id, &task_id, state).await?;
            }
        }
        Ok(())
    }

    /// Persists the run's final status and emits the terminal event.
    async fn finalize(&self, run_id: &RunId, status: RunStatus) -> Result<RunStatus, RuntimeError> {
        let now = now_ms();
        if let Some(mut run) = self.store.load_run(run_id).await? {
            run.status = status;
            run.updated_at = now;
            self.store.update_run(&run).await?;
        }
        self.emit_run(run_id, status).await?;
        Ok(status)
    }

    async fn emit_run(&self, run_id: &RunId, status: RunStatus) -> Result<(), RuntimeError> {
        let event = RunEvent::new(
            EventKind::RunStateChanged,
            run_id.clone(),
            json!({ "status": status.as_str() }),
        );
        self.store.append_event(&event).await?;
        Ok(())
    }

    async fn emit_task(
        &self,
        run_id: &RunId,
        task_id: &TaskId,
        state: TaskState,
    ) -> Result<(), RuntimeError> {
        let event = RunEvent::new(
            EventKind::TaskStateChanged,
            run_id.clone(),
            json!({ "state": state.as_str() }),
        )
        .with_task(task_id.clone());
        self.store.append_event(&event).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use swarm_core::provider::ProviderErrorClass;
    use swarm_providers::{coding_script, MockProvider, MockStep, Provider};
    use swarm_scheduler::SchedulerConfig;
    use swarm_tools::SandboxConfig;

    /// Builds a throwaway workspace + store + runtime whose mock provider
    /// replays `script` for every session, with the given provider limit.
    async fn setup(
        script: Vec<MockStep>,
        provider_limit: usize,
    ) -> (TempDir, Arc<Store>, Arc<Runtime>, Run) {
        setup_with_latency(script, provider_limit, Duration::ZERO).await
    }

    async fn setup_with_latency(
        script: Vec<MockStep>,
        provider_limit: usize,
        latency: Duration,
    ) -> (TempDir, Arc<Store>, Arc<Runtime>, Run) {
        let dir = TempDir::new().unwrap();
        let ws = dir.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let mut mock = MockProvider::unowned().with_script(script);
        mock.latency = latency;
        let mut registry = ProviderRegistry::new();
        let provider_entry: Arc<dyn Provider> = Arc::new(mock);
        registry.register(provider_entry);
        let registry = Arc::new(registry);
        let store = Arc::new(Store::open(&dir.path().join("store")).await.unwrap());
        let executor = Arc::new(ToolExecutor::new(SandboxConfig::new(ws)));
        let scheduler = Scheduler::new(SchedulerConfig {
            max_active: 4,
            provider_limits: BTreeMap::from([("mock".to_string(), provider_limit)]),
            default_provider_limit: provider_limit,
            max_per_group: 4,
        });
        let rt = Arc::new(Runtime::new(store.clone(), registry, executor, scheduler));
        let cfg = RunConfig::mock("goal", 1);
        let run = rt.create_run(&cfg, RunMode::General).await.unwrap();
        (dir, store, rt, run)
    }

    #[tokio::test]
    async fn a_single_task_completes_and_artifact_written() {
        let (dir, store, rt, run) = setup(coding_script(), 1).await;
        let status = rt.drive(&run.id).await.unwrap();
        assert!(matches!(status, RunStatus::Completed), "expected Completed");
        assert!(dir.path().join("ws/hello.txt").exists());
        let run2 = store.load_run(&run.id).await.unwrap().unwrap();
        assert!(matches!(run2.status, RunStatus::Completed));
    }

    #[tokio::test]
    async fn b_two_tasks_never_run_concurrently_under_limit_one() {
        let (_dir, store, rt, run) = setup(coding_script(), 1).await;
        // Insert a second independent task (both Ready, no deps).
        let now = now_ms();
        let task2 = Task {
            id: TaskId::new(),
            run_id: run.id.clone(),
            parent_id: None,
            depth: 0,
            title: "task 2".into(),
            objective: "task 2".into(),
            role: AgentRole::Worker,
            group: "main".into(),
            priority: 0,
            depends_on: Vec::new(),
            state: TaskState::Ready,
            attempts: 0,
            created_at: now,
            updated_at: now,
        };
        store.create_task(&task2).await.unwrap();

        let status = rt.drive(&run.id).await.unwrap();
        assert!(matches!(status, RunStatus::Completed), "expected Completed");

        // Concurrency never exceeded 1 (the provider limit).
        let events = store.latest_events(&run.id, 10_000).await.unwrap();
        let mut running = 0i32;
        let mut max_running = 0i32;
        for e in &events {
            if e.kind == EventKind::TaskStateChanged {
                let state = e.payload.get("state").and_then(|v| v.as_str()).unwrap_or("");
                match state {
                    "running" => running += 1,
                    "completed" => running -= 1,
                    _ => {}
                }
                max_running = max_running.max(running);
            }
        }
        assert!(max_running <= 1, "max in flight was {max_running}, limit is 1");
    }

    #[tokio::test]
    async fn c_failing_task_fails_the_run() {
        let (_dir, store, rt, run) =
            setup(vec![MockStep::failing(ProviderErrorClass::Permanent)], 1).await;
        let status = rt.drive(&run.id).await.unwrap();
        assert!(matches!(status, RunStatus::Failed), "expected Failed");
        let task = store.load_task(run.root_task_id.as_ref().unwrap()).await.unwrap().unwrap();
        assert!(matches!(task.state, TaskState::Failed));
        let run2 = store.load_run(&run.id).await.unwrap().unwrap();
        assert!(matches!(run2.status, RunStatus::Failed));
    }

    #[tokio::test]
    async fn d_expired_lease_is_recovered_and_rerun() {
        let (dir, store, rt, run) = setup(coding_script(), 1).await;
        // Pre-claim with a past `now` so the lease is already expired.
        let root = run.root_task_id.as_ref().unwrap().clone();
        let past = now_ms() - 200_000;
        store.claim_task(&root, "stale-token", past, DEFAULT_LEASE_TTL_MS).await.unwrap();

        let status = rt.drive(&run.id).await.unwrap();
        assert!(matches!(status, RunStatus::Completed), "expected Completed");
        assert!(dir.path().join("ws/hello.txt").exists());
    }

    #[tokio::test]
    async fn dependent_task_starts_after_its_predecessor_completes() {
        let (_dir, store, rt, run) = setup(
            vec![MockStep::text("created"), MockStep::text("verified")], 2,
        ).await;
        let root_id = run.root_task_id.as_ref().unwrap().clone();
        let now = now_ms();
        let dependent = Task {
            id: TaskId::new(),
            run_id: run.id.clone(),
            parent_id: Some(root_id.clone()),
            depth: 1,
            title: "verify".into(),
            objective: "verify".into(),
            role: AgentRole::Worker,
            group: "verification".into(),
            priority: 0,
            depends_on: vec![root_id.clone()],
            state: TaskState::Pending,
            attempts: 0,
            created_at: now,
            updated_at: now,
        };
        store.create_task(&dependent).await.unwrap();
        assert_eq!(rt.drive(&run.id).await.unwrap(), RunStatus::Completed);
        let events = store.latest_events(&run.id, 10_000).await.unwrap();
        let transition = |id: &TaskId, state: &str| events.iter()
            .find(|event| event.kind == EventKind::TaskStateChanged
                && event.task_id.as_ref() == Some(id)
                && event.payload.get("state").and_then(|value| value.as_str()) == Some(state))
            .unwrap().seq;
        assert!(transition(&root_id, "completed") < transition(&dependent.id, "running"));
    }

    #[tokio::test]
    async fn wall_clock_budget_stops_inference_and_finalizes_tasks() {
        let (_dir, store, rt, run) = setup_with_latency(
            vec![MockStep::text("too late")], 1, Duration::from_secs(2),
        ).await;
        let mut limited = run.clone();
        limited.budget.max_wall_clock_secs = 1;
        store.update_run(&limited).await.unwrap();
        let status = tokio::time::timeout(Duration::from_secs(3), rt.drive(&run.id))
            .await.unwrap().unwrap();
        assert_eq!(status, RunStatus::Failed);
        assert!(store.list_tasks(&run.id).await.unwrap().iter()
            .all(|task| task.state == TaskState::Failed));
        assert!(store.agents_for_run(&run.id).await.unwrap().iter()
            .all(|agent| agent.state == AgentState::Failed));
    }
}
