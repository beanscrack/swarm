//! Deterministic offline provider — the backbone of the offline test suite.
//!
//! `MockProvider` answers every call deterministically (no wall-clock
//! dependence in usage accounting): either a default numbered reply, or a
//! scripted sequence of steps (text, tool calls, or injected errors). This is
//! what makes full mock multi-agent runs and quota-failure tests possible
//! without a network or a model.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use futures::stream;
use serde_json::json;

use swarm_core::budget::{BillingMode, TokenUsage};
use swarm_core::message::ChatMessage;
use swarm_core::provider::{ProviderCapabilities, ProviderErrorClass, ProviderKind};
use swarm_core::{CoreError, ProviderId, SessionId};

use crate::error::ProviderError;
use crate::pricing::ModelPricing;
use crate::provider::{
    build_chunks, FinishReason, NativeToolCall, Provider, ProviderStream, SendRequest, Session,
    SessionConfig,
};

/// One canned response of a scripted session.
#[derive(Debug, Clone)]
pub struct MockStep {
    pub text: String,
    pub tool_calls: Vec<NativeToolCall>,
    pub finish: FinishReason,
    /// When set, the step returns this error instead of a response.
    pub error: Option<ProviderErrorClass>,
}

impl MockStep {
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            tool_calls: Vec::new(),
            finish: FinishReason::Stop,
            error: None,
        }
    }

    pub fn tool_calls(calls: Vec<NativeToolCall>) -> Self {
        Self {
            text: String::new(),
            tool_calls: calls,
            finish: FinishReason::ToolCalls,
            error: None,
        }
    }

    pub fn failing(class: ProviderErrorClass) -> Self {
        Self {
            text: String::new(),
            tool_calls: Vec::new(),
            finish: FinishReason::Other,
            error: Some(class),
        }
    }
}

/// A deterministic multi-turn fixture for a coding agent: write a file, run a
/// check, report done. Used by the offline mock multi-agent run.
pub fn coding_script() -> Vec<MockStep> {
    vec![
        MockStep::tool_calls(vec![NativeToolCall::new(
            "call_write_file",
            "write_file",
            json!({"path": "hello.txt", "content": "hello, swarm\n"}),
        )]),
        MockStep::tool_calls(vec![NativeToolCall::new(
            "call_bash",
            "bash",
            json!({"command": "test -f hello.txt && cat hello.txt"}),
        )]),
        MockStep::text("Task complete: hello.txt created and verified."),
    ]
}

/// Deterministic offline provider.
#[derive(Debug)]
pub struct MockProvider {
    pub id: ProviderId,
    pub model: String,
    pub billing: BillingMode,
    pub pricing: ModelPricing,
    /// Artificial latency between calls (0 for instant tests).
    pub latency: Duration,
    /// Scripted per-session responses; `None` => default numbered replies.
    pub script: Option<Vec<MockStep>>,
 /// Shared across all sessions of this provider instance so default
 /// numbered replies keep incrementing across dispatches.
 call_counter: std::sync::Arc<AtomicU64>,
}

impl MockProvider {
    /// Plain fields; wrap in `Arc` when registering.
    pub fn unowned() -> Self {
        Self {
            id: ProviderId::new("mock").expect("mock id is valid"),
            model: "mock-model".to_string(),
            billing: BillingMode::ZeroCost,
            pricing: ModelPricing::default(),
            latency: Duration::ZERO,
            script: None,
            call_counter: std::sync::Arc::new(AtomicU64::new(0)),
        }
    }

    pub fn new() -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self::unowned())
    }

    /// A zero-cost provider that fails every call with `class`.
    pub fn failing(class: ProviderErrorClass) -> std::sync::Arc<Self> {
        let mut p = Self::unowned();
        p.script = Some(vec![MockStep::failing(class)]);
        std::sync::Arc::new(p)
    }

    pub fn with_billing(mut self, billing: BillingMode) -> Self {
        self.billing = billing;
        self
    }

    pub fn with_script(mut self, script: Vec<MockStep>) -> Self {
        self.script = Some(script);
        self
    }
}

