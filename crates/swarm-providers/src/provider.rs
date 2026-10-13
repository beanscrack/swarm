//! The provider contract: normalized request/response types and the async
//! [`Provider`] / [`Session`] traits implemented by every backend.
//!
//! Providers are **stateless functions of the normalized conversation**: the
//! durable store owns message history, a session is a thin handle, and every
//! backend answers [`Session::stream`] / [`Session::send`] with normalized
//! chunks. Two execution models are supported (see
//! `swarm_core::provider::ProviderKind`):
//!
//! * reasoning-only backends (`Mock`, `OpenAiCompatible`) — Swarm owns the
//!   tool-use loop and executes validated tool requests itself;
//! * managed agents (`ManagedAgent`) — the backend has its own loop and
//!   tools; Swarm manages the process, workspace, and captured output only.

use std::fmt;
use std::pin::Pin;
use std::time::Duration;

use async_trait::async_trait;
use futures::{Stream, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use swarm_core::budget::{BillingMode, TokenUsage};
use swarm_core::message::ChatMessage;
use swarm_core::provider::{ProviderCapabilities, ProviderKind, ProviderTextChunk, ToolCallKind};
use swarm_core::tool::ToolSpec;
use swarm_core::{CoreError, ProviderId, RunId, SessionId};

use crate::error::ProviderError;
use crate::pricing::ModelPricing;

/// A boxed stream of response chunks (the provider-side half of the event bus).
pub type ProviderStream =
    Pin<Box<dyn Stream<Item = Result<ProviderTextChunk, ProviderError>> + Send>>;

/// Why the model stopped generating.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FinishReason {
    /// Natural end of the answer.
    Stop,
    /// The model requested tool calls; the loop must continue.
    ToolCalls,
    /// Cut off by the token budget.
    Length,
    /// A text-protocol tool request needs a format correction.
    MalformedTool,
    /// Something else the provider reported.
    Other,
}

impl FinishReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Stop => "stop",
            Self::ToolCalls => "tool_calls",
            Self::Length => "length",
            Self::MalformedTool => "malformed_tool",
            Self::Other => "other",
        }
    }

    /// Map provider wire values to the normalized enum.
    pub fn from_provider_str(value: &str) -> Self {
        match value {
            "stop" | "completed" => Self::Stop,
            "tool_calls" | "function_call" => Self::ToolCalls,
            "length" | "max_tokens" => Self::Length,
            "malformed_tool" => Self::MalformedTool,
            _ => Self::Other,
        }
    }
}

impl fmt::Display for FinishReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A tool call the model requested natively (provider wire format normalized).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NativeToolCall {
    /// Provider-assigned call id (used to match results).
    pub id: String,
    /// Tool name as declared in the catalog.
    pub name: String,
    /// Parsed arguments (fallback: raw JSON string under no key is invalid —
    /// unparsable payloads are stored as a JSON string value).
    pub arguments: Value,
}

impl NativeToolCall {
    pub fn new(
        id: impl Into<String>,
        name: impl Into<String>,
        arguments: Value,
    ) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            arguments,
        }
    }

    /// JSON payload carried by `ToolCall` stream chunks.
    pub fn chunk_text(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| "null".to_string())
    }
}

/// How tool descriptions reach the model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolProtocol {
    /// Native function calling: tools go in the provider `tools` parameter.
    Native,
    /// Descriptions are inlined as a system message; the model emits the
    /// Swarm fenced-block protocol in text. Works with any chat model.
    Text,
    /// No tools for this call.
    None,
}

/// Everything a single inference call needs. The provider is stateless: the
/// caller owns the full normalized history.
#[derive(Debug, Clone)]
pub struct SendRequest {
    /// Normalized conversation so far (system first, oldest to newest).
    pub messages: Vec<ChatMessage>,
    /// Tool catalog the agent may call this step.
    pub tools: Vec<ToolSpec>,
    pub tool_protocol: ToolProtocol,
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
    /// Set for zero-cost runs: only `ZeroCost` providers may be called.
    pub zero_cost: bool,
    /// Optional per-request deadline.
    pub deadline: Option<Duration>,
}

impl SendRequest {
    pub fn new(messages: Vec<ChatMessage>) -> Self {
        Self {
            messages,
            tools: Vec::new(),
            tool_protocol: ToolProtocol::None,
            temperature: None,
            max_tokens: None,
            zero_cost: false,
            deadline: None,
        }
    }

    /// Deterministic text rendering of the tool catalog (Text protocol).
    pub fn tools_as_text(&self) -> Option<String> {
        if self.tools.is_empty() {
            return None;
        }
        let mut out = String::from(
            "Available tools (emit one JSON tool request per call):\n",
        );
        for t in &self.tools {
            out.push_str(&format!(
                "- {} ({}): {}\n  input JSON schema: {}\n",
                t.name,
                t.kind.as_str(),
                t.description,
                t.input_schema.as_value(),
            ));
        }
        Some(out)
    }
}

