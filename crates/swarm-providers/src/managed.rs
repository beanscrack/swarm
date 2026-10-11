//! Managed-agent backend.
//!
//! A managed agent is an external CLI that owns its own tool loop (Claude
//! Code, Codex, an in-house runner, ...). Swarm never re-executes its tool
//! calls: it spawns the process with the task prompt, runs it to completion
//! (or a timeout), captures stdout/stderr, and exposes that as normalized
//! chunks. The managed agent's workspace, process, and session lifecycle are
//! what Swarm manages here.

use std::path::PathBuf;
use std::time::Duration;

use async_trait::async_trait;

use swarm_core::budget::{BillingMode, TokenUsage};
use swarm_core::provider::{ProviderCapabilities, ProviderKind};
use swarm_core::{CoreError, ProviderId, SessionId};

use crate::error::ProviderError;
use crate::pricing::ModelPricing;
use crate::provider::{
    FinishReason, Provider, ProviderStream, SendRequest, Session, SessionConfig, build_chunks,
};

type Result<T> = std::result::Result<T, ProviderError>;

/// Configuration for a managed-agent process.
#[derive(Debug, Clone)]
pub struct ManagedAgentConfig {
    /// Agent label; also reported as the provider model.
    pub name: String,
    /// Program plus arguments, e.g. `["claude", "-p", ...]`.
    pub command: Vec<String>,
    /// Working directory the agent runs in.
    pub workdir: PathBuf,
    /// Extra environment variables for the child process.
    pub env: Vec<(String, String)>,
    /// Whole-run timeout.
    pub timeout: Duration,
    pub billing: BillingMode,
    pub pricing: ModelPricing,
}

impl ManagedAgentConfig {
    pub fn new(name: impl Into<String>, command: Vec<String>, workdir: PathBuf) -> Self {
        Self {
            name: name.into(),
            command,
            workdir,
            env: Vec::new(),
            timeout: Duration::from_secs(600),
            billing: BillingMode::OnDemand,
            pricing: ModelPricing::default(),
        }
    }
    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }
    pub fn timeout(mut self, d: Duration) -> Self {
        self.timeout = d;
        self
    }
    pub fn billing(mut self, b: BillingMode) -> Self {
        self.billing = b;
        self
    }
    pub fn pricing(mut self, p: ModelPricing) -> Self {
        self.pricing = p;
        self
    }
}

/// A managed-agent provider backend.
#[derive(Clone)]
pub struct ManagedAgentProvider {
    id: ProviderId,
    config: ManagedAgentConfig,
    caps: ProviderCapabilities,
}

impl ManagedAgentProvider {
    pub fn new(id: ProviderId, config: ManagedAgentConfig) -> Result<Self, CoreError> {
        if config.command.is_empty() {
            return Err(CoreError::Config("managed agent command must be non-empty".into()));
        }
        let caps = ProviderCapabilities {
            streaming: false, // run to completion; no incremental tokens
            tools: false, // the agent owns its tool loop
            json_mode: false,
            reasoning: false,
            managed_agent: true,
            max_output_tokens: None,
            max_concurrency: Some(1),
            notes: Some(format!("managed agent: {}", config.name)),
        };
        Ok(Self { id, config, caps })
    }
}

#[async_trait]
impl Provider for ManagedAgentProvider {
    fn id(&self) -> &ProviderId {
        &self.id
    }
    fn kind(&self) -> ProviderKind {
        ProviderKind::ManagedAgent
    }
    fn model(&self) -> &str {
        &self.config.name
    }
    fn capabilities(&self) -> ProviderCapabilities {
        self.caps
    }
    fn billing(&self) -> BillingMode {
        self.config.billing
    }
    fn pricing(&self) -> ModelPricing {
        self.config.pricing
    }
    async fn start_session(&self, config: SessionConfig) -> Result<Box<dyn Session>> {
        Ok(Box::new(ManagedAgentSession {
            id: self.id.clone(),
            model: self.config.name.clone(),
            config: self.config.clone(),
            session_config: config,
            session_id: SessionId::new(),
        }))
    }
}

struct ManagedAgentSession {
    id: ProviderId,
    model: String,
    config: ManagedAgentConfig,
    session_config: SessionConfig,
    session_id: SessionId,
}

