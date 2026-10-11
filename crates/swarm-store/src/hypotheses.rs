//! Research hypotheses: structured claims with evidence and verification.

use crate::Store;
use anyhow::{Context, Result};
use sqlx::Row;
use swarm_core::{
    HypothesisId, RunId,
    research::Hypothesis,
};

impl Store {
    /// Upserts a hypothesis (keyed by hypothesis id).
    pub async fn upsert_hypothesis(&self, h: &Hypothesis) -> Result<()> {
        sqlx::query(
            "INSERT INTO hypotheses (id, run_id, json, updated_at) VALUES (?, ?, ?, ?) \
             ON CONFLICT(id) DO UPDATE SET json = excluded.json, updated_at = excluded.updated_at",
        )
        .bind(h.id.as_str())
        .bind(h.run_id.as_str())
        .bind(serde_json::to_string(h)?)
        .bind(h.updated_at)
        .execute(&self.pool)
        .await
        .context("upsert hypothesis")?;
        Ok(())
    }

    pub async fn load_hypothesis(
        &self,
        id: &HypothesisId,
    ) -> Result<Option<Hypothesis>> {
        let row = sqlx::query("SELECT json FROM hypotheses WHERE id = ?")
            .bind(id.as_str())
            .fetch_optional(&self.pool)
            .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let json: String = row.try_get(0)?;
        Ok(Some(serde_json::from_str(&json)?))
    }

    pub async fn list_hypotheses(&self, run: &RunId) -> Result<Vec<Hypothesis>> {
        let rows = sqlx::query("SELECT json FROM hypotheses WHERE run_id = ? ORDER BY updated_at")
            .bind(run.as_str())
            .fetch_all(&self.pool)
            .await?;
        rows.iter()
            .map(|r| {
                let json: String = r.try_get(0)?;
                Ok(serde_json::from_str(&json)?)
            })
            .collect::<Result<Vec<_>, _>>()
    }
}

#[allow(dead_code)]
fn _unused(row: &sqlx::sqlite::SqliteRow) -> Result<i64> {
    Ok(row.try_get(0)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests_util;
    use swarm_core::budget::ResourceBudget;
    use swarm_core::run::Run;

    #[tokio::test]
    async fn hypothesis_round_trip() {
        let (store, _tmp) = tests_util::open_test_store().await;
        let run = Run::new("t", "mock", None, 1, true, ResourceBudget::default(), 1);
        store.create_run(&run).await.unwrap();
        let run_id = run.id.clone();
        let mut h = Hypothesis::new(
            HypothesisId::new(),
            run_id.clone(),
            "h1",
            "17 is prime",
        );
        h.confidence = 0.8;
        store.upsert_hypothesis(&h).await.unwrap();
        let loaded = store
            .load_hypothesis(&h.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(loaded.title, "h1");
        assert!((loaded.confidence - 0.8).abs() < 1e-9);
        assert_eq!(store.list_hypotheses(&run_id).await.unwrap().len(), 1);
    }
}
