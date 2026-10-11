//! Shared findings (blackboard): deduplicated, FTS-searchable knowledge.

use crate::Store;
use anyhow::{Context, Result};
use serde::{Serialize, Deserialize};
use serde_json::Value;
use sqlx::Row;
use swarm_core::RunId;

/// A finding entry as returned by the store.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Finding {
    pub id: u64,
    pub run_id: RunId,
    pub title: String,
    pub content: String,
    pub confidence: f64,
    pub tags: Vec<String>,
    /// The structured payload submitted (schema depends on the finder).
    pub json: Value,
    pub created_at: i64,
}

impl Store {
    /// Submits a finding. Identical (title, content) pairs are deduplicated:
    /// the pre-existing row is returned and `new` is false.
    pub async fn submit_finding(
        &self,
        run: &RunId,
        title: impl AsRef<str>,
        content: impl AsRef<str>,
        confidence: f64,
        tags: &[String],
        json: Value,
        now: i64,
    ) -> Result<(u64, bool)> {
        let title = title.as_ref().to_string();
        let content = content.as_ref().to_string();
        // Dedup check first (same run, same title+content).
        let dup = sqlx::query("SELECT id FROM findings WHERE run_id = ? AND title = ? AND content = ? LIMIT 1")
            .bind(run.as_str())
            .bind(&title)
            .bind(&content)
            .fetch_optional(&self.pool)
            .await?;
        if let Some(row) = dup {
            return Ok((row.try_get::<i64, _>(0)? as u64, false));
        }
        let res = sqlx::query(
            "INSERT INTO findings (run_id, title, content, confidence, tags, json, created_at) VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(run.as_str())
        .bind(&title)
        .bind(&content)
        .bind(confidence)
        .bind(serde_json::to_string(tags)?)
        .bind(serde_json::to_string(&json)?)
        .bind(now)
        .execute(&self.pool)
        .await
        .context("insert finding")?;
        Ok((res.last_insert_rowid() as u64, true))
    }

    /// Full-text search over a run's findings.
    pub async fn search_findings(
        &self,
        run: &RunId,
        query: &str,
        limit: u32,
    ) -> Result<Vec<Finding>> {
        // Escape the query as a single FTS5 phrase so arbitrary text is safe.
        let phrase: String = query
            .chars()
            .flat_map(|c| match c {
                '"' => vec!['"', '"', '"'],
                _ => vec![c],
            })
            .collect();
        let rows = sqlx::query(
            "SELECT f.id, f.run_id, f.title, f.content, f.confidence, f.tags, f.json, f.created_at \
             FROM findings AS f JOIN findings_fts ON findings_fts.rowid = f.id \
             WHERE findings_fts MATCH ? AND f.run_id = ? ORDER BY f.id DESC LIMIT ?",
        )
        .bind(format!("\"{}\"", phrase))
        .bind(run.as_str())
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await
        .context("fts search")?;
        rows.iter()
            .map(row_to_finding)
            .collect::<Result<Vec<_>, _>>()
    }

    /// Most recent findings for the run.
    pub async fn recent_findings(&self, run: &RunId, limit: u32) -> Result<Vec<Finding>> {
        let rows = sqlx::query(
            "SELECT id, run_id, title, content, confidence, tags, json, created_at \
             FROM findings WHERE run_id = ? ORDER BY id DESC LIMIT ?",
        )
        .bind(run.as_str())
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(row_to_finding)
            .collect::<Result<Vec<_>, _>>()
    }

    pub async fn load_finding(&self, id: u64) -> Result<Option<Finding>> {
        let row = sqlx::query(
            "SELECT id, run_id, title, content, confidence, tags, json, created_at FROM findings WHERE id = ?",
        )
        .bind(id as i64)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.as_ref().map(row_to_finding).transpose()?)
    }
}

fn row_to_finding(row: &sqlx::sqlite::SqliteRow) -> Result<Finding> {
    Ok(Finding {
        id: row.try_get::<i64, _>("id")? as u64,
        run_id: RunId::parse(&row.try_get::<String, _>("run_id")?)?,
        title: row.try_get("title")?,
        content: row.try_get("content")?,
        confidence: row.try_get("confidence")?,
        tags: serde_json::from_str(&row.try_get::<String, _>("tags")?)?,
        json: serde_json::from_str(&row.try_get::<String, _>("json")?)?,
        created_at: row.try_get("created_at")?,
    })
}

#[cfg(test)]
mod tests {
    use crate::tests_util;
    use serde_json::json;
    use swarm_core::budget::ResourceBudget;
    use swarm_core::run::Run;

    #[tokio::test]
    async fn findings_dedup_and_search() {
        let (store, _tmp) = tests_util::open_test_store().await;
        let run = Run::new("t", "mock", None, 1, true, ResourceBudget::default(), 1);
        store.create_run(&run).await.unwrap();
        let (id1, new1) = store
            .submit_finding(
                &run.id,
                "f1",
                "prime 17 is prime",
                0.9,
                &vec!["math".to_string()],
                json!({"n": 17}),
                1,
            )
            .await
            .unwrap();
        assert!(new1);
        let (id2, new2) = store
            .submit_finding(
                &run.id,
                "f1",
                "prime 17 is prime",
                0.9,
                &vec!["math".to_string()],
                json!({"n": 17}),
                2,
            )
            .await
            .unwrap();
        assert!(!new2);
        assert_eq!(id1, id2);

        let hits = store.search_findings(&run.id, "prime", 10).await.unwrap();
        assert_eq!(hits.len(), 1);
        let recent = store.recent_findings(&run.id, 5).await.unwrap();
        assert_eq!(recent.len(), 1);
    }
}
