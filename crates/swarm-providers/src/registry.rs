//! Provider registry: dispatch table plus run-policy enforcement.
//!
//! [`ProviderRegistry::dispatch`] is the only path from the scheduler to a
//! provider: it resolves the provider by id, applies the zero-cost policy and
//! the [`QuotaGate`] (cooldowns, exhaustion, spend caps), runs the call, and
//! feeds the outcome back into the gate so quota errors stop instead of
//! looping.

use std::sync::Arc;

use swarm_core::provider::{ProviderEntry, ProviderStatus};
use swarm_core::{CoreError, ProviderId};

use crate::error::ProviderError;
use crate::provider::{InferenceResponse, Provider, SendRequest, Session, SessionConfig};
use crate::quota::QuotaGate;

/// Dispatch table plus shared quota gate.
#[derive(Default)]
pub struct ProviderRegistry {
    providers: Vec<Arc<dyn Provider>>,
    pub gate: QuotaGate,
}

impl ProviderRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, provider: Arc<dyn Provider>) {
        self.providers.push(provider);
    }

    pub fn len(&self) -> usize {
        self.providers.len()
    }

    pub fn is_empty(&self) -> bool {
        self.providers.is_empty()
    }

    pub fn find(&self, id: &ProviderId) -> Option<Arc<dyn Provider>> {
        self.providers
            .iter()
            .find(|p| p.id() == id)
            .map(Arc::clone)
    }

    /// One run entry per registered provider (for the TUI / run reports).
    pub fn entries(&self) -> Vec<ProviderEntry> {
        self.providers
            .iter()
            .map(|p| ProviderEntry {
                id: p.id().clone(),
                kind: p.kind(),
                model: p.model().to_string(),
                capabilities: p.capabilities(),
                status: self.gate.status_for(p.id().as_str()),
                notes: None,
            })
            .collect()
    }

    /// Dispatch one inference call to the named provider under the run
    /// policies. This is where quota/cooldown/spend-cap decisions are
    /// observed before *and* after the call.
    pub async fn dispatch(
        &self,
        id: &ProviderId,
        request: SendRequest,
    ) -> Result<InferenceResponse, ProviderError> {
        let provider = self
            .find(id)
            .ok_or_else(|| ProviderError::Session(format!("unknown provider: {id}")))?;

        // Zero-cost policy: a zero-cost run may only touch ZeroCost providers.
        if request.zero_cost && !provider.billing().is_zero_cost() {
            return Err(CoreError::ZeroCostViolation {
                provider: id.as_str().to_string(),
                billing: provider.billing().as_str(),
            }
            .into());
        }

        let key = provider.id().as_str().to_string();
        if !self.gate.can_dispatch(&key) {
            return Err(ProviderError::Throttled {
                provider: key,
                reason: format!(
                    "provider is not accepting work (status {:?})",
                    self.gate.status_for(&key)
                ),
                retry_after_ms: self.gate.retry_after_ms(&key),
            });
        }

        let session: Box<dyn Session> = provider
            .start_session(SessionConfig::default())
            .await?;
        let result = session.send(request).await;

        match &result {
            Ok(resp) => {
                self.gate.observe_success(&key);
                let cost = provider.pricing().estimate(&resp.usage);
                if cost > 0 {
                    // Flip to Unavailable (and report BudgetExceeded) when a
                    // spend cap is crossed; subsequent dispatches are throttled.
                    let _ = self.gate.add_spend(&key, cost);
                }
            }
            Err(e) => {
                self.gate.observe_error(&key, &e.classify());
            }
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use swarm_core::budget::BillingMode;
    use swarm_core::message::ChatMessage;
    use swarm_core::provider::ProviderErrorClass;

    use crate::mock::MockProvider;
    use crate::pricing::ModelPricing;

    fn req(text: &str) -> SendRequest {
        SendRequest::new(vec![ChatMessage::user(text)])
    }

    #[tokio::test]
    async fn dispatch_routes_to_the_requested_provider() {
        let mut reg = ProviderRegistry::new();
        reg.register(MockProvider::new());
        let id = ProviderId::new("mock").unwrap();
        let resp = reg.dispatch(&id, req("hi")).await.unwrap();
        assert!(resp.text.contains("reply #1"));
        assert!(resp.model == "mock-model");
    }

    #[tokio::test]
    async fn zero_cost_runs_cannot_use_paid_providers() {
        let paid = MockProvider::unowned().with_billing(BillingMode::OnDemand);
        let mut reg = ProviderRegistry::new();
        reg.register(std::sync::Arc::new(paid));
        let id = ProviderId::new("mock").unwrap();
        let mut request = req("hi");
        request.zero_cost = true;
        let err = reg.dispatch(&id, request).await.unwrap_err();
        assert!(matches!(
            err,
            ProviderError::Core(CoreError::ZeroCostViolation { .. })
        ));
    }

    #[tokio::test]
    async fn quota_failures_stop_instead_of_looping() {
        let mut reg = ProviderRegistry::new();
        reg.register(MockProvider::failing(ProviderErrorClass::QuotaExhausted));
        let id = ProviderId::new("mock").unwrap();
        // First dispatch reaches the provider and observes the quota error.
        let first = reg.dispatch(&id, req("a")).await.unwrap_err();
        assert!(matches!(first, ProviderError::Api { .. }));
        // Every later dispatch is throttled without touching the provider.
        for _ in 0..5 {
            let e = reg.dispatch(&id, req("b")).await.unwrap_err();
            assert!(matches!(e, ProviderError::Throttled { .. }));
        }
        assert_eq!(reg.gate.status_for("mock"), ProviderStatus::Exhausted);
    }

    #[tokio::test]
    async fn spend_cap_halts_further_dispatch() {
        let mut p = MockProvider::unowned();
        p.pricing = ModelPricing {
            input_per_million_micros: 10_000_000,
            output_per_million_micros: 10_000_000,
        };
        let mut reg = ProviderRegistry::new();
        reg.register(std::sync::Arc::new(p));
        reg.gate.set_spend_cap("mock", Some(10));
        let id = ProviderId::new("mock").unwrap();
        // One call costs 11 micro-USD: over the cap.
        let first = reg.dispatch(&id, req("x")).await.unwrap();
        assert!(first.usage.input_tokens > 0);
        assert!(reg.gate.spend_micros("mock") >= 10);
        // The cap is now crossed: further dispatch is throttled.
        let e = reg.dispatch(&id, req("y")).await.unwrap_err();
        assert!(matches!(e, ProviderError::Throttled { .. }));
    }
}
