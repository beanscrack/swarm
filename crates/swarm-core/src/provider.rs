//! Provider domain types shared across the whole system.
//!
//! This module is deliberately dependency-free (no `tokio`, no HTTP): it holds
//! the vocabulary that every layer needs — capabilities, error classification,
//! provider status, streaming chunks and the registry descriptor. The async
//! [`Provider`](crate) trait and its implementations live in `swarm-providers`.

use serde::{Deserialize, Serialize};

use crate::budget::TokenUsage;
use crate::ids::{ProviderId, SessionId};

string_enum! {
        /// The family of a provider. Determines the transport and execution model.
        pub enum ProviderKind {
        Mock => "mock",
        OpenAiCompatible => "open_ai_compatible",
        ManagedAgent => "managed_agent",
        GeminiWeb => "gemini_web",
        ChatGptWeb => "chatgpt_web",
    }
}

string_enum! {
        /// The kind of an individual streamed token/chunk.
        pub enum ToolCallKind {
        Text => "text",
        Thinking => "thinking",
        ToolCall => "tool_call",
        Error => "error",
    }
}

string_enum! {
        /// Coarse status of a provider, used by the scheduler to route work.
        pub enum ProviderStatus {
        Healthy => "healthy",
        Degraded => "degraded",
        Exhausted => "exhausted",
        Unavailable => "unavailable",
        Disabled => "disabled",
    }
}

impl ProviderStatus {
    /// Whether this provider currently accepts new inference requests.
    pub fn accepts(&self) -> bool {
        matches!(self, Self::Healthy | Self::Degraded)
    }
}

/// What a provider can do. Providers *declare* capabilities rather than
/// pretending every model supports identical operations.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ProviderCapabilities {
    pub streaming: bool,
    pub tools: bool,
    pub json_mode: bool,
    pub reasoning: bool,
    /// True for managed-agent CLIs with their own internal tool loop.
    pub managed_agent: bool,
    pub max_output_tokens: Option<u32>,
    pub max_concurrency: Option<u32>,
    pub notes: Option<String>,
}

/// A classified provider error. The scheduler uses this to choose backoff and
/// whether to retry at all (quota/auth errors must not spin).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ProviderErrorClass {
    Transient,
    RateLimited {
        retry_after_ms: Option<u64>,
    },
    QuotaExhausted,
    Auth,
    Unsupported,
    Permanent,
}

impl ProviderErrorClass {
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            Self::Transient | Self::RateLimited { .. } | Self::QuotaExhausted
        )
    }

    pub fn is_rate_limit(&self) -> bool {
        matches!(self, Self::RateLimited { .. } | Self::QuotaExhausted)
    }

    pub fn retry_after_ms(&self) -> Option<u64> {
        match self {
            Self::RateLimited { retry_after_ms } => *retry_after_ms,
            _ => None,
        }
    }
}

/// One chunk of a streamed inference response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderTextChunk {
    pub text: String,
    pub kind: ToolCallKind,
    pub finish_reason: Option<String>,
    pub usage: Option<TokenUsage>,
}

/// The collected result of a (streamed) inference call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderStreamingResponse {
    pub chunks: Vec<ProviderTextChunk>,
    pub finish_reason: Option<String>,
    pub error: Option<String>,
    pub usage: Option<TokenUsage>,
}

impl ProviderStreamingResponse {
    pub fn new() -> Self {
        Self {
            chunks: Vec::new(),
            finish_reason: None,
            error: None,
            usage: None,
        }
    }

    /// Concatenation of all `Text` chunks.
    pub fn text(&self) -> String {
        self.chunks
            .iter()
            .filter(|c| c.kind == ToolCallKind::Text)
            .map(|c| c.text.as_str())
            .collect()
    }
}

/// An opaque handle to an open provider session (the async object lives in
/// `swarm-providers`; this is the core-level identity for it).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderSession {
    pub id: SessionId,
}

impl ProviderSession {
    pub fn new(id: SessionId) -> Self {
        Self { id }
    }
}

/// The outcome of executing a tool request (recorded and checkpointed).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolRequestOutcome {
    pub success: bool,
    pub summary: String,
    pub artifact_ref: Option<crate::artifact::ArtifactRef>,
    pub error: Option<String>,
    pub duration_ms: u64,
}

/// A descriptor of a registered provider (core-level; no async).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderEntry {
    pub id: ProviderId,
    pub kind: ProviderKind,
    pub model: String,
    pub capabilities: ProviderCapabilities,
    pub status: ProviderStatus,
    pub zero_cost: bool,
    pub concurrency: u32,
}

/// The (core-level) registry of providers known to a run.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProviderRegistry {
    pub entries: Vec<ProviderEntry>,
}

impl ProviderRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, entry: ProviderEntry) {
        self.entries.push(entry);
    }

    pub fn get(&self, id: &ProviderId) -> Option<&ProviderEntry> {
        self.entries.iter().find(|e| &e.id == id)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

// Convenience re-exports so core-level code can speak in one place.
pub use crate::message::ChatMessage;
pub use crate::tool::ToolCallRecord;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_limit_is_retryable_but_auth_is_not() {
        assert!(ProviderErrorClass::RateLimited {
            retry_after_ms: Some(100)
        }
        .is_retryable());
        assert!(ProviderErrorClass::QuotaExhausted.is_retryable());
        assert!(!ProviderErrorClass::Auth.is_retryable());
        assert!(!ProviderErrorClass::Permanent.is_retryable());
    }

    #[test]
    fn status_accepts() {
        assert!(ProviderStatus::Healthy.accepts());
        assert!(ProviderStatus::Degraded.accepts());
        assert!(!ProviderStatus::Exhausted.accepts());
        assert!(!ProviderStatus::Unavailable.accepts());
        assert!(!ProviderStatus::Disabled.accepts());
    }
}
