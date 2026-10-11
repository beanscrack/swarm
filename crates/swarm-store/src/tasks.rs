//! Task lifecycle and atomic claiming.
//!
//! Tasks are claimed atomically: the conditional UPDATE inside
//! [`Store::claim_task`] is a single statement, so at most one claimant
//! (across concurrent tasks *and* processes) can win a given task.

use crate::Store;
use anyhow::{ensure, Context, Result};
use sqlx::Row;
use swarm_core::{AgentRole, RunId, TaskId, TaskState, task::Task};

impl Store {
    pub async fn create_task(&self, task: &Task) -> Result<()> {
        sqlx::query(
            "INSERT INTO tasks (id, run_id, parent_id, depth, title, objective, role, group_name, priority, depends_on, state, attempts, lease_token, lease_expires_at, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NULL, NULL, ?, ?)",
        )
        .bind(task.id.as_str())
        .bind(task.run_id.as_str())
        .bind(task.parent_id.as_ref().map(|t| t.as_str()))
        .bind(task.depth as i64)
        .bind(&task.title)
        .bind(&task.objective)
        .bind(task.role.as_str())
        .bind(&task.group)
        .bind(task.priority as i64)
        .bind(serde_json::to_string(&task.depends_on)?)
        .bind(task.state.as_str())
        .bind(task.attempts as i64)
        .bind(task.created_at)
        .bind(task.updated_at)
        .execute(&self.pool)
        .await
        .context("insert task")?;
        Ok(())
    }

