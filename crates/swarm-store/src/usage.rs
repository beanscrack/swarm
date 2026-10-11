//! Per-run usage accounting (inference calls, tokens, spend, work counters).

use crate::Store;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use swarm_core::{RunId, budget::BudgetUsage};

/// Measurable counters for a run (see the efficiency requirements: we must be
/// able to say how much inference produced verified results vs. wasted work).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RunStats {
    pub usage: BudgetUsage,
    /// Number of results that passed independent verification.
    pub verified_results: u64,
    /// Work that was discarded (stale branches, contradictions, rollbacks).
    pub wasted_work: u64,
    /// Prompt-prefix cache hits, when the provider reports them.
    pub cache_reuse: u64,
}

impl RunStats {
    pub fn total_tokens(&self) -> u64 {
        self.usage.total_tokens()
    }
}

impl Store {
    /// Loads the run's stats (or the zero value if none recorded yet).
    pub async fn load_stats(&self, run: &RunId) -> Result<RunStats> {
        let row = sqlx::query("SELECT json FROM usage WHERE run_id = ?")
            .bind(run.as_str())
            .fetch_optional(&self.pool)
            .await?;
        Ok(row
            .map(|r| -> anyhow::Result<RunStats> {
                let json: String = r.try_get(0).context("usage json")?;
                let stats: RunStats = serde_json::from_str(&json)?;
                Ok(stats)
            })
            .transpose()?
            .unwrap_or_default())
    }

    /// Atomically folds `usage` into the run's counters.
    pub async fn record_usage(&self, run: &RunId, usage: &BudgetUsage, now: i64) -> Result<RunStats> {
        let mut stats = self.load_stats(run).await?;
        stats.usage.inference_calls += usage.inference_calls;
        stats.usage.input_tokens += usage.input_tokens;
        stats.usage.output_tokens += usage.output_tokens;
        stats.usage.spend_micros += usage.spend_micros;
        stats.usage.tool_calls += usage.tool_calls;
        self.save_stats(run, &stats, now).await
    }

    /// Increments a named work counter (verified_results | wasted_work |
    /// cache_reuse) by `n`.
    pub async fn increment(
        &self,
        run: &RunId,
        field: &str,
        n: u64,
        now: i64,
    ) -> Result<RunStats> {
        let mut stats = self.load_stats(run).await?;
        match field {
            "verified_results" => stats.verified_results += n,
            "wasted_work" => stats.wasted_work += n,
            "cache_reuse" => stats.cache_reuse += n,
            _ => return Err(anyhow::anyhow!("unknown stats field: {field}")),
        }
        self.save_stats(run, &stats, now).await
    }

    async fn save_stats(&self, run: &RunId, stats: &RunStats, now: i64) -> Result<RunStats> {
        sqlx::query(
            "INSERT INTO usage (run_id, json, updated_at) VALUES (?, ?, ?) \
             ON CONFLICT(run_id) DO UPDATE SET json = excluded.json, updated_at = excluded.updated_at",
        )
        .bind(run.as_str())
        .bind(serde_json::to_string(stats)?)
        .bind(now)
        .execute(&self.pool)
        .await
        .context("upsert usage")?;
        Ok(stats.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests_util;
    use swarm_core::budget::ResourceBudget;
    use swarm_core::run::Run;

    #[tokio::test]
    async fn usage_accumulates() {
        let (store, _tmp) = tests_util::open_test_store().await;
        let run = Run::new("t", "mock", None, 1, true, ResourceBudget::default(), 1);
        store.create_run(&run).await.unwrap();
        let mut u = BudgetUsage::default();
        u.inference_calls = 3;
        u.input_tokens = 100;
        u.output_tokens = 40;
        store.record_usage(&run.id, &u, 1).await.unwrap();
        let mut u2 = u.clone();
        u2.inference_calls = 1;
        u2.input_tokens = 50;
        u2.output_tokens = 10;
        let stats = store
            .record_usage(&run.id, &u2, 2)
            .await
            .unwrap();
        assert_eq!(stats.usage.inference_calls, 4);
        assert_eq!(stats.usage.input_tokens, 150);
        assert_eq!(stats.usage.output_tokens, 50);
        let stats = store.increment(&run.id, "verified_results", 2, 3).await.unwrap();
        assert_eq!(stats.verified_results, 2);
    }
}
