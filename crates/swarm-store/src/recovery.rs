//! Crash-restart recovery: re-lease expired tasks and normalize stuck agents.

use crate::{agents::row_to_agent, Store};
use anyhow::Result;
use swarm_core::agent::Agent;
use swarm_core::RunId;

/// Outcome of a recovery pass over one run.
#[derive(Debug, Default)]
pub struct RecoveryReport {
    pub released_tasks: usize,
    pub reset_agents: usize,
}

impl Store {
    /// Makes a run consistent after a crash/restart:
    /// * running tasks with expired leases go back to `ready`;
    /// * agents stuck in `running` without a live lease go to `paused`
    ///   (checkpointed) so they can resume.
    pub async fn recover_run(&self, run: &RunId, now: i64) -> Result<RecoveryReport> {
        let mut report = RecoveryReport::default();

        let reaped = self.reclaim_expired_leases(now).await?;
        report.released_tasks += reaped.len();

        // Agents stuck running with no live lease are presumed crashed;
        // move them to paused so the scheduler can resume them.
        let stuck: Vec<Agent> = {
            let rows = sqlx::query(
                "SELECT * FROM agents WHERE run_id = ? AND state = 'running'",
            )
            .bind(run.as_str())
            .fetch_all(&self.pool)
            .await?;
            rows.iter().map(row_to_agent).collect::<Result<Vec<_>, _>>()?
        };
        for agent in &stuck {
            let row = sqlx::query(
                "UPDATE agents SET state = 'paused', updated_at = ? WHERE id = ? AND state = 'running'",
            )
            .bind(now)
            .bind(agent.id.as_str())
            .execute(&self.pool)
            .await?;
            if row.rows_affected() == 1 {
                report.reset_agents += 1;
            }
        }
        Ok(report)
    }

    /// Loads all live agents for a run.
    pub async fn live_agents(&self, run: &RunId) -> Result<Vec<Agent>> {
        self.agents_for_run(run).await
    }

