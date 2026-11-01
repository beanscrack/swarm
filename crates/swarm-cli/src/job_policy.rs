//! Job acceptance policy, evaluated against recorded tools rather than worker prose.
use clap::Args;
use serde::{Deserialize, Serialize};
use swarm_core::config::TaskPreset;

fn builder() -> TaskPreset {
    TaskPreset::Builder
}

#[derive(Debug, Clone, Args, Serialize, Deserialize)]
#[serde(default)]
pub struct JobPolicy {
    /// Builders/test writers require a check that fails before work begins.
    #[arg(long, default_value = "builder")]
    pub preset: TaskPreset,
    /// Minimum successful, substantive tool calls required to trust a review (default 2).
    #[arg(long, default_value_t = 2)]
    pub min_tool_calls: usize,
    /// Baseline remote ref to check; defaults to origin/main when present.
    #[arg(long)]
    pub freshness_ref: Option<String>,
    /// Maximum simultaneously running jobs in this jobs directory (default 4).
    #[arg(long, default_value_t = 4)]
    pub max_active_jobs: usize,
    /// Higher priorities run first; queued jobs gain one point per minute.
    #[arg(long)]
    pub priority: Option<i32>,
    /// Run command tools inside this preinstalled Docker image, without network or host credentials.
    #[arg(long)]
    pub sandbox_image: Option<String>,
}

impl Default for JobPolicy {
    fn default() -> Self {
        Self {
            preset: builder(),
            min_tool_calls: 2,
            freshness_ref: None,
            max_active_jobs: 4,
            priority: None,
            sandbox_image: None,
        }
    }
}

impl JobPolicy {
    pub fn priority_at(&self, submitted: i64, now: i64) -> i64 {
        let base = self.priority.unwrap_or(match self.preset {
            TaskPreset::Reviewer => 20,
            TaskPreset::TestWriter => 10,
            _ => 0,
        });
        i64::from(base) + (now.saturating_sub(submitted) / 60_000).max(0)
    }
    pub fn needs_red_gate(&self) -> bool {
        matches!(self.preset, TaskPreset::Builder | TaskPreset::TestWriter)
    }
}

pub use swarm_runtime::review::{audit_review, work_calls};

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reviewers_start_first_but_old_builders_eventually_overtake() {
        let builder = JobPolicy::default();
        let reviewer = JobPolicy {
            preset: TaskPreset::Reviewer,
            ..Default::default()
        };
        assert!(reviewer.priority_at(0, 0) > builder.priority_at(0, 0));
        assert!(
            builder.priority_at(0, 21 * 60_000) > reviewer.priority_at(21 * 60_000, 21 * 60_000)
        );
    }
}
