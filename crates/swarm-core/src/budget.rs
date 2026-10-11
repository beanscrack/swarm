use crate::{CoreError, CoreResult};
use serde::{Deserialize, Serialize};

string_enum! {
        /// How a provider session is billed. Drives zero-cost enforcement: a run that
    /// is `zero_cost` may only dispatch to providers whose `BillingMode` is
    /// [`ZeroCost`].
        pub enum BillingMode {
        /// Costs nothing to call (local, mock, or bundled free tier).
        ZeroCost => "zero_cost",
        /// A paid subscription; usage is metered but not per-call billed here.
        Subscription => "subscription",
        /// Metered per token/request; incurs real cost.
        OnDemand => "on_demand",
    }
}

impl BillingMode {
    /// Whether this mode is safe under a strict zero-cost policy.
    pub fn is_zero_cost(self) -> bool {
        matches!(self, BillingMode::ZeroCost)
    }
}

/// Token accounting for a single inference call, reported by the provider.
/// Values are `0` when the provider does not report them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// Subset of `input_tokens` served from prompt cache (when reported).
    pub cached_input_tokens: u64,
}

impl TokenUsage {
    /// The full token footprint of the call (input + output).
    pub fn total(&self) -> u64 {
        self.input_tokens.saturating_add(self.output_tokens)
    }

    /// Adds another usage record into this one, saturating on overflow.
    pub fn add(&mut self, other: &TokenUsage) {
        self.input_tokens = self.input_tokens.saturating_add(other.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(other.output_tokens);
        self.cached_input_tokens = self.cached_input_tokens.saturating_add(other.cached_input_tokens);
    }
}

/// Hard limits enforced by the runtime. Every limit has a conservative default
/// and can be overridden in `swarm.toml`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ResourceBudget {
    /// Maximum inference steps for a single agent.
    pub max_steps_per_agent: u32,
    /// Maximum depth of child agents below the root.
    pub max_depth: u32,
    /// Maximum subtasks a single decomposition may create.
    pub max_children_per_task: u32,
    /// Maximum tasks that may be pending or ready in one run.
    pub max_pending_tasks: u32,
    /// Maximum inference requests for the whole run.
    pub max_inference_calls: u64,
    /// Maximum input plus output tokens for the whole run.
    pub max_total_tokens: u64,
    /// Maximum spend for the whole run in micro-USD. Zero forbids any spend.
    pub max_spend_micros: u64,
    /// Wall-clock limit for a run in seconds.
    pub max_wall_clock_secs: u64,
    /// Timeout for a single tool execution in milliseconds.
    pub tool_timeout_ms: u64,
    /// Output bytes kept from a single tool execution.
    pub max_tool_output_bytes: usize,
    /// Memory limit for a child process in megabytes (enforced via Job Objects on Windows).
    pub tool_memory_limit_mb: u64,
    /// Malformed replies tolerated per step before the agent fails.
    pub max_format_retries: u32,
    /// Times a task may be started (including lease recovery) before it fails.
    pub max_task_attempts: u32,
}

impl Default for ResourceBudget {
    fn default() -> Self {
        Self {
            max_steps_per_agent: 16,
            max_depth: 3,
            max_children_per_task: 16,
            max_pending_tasks: 10_000,
            max_inference_calls: 10_000,
            max_total_tokens: 50_000_000,
            max_spend_micros: 0,
            max_wall_clock_secs: 3_600,
            tool_timeout_ms: 30_000,
            max_tool_output_bytes: 65_536,
            tool_memory_limit_mb: 512,
            max_format_retries: 2,
            max_task_attempts: 2,
        }
    }
}

/// Resources consumed so far by a run or agent.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BudgetUsage {
    pub inference_calls: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub spend_micros: u64,
    pub tool_calls: u64,
}

impl BudgetUsage {
    /// Input plus output tokens.
    pub fn total_tokens(&self) -> u64 {
        self.input_tokens.saturating_add(self.output_tokens)
    }

    /// Adds another usage record into this one, saturating on overflow.
    pub fn add(&mut self, other: &BudgetUsage) {
        self.inference_calls = self.inference_calls.saturating_add(other.inference_calls);
        self.input_tokens = self.input_tokens.saturating_add(other.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(other.output_tokens);
        self.spend_micros = self.spend_micros.saturating_add(other.spend_micros);
        self.tool_calls = self.tool_calls.saturating_add(other.tool_calls);
    }
}

impl ResourceBudget {
    /// Checks whether one more inference call fits in the run budget.
    pub fn check_inference(&self, used: &BudgetUsage) -> CoreResult<()> {
        check_limit("inference_calls", self.max_inference_calls, used.inference_calls.saturating_add(1))?;
        check_limit("tokens", self.max_total_tokens, used.total_tokens())
    }

