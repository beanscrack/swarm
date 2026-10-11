//! Scheduler configuration and in-flight occupancy accounting.

use std::collections::BTreeMap;

/// Tunable knobs for the inference scheduler.
///
/// The scheduler models a run's "inference capacity" as independent budgets a
/// task must satisfy to be admitted:
///
/// * **Global lanes** — [`SchedulerConfig::max_active`], the ceiling on
///   simultaneous inferences summed over all providers.
/// * **Provider lanes** — [`SchedulerConfig::provider_limits`] (falling back to
///   [`SchedulerConfig::default_provider_limit`]), the ceiling on simultaneous
///   inferences sent to a single provider.
/// * **Group fairness** — [`SchedulerConfig::max_per_group`], the ceiling on
///   new tasks admitted from one research group in a single dispatch cycle.
///
/// Keeping these independent lets a fast, low-capacity local model and a slow,
/// high-value paid model coexist without either monopolising the batch.
#[derive(Debug, Clone)]
pub struct SchedulerConfig {
    /// Maximum number of simultaneous inferences, summed over all providers.
    pub max_active: usize,
    /// Per-provider concurrency caps, keyed by provider id.
    pub provider_limits: BTreeMap<String, usize>,
    /// Concurrency cap applied to providers not named in `provider_limits`.
    pub default_provider_limit: usize,
    /// Maximum new tasks admitted per research group in a single dispatch cycle.
    pub max_per_group: usize,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            max_active: 8,
            provider_limits: BTreeMap::new(),
            default_provider_limit: 4,
            max_per_group: 2,
        }
    }
}

impl SchedulerConfig {
    /// The concurrency cap for `provider`, honouring per-provider overrides.
    pub fn limit_for(&self, provider: &str) -> usize {
        self.provider_limits
            .get(provider)
            .copied()
            .unwrap_or(self.default_provider_limit)
    }
}

/// Snapshot of how many inferences are already in flight, per provider.
///
/// Passed into [`crate::Scheduler::plan`] so it admits only enough new work to
/// fill the *remaining* lanes, rather than assuming a cold start.
#[derive(Debug, Clone, Default)]
pub struct Occupancy(BTreeMap<String, usize>);

impl Occupancy {
    pub fn new() -> Self {
        Self::default()
    }

    /// In-flight inferences currently held by `provider` (0 if none).
    pub fn get(&self, provider: &str) -> usize {
        self.0.get(provider).copied().unwrap_or(0)
    }

    /// Record a change in in-flight count for `provider` (0 removes the entry).
    pub fn set(&mut self, provider: &str, n: usize) {
        if n == 0 {
            self.0.remove(provider);
        } else {
            self.0.insert(provider.to_string(), n);
        }
    }

    /// Increase the in-flight count for `provider` by `delta`.
    pub fn add(&mut self, provider: &str, delta: usize) {
        *self.0.entry(provider.to_string()).or_insert(0) += delta;
    }

    /// Total in-flight inferences across all providers.
    pub fn total(&self) -> usize {
        self.0.values().sum()
    }

    /// True when no provider has an in-flight inference.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The providers with a non-zero in-flight count.
    pub fn providers(&self) -> impl Iterator<Item = &str> + '_ {
        self.0.keys().map(String::as_str)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limit_for_honours_overrides() {
        let mut cfg = SchedulerConfig::default();
        cfg.provider_limits.insert("mock".into(), 2);
        assert_eq!(cfg.limit_for("mock"), 2);
        assert_eq!(cfg.limit_for("openai"), cfg.default_provider_limit);
    }

    #[test]
    fn occupancy_tracks_counts_and_totals() {
        let mut o = Occupancy::new();
        assert!(o.is_empty());
        o.set("mock", 3);
        assert_eq!(o.get("mock"), 3);
        assert_eq!(o.total(), 3);

        o.set("mock", 0);
        assert_eq!(o.get("mock"), 0);
        assert!(o.is_empty());

        o.add("mock", 2);
        o.add("openai", 1);
        assert_eq!(o.total(), 3);
        assert_eq!(o.providers().count(), 2);
        assert_eq!(o.get("absent"), 0);
    }
}
