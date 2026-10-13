//! Provider-registry and scheduler-config glue.
//!
//! Turns a declarative [`RunConfig`] into live provider objects (a
//! [`ProviderRegistry`]) and the scheduler's concurrency caps. This is the
//! seam between "what the user asked for" and "what the runtime can call".

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use swarm_core::budget::BillingMode;
use swarm_core::config::{ProviderConfig, RunConfig};
use swarm_core::provider::ProviderKind;
use swarm_core::ProviderId;
use swarm_providers::{ManagedAgentConfig, ManagedAgentProvider, MockProvider, OpenAiConfig, OpenAiProvider, Provider, ProviderRegistry};
use swarm_scheduler::SchedulerConfig;

use crate::error::RuntimeError;

/// Whole-call timeout (ms) for OpenAI-compatible requests. The 60 s default
/// kills local reasoning models whose calls take 100-265 s.
const OPENAI_TIMEOUT_MS: u64 = 600_000;

/// Builds a provider registry from `cfg.providers`, skipping providers the
/// run's zero-cost policy disallows.
pub fn build_registry(cfg: &RunConfig) -> Result<ProviderRegistry, RuntimeError> {
    let mut registry = ProviderRegistry::new();
    for pc in &cfg.providers {
        if !cfg.may_run(pc) {
            continue;
        }
        registry.register(provider_for(pc)?);
    }
    Ok(registry)
}

/// Maps a run's provider/scheduler settings onto the scheduler's config.
pub fn scheduler_config(cfg: &RunConfig, registry: &ProviderRegistry) -> SchedulerConfig {
    let mut provider_limits = BTreeMap::new();
    for pc in &cfg.providers {
        if !cfg.may_run(pc) {
            continue;
        }
        let requested = pc
            .max_concurrency
            .unwrap_or(cfg.scheduler.default_provider_concurrency) as usize;
        let limit = registry.find(&pc.name)
            .and_then(|provider| provider.capabilities().max_concurrency)
            .map_or(requested, |cap| requested.min(cap as usize));
        provider_limits.insert(pc.name.as_str().to_string(), limit);
    }
    SchedulerConfig {
        max_active: cfg.scheduler.max_active_inferences as usize,
        provider_limits,
        default_provider_limit: cfg.scheduler.default_provider_concurrency as usize,
        max_per_group: 2,
    }
}

/// A ready-made provider config for the local llama-server (qwen3.8-27b).
pub fn local_provider_config() -> ProviderConfig {
    ProviderConfig {
        name: ProviderId::new("local").expect("provider name 'local' is valid"),
        kind: ProviderKind::OpenAiCompatible,
        model: "qwen3.8-27b-hauhau".to_string(),
        base_url: Some("http://127.0.0.1:8080/v1".to_string()),
        api_key: None,
        api_key_env: None,
        max_output_tokens: 8192,
        temperature: Some(0.0),
        workspace_dir: None,
        command: Vec::new(),
        timeout_secs: None,
        extra_args: Vec::new(),
        zero_cost: true,
        billing: BillingMode::ZeroCost,
        max_concurrency: Some(1),
    }
}

/// Constructs the live provider for a single [`ProviderConfig`].
fn provider_for(pc: &ProviderConfig) -> Result<Arc<dyn Provider>, RuntimeError> {
    match pc.kind {
        ProviderKind::Mock => {
            let mut p = MockProvider::unowned();
            p.id = pc.name.clone();
            p.model = pc.model.clone();
            let provider: Arc<dyn Provider> = Arc::new(p);
            Ok(provider)
        }
        ProviderKind::OpenAiCompatible => {
            let base = pc.base_url.as_deref().unwrap_or("");
            let mut oai = OpenAiConfig::new(base, &pc.model)?;
            oai.api_key = pc.api_key_resolved();
            oai.temperature = pc.temperature.unwrap_or(0.0);
            oai.max_tokens = Some(pc.max_output_tokens);
            oai.timeout_ms = OPENAI_TIMEOUT_MS;
            oai.billing = pc.billing.clone();
            let provider: Arc<dyn Provider> = Arc::new(OpenAiProvider::new(pc.name.clone(), oai)?);
            Ok(provider)
        }
        ProviderKind::ManagedAgent => {
            if pc.command.is_empty() {
                return Err(RuntimeError::Config(format!(
                    "managed-agent provider '{}' needs a command", pc.name,
                )));
            }
            let workdir = pc.workspace_dir.clone().ok_or_else(|| RuntimeError::Config(format!(
                "managed-agent provider '{}' needs a workspace_dir", pc.name,
            )))?;
            let mut command = pc.command.clone();
            command.extend(pc.extra_args.iter().cloned());
            let config = ManagedAgentConfig::new(&pc.model, command, workdir)
                .billing(pc.billing)
                .timeout(Duration::from_secs(pc.timeout_secs.unwrap_or(600).max(1)));
            let provider: Arc<dyn Provider> = Arc::new(ManagedAgentProvider::new(pc.name.clone(), config)?);
            Ok(provider)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mock_config_builds_one_provider() {
        let cfg = RunConfig::mock("goal", 1);
        let registry = build_registry(&cfg).unwrap();
        assert_eq!(registry.len(), 1);
        assert!(registry.find(&ProviderId::new("mock").unwrap()).is_some());
    }

    #[test]
    fn local_provider_config_builds_without_network() {
        let pc = local_provider_config();
        assert_eq!(pc.name.as_str(), "local");
        assert_eq!(pc.model, "qwen3.8-27b-hauhau");
        assert_eq!(pc.base_url.as_deref(), Some("http://127.0.0.1:8080/v1"));
        assert!(pc.zero_cost);
        assert_eq!(pc.max_concurrency, Some(1));

        let mut cfg = RunConfig::mock("goal", 1);
        cfg.providers = vec![pc];
        let registry = build_registry(&cfg).unwrap();
        assert_eq!(registry.len(), 1);
        assert!(registry.find(&ProviderId::new("local").unwrap()).is_some());
    }

    #[test]
    fn scheduler_config_honours_max_concurrency() {
        let mut cfg = RunConfig::mock("goal", 1);
        cfg.providers[0].max_concurrency = Some(1);
        let registry = build_registry(&cfg).unwrap();
        let s = scheduler_config(&cfg, &registry);
        assert_eq!(s.provider_limits.get("mock").copied(), Some(1));
        assert_eq!(s.max_per_group, 2);
        assert_eq!(
            s.default_provider_limit,
            cfg.scheduler.default_provider_concurrency as usize
        );
    }

    #[test]
    fn managed_provider_builds_from_command_and_workspace() {
        let mut cfg = RunConfig::mock("goal", 1);
        let mut provider = ProviderConfig::mock("managed", "local-agent");
        provider.kind = ProviderKind::ManagedAgent;
        provider.command = vec!["agent-program".into(), "{prompt}".into()];
        provider.workspace_dir = Some(std::env::temp_dir());
        cfg.provider = provider.name.clone();
        cfg.providers = vec![provider];
        let registry = build_registry(&cfg).unwrap();
        let managed = registry.find(&cfg.provider).unwrap();
        assert_eq!(managed.kind(), ProviderKind::ManagedAgent);
        let scheduler = scheduler_config(&cfg, &registry);
        assert_eq!(scheduler.provider_limits.get("managed"), Some(&1));
    }
}