    /// Checks whether `additional_micros` of spend stays within the cap.
    pub fn check_spend(&self, used: &BudgetUsage, additional_micros: u64) -> CoreResult<()> {
        check_limit(
            "spend_micros",
            self.max_spend_micros,
            used.spend_micros.saturating_add(additional_micros),
        )
    }

    /// Checks a child task depth.
    pub fn check_depth(&self, depth: u32) -> CoreResult<()> {
        check_limit("depth", u64::from(self.max_depth), u64::from(depth))
    }

    /// Checks whether a decomposition may create `children` subtasks.
    pub fn check_children(&self, children: usize) -> CoreResult<()> {
        check_limit(
            "children_per_task",
            u64::from(self.max_children_per_task),
            u64::try_from(children).unwrap_or(u64::MAX),
        )
    }

    /// Checks whether `pending` tasks plus `new` tasks stay under the pending limit.
    pub fn check_pending_tasks(&self, pending: usize, new: usize) -> CoreResult<()> {
        let total = u64::try_from(pending.saturating_add(new)).unwrap_or(u64::MAX);
        check_limit("pending_tasks", u64::from(self.max_pending_tasks), total)
    }

    /// Checks whether an agent may take another step.
    pub fn check_steps(&self, steps_taken: u32) -> CoreResult<()> {
        check_limit(
            "steps",
            u64::from(self.max_steps_per_agent),
            u64::from(steps_taken).saturating_add(1),
        )
    }
}

fn check_limit(resource: &'static str, limit: u64, requested: u64) -> CoreResult<()> {
    if requested > limit {
        Err(CoreError::BudgetExceeded {
            resource,
            limit,
            requested,
        })
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inference_limit_is_inclusive() {
        let budget = ResourceBudget {
            max_inference_calls: 2,
            ..ResourceBudget::default()
        };
        let mut used = BudgetUsage::default();
        assert!(budget.check_inference(&used).is_ok());
        used.inference_calls = 1;
        assert!(budget.check_inference(&used).is_ok());
        used.inference_calls = 2;
        assert!(matches!(
            budget.check_inference(&used),
            Err(CoreError::BudgetExceeded { resource: "inference_calls", .. })
        ));
    }

    #[test]
    fn zero_spend_budget_forbids_any_spend() {
        let budget = ResourceBudget::default();
        assert_eq!(budget.max_spend_micros, 0);
        assert!(budget.check_spend(&BudgetUsage::default(), 0).is_ok());
        assert!(budget.check_spend(&BudgetUsage::default(), 1).is_err());
    }

    #[test]
    fn spend_accumulates_across_calls() {
        let budget = ResourceBudget {
            max_spend_micros: 100,
            ..ResourceBudget::default()
        };
        let used = BudgetUsage {
            spend_micros: 60,
            ..BudgetUsage::default()
        };
        assert!(budget.check_spend(&used, 40).is_ok());
        assert!(budget.check_spend(&used, 41).is_err());
    }

    #[test]
    fn depth_children_pending_and_steps_are_enforced() {
        let budget = ResourceBudget {
            max_depth: 2,
            max_children_per_task: 3,
            max_pending_tasks: 5,
            max_steps_per_agent: 4,
            ..ResourceBudget::default()
        };
        assert!(budget.check_depth(2).is_ok());
        assert!(budget.check_depth(3).is_err());
        assert!(budget.check_children(3).is_ok());
        assert!(budget.check_children(4).is_err());
        assert!(budget.check_pending_tasks(3, 2).is_ok());
        assert!(budget.check_pending_tasks(3, 3).is_err());
        assert!(budget.check_steps(3).is_ok());
        assert!(budget.check_steps(4).is_err());
    }

    #[test]
    fn usage_addition_saturates() {
        let mut a = BudgetUsage {
            input_tokens: u64::MAX,
            ..BudgetUsage::default()
        };
        a.add(&BudgetUsage {
            input_tokens: 10,
            ..BudgetUsage::default()
        });
        assert_eq!(a.input_tokens, u64::MAX);
    }
}
