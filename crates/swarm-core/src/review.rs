//! Machine-readable reviews. Claims remain claims; verification describes evidence.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ReviewVerdict {
    Approve,
    RequestChanges,
    Inconclusive,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verification {
    Executed,
    CodeInspection,
    Unverified,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewFinding {
    pub file: String,
    pub line: u32,
    pub claim: String,
    pub verification: Verification,
    pub repro_command: Option<String>,
    pub observed_output: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewCheck {
    pub command: String,
    pub observed_output: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewReport {
    pub verdict: ReviewVerdict,
    pub blockers: Vec<ReviewFinding>,
    pub checks: Vec<ReviewCheck>,
}

impl ReviewReport {
    pub fn validate(&self) -> Result<(), String> {
        if self.verdict == ReviewVerdict::Approve && !self.blockers.is_empty() {
            return Err("APPROVE cannot contain blockers".into());
        }
        if self.verdict == ReviewVerdict::RequestChanges && self.blockers.is_empty() {
            return Err("REQUEST_CHANGES requires at least one blocker".into());
        }
        for finding in &self.blockers {
            if finding.file.trim().is_empty()
                || finding.line == 0
                || finding.claim.trim().is_empty()
            {
                return Err("each blocker needs a file, positive line, and claim".into());
            }
            if finding.verification == Verification::Executed
                && (finding
                    .repro_command
                    .as_deref()
                    .unwrap_or("")
                    .trim()
                    .is_empty()
                    || finding
                        .observed_output
                        .as_deref()
                        .unwrap_or("")
                        .trim()
                        .is_empty())
            {
                return Err("executed blockers need repro_command and observed_output".into());
            }
        }
        if self
            .checks
            .iter()
            .any(|c| c.command.trim().is_empty() || c.observed_output.trim().is_empty())
        {
            return Err("checks need a command and observed_output".into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fabricated_execution_and_inconsistent_verdicts_are_rejected() {
        let mut report = ReviewReport {
            verdict: ReviewVerdict::RequestChanges,
            blockers: vec![],
            checks: vec![],
        };
        assert!(report.validate().is_err());
        report.blockers.push(ReviewFinding {
            file: "src/a.rs".into(),
            line: 1,
            claim: "claim".into(),
            verification: Verification::Executed,
            repro_command: None,
            observed_output: None,
        });
        assert!(report.validate().is_err());
        report.blockers[0].verification = Verification::CodeInspection;
        assert!(report.validate().is_ok());
        report.verdict = ReviewVerdict::Approve;
        assert!(report.validate().is_err());
    }
}