    pub async fn load_task(&self, id: &TaskId) -> Result<Option<Task>> {
        let row = sqlx::query("SELECT * FROM tasks WHERE id = ?")
            .bind(id.as_str())
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.as_ref().map(row_to_task).transpose()?)
    }

    pub async fn list_tasks(&self, run: &RunId) -> Result<Vec<Task>> {
        let rows = sqlx::query(
            "SELECT * FROM tasks WHERE run_id = ? ORDER BY depth, created_at, id",
        )
        .bind(run.as_str())
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(row_to_task)
            .collect::<Result<Vec<_>, _>>()
    }

    pub async fn tasks_by_state(&self, run: &RunId, state: TaskState) -> Result<Vec<Task>> {
        let rows = sqlx::query("SELECT * FROM tasks WHERE run_id = ? AND state = ?")
            .bind(run.as_str())
            .bind(state.as_str())
            .fetch_all(&self.pool)
            .await?;
        rows.iter()
            .map(row_to_task)
            .collect::<Result<Vec<_>, _>>()
    }

    /// Counts a run's tasks in terminal states (completed, failed, cancelled).
    pub async fn finished_task_count(&self, run: &RunId) -> Result<u32> {
        let row = sqlx::query(
            "SELECT COUNT(*) FROM tasks WHERE run_id = ? AND state IN ('completed', 'failed', 'cancelled')",
        )
        .bind(run.as_str())
        .fetch_one(&self.pool)
        .await?;
        Ok(row.try_get::<i64, _>(0)? as u32)
    }

    /// Atomically claims a `ready` task for `token`.
    ///
    /// Returns `true` only if this caller won the claim.
    pub async fn claim_task(
        &self,
        id: &TaskId,
        token: &str,
        now: i64,
        ttl_ms: i64,
    ) -> Result<bool> {
        let res = sqlx::query(
            "UPDATE tasks SET state = 'running', attempts = attempts + 1, lease_token = ?, lease_expires_at = ?, updated_at = ? \
             WHERE id = ? AND state = 'ready'",
        )
        .bind(token)
        .bind(now + ttl_ms)
        .bind(now)
        .bind(id.as_str())
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected() == 1)
    }

    /// Extends the lease for a task that `token` currently holds.
    pub async fn renew_lease(
        &self,
        id: &TaskId,
        token: &str,
        now: i64,
        ttl_ms: i64,
    ) -> Result<bool> {
        let res = sqlx::query(
            "UPDATE tasks SET lease_expires_at = ? WHERE id = ? AND state = 'running' AND lease_token = ?",
        )
        .bind(now + ttl_ms)
        .bind(id.as_str())
        .bind(token)
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected() == 1)
    }

    /// Re-releases a task (held by `token`) back to `ready`.
    pub async fn release_lease(&self, id: &TaskId, token: &str, now: i64) -> Result<bool> {
        let res = sqlx::query(
            "UPDATE tasks SET state = 'ready', lease_token = NULL, lease_expires_at = NULL, updated_at = ? \
             WHERE id = ? AND state = 'running' AND lease_token = ?",
        )
        .bind(now)
        .bind(id.as_str())
        .bind(token)
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected() == 1)
    }

    /// Re-releases all tasks whose lease expired before `now`; returns them.
    pub async fn reclaim_expired_leases(&self, now: i64) -> Result<Vec<Task>> {
        let rows = sqlx::query(
            "SELECT id FROM tasks WHERE state = 'running' AND lease_expires_at IS NOT NULL AND lease_expires_at < ?",
        )
        .bind(now)
        .fetch_all(&self.pool)
        .await?;
        let mut out = Vec::new();
        for row in rows {
            let id = TaskId::parse(&row.try_get::<String, _>(0)?)?;
            let n = sqlx::query(
                "UPDATE tasks SET state = 'ready', lease_token = NULL, lease_expires_at = NULL, updated_at = ? \
                 WHERE id = ? AND state = 'running' AND lease_expires_at < ?",
            )
            .bind(now)
            .bind(id.as_str())
            .bind(now)
            .execute(&self.pool)
            .await?
            .rows_affected();
            if n == 1 {
                if let Some(t) = self.load_task(&id).await? {
                    out.push(t);
                }
            }
        }
        Ok(out)
    }

    /// Marks a task completed. Only the current lease holder may do this.
    pub async fn complete_task(&self, id: &TaskId, token: &str, now: i64) -> Result<()> {
        let n = sqlx::query(
            "UPDATE tasks SET state = 'completed', lease_token = NULL, lease_expires_at = NULL, updated_at = ? \
             WHERE id = ? AND state = 'running' AND lease_token = ?",
        )
        .bind(now)
        .bind(id.as_str())
        .bind(token)
        .execute(&self.pool)
        .await?
        .rows_affected();
        ensure!(
            n == 1,
            "task {} could not be completed (stale lease?)",
            id.as_str()
        );
        Ok(())
    }

    /// Fails a task. With `retry = true` it is requeued to `ready`,
    /// otherwise it becomes the terminal `failed` state.
    pub async fn fail_task(
        &self,
        id: &TaskId,
        token: &str,
        now: i64,
        retry: bool,
    ) -> Result<()> {
        let new_state = if retry { "ready" } else { "failed" };
        let n = sqlx::query(
            "UPDATE tasks SET state = ?, lease_token = NULL, lease_expires_at = NULL, updated_at = ? \
             WHERE id = ? AND state = 'running' AND lease_token = ?",
        )
        .bind(new_state)
        .bind(now)
        .bind(id.as_str())
        .bind(token)
        .execute(&self.pool)
        .await?
        .rows_affected();
        ensure!(
            n == 1,
            "task {} could not be failed (stale lease?)",
            id.as_str()
        );
        Ok(())
    }

    /// Sets a task's state after a *validated* transition (by the caller),
    /// for state changes that do not involve the lease (e.g. dependency
    /// failures cascading to dependents).
    pub async fn set_task_state(
        &self,
        id: &TaskId,
        state: TaskState,
        now: i64,
    ) -> Result<()> {
        let n = sqlx::query("UPDATE tasks SET state = ?, updated_at = ? WHERE id = ?")
            .bind(state.as_str())
            .bind(now)
            .bind(id.as_str())
            .execute(&self.pool)
            .await?
            .rows_affected();
        ensure!(n == 1, "task {} not found", id.as_str());
        Ok(())
    }
}

