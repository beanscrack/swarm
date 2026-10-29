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
use swarm_providers::{coding_script, BrowserChatConfig, BrowserChatProvider, ChatSite, ManagedAgentConfig, ManagedAgentProvider, MockProvider, OpenAiConfig, OpenAiProvider, Provider, ProviderRegistry};
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

/// Browser-backed ChatGPT chat preset. Requires the local bridge to be running.
///
/// `model` is matched against the account's model picker; "auto" leaves the
/// site's own default in place.
pub fn chatgpt_web_provider_config() -> ProviderConfig {
    ProviderConfig {
        name: ProviderId::new("chatgpt-web").expect("valid provider name"),
        kind: ProviderKind::ChatGptWeb,
        model: "auto".to_string(),
        base_url: Some("http://127.0.0.1:8766".to_string()),
        api_key: None,
        api_key_env: None,
        max_output_tokens: 8192,
        temperature: None,
        workspace_dir: None,
        command: Vec::new(),
        timeout_secs: None,
        extra_args: Vec::new(),
        zero_cost: false,
        billing: BillingMode::Subscription,
        max_concurrency: Some(4),
    }
}

/// Browser-backed Gemini chat preset. Requires the local bridge to be running.
pub fn gemini_web_provider_config() -> ProviderConfig {
    ProviderConfig {
        name: ProviderId::new("gemini-web").expect("valid provider name"),
        kind: ProviderKind::GeminiWeb,
        model: "4 Argon".to_string(),
        base_url: Some(ChatSite::Gemini.default_bridge_url().to_string()),
        api_key: None,
        api_key_env: None,
        max_output_tokens: 8192,
        temperature: None,
        workspace_dir: None,
        command: Vec::new(),
        timeout_secs: None,
        extra_args: Vec::new(),
        zero_cost: false,
        billing: BillingMode::Subscription,
        max_concurrency: Some(4),
    }
}

/// Constructs the live provider for a single [`ProviderConfig`].
fn provider_for(pc: &ProviderConfig) -> Result<Arc<dyn Provider>, RuntimeError> {
    match pc.kind {
        ProviderKind::Mock => {
            let mut p = MockProvider::unowned();
            p.id = pc.name.clone();
            p.model = pc.model.clone();
            // `SWARM_MOCK_SCRIPT=coding` makes the offline mock act like a coding
            // agent (write hello.txt, check it, finish) instead of only replying:
            // enough to exercise tools, patches and jobs without a model.
            if std::env::var("SWARM_MOCK_SCRIPT").as_deref() == Ok("coding") {
                p.script = Some(coding_script());
            }
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
        kind @ (ProviderKind::GeminiWeb | ProviderKind::ChatGptWeb) => {
            let site = match kind {
                ProviderKind::ChatGptWeb => ChatSite::ChatGpt,
                _ => ChatSite::Gemini,
            };
            let base_url = pc
                .base_url
                .as_deref()
                .unwrap_or_else(|| site.default_bridge_url());
            let config = BrowserChatConfig::new(site, base_url, &pc.model)
                .billing(pc.billing)
                .max_concurrency(pc.max_concurrency.unwrap_or(4));
            let provider: Arc<dyn Provider> =
                Arc::new(BrowserChatProvider::new(pc.name.clone(), config)?);
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

    #[test]
    fn chatgpt_web_preset_builds_as_subscription_provider() {
        let pc = chatgpt_web_provider_config();
        assert_eq!(pc.kind, ProviderKind::ChatGptWeb);
        assert_eq!(pc.name.as_str(), "chatgpt-web");
        assert_eq!(pc.model, "auto");
        assert_eq!(pc.base_url.as_deref(), Some("http://127.0.0.1:8766"));
        assert!(!pc.zero_cost);
        let mut cfg = RunConfig::mock("goal", 2);
        cfg.provider = pc.name.clone();
        cfg.providers = vec![pc];
        cfg.zero_cost = false;
        cfg.scheduler.zero_cost = false;
        let registry = build_registry(&cfg).unwrap();
        assert_eq!(registry.find(&cfg.provider).unwrap().kind(), ProviderKind::ChatGptWeb);
        // ChatGPT's own pick is used, so the model name is not an API slug.
        assert_eq!(registry.find(&cfg.provider).unwrap().model(), "auto");
    }

    #[test]
    fn gemini_web_preset_builds_as_subscription_provider() {
        let pc = gemini_web_provider_config();
        assert_eq!(pc.kind, ProviderKind::GeminiWeb);
        assert_eq!(pc.model, "4 Argon");
        assert!(!pc.zero_cost);
        let mut cfg = RunConfig::mock("goal", 2);
        cfg.provider = pc.name.clone();
        cfg.providers = vec![pc];
        cfg.zero_cost = false;
        cfg.scheduler.zero_cost = false;
        let registry = build_registry(&cfg).unwrap();
        assert_eq!(registry.len(), 1);
        assert_eq!(registry.find(&cfg.provider).unwrap().kind(), ProviderKind::GeminiWeb);
    }
}