impl Default for MockProvider {
    fn default() -> Self {
        Self::unowned()
    }
}

pub struct MockSession {
    provider: std::sync::Arc<MockProvider>,
    session_id: SessionId,
    script: Mutex<VecDeque<MockStep>>,
}

impl MockProvider {
    pub(crate) fn session(&self) -> MockSession {
        MockSession {
            provider: std::sync::Arc::new(MockProvider {
                id: self.id.clone(),
                model: self.model.clone(),
                billing: self.billing,
                pricing: self.pricing,
                latency: self.latency,
                script: self.script.clone(),
                call_counter: self.call_counter.clone(),
            }),
            session_id: SessionId::new(),
            script: Mutex::new(
                self.script
                    .clone()
                    .unwrap_or_default()
                    .into_iter()
                    .collect(),
            ),
        }
    }

    fn usage_for(&self, request: &SendRequest, step_text: &str) -> TokenUsage {
        let chars: usize = request
            .messages
            .iter()
            .map(|m: &ChatMessage| m.content.chars().count())
            .sum();
        let out = step_text.chars().count();
        TokenUsage {
            input_tokens: (chars as u64).div_ceil(4).max(1),
            output_tokens: (out as u64).div_ceil(4).max(1),
            cached_input_tokens: 0,
        }
    }
}

#[async_trait]
impl Provider for MockProvider {
    fn id(&self) -> &ProviderId {
        &self.id
    }

    fn kind(&self) -> ProviderKind {
        ProviderKind::Mock
    }

    fn model(&self) -> &str {
        &self.model
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            streaming: true,
            tools: true,
            json_mode: true,
            reasoning: false,
            managed_agent: false,
            max_output_tokens: Some(8_192),
            max_concurrency: Some(16),
            notes: Some("deterministic offline mock".into()),
        }
    }

    fn billing(&self) -> BillingMode {
        self.billing
    }

    fn pricing(&self) -> ModelPricing {
        self.pricing
    }

    async fn start_session(
        &self,
        _config: SessionConfig,
    ) -> Result<Box<dyn Session>, ProviderError> {
        Ok(Box::new(self.session()))
    }
}

#[async_trait]
impl Session for MockSession {
    fn provider_id(&self) -> &ProviderId {
        &self.provider.id
    }

    fn model(&self) -> &str {
        &self.provider.model
    }

    fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    async fn stream(&self, request: &SendRequest) -> Result<ProviderStream, ProviderError> {
        if request.zero_cost && !self.provider.billing.is_zero_cost() {
            return Err(CoreError::ZeroCostViolation {
                provider: self.provider.id.as_str().to_string(),
                billing: self.provider.billing.as_str(),
            }
            .into());
        }
        if self.provider.latency > Duration::ZERO {
            tokio::time::sleep(self.provider.latency).await;
        }

        let step = self.next_step(request)?;
        if let Some(class) = &step.error {
            return Err(ProviderError::Api {
                status: 0,
                message: format!("injected {class:?}"),
                class: class.clone(),
            });
        }
        let usage = self.provider.usage_for(request, &step.text);
        let chunks = build_chunks(&step.text, &step.tool_calls, step.finish, Some(usage));
        Ok(Box::pin(stream::iter(chunks.into_iter().map(Ok))))
    }

    async fn close(&self) -> Result<(), ProviderError> {
        Ok(())
    }
}