#[async_trait]
impl Session for ManagedAgentSession {
    fn provider_id(&self) -> &ProviderId {
        &self.id
    }
    fn model(&self) -> &str {
        &self.model
    }
    fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    async fn stream(&self, _request: &SendRequest) -> Result<ProviderStream> {
        let output = self.run().await?;
        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();

        if !output.status.success() {
            let code = output.status.code().unwrap_or(-1);
            return Err(ProviderError::Process(format!(
                "agent `{}` exited {}: {}",
                self.config.name,
                code,
                truncate_at(&stderr, 2048)
            )));
        }
        let chunks = build_chunks(
            &stdout,
            &[],
            FinishReason::Stop,
            Some(TokenUsage::default()),
        );
        Ok(futures::stream::iter(chunks.into_iter().map(Ok)).boxed())
    }
}

impl ManagedAgentSession {
    /// Run the managed-agent process to completion (or timeout).
    async fn run(&self) -> Result<tokio::process::Output> {
        let mut cmd = tokio::process::Command::new(&self.config.command[0]);
        cmd.args(&self.config.command[1..]);
        cmd.current_dir(&self.config.workdir);
        for (k, v) in &self.config.env {
            cmd.env(k, v);
        }
        cmd.kill_on_drop(true);

        let mut child = cmd
            .spawn()
            .map_err(|e| ProviderError::Process(format!("spawn `{}`: {e}", self.config.name)))?;
        let output = tokio::time::timeout(self.config.timeout, child.wait_with_output())
            .await
            .map_err(|_| ProviderError::Timeout(self.config.timeout))?
            .map_err(|e| ProviderError::Process(format!("wait `{}`: {e}", self.config.name)))?;
        Ok((output, start.elapsed()))
    }
}

fn truncate_at(s: &str, max: usize) -> &str {
    match s.char_indices().nth(max) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::SendRequest;

    fn cfg_for(name: &str, cmd: &[&str]) -> ManagedAgentConfig {
        ManagedAgentConfig::new(
            name,
            cmd.iter().map(|s| s.to_string()).collect(),
            std::env::temp_dir(),
        )
    }

    #[tokio::test]
    async fn runs_to_completion_and_captures_stdout() {
        let cfg = cfg_for("echo-agent", shell_cmd("echo hello-from-agent"));
        let p = ManagedAgentProvider::new(ProviderId::new("managed-test").unwrap(), cfg).unwrap();
        let session = p.start_session(SessionConfig::default()).await.unwrap();
        let resp = session.send(SendRequest::new(Vec::new())).await.unwrap();
        assert!(resp.text.contains("hello-from-agent"), "got: {}", resp.text);
        // Usage reported as zero (managed agents don't report tokens).
        assert_eq!(resp.usage.input_tokens, 0);
    }

    #[tokio::test]
    async fn non_zero_exit_is_a_process_error() {
        let cfg = ManagedAgentConfig {
            ..cfg_for("failing", shell_cmd("exit 3"))
        };
        let p = ManagedAgentProvider::new(ProviderId::new("managed-test").unwrap(), cfg).unwrap();
        let session = p.start_session(SessionConfig::default()).await.unwrap();
        let err = session.send(SendRequest::new(Vec::new())).await.unwrap_err();
        assert!(matches!(err, ProviderError::Process(_)), "got: {err:?}");
    }

    #[tokio::test]
    async fn timeout_returns_timeout_error() {
        let mut cfg = cfg_for(
            "sleeper",
            if cfg!(windows) {
                &["cmd", "/C", "timeout", "/t", "10", "/nobreak"]
            } else {
                &["sleep", "10"]
            },
        );
        cfg.timeout = Duration::from_millis(500);
        let p = ManagedAgentProvider::new(ProviderId::new("managed-test").unwrap(), cfg).unwrap();
        let session = p.start_session(SessionConfig::default()).await.unwrap();
        let err = session.send(SendRequest::new(Vec::new())).await.unwrap_err();
        assert!(matches!(err, ProviderError::Timeout(_)), "got: {err:?}");
    }

    /// Build a shell command that works on both Unix and Windows.
    fn shell_cmd(script: &str) -> Vec<String> {
        if cfg!(windows) {
            // Translate the small subset we use in tests.
            let (head, rest) = script.split_once(' ').unwrap_or((script, ""));
            let mut v = vec!["cmd".to_string(), "/C".to_string()];
            v.push(head.to_string());
            if !rest.is_empty() {
                v.push(rest.to_string());
            }
            v
        } else {
            vec!["sh".to_string(), "-c".to_string(), script.to_string()]
        }
    }
}
