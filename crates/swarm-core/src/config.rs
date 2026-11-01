//! Run configuration, loaded from `swarm.toml` with CLI overrides.
//!
//! Everything here is pure data (serde) so a run can be described by a file,
//! reproduced exactly, and round-tripped through the store.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::budget::{BillingMode, ResourceBudget};
use crate::ids::ProviderId;
use crate::provider::ProviderKind;

string_enum! {
        /// Whether tool calls need human approval before execution.
        pub enum ApprovalPolicy {
        Never => "never",
        AskDangerous => "ask_dangerous",
        AskAll => "ask_all",
    }
}

string_enum! {
    /// Tool transport selection. Auto follows the provider's declared capabilities.
    pub enum ToolProtocolSelection {
        Auto => "auto",
        Native => "native",
        Text => "text",
    }
}

impl Default for ToolProtocolSelection {
    fn default() -> Self { Self::Auto }
}

/// Configuration for one provider endpoint.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct OpenAiOptions {
    pub chat_template_kwargs: serde_json::Map<String, serde_json::Value>,
    pub min_output_tokens: Option<u32>,
    pub headers: std::collections::BTreeMap<String, String>,
    pub streaming: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TaskPreset { General, Builder, Reviewer, TestWriter }
impl TaskPreset {
    pub fn as_str(self) -> &'static str { match self {
        Self::General => "general", Self::Builder => "builder", Self::Reviewer => "reviewer", Self::TestWriter => "test-writer",
    }}
}
impl std::str::FromStr for TaskPreset {
    type Err = crate::CoreError;
    fn from_str(s: &str) -> Result<Self, Self::Err> { match s {
        "general" => Ok(Self::General), "builder" => Ok(Self::Builder), "reviewer" => Ok(Self::Reviewer),
        "test-writer" => Ok(Self::TestWriter),
        _ => Err(crate::CoreError::Config(format!("unknown task preset: {s}"))),
    }}
}

impl Default for TaskPreset { fn default() -> Self { Self::General } }

impl TaskPreset {
    pub fn limits(self) -> Option<(u64, u32)> {
        match self {
            Self::General => None,
            Self::Builder => Some((14_000_000, 260)),
            Self::Reviewer => Some((6_000_000, 160)),
            Self::TestWriter => Some((8_000_000, 200)),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderEndpoint {
    pub base_url: String,
    pub max_concurrency: u32,
}

/// Configuration for one provider endpoint or a pool of equivalent endpoints.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    pub name: ProviderId,
    pub kind: ProviderKind,
    pub model: String,
    pub base_url: Option<String>,
    /// Literal API key. Prefer [`Self::api_key_env`] to keep secrets out of files.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    /// Name of an environment variable holding the API key.
    pub api_key_env: Option<String>,
    pub max_output_tokens: u32,
    pub temperature: Option<f32>,
    pub workspace_dir: Option<PathBuf>,
    /// Managed-agent executable and arguments; use `{prompt}` in an argument
    /// to pass the task there, otherwise the prompt is sent on stdin.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub command: Vec<String>,
    /// Whole-process timeout for a managed agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
    pub extra_args: Vec<String>,
    /// Zero-cost providers never trigger a paid request by default.
    pub zero_cost: bool,
    pub billing: BillingMode,
    pub max_concurrency: Option<u32>,
    #[serde(default)]
    pub openai: OpenAiOptions,
    #[serde(default)]
    pub endpoints: Vec<ProviderEndpoint>,
}

impl ProviderConfig {
    /// A deterministic, offline mock provider.
    pub fn mock(name: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            name: ProviderId::new(name).expect("provider id"),
            kind: ProviderKind::Mock,
            model: model.into(),
            base_url: None,
            api_key: None,
            api_key_env: None,
            max_output_tokens: 2048,
            temperature: Some(0.2),
            workspace_dir: None,
            command: Vec::new(),
            timeout_secs: None,
            extra_args: Vec::new(),
            zero_cost: true,
            billing: BillingMode::ZeroCost,
            max_concurrency: Some(4),
            openai: OpenAiOptions::default(),
            endpoints: Vec::new(),
        }
    }

    /// A real OpenAI-compatible endpoint.
    pub fn openai_compatible(name: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            name: ProviderId::new(name).expect("provider id"),
            kind: ProviderKind::OpenAiCompatible,
            model: model.into(),
            base_url: None,
            api_key: None,
            api_key_env: Some("OPENAI_API_KEY".into()),
            max_output_tokens: 4096,
            temperature: Some(0.2),
            workspace_dir: None,
            command: Vec::new(),
            timeout_secs: None,
            extra_args: Vec::new(),
            zero_cost: false,
            billing: BillingMode::OnDemand,
            max_concurrency: Some(4),
            openai: OpenAiOptions::default(),
            endpoints: Vec::new(),
        }
    }

    /// Resolve the API key from the env var first, else the literal value.
    pub fn api_key_resolved(&self) -> Option<String> {
        self.api_key_env
            .as_ref()
            .and_then(|var| std::env::var(var).ok())
            .or_else(|| self.api_key.clone())
    }
}

/// Exponential backoff parameters (mirrors the budget-level type).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct BackoffConfig {
    pub base_ms: u64,
    pub max_ms: u64,
    pub jitter: f64,
}

impl Default for BackoffConfig {
    fn default() -> Self {
        Self { base_ms: 500, max_ms: 60_000, jitter: 0.2 }
    }
}