impl MockSession {
    fn next_step(&self, request: &SendRequest) -> Result<MockStep, ProviderError> {
        if let Some(template) = &self.provider.script {
            if template.is_empty() {
                return Err(ProviderError::Session("empty mock script".into()));
            }
            let mut q = self.script.lock().unwrap_or_else(|e| e.into_inner());
            return q
                .pop_front()
                .ok_or_else(|| ProviderError::Session("mock script exhausted".into()));
        }
        let n = self
            .provider
            .call_counter
            .fetch_add(1, Ordering::Relaxed);
        if request.messages.iter().any(|m| m.content.contains("You are a planner.")) {
            let user = request.messages.iter().find(|m| m.content.contains("Worker subtasks:"));
            let workers = user.and_then(|m| m.content.lines()
                .find_map(|line| line.strip_prefix("Worker subtasks: ")))
                .and_then(|n| n.parse::<usize>().ok()).unwrap_or(1);
            let objective = user.and_then(|m| m.content.lines()
                .find_map(|line| line.strip_prefix("Objective: "))).unwrap_or("the objective");
            let steps: Vec<_> = (1..=workers).map(|i| json!({
                "description": format!("Complete independent work item {i} for: {objective}"),
                "kind": "implementation", "priority": 0,
            })).collect();
            return Ok(MockStep::text(format!("```swarm\n{}\n```", json!({"plan": {
                "steps": steps, "expected_artifacts": [], "success_criteria": []
            }}))));
        }
        Ok(MockStep {
            text: format!(
                "[mock:{}] reply #{}: understood.",
                self.provider.model,
                n + 1
            ),
            tool_calls: Vec::new(),
            finish: FinishReason::Stop,
            error: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use swarm_core::provider::ProviderErrorClass;

    fn req(text: &str) -> SendRequest {
        SendRequest::new(vec![swarm_core::message::ChatMessage::user(text)])
    }

    #[tokio::test]
    async fn default_replies_are_numbered_and_deterministic() {
        let p = MockProvider::new();
        let mut reg = crate::registry::ProviderRegistry::new();
        reg.register(p);
        let id = ProviderId::new("mock").unwrap();
        let r1 = reg.dispatch(&id, req("first")).await.unwrap();
        let r2 = reg.dispatch(&id, req("first")).await.unwrap();
        assert!(r1.text.contains("reply #1"));
        assert!(r2.text.contains("reply #2"));
        // Deterministic usage accounting: same request => same usage.
        assert_eq!(r1.usage, r2.usage);
        assert!(r1.usage.input_tokens > 0);
    }

    #[tokio::test]
    async fn scripted_coding_run_drives_the_full_tool_loop() {
        let p = MockProvider::unowned().with_script(coding_script());
        let session = p.start_session(SessionConfig::default()).await.unwrap();
        let write = session.send(req("write hello.txt")).await.unwrap();
        assert!(write.has_tool_calls());
        assert_eq!(write.tool_calls[0].name, "write_file");
        let bash = session.send(req("run checks")).await.unwrap();
        assert_eq!(bash.tool_calls[0].name, "bash");
        let done = session.send(req("status")).await.unwrap();
        assert!(!done.has_tool_calls());
        assert!(done.text.contains("Task complete"));
    }

    #[tokio::test]
    async fn script_exhaustion_is_an_error_not_a_hang() {
        let p = MockProvider::unowned().with_script(vec![MockStep::text("only")]);
        let session = p.start_session(SessionConfig::default()).await.unwrap();
        assert!(session.send(req("a")).await.is_ok());
        let err = session.send(req("b")).await.unwrap_err();
        assert!(!err.is_retryable());
    }

    #[tokio::test]
    async fn failing_provider_classifies_errors() {
        let p = MockProvider::failing(ProviderErrorClass::QuotaExhausted);
        let session = p.start_session(SessionConfig::default()).await.unwrap();
        let err = session.send(req("a")).await.unwrap_err();
        assert_eq!(err.classify(), ProviderErrorClass::QuotaExhausted);
        assert!(matches!(err, ProviderError::Api { .. }));
    }

    #[test]
    fn mock_is_zero_cost_by_default() {
        let p = MockProvider::unowned();
        assert!(p.billing.is_zero_cost());
        assert_eq!(p.model(), "mock-model");
        assert!(p.capabilities().tools);
    }
}
