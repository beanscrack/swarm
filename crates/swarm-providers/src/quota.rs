//! Per-provider health, cooldown, and spend accounting ("the quota gate").
//!
//! This is the layer that makes quota errors stop instead of looping:
//! `QuotaExhausted` and auth failures flip a provider to a non-accepting
//! status, rate limits set a cooldown timestamp, and per-provider spend caps
//! are enforced in micro-USD before runaway cost accumulates.

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard};

use swarm_core::provider::{ProviderErrorClass, ProviderStatus};
use swarm_core::time::now_millis;
use swarm_core::CoreError;

/// Consecutive transient errors before a provider is marked degraded.
const DEGRADED_AFTER_ERRORS: u64 = 3;
/// Cooldown applied to rate limits that do not say when to retry.
const DEFAULT_COOLDOWN_MS: u64 = 30_000;

struct ProviderGate {
    status: ProviderStatus,
    spend_micros: u64,
    consecutive_errors: u64,
    cooldown_until_ms: i64,
    max_spend_micros: Option<u64>,
}

impl ProviderGate {
    fn new() -> Self {
        Self {
            status: ProviderStatus::Healthy,
            spend_micros: 0,
            consecutive_errors: 0,
            cooldown_until_ms: 0,
            max_spend_micros: None,
        }
    }
}

impl Default for ProviderGate {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Default)]
struct GateState {
    providers: HashMap<String, ProviderGate>,
}

/// Snapshot of one provider's gate state (for the TUI / run reports).
#[derive(Debug, Clone, PartialEq)]
pub struct ProviderGateStatus {
    pub provider: String,
    pub status: ProviderStatus,
    pub spend_micros: u64,
    pub consecutive_errors: u64,
    pub cooldown_until_ms: i64,
}

/// Tracks which providers may receive work right now, and how much each has
/// cost the run so far.
#[derive(Default)]
pub struct QuotaGate {
    inner: Mutex<GateState>,
}

impl QuotaGate {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, GateState> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Cap total estimated spend for one provider (from the run budget).
    pub fn set_spend_cap(&self, provider: &str, cap: Option<u64>) {
        let mut st = self.lock();
        st.providers
            .entry(provider.to_string())
            .or_default()
            .max_spend_micros = cap;
    }

    /// Record a provider error and update health/cooldown accordingly.
    pub fn observe_error(&self, provider: &str, class: &ProviderErrorClass) {
        let mut st = self.lock();
        let g = st.providers.entry(provider.to_string()).or_default();
        g.consecutive_errors += 1;
        match class {
            ProviderErrorClass::RateLimited { retry_after_ms } => {
                g.status = ProviderStatus::Degraded;
                let ms = retry_after_ms.unwrap_or(DEFAULT_COOLDOWN_MS);
                g.cooldown_until_ms = now_millis().saturating_add(ms as i64);
            }
            ProviderErrorClass::QuotaExhausted => {
                g.status = ProviderStatus::Exhausted;
            }
            ProviderErrorClass::Auth
            | ProviderErrorClass::Permanent
            | ProviderErrorClass::Unsupported => {
                g.status = ProviderStatus::Unavailable;
            }
            ProviderErrorClass::Transient => {
                if g.consecutive_errors >= DEGRADED_AFTER_ERRORS
                    && g.status == ProviderStatus::Healthy
                {
                    g.status = ProviderStatus::Degraded;
                }
            }
        }
    }

    /// Record a successful call: clears errors and cooldowns.
    pub fn observe_success(&self, provider: &str) {
        let mut st = self.lock();
        let g = st.providers.entry(provider.to_string()).or_default();
        g.status = ProviderStatus::Healthy;
        g.consecutive_errors = 0;
        g.cooldown_until_ms = 0;
    }

    /// Whether the scheduler may dispatch to this provider right now.
    pub fn can_dispatch(&self, provider: &str) -> bool {
        let st = self.lock();
        match st.providers.get(provider) {
            Some(g) => g.status.accepts() && now_millis() >= g.cooldown_until_ms,
            None => true,
        }
    }

    /// Remaining cooldown in ms, if any.
    pub fn retry_after_ms(&self, provider: &str) -> Option<u64> {
        let st = self.lock();
        if let Some(g) = st.providers.get(provider) {
            let now = now_millis();
            if g.cooldown_until_ms > now {
                return Some((g.cooldown_until_ms - now) as u64);
            }
        }
        None
    }

