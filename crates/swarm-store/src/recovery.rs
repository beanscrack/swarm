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
}

#[cfg(test)]
mod tests {
    use crate::tests_util;
    use swarm_core::budget::ResourceBudget;
    use swarm_core::run::Run;
    use swarm_core::task::Task;
    use swarm_core::{TaskId, TaskState};

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