/// The complete result of one inference call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InferenceResponse {
    /// Assistant text (empty when the model only issued tool calls).
    pub text: String,
    /// Reasoning/thinking content, when the model emits it.
    pub thinking: String,
    /// Native tool calls requested by the model.
    pub tool_calls: Vec<NativeToolCall>,
    pub finish_reason: FinishReason,
    /// Usage reported by the provider; zero fields when unreported.
    pub usage: TokenUsage,
    pub provider: String,
    pub model: String,
    /// Raw provider payload kept for auditing (when the transport allows).
    pub raw: Option<Value>,
}

impl InferenceResponse {
    pub fn new() -> Self {
        Self {
            text: String::new(),
            thinking: String::new(),
            tool_calls: Vec::new(),
            finish_reason: FinishReason::Stop,
            usage: TokenUsage::default(),
            provider: String::new(),
            model: String::new(),
            raw: None,
        }
    }

    pub fn has_tool_calls(&self) -> bool {
        !self.tool_calls.is_empty()
    }
}

/// Session-level configuration (provider-specific state lives with the
/// backend; this carries what is shared).
#[derive(Debug, Clone, Default)]
pub struct SessionConfig {
    /// The run this session belongs to (bookkeeping only).
    pub run_id: Option<RunId>,
    /// Session-level system prompt, prepended before `messages`.
    pub system: Option<String>,
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
}

/// Build the normalized chunk sequence for one non-streaming result.
pub fn build_chunks(
    text: &str,
    tool_calls: &[NativeToolCall],
    finish: FinishReason,
    usage: Option<TokenUsage>,
) -> Vec<ProviderTextChunk> {
    let mut chunks = Vec::new();
    for piece in text.chars().collect::<Vec<char>>().chunks(24) {
        let piece: String = piece.iter().collect();
        if !piece.is_empty() {
            chunks.push(ProviderTextChunk {
                text: piece,
                kind: ToolCallKind::Text,
                finish_reason: None,
                usage: None,
            });
        }
    }
    for call in tool_calls {
        chunks.push(ProviderTextChunk {
            text: call.chunk_text(),
            kind: ToolCallKind::ToolCall,
            finish_reason: None,
            usage: None,
        });
    }
    // Final meta chunk carries finish reason and usage.
    chunks.push(ProviderTextChunk {
        text: String::new(),
        kind: ToolCallKind::Text,
        finish_reason: Some(finish.as_str().to_string()),
        usage,
    });
    chunks
}

/// Collect a chunk stream into a full response (default `send` implementation).
pub async fn collect_response(
    mut stream: ProviderStream,
    provider: &str,
    model: &str,
) -> Result<InferenceResponse, ProviderError> {
    let mut resp = InferenceResponse::new();
    resp.provider = provider.to_string();
    resp.model = model.to_string();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        match chunk.kind {
            ToolCallKind::Text => resp.text.push_str(&chunk.text),
            ToolCallKind::Thinking => resp.thinking.push_str(&chunk.text),
            ToolCallKind::ToolCall => {
                let call: NativeToolCall = serde_json::from_str(&chunk.text)
                    .map_err(|e| ProviderError::Malformed(format!("tool call chunk: {e}")))?;
                resp.tool_calls.push(call);
            }
            ToolCallKind::Error => {
                return Err(ProviderError::Session(chunk.text));
            }
        }
        if let Some(u) = chunk.usage {
            resp.usage = u;
        }
        if let Some(f) = &chunk.finish_reason {
            resp.finish_reason = FinishReason::from_provider_str(f);
        }
    }
    Ok(resp)
}

/// Zero-cost policy gate: a zero-cost run may only dispatch to providers
/// whose billing mode is `ZeroCost`. Enforced here (registry) and inside
/// every backend `send` (defense in depth).
pub fn enforce_zero_cost(
    provider: &dyn Provider,
    zero_cost: bool,
) -> Result<(), CoreError> {
    if zero_cost && !provider.billing().is_zero_cost() {
        return Err(CoreError::ZeroCostViolation {
            provider: provider.id().as_str().to_string(),
            billing: provider.billing().as_str(),
        });
    }
    Ok(())
}

/// A provider backend: a factory of sessions plus declarative capabilities.
#[async_trait]
pub trait Provider: Send + Sync {
    fn id(&self) -> &ProviderId;
    fn kind(&self) -> ProviderKind;
    fn model(&self) -> &str;
    /// Declared capabilities — callers must branch on these, never assume.
    fn capabilities(&self) -> ProviderCapabilities;
    /// Billing mode of the underlying account/access.
    fn billing(&self) -> BillingMode;
    /// Pricing used to estimate spend (zero for free tiers).
    fn pricing(&self) -> ModelPricing;
    /// Open a session. Stateless backends keep no state; managed agents
    /// validate their process configuration here.
    async fn start_session(&self, config: SessionConfig) -> Result<Box<dyn Session>, ProviderError>;
}

