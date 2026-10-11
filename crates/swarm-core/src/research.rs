//! Research-mode state: hypotheses, evidence, branches and verification.
//!
//! This is the durable, structured representation behind "adapting allocation
//! of scarce inference to the most promising directions." A hypothesis is a
//! claim an agent is investigating; evidence is the proof attached to it; a
//! branch is a worktree where a competing implementation lives.

use serde::{Deserialize, Serialize};

use crate::artifact::ArtifactRef;
use crate::ids::{HypothesisId, RunId};
use crate::time::now_millis;

string_enum! {
        /// Lifecycle of a hypothesis.
        pub enum HypothesisStatus {
        Active => "active",
        Promising => "promising",
        Exhausted => "exhausted",
        Promoted => "promoted",
        Abandoned => "abandoned",
    }
}

string_enum! {
        /// Whether the hypothesis has been independently verified.
        pub enum VerificationStatus {
        Unverified => "unverified",
        Reproduced => "reproduced",
        Contradicted => "contradicted",
        Stale => "stale",
    }
}

string_enum! {
        /// Where a piece of evidence came from.
        pub enum EvidenceSource {
        Test => "test",
        Build => "build",
        Lint => "lint",
        Inspection => "inspection",
        ModelReport => "model_report",
        Reference => "reference",
        Other => "other",
    }
}

/// A single unit of evidence supporting or contradicting a hypothesis.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvidenceRecord {
    pub id: String,
    pub source: EvidenceSource,
    pub title: String,
    pub detail: String,
    pub artifact_ref: Option<ArtifactRef>,
    /// A weight in `[0, 1]`; the scheduler blends it into hypothesis scores.
    pub weight: f64,
    pub created_at: i64,
}

impl EvidenceRecord {
    pub fn new(source: EvidenceSource, title: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            id: uuid_like(),
            source,
            title: title.into(),
            detail: detail.into(),
            artifact_ref: None,
            weight: 0.5,
            created_at: now_millis(),
        }
    }

    pub fn with_ref(mut self, ref_: ArtifactRef) -> Self {
        self.artifact_ref = Some(ref_);
        self
    }
}

fn uuid_like() -> String {
    // Core has `uuid` available; generate a v4 id for evidence records.
    uuid::Uuid::new_v4().to_string()
}

/// A claim an agent is investigating.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Hypothesis {
    pub id: HypothesisId,
    pub run_id: RunId,
    pub parent: Option<HypothesisId>,
    pub title: String,
    pub statement: String,
    pub artifact_ref: Option<ArtifactRef>,
    pub confidence: f64,
    pub status: HypothesisStatus,
    pub verification: VerificationStatus,
    pub evidence: Vec<EvidenceRecord>,
    pub tags: Vec<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

impl Hypothesis {
    pub fn new(id: HypothesisId, run_id: RunId, title: impl Into<String>, statement: impl Into<String>) -> Self {
        let now = now_millis();
        Self {
            id,
            run_id,
            parent: None,
            title: title.into(),
            statement: statement.into(),
            artifact_ref: None,
            confidence: 0.5,
            status: HypothesisStatus::Active,
            verification: VerificationStatus::Unverified,
            evidence: Vec::new(),
            tags: Vec::new(),
            created_at: now,
            updated_at: now,
        }
    }
}

/// A stored projection of a hypothesis, used in listings and checkpoints.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HypothesisRecord {
    pub id: HypothesisId,
    pub title: String,
    pub statement: String,
    pub confidence: f64,
    pub status: HypothesisStatus,
    pub verification: VerificationStatus,
    pub artifact_ref: Option<ArtifactRef>,
    pub summary: String,
    pub created_at: i64,
    pub updated_at: i64,
}

impl From<&Hypothesis> for HypothesisRecord {
    fn from(h: &Hypothesis) -> Self {
        Self {
            id: h.id.clone(),
            title: h.title.clone(),
            statement: h.statement.clone(),
            confidence: h.confidence,
            status: h.status,
            verification: h.verification,
            artifact_ref: h.artifact_ref.clone(),
            summary: h.statement.clone(),
            created_at: h.created_at,
            updated_at: h.updated_at,
        }
    }
}

/// A line of investigation: a hypothesis plus the worktree branch exploring it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResearchBranch {
    pub hypothesis_id: HypothesisId,
    pub attempt: u32,
    pub artifact_ref: Option<ArtifactRef>,
    pub summary: Option<String>,
    pub created_at: i64,
}

/// The result of an independent verification pass over a hypothesis.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerificationResult {
    pub status: VerificationStatus,
    pub reproduction_passed: bool,
    pub reproduction_ref: Option<ArtifactRef>,
    pub contradictions: Vec<String>,
    pub notes: String,
    pub checked_at: i64,
}

/// Roll-up research state for a run (for the TUI and checkpoints).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ResearchState {
    pub active_hypotheses: usize,
    pub promoted: usize,
    pub branches: usize,
    pub evidence: usize,
    pub exploration_fraction: f64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hypothesis_round_trips() {
        let h = Hypothesis::new(HypothesisId::new(), RunId::new(), "h1", "statement");
        let json = serde_json::to_string(&h).unwrap();
        let back: Hypothesis = serde_json::from_str(&json).unwrap();
        assert_eq!(h, back);
        let rec = HypothesisRecord::from(&h);
        assert_eq!(rec.title, "h1");
    }
}
