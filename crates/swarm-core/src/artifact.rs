//! Artifacts: immutable, content-addressed outputs that agents reference by ID.
//!
//! Agents exchange `ArtifactRef`s instead of inlining large outputs. This keeps
//! prompts and messages bounded, lets several agents reuse the same output, and
//! lets the store keep only one canonical copy per artifact.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::ids::ArtifactId;
use crate::time::now_millis;

string_enum! {
    /// The category of an artifact. Drives how it is summarized and previewed.
    pub enum ArtifactKind {
        Code => "code",
        Text => "text",
        Data => "data",
        Binary => "binary",
        Directory => "directory",
        GitCommit => "git_commit",
        TestReport => "test_report",
        Document => "document",
        Other => "other",
    }
}

/// A lightweight, serializable reference to an artifact.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ArtifactRef {
    pub id: ArtifactId,
    pub kind: ArtifactKind,
    /// Path relative to the artifact-store root (for `GitCommit`, the commit id).
    pub path: String,
    pub size_bytes: u64,
    /// Lowercase hex SHA-256 of the content, when known.
    pub sha256: Option<String>,
    /// A one-line, model-readable summary shown in place of the raw content.
    pub summary: String,
    /// A small preview (first N bytes) safe to inline in a prompt.
    pub preview: Option<String>,
    pub created_at: i64,
}

impl ArtifactRef {
    pub fn new(
        id: ArtifactId,
        kind: ArtifactKind,
        path: impl Into<String>,
        size_bytes: u64,
        summary: impl Into<String>,
    ) -> Self {
        Self {
            id,
            kind,
            path: path.into(),
            size_bytes,
            sha256: None,
            summary: summary.into(),
            preview: None,
            created_at: now_millis(),
        }
    }

    pub fn with_sha256(mut self, sha: impl Into<String>) -> Self {
        self.sha256 = Some(sha.into());
        self
    }

    pub fn with_preview(mut self, preview: impl Into<String>) -> Self {
        self.preview = Some(preview.into());
        self
    }
}

/// A fully-materialized artifact: its metadata plus its content.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Artifact {
    pub meta: ArtifactRef,
    /// Raw content bytes when materialized inline.
    pub inline: Option<Vec<u8>>,
    /// Absolute path to the stored bytes, when written to disk.
    pub path: Option<PathBuf>,
}

impl Artifact {
    pub fn inline(meta: ArtifactRef, bytes: Vec<u8>) -> Self {
        Self { meta, inline: Some(bytes), path: None }
    }

    pub fn from_path(meta: ArtifactRef, path: PathBuf) -> Self {
        Self { meta, inline: None, path: Some(path) }
    }

    /// The materialized bytes, from the inline field (disk is read by the store).
    pub fn bytes(&self) -> Option<&[u8]> {
        self.inline.as_deref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ref_round_trips() {
        let r = ArtifactRef::new(ArtifactId::new(), ArtifactKind::Code, "a.rs", 12, "a module");
        let json = serde_json::to_string(&r).unwrap();
        let back: ArtifactRef = serde_json::from_str(&json).unwrap();
        assert_eq!(r, back);
        assert_eq!(r.kind.as_str(), "code");
    }
}