    /// Undo a budget death so a run resumed with a raised ceiling dispatches again.
    ///
    /// A budget failure leaves the agent `failed` and its task `failed`, so raising the
    /// ceilings on the run row is not enough: the scheduler has nothing to dispatch and the
    /// run just re-finalizes, replaying the stored reason. The agent's conversation is
    /// checkpointed after every step, so the work survives - only its terminal state has to
    /// be undone, by putting the agent back to `paused` (the state crash recovery uses) and
    /// its task back to `ready`.
    ///
    /// Only failures whose recorded reason is a budget failure are touched, and only when
    /// `raised` is true: resuming a budget death on the same ceiling would fail the very
    /// next inference call the same way, so an unraised resume must stay terminal.
    pub async fn recover_budget_death(&self, run: &RunId, now: i64, raised: bool) -> Result<RecoveryReport> {
        let mut report = RecoveryReport::default();
        if !raised {
            return Ok(report);
        }
        // The reason that killed an agent is on its last `to: "failed"` state change.
        let mut reasons: std::collections::HashMap<String, String> = Default::default();
        for event in self.latest_events(run, 20_000).await? {
            if event.kind.as_str() != "agent_state_changed" {
                continue;
            }
            if event.payload.get("to").and_then(serde_json::Value::as_str) != Some("failed") {
                continue;
            }
            if let (Some(agent), Some(reason)) = (
                event.agent_id.as_ref(),
                event.payload.get("reason").and_then(serde_json::Value::as_str),
            ) {
                reasons.insert(agent.as_str().to_string(), reason.to_string());
            }
        }
        for agent in self.agents_for_run(run).await? {
            if agent.state.as_str() != "failed" {
                continue;
            }
            if !reasons.get(agent.id.as_str()).is_some_and(|r| r.contains("budget exceeded")) {
                continue;
            }
            let revived = sqlx::query("UPDATE agents SET state = 'paused', updated_at = ? WHERE id = ? AND state = 'failed'")
                .bind(now)
                .bind(agent.id.as_str())
                .execute(&self.pool)
                .await?;
            if revived.rows_affected() != 1 {
                continue;
            }
            report.reset_agents += 1;
            let released = sqlx::query(
                "UPDATE tasks SET state = 'ready', lease_token = NULL, lease_expires_at = NULL, updated_at = ? \
                 WHERE id = ? AND state = 'failed'",
            )
            .bind(now)
            .bind(agent.task_id.as_str())
            .execute(&self.pool)
            .await?;
            report.released_tasks += usize::try_from(released.rows_affected()).unwrap_or(usize::MAX);
        }
        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use crate::tests_util;
    use swarm_core::budget::ResourceBudget;
    use swarm_core::run::Run;
    use swarm_core::task::Task;
    use swarm_core::{TaskId, TaskState};

    #[tokio::test]
    async fn a_budget_death_is_revived_and_a_real_failure_is_not() {
        use swarm_core::agent::Agent;
        use swarm_core::event::{EventKind, RunEvent};
        use swarm_core::AgentState;

        let (store, _tmp) = tests_util::open_test_store().await;
        let run = Run::new("t", "mock", None, 1, true, ResourceBudget::default(), 1);
        store.create_run(&run).await.unwrap();
        let run_id = run.id.clone();
        let task = Task {
            id: TaskId::new(),
            run_id: run_id.clone(),
            parent_id: None,
            depth: 0,
            title: "t".into(),
            objective: "o".into(),
            role: swarm_core::AgentRole::Worker,
            group: "g".into(),
            priority: 0,
            depends_on: vec![],
            state: TaskState::Failed,
            attempts: 1,
            created_at: 1,
            updated_at: 1,
        };
        store.create_task(&task).await.unwrap();
        let mut agent = Agent::new(run_id.clone(), task.id.clone(), None, swarm_core::AgentRole::Worker, 0, 1);
        agent.state = AgentState::Failed;
        store.create_agent(&agent).await.unwrap();
        let kill = |reason: &str| {
            let mut event = RunEvent::new(
                EventKind::AgentStateChanged,
                run_id.clone(),
                serde_json::json!({ "from": "running", "to": "failed", "reason": reason }),
            );
            event.agent_id = Some(agent.id.clone());
            event
        };

        // A transport failure is a real failure: terminal, nothing to revive.
        store.append_event(&kill("transport error: connection refused")).await.unwrap();
        let report = store.recover_budget_death(&run_id, 10, true).await.unwrap();
        assert_eq!(report.reset_agents, 0, "a real failure stays terminal");

        // A budget ceiling is a pause. With a raised ceiling the agent goes back to
        // `paused` and its task back to `ready`, so the scheduler dispatches it again.
        store
            .append_event(&kill("budget: budget exceeded for tokens: requested 5000 against limit 4500"))
            .await
            .unwrap();
        let unraised = store.recover_budget_death(&run_id, 11, false).await.unwrap();
        assert_eq!(unraised.reset_agents, 0, "an unraised resume of a budget death stays terminal");
        let report = store.recover_budget_death(&run_id, 12, true).await.unwrap();
        assert_eq!((report.reset_agents, report.released_tasks), (1, 1));
        assert_eq!(store.load_task(&task.id).await.unwrap().unwrap().state, TaskState::Ready);
        assert_eq!(store.agents_for_run(&run_id).await.unwrap()[0].state.as_str(), "paused");
    }

    #[tokio::test]
    async fn recovery_releases_expired_tasks() {
        let (store, _tmp) = tests_util::open_test_store().await;
        let run = Run::new("t", "mock", None, 1, true, ResourceBudget::default(), 1);
        store.create_run(&run).await.unwrap();
        let run_id = run.id.clone();
        let task = Task {
            id: TaskId::new(),
            run_id: run_id.clone(),
            parent_id: None,
            depth: 0,
            title: "t".into(),
            objective: "o".into(),
            role: swarm_core::AgentRole::Worker,
            group: "g".into(),
            priority: 0,
            depends_on: vec![],
            state: TaskState::Ready,
            attempts: 0,
            created_at: 1,
            updated_at: 1,
        };
        store.create_task(&task).await.unwrap();
        // Claim with an already-expired lease.
        store
            .claim_task(&task.id, "dead-token", 100, 50)
            .await
            .unwrap();
        let report = store.recover_run(&run_id, 999).await.unwrap();
        assert_eq!(report.released_tasks, 1);
        let t = store.load_task(&task.id).await.unwrap().unwrap();
        assert_eq!(t.state, TaskState::Ready);
    }
}
