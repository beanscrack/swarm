//! Artifact storage: metadata (ArtifactRef) in SQLite, bytes on disk.

use crate::Store;
use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use sqlx::Row;
use swarm_core::artifact::{Artifact, ArtifactRef};
use swarm_core::{ArtifactId, RunId};

impl Store {
    /// Stores an artifact: inline bytes written to disk, metadata upserted.
    pub async fn store_artifact(&self, run: &RunId, artifact: &Artifact, now: i64) -> Result<()> {
        let meta = &artifact.meta;
        let sha = if let Some(bytes) = &artifact.inline {
            hex_str(&Sha256::digest(bytes))
        } else {
            meta.sha256.clone().unwrap_or_default()
        };
        let size: i64 = artifact
            .inline
            .as_ref()
            .map(|b| b.len() as i64)
            .unwrap_or(meta.size_bytes as i64);
        if let Some(bytes) = &artifact.inline {
            let path = self.artifact_path(&meta.id);
            if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
                tokio::fs::create_dir_all(parent).await?;
            }
            tokio::fs::write(&path, bytes)
                .await
                .with_context(|| format!("write artifact {}", meta.id.as_str()))?;
        }
        let ref_json = serde_json::to_string(&meta)?;
        sqlx::query(
            "INSERT INTO artifacts (id, run_id, kind, path, size_bytes, sha256, description, ref_json, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT(id) DO UPDATE SET kind = excluded.kind, path = excluded.path, \
             size_bytes = excluded.size_bytes, sha256 = excluded.sha256, \
             description = excluded.description, ref_json = excluded.ref_json, \
             created_at = excluded.created_at",
        )
        .bind(meta.id.as_str())
        .bind(run.as_str())
        .bind(meta.kind.as_str())
        .bind(&meta.path)
        .bind(size)
        .bind(&sha)
        .bind(&meta.summary)
        .bind(ref_json)
        .bind(now)
        .execute(&self.pool)
        .await
        .context("upsert artifact")?;
        Ok(())
    }

    /// Disk location for an artifact's inline bytes.
    pub fn artifact_path(&self, id: &ArtifactId) -> std::path::PathBuf {
        self.artifacts_dir().join(format!("{}.bin", id.as_str()))
    }

    pub async fn load_artifact(&self, id: &ArtifactId) -> Result<Option<Artifact>> {
        let row = sqlx::query("SELECT ref_json FROM artifacts WHERE id = ?")
            .bind(id.as_str())
            .fetch_optional(&self.pool)
            .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let meta: ArtifactRef = serde_json::from_str(&row.try_get::<String, _>("ref_json")?)?;
        let disk = self.artifact_path(&meta.id);
        let inline = tokio::fs::read(&disk).await.ok();
        Ok(Some(Artifact {
            meta,
            inline,
            path: None,
        }))
    }

    pub async fn list_artifacts(&self, run: &RunId) -> Result<Vec<ArtifactRef>> {
        let rows = sqlx::query("SELECT ref_json FROM artifacts WHERE run_id = ? ORDER BY id")
            .bind(run.as_str())
            .fetch_all(&self.pool)
            .await?;
        let mut out = Vec::new();
        for row in rows {
            out.push(serde_json::from_str(&row.try_get::<String, _>(0)?)?);
        }
        Ok(out)
    }
}

fn hex_str(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests_util;
    use swarm_core::artifact::ArtifactKind;
    use swarm_core::budget::ResourceBudget;
    use swarm_core::run::Run;

    #[tokio::test]
    async fn artifact_round_trip_with_checksum() {
        let (store, _tmp) = tests_util::open_test_store().await;
        let run = Run::new("t", "mock", None, 1, true, ResourceBudget::default(), 1);
        store.create_run(&run).await.unwrap();

        let meta = ArtifactRef::new(
            ArtifactId::new(),
            ArtifactKind::Code,
            "patches/one.diff",
            12,
            "a test patch",
        );
        let id = meta.id.clone();
        let art = Artifact::inline(meta, b"- old\n+ new\n".to_vec());
        store.store_artifact(&run.id, &art, 10).await.unwrap();

        let back = store.load_artifact(&id).await.unwrap().unwrap();
        assert_eq!(back.inline.as_deref(), Some(b"- old\n+ new\n".as_slice()));
        assert_eq!(back.meta.kind, ArtifactKind::Code);
        assert_eq!(store.list_artifacts(&run.id).await.unwrap().len(), 1);
    }
}
