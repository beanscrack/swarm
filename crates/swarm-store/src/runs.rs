//! Run lifecycle persistence.

use crate::Store;
use anyhow::{Context, Result};
use sqlx::Row;
use swarm_core::{RunId, RunMode, RunStatus, TaskId, run::Run};

impl Store {
    pub async fn create_run(&self, run: &Run) -> Result<()> {
        sqlx::query(
            "INSERT INTO runs (id, objective, status, mode, provider, model, root_task_id, agent_target, zero_cost, budget_json, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(run.id.as_str())
        .bind(&run.objective)
        .bind(run.status.as_str())
        .bind(run.mode.as_str())
        .bind(&run.provider)
        .bind(&run.model)
        .bind(run.root_task_id.as_ref().map(|t| t.as_str()))
        .bind(run.agent_target as i64)
        .bind(run.zero_cost as i64)
        .bind(serde_json::to_string(&run.budget)?)
        .bind(run.created_at)
        .bind(run.updated_at)
        .execute(&self.pool)
        .await
        .context("insert run")?;
        Ok(())
    }

    pub async fn load_run(&self, id: &RunId) -> Result<Option<Run>> {
        let row = sqlx::query("SELECT * FROM runs WHERE id = ?")
            .bind(id.as_str())
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.as_ref().map(row_to_run).transpose()?)
    }

    pub async fn load_run_required(&self, id: &RunId) -> Result<Run> {
        self.load_run(id)
            .await?
            .with_context(|| format!("run {} not found", id.as_str()))
    }

    pub async fn list_runs(&self) -> Result<Vec<Run>> {
        let rows = sqlx::query("SELECT * FROM runs ORDER BY created_at DESC, id DESC")
            .fetch_all(&self.pool)
            .await?;
        rows.iter()
            .map(row_to_run)
            .collect::<Result<Vec<_>, _>>()
    }

    /// Upserts the run row. The status transition must have been validated
    /// against the loaded state before calling this.
    pub async fn update_run(&self, run: &Run) -> Result<()> {
        let n = sqlx::query(
            "UPDATE runs SET status = ?, mode = ?, objective = ?, provider = ?, model = ?, root_task_id = ?, agent_target = ?, zero_cost = ?, budget_json = ?, updated_at = ? \
             WHERE id = ?",
        )
        .bind(run.status.as_str())
        .bind(run.mode.as_str())
        .bind(&run.objective)
        .bind(&run.provider)
        .bind(&run.model)
        .bind(run.root_task_id.as_ref().map(|t| t.as_str()))
        .bind(run.agent_target as i64)
        .bind(run.zero_cost as i64)
        .bind(serde_json::to_string(&run.budget)?)
        .bind(run.updated_at)
        .bind(run.id.as_str())
        .execute(&self.pool)
        .await?
        .rows_affected();
        anyhow::ensure!(
            n == 1,
            "run {} not found for update",
            run.id.as_str()
        );
        Ok(())
    }

    pub async fn set_run_root_task(&self, id: &RunId, task: &TaskId, now: i64) -> Result<()> {
        sqlx::query("UPDATE runs SET root_task_id = ?, updated_at = ? WHERE id = ?")
            .bind(task.as_str())
            .bind(now)
            .bind(id.as_str())
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}

fn row_to_run(row: &sqlx::sqlite::SqliteRow) -> Result<Run> {
    let budget_json: String = row.try_get("budget_json")?;
    Ok(Run {
        id: RunId::parse(&row.try_get::<String, _>("id")?)?,
        objective: row.try_get("objective")?,
        status: row.try_get::<String, _>("status")?.parse::<RunStatus>()?,
        mode: row.try_get::<String, _>("mode")?.parse::<RunMode>()?,
        provider: row.try_get("provider")?,
        model: row.try_get("model")?,
        root_task_id: row
            .try_get::<Option<String>, _>("root_task_id")?
            .map(|s| TaskId::parse(&s))
            .transpose()?,
        agent_target: row.try_get::<i64, _>("agent_target")? as u32,
        zero_cost: row.try_get::<i64, _>("zero_cost")? != 0,
        budget: serde_json::from_str(&budget_json)?,
        created_at: row.try_get("created_at")?,
        updated_at: row.try_get("updated_at")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests_util;
    use swarm_core::budget::ResourceBudget;

    #[tokio::test]
    async fn run_round_trip() {
        let (store, _tmp) = tests_util::open_test_store().await;
        let mut run = Run::new(
            "test objective",
            "mock",
            Some("mock-model".to_string()),
            4,
            true,
            ResourceBudget::default(),
            1_700_000_000_000,
        );
        run.mode = RunMode::Research;
        store.create_run(&run).await.unwrap();
        let loaded = store.load_run(&run.id).await.unwrap().unwrap();
        assert_eq!(loaded.id, run.id);
        assert_eq!(loaded.objective, "test objective");
        assert!(loaded.zero_cost);
        assert_eq!(loaded.mode, RunMode::Research);

        run.status = RunStatus::Completed;
        run.updated_at = 1_700_000_000_001;
        store.update_run(&run).await.unwrap();
        let loaded = store.load_run(&run.id).await.unwrap().unwrap();
        assert_eq!(loaded.status, RunStatus::Completed);
    }
}
