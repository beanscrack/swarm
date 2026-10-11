//! Cost accounting for provider usage.

use serde::{Deserialize, Serialize};

use swarm_core::budget::TokenUsage;

/// Per-model pricing in micro-USD per one million tokens.
///
/// Zero for free tiers and mocks; used only to *estimate* spend (the gate is
/// about preventing runaway cost, not billing precision).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelPricing {
    /// Cost of input tokens in micro-USD per 1M tokens.
    pub input_per_million_micros: u64,
    /// Cost of output tokens in micro-USD per 1M tokens.
    pub output_per_million_micros: u64,
}

impl ModelPricing {
    /// Estimated cost of a usage record in micro-USD (truncated per side).
    pub fn estimate(&self, usage: &TokenUsage) -> u64 {
        let in_cost =
            usage.input_tokens.saturating_mul(self.input_per_million_micros) / 1_000_000;
        let out_cost =
            usage.output_tokens.saturating_mul(self.output_per_million_micros) / 1_000_000;
        in_cost.saturating_add(out_cost)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estimate_scales_linearly() {
        let p = ModelPricing {
            input_per_million_micros: 1_000_000,
            output_per_million_micros: 2_000_000,
        };
        let u = TokenUsage {
            input_tokens: 1_000_000,
            output_tokens: 500_000,
            cached_input_tokens: 0,
        };
        assert_eq!(p.estimate(&u), 2_000_000);
    }

    #[test]
    fn estimate_floors_per_side_and_never_overflows() {
        let p = ModelPricing {
            input_per_million_micros: 10,
            output_per_million_micros: 10,
        };
        // 999 tokens of a side < 1M costs nothing after truncation.
        assert_eq!(
            p.estimate(&TokenUsage {
                input_tokens: 999,
                output_tokens: 999,
                cached_input_tokens: 0,
            }),
            0
        );
        let huge = TokenUsage {
            input_tokens: u64::MAX,
            output_tokens: u64::MAX,
            cached_input_tokens: 0,
        };
        // Saturating: no panic, no wraparound.
        assert!(p.estimate(&huge) < u64::MAX);
    }
}