/// One conversation with a provider. Swarm owns the history; the session is
/// a handle plus cleanup.
#[async_trait]
pub trait Session: Send + Sync {
    fn provider_id(&self) -> &ProviderId;
    fn model(&self) -> &str;
    fn session_id(&self) -> &SessionId;

    /// Stream the response to one inference call.
    async fn stream(&self, request: &SendRequest) -> Result<ProviderStream, ProviderError>;

    /// Send one call and collect the full response.
    async fn send(&self, request: SendRequest) -> Result<InferenceResponse, ProviderError> {
        let stream = self.stream(&request).await?;
        collect_response(stream, self.provider_id().as_str(), self.model()).await
    }

    /// Release resources (kill process, drop connections). Idempotent.
    async fn close(&self) -> Result<(), ProviderError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::stream;

    #[test]
    fn finish_reason_maps_provider_strings() {
        assert_eq!(FinishReason::from_provider_str("stop"), FinishReason::Stop);
        assert_eq!(
            FinishReason::from_provider_str("tool_calls"),
            FinishReason::ToolCalls
        );
        assert_eq!(
            FinishReason::from_provider_str("max_tokens"),
            FinishReason::Length
        );
        assert_eq!(FinishReason::from_provider_str("weird"), FinishReason::Other);
        assert_eq!(FinishReason::ToolCalls.as_str(), "tool_calls");
    }

    #[test]
    fn native_tool_call_round_trips_through_chunk_text() {
        let call = NativeToolCall::new("call_1", "bash", serde_json::json!({"command": "ls"}));
        let parsed: NativeToolCall = serde_json::from_str(&call.chunk_text()).unwrap();
        assert_eq!(parsed, call);
    }

    #[test]
    fn tools_as_text_is_deterministic_and_empty_friendly() {
        let mut req = SendRequest::new(Vec::new());
        assert_eq!(req.tools_as_text(), None);
        req.tools.push(ToolSpec {
            name: "bash".into(),
            kind: swarm_core::tool::ToolKind::Bash,
            description: "run a command".into(),
            input_schema: swarm_core::tool::ToolSchema(serde_json::json!({})),
            sandboxed: true,
            max_output_bytes: 1024,
            requires_approval: false,
        });
        let text = req.tools_as_text().unwrap();
        assert!(text.contains("bash"));
        assert!(text.contains("run a command"));
    }

    #[tokio::test]
    async fn collect_response_assembles_text_usage_and_finish() {
        let chunks = vec![
            ProviderTextChunk {
                text: "hel".into(),
                kind: ToolCallKind::Text,
                finish_reason: None,
                usage: None,
            },
            ProviderTextChunk {
                text: "lo".into(),
                kind: ToolCallKind::Text,
                finish_reason: Some("stop".into()),
                usage: Some(TokenUsage {
                    input_tokens: 7,
                    output_tokens: 2,
                    cached_input_tokens: 0,
                }),
            },
        ];
        let resp =
            collect_response(Box::pin(stream::iter(chunks.into_iter().map(Ok))), "p", "m").await;
        let resp = resp.unwrap();
        assert_eq!(resp.text, "hello");
        assert_eq!(resp.finish_reason, FinishReason::Stop);
        assert_eq!(resp.usage.input_tokens, 7);
    }

    #[tokio::test]
    async fn collect_response_errors_on_error_chunk() {
        let chunks = vec![ProviderTextChunk {
            text: "boom".into(),
            kind: ToolCallKind::Error,
            finish_reason: None,
            usage: None,
        }];
        let resp =
            collect_response(Box::pin(stream::iter(chunks.into_iter().map(Ok))), "p", "m").await;
        assert!(matches!(resp, Err(ProviderError::Session(_))));
    }

    #[test]
    fn zero_cost_gate_rejects_paid_providers() {
        struct Paid {
            id: ProviderId,
        }
        #[async_trait]
        impl Provider for Paid {
            fn id(&self) -> &ProviderId {
                &self.id
            }
            fn kind(&self) -> ProviderKind {
                ProviderKind::OpenAiCompatible
            }
            fn model(&self) -> &str {
                "paid"
            }
            fn capabilities(&self) -> ProviderCapabilities {
                ProviderCapabilities::default()
            }
            fn billing(&self) -> BillingMode {
                BillingMode::OnDemand
            }
            fn pricing(&self) -> ModelPricing {
                ModelPricing::default()
            }
            async fn start_session(
                &self,
                _c: SessionConfig,
            ) -> Result<Box<dyn Session>, ProviderError> {
                unimplemented!()
            }
        }
        let p = Paid {
            id: ProviderId::new("paid").unwrap(),
        };
        assert!(enforce_zero_cost(&p, false).is_ok());
        let e = enforce_zero_cost(&p, true).unwrap_err();
        assert!(matches!(e, CoreError::ZeroCostViolation { .. }));
    }
}