/// Scheduling policy for the runtime.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SchedulerConfig {
    pub max_active_inferences: u32,
    pub default_provider_concurrency: u32,
    pub adaptive_concurrency: bool,
    pub fairness_round_robin: bool,
    /// When true, only zero-cost providers may be selected.
    pub zero_cost: bool,
    pub calls_per_minute: Option<u32>,
    pub backoff: BackoffConfig,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            max_active_inferences: 8,
            default_provider_concurrency: 4,
            adaptive_concurrency: true,
            fairness_round_robin: true,
            zero_cost: true,
            calls_per_minute: None,
            backoff: BackoffConfig::default(),
        }
    }
}

/// How much shared memory may be injected into an agent prompt.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct MemoryConfig {
    pub max_findings_in_prompt: usize,
    pub max_memory_tokens: usize,
    pub terms_per_finding: usize,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self { max_findings_in_prompt: 8, max_memory_tokens: 4000, terms_per_finding: 6 }
    }
}

/// Research-mode allocation policy.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ResearchConfig {
    pub max_branch_depth: u32,
    pub branches_per_hypothesis: u32,
    pub promote_threshold: f64,
    pub exploration_fraction: f64,
    pub require_reproduction: bool,
}

impl Default for ResearchConfig {
    fn default() -> Self {
        Self {
            max_branch_depth: 4,
            branches_per_hypothesis: 3,
            promote_threshold: 0.7,
            exploration_fraction: 0.3,
            require_reproduction: true,
        }
    }
}

/// Sandbox policy for tool execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSandboxConfig {
    pub workspace_root: PathBuf,
    pub blocked_paths: Vec<PathBuf>,
    pub max_output_bytes: u64,
    pub command_timeout_secs: u64,
    pub memory_limit_mb: Option<u64>,
    pub allow_network: bool,
    /// Optional preinstalled Linux image for OS-isolated, network-free command tools.
    #[serde(default)]
    pub docker_image: Option<String>,
    /// Refs this run's workers may `git push` to: branch names, `remote/branch`, or `*`.
    /// Empty (the default) means no push is allowed - the orchestrator owns remote writes.
    /// This is a guardrail on the shell command string, not a sandbox boundary.
    #[serde(default)]
    pub allow_push: Vec<String>,
    pub approval: ApprovalPolicy,
}

impl Default for ToolSandboxConfig {
    fn default() -> Self {
        Self {
            workspace_root: PathBuf::from("./work"),
            blocked_paths: Vec::new(),
            max_output_bytes: 256 * 1024,
            command_timeout_secs: 120,
            memory_limit_mb: None,
            allow_network: false,
            docker_image: None,
            allow_push: Vec::new(),
            approval: ApprovalPolicy::AskDangerous,
        }
    }
}

/// The full configuration for a single run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunConfig {
    pub objective: String,
    #[serde(default)]
    pub tool_protocol: ToolProtocolSelection,
    #[serde(default)]
    pub preset: TaskPreset,
    pub provider: ProviderId,
    pub model: String,
    pub agents: u32,
    pub zero_cost: bool,
    pub budget: ResourceBudget,
    pub providers: Vec<ProviderConfig>,
    pub scheduler: SchedulerConfig,
    pub memory: MemoryConfig,
    pub research: ResearchConfig,
    pub tools: ToolSandboxConfig,
    #[serde(default = "default_repo_map_chars")]
    pub repo_map_chars: usize,
    #[serde(default)]
    pub repo_map_file: Option<PathBuf>,
}

fn default_repo_map_chars() -> usize { 12_000 }

impl RunConfig {
    /// A fully offline, zero-cost configuration using the mock provider.
    pub fn mock(objective: impl Into<String>, agents: u32) -> Self {
        let agents = agents.max(1);
        Self {
            objective: objective.into(),
            tool_protocol: ToolProtocolSelection::Auto,
            preset: TaskPreset::General,
            provider: ProviderId::new("mock").expect("provider id"),
            model: "mock-model".into(),
            agents,
            zero_cost: true,
            budget: ResourceBudget {
                max_steps_per_agent: 40,
                max_inference_calls: agents as u64 * 40,
                max_total_tokens: 200_000,
                max_pending_tasks: (agents as u32 * 4).max(16),
                ..ResourceBudget::default()
            },
            providers: vec![ProviderConfig::mock("mock", "mock-model")],
            scheduler: SchedulerConfig { zero_cost: true, ..Default::default() },
            memory: MemoryConfig::default(),
            research: ResearchConfig::default(),
            tools: ToolSandboxConfig::default(),
            repo_map_chars: default_repo_map_chars(),
            repo_map_file: None,
        }
    }

    /// The provider config selected by `provider`, or the first if none match.
    pub fn provider_config(&self, id: &ProviderId) -> Option<ProviderConfig> {
        self.providers
            .iter()
            .find(|p| &p.name == id)
            .or_else(|| self.providers.first())
            .cloned()
    }

    /// True if the given provider may run under the current zero-cost policy.
    pub fn may_run(&self, provider: &ProviderConfig) -> bool {
        if self.zero_cost && !provider.zero_cost {
            return false;
        }
        if self.scheduler.zero_cost && !provider.zero_cost {
            return false;
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mock_config_is_zero_cost() {
        let cfg = RunConfig::mock("do the thing", 4);
        assert!(cfg.may_run(&cfg.providers[0]));
        let paid = ProviderConfig { zero_cost: false, ..ProviderConfig::mock("paid", "m") };
        assert!(!cfg.may_run(&paid));
    }
}
