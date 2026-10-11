//! Durable, append-only run event log.

use crate::Store;
use anyhow::{Context, Result};
use sqlx::Row;
use swarm_core::event::{EventKind, RunEvent};
use swarm_core::{AgentId, RunId, TaskId};

impl Store {
    /// Appends an event, assigning the next per-run sequence number.
    /// Returns the assigned `seq`.
    pub async fn append_event(&self, event: &RunEvent) -> Result<u64> {
        let res = sqlx::query(
            "INSERT INTO events (run_id, kind, payload, agent_id, task_id, created_at) VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(event.run_id.as_str())
        .bind(event.kind.as_str())
        .bind(serde_json::to_string(&event.payload)?)
        .bind(event.agent_id.as_ref().map(|a| a.as_str()))
        .bind(event.task_id.as_ref().map(|t| t.as_str()))
        .bind(event.created_at)
        .execute(&self.pool)
        .await
        .context("append event")?;
        let row = sqlx::query("SELECT seq FROM events WHERE rowid = ?")
            .bind(res.last_insert_rowid())
            .fetch_one(&self.pool)
            .await?;
        Ok(row.try_get::<i64, _>("seq")? as u64)
    }

    pub async fn last_seq(&self, run: &RunId) -> Result<u64> {
        let row = sqlx::query("SELECT COALESCE(MAX(seq), 0) FROM events WHERE run_id = ?")
            .bind(run.as_str())
            .fetch_one(&self.pool)
            .await?;
        Ok(row.try_get::<i64, _>(0)? as u64)
    }

    /// Events after `since` (exclusive), oldest first, capped at `limit`.
    pub async fn events_since(
        &self,
        run: &RunId,
        since: u64,
        limit: u32,
    ) -> Result<Vec<RunEvent>> {
        let rows = sqlx::query(
            "SELECT * FROM events WHERE run_id = ? AND seq > ? ORDER BY seq ASC LIMIT ?",
        )
        .bind(run.as_str())
        .bind(since as i64)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(row_to_event)
            .collect::<Result<Vec<_>, _>>()
    }

    /// The most recent events for the run, oldest first.
    pub async fn latest_events(&self, run: &RunId, limit: u32) -> Result<Vec<RunEvent>> {
        let rows = sqlx::query(
            "SELECT * FROM (SELECT * FROM events WHERE run_id = ? ORDER BY seq DESC LIMIT ?) ORDER BY seq ASC",
        )
        .bind(run.as_str())
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(row_to_event)
            .collect::<Result<Vec<_>, _>>()
    }
}

fn row_to_event(row: &sqlx::sqlite::SqliteRow) -> Result<RunEvent> {
    let kind_str: String = row.try_get("kind")?;
    Ok(RunEvent {
        seq: row.try_get::<i64, _>("seq")? as u64,
        run_id: RunId::parse(&row.try_get::<String, _>("run_id")?)?,
        kind: kind_str.parse::<EventKind>()?,
        payload: serde_json::from_str(&row.try_get::<String, _>("payload")?)?,
        agent_id: row
            .try_get::<Option<String>, _>("agent_id")?
            .map(|s| AgentId::parse(&s))
            .transpose()?,
        task_id: row
            .try_get::<Option<String>, _>("task_id")?
            .map(|s| TaskId::parse(&s))
            .transpose()?,
        created_at: row.try_get("created_at")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests_util;
    use serde_json::json;
    use swarm_core::budget::ResourceBudget;
    use swarm_core::run::Run;

    #[tokio::test]
    async fn events_are_ordered_and_cursor_paged() {
        let (store, _tmp) = tests_util::open_test_store().await;
        let run = Run::new("t", "mock", None, 1, true, ResourceBudget::default(), 1);
        store.create_run(&run).await.unwrap();
        let run_id = run.id.clone();
        for i in 0..5 {
            store
                .append_event(&RunEvent::new(EventKind::Log, run_id.clone(), json!({ "i": i })))
                .await
                .unwrap();
        }
        assert_eq!(store.last_seq(&run_id).await.unwrap(), 5);
        let page = store.events_since(&run_id, 1, 3).await.unwrap();
        assert_eq!(page.len(), 3);
        assert_eq!(page[0].seq, 2);
        assert_eq!(page[2].seq, 4);
        let latest = store.latest_events(&run.id, 2).await.unwrap();
        assert_eq!(latest[0].seq, 4);
        assert_eq!(latest[1].seq, 5);
    }
}