fn row_to_task(row: &sqlx::sqlite::SqliteRow) -> Result<Task> {
    let dep_str: String = row.try_get("depends_on")?;
    let role_str: String = row.try_get("role")?;
    Ok(Task {
        id: TaskId::parse(&row.try_get::<String, _>("id")?)?,
        run_id: RunId::parse(&row.try_get::<String, _>("run_id")?)?,
        parent_id: row
            .try_get::<Option<String>, _>("parent_id")?
            .map(|s| TaskId::parse(&s))
            .transpose()?,
        depth: row.try_get::<i64, _>("depth")? as u32,
        title: row.try_get("title")?,
        objective: row.try_get("objective")?,
        role: role_str.parse::<AgentRole>()?,
        group: row.try_get("group_name")?,
        priority: row.try_get::<i64, _>("priority")? as i32,
        depends_on: serde_json::from_str(&dep_str)?,
        state: row.try_get::<String, _>("state")?.parse::<TaskState>()?,
        attempts: row.try_get::<i64, _>("attempts")? as u32,
        created_at: row.try_get("created_at")?,
        updated_at: row.try_get("updated_at")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{tests_util, DEFAULT_LEASE_TTL_MS};
    use swarm_core::budget::ResourceBudget;
    use swarm_core::run::Run;

    async fn setup() -> (crate::Store, Run, Task) {
        let (store, _tmp) = tests_util::open_test_store().await;
        let run = Run::new(
            "t",
            "mock",
            None,
            1,
            true,
            ResourceBudget::default(),
            1_700_000_000_000,
        );
        store.create_run(&run).await.unwrap();
        let run_id = run.id.clone();
        let task = Task {
            id: TaskId::new(),
            run_id: run_id,
            parent_id: None,
            depth: 0,
            title: "root".into(),
            objective: "root".into(),
            role: AgentRole::Worker,
            group: "g".into(),
            priority: 0,
            depends_on: vec![],
            state: TaskState::Ready,
            attempts: 0,
            created_at: 1,
            updated_at: 1,
        };
        store.create_task(&task).await.unwrap();
        (store, run, task)
    }

    #[tokio::test]
    async fn claim_is_atomic_and_lease_gated() {
        let (store, run, task) = setup().await;
        assert!(store
            .claim_task(&task.id, "tok-a", 1000, DEFAULT_LEASE_TTL_MS)
            .await
            .unwrap());
        // Second claimant must lose: state is no longer 'ready'.
        assert!(!store
            .claim_task(&task.id, "tok-b", 1000, DEFAULT_LEASE_TTL_MS)
            .await
            .unwrap());
        // Only the lease holder may complete it.
        assert!(store.complete_task(&task.id, "tok-b", 2000).await.is_err());
        store
            .complete_task(&task.id, "tok-a", 2000)
            .await
            .unwrap();
        let t = store.load_task(&task.id).await.unwrap().unwrap();
        assert_eq!(t.state, TaskState::Completed);
        assert_eq!(store.finished_task_count(&run.id).await.unwrap(), 1);
    }

    #[tokio::test]
    async fn expired_lease_is_reclaimed() {
        let (store, _run, task) = setup().await;
        store
            .claim_task(&task.id, "tok-a", 100, 50)
            .await
            .unwrap();
        let reaped = store.reclaim_expired_leases(999).await.unwrap();
        assert_eq!(reaped.len(), 1);
        // After reclaim, a new claimant can claim it.
        assert!(store
            .claim_task(&task.id, "tok-b", 1000, DEFAULT_LEASE_TTL_MS)
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn fail_with_retry_requeues() {
        let (store, _run, task) = setup().await;
        store
            .claim_task(&task.id, "tok-a", 100, DEFAULT_LEASE_TTL_MS)
            .await
            .unwrap();
        store
            .fail_task(&task.id, "tok-a", 200, true)
            .await
            .unwrap();
        let t = store.load_task(&task.id).await.unwrap().unwrap();
        assert_eq!(t.state, TaskState::Ready);
        assert_eq!(t.attempts, 1);
    }
}