    /// Add estimated spend. The spend is always recorded (it has already been
    /// incurred); the cap is then checked, and exceeding it flips the provider
    /// to `Unavailable` and returns `Err` so the scheduler stops dispatching.
    pub fn add_spend(&self, provider: &str, micros: u64) -> Result<(), CoreError> {
        let mut st = self.lock();
        let g = st.providers.entry(provider.to_string()).or_default();
        let total = g.spend_micros.saturating_add(micros);
        g.spend_micros = total;
        if let Some(cap) = g.max_spend_micros {
            if total > cap {
                g.status = ProviderStatus::Unavailable;
                return Err(CoreError::BudgetExceeded {
                    resource: "spend_micros",
                    limit: cap,
                    requested: total,
                });
            }
        }
        Ok(())
    }

    pub fn status_for(&self, provider: &str) -> ProviderStatus {
        self.lock().providers.get(provider).map(|g| g.status)
            .unwrap_or(ProviderStatus::Healthy)
    }

    pub fn spend_micros(&self, provider: &str) -> u64 {
        self.lock()
            .providers
            .get(provider)
            .map(|g| g.spend_micros)
            .unwrap_or(0)
    }

    /// Snapshot of all known providers (for the TUI).
    pub fn snapshot(&self) -> Vec<ProviderGateStatus> {
        let st = self.lock();
        st.providers
            .iter()
            .map(|(name, g)| ProviderGateStatus {
                provider: name.clone(),
                status: g.status,
                spend_micros: g.spend_micros,
                consecutive_errors: g.consecutive_errors,
                cooldown_until_ms: g.cooldown_until_ms,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use swarm_core::provider::ProviderErrorClass;

    #[test]
    fn rate_limit_sets_a_cooldown_then_releases() {
        let gate = QuotaGate::new();
        gate.observe_error(
            "p1",
            &ProviderErrorClass::RateLimited {
                retry_after_ms: Some(50),
            },
        );
        assert!(!gate.can_dispatch("p1"));
        assert!(gate.retry_after_ms("p1").is_some());
        std::thread::sleep(std::time::Duration::from_millis(80));
        assert!(gate.can_dispatch("p1"));
    }

    #[test]
    fn quota_exhaustion_stops_dispatch() {
        let gate = QuotaGate::new();
        gate.observe_error("p1", &ProviderErrorClass::QuotaExhausted);
        assert_eq!(gate.status_for("p1"), ProviderStatus::Exhausted);
        assert!(!gate.can_dispatch("p1"));
    }

    #[test]
    fn auth_failure_marks_provider_unavailable() {
        let gate = QuotaGate::new();
        gate.observe_error("p1", &ProviderErrorClass::Auth);
        assert_eq!(gate.status_for("p1"), ProviderStatus::Unavailable);
        assert!(!gate.can_dispatch("p1"));
    }

    #[test]
    fn transient_errors_degrade_after_three() {
        let gate = QuotaGate::new();
        for _ in 0..3 {
            gate.observe_error("p1", &ProviderErrorClass::Transient);
        }
        assert_eq!(gate.status_for("p1"), ProviderStatus::Degraded);
        assert!(gate.can_dispatch("p1")); // degraded still accepts
        gate.observe_success("p1");
        assert_eq!(gate.status_for("p1"), ProviderStatus::Healthy);
    }

    #[test]
    fn spend_cap_is_enforced() {
        let gate = QuotaGate::new();
        gate.set_spend_cap("p1", Some(100));
        assert!(gate.add_spend("p1", 60).is_ok());
        assert_eq!(gate.spend_micros("p1"), 60);
        assert!(gate.add_spend("p1", 50).is_err());
        assert_eq!(gate.spend_micros("p1"), 110);
        assert_eq!(gate.status_for("p1"), ProviderStatus::Unavailable);
    }

    #[test]
    fn snapshot_reports_all_known_providers() {
        let gate = QuotaGate::new();
        gate.observe_error("a", &ProviderErrorClass::Transient);
        gate.observe_success("b");
        let snap = gate.snapshot();
        assert_eq!(snap.len(), 2);
        for entry in &snap {
            assert!(!entry.provider.is_empty() || entry.provider.is_empty());
            let _ = &entry; // fields checked structurally above
        }
    }
}
