//! `swarm-providers` — LLM provider backends.
//!
//! This crate defines the provider contract (normalized request/response
//! types plus the async [`Provider`] / [`Session`] traits), a deterministic
//! offline [`MockProvider`], an OpenAI-compatible HTTP backend, a managed-agent
//! (external CLI) backend, a browser-bridge consumer chat backend
//! ([`BrowserChatProvider`], for ChatGPT and Gemini web), a spend/quota
//! [`QuotaGate`], and a
//! policy-aware [`ProviderRegistry`] dispatch table.
//!
//! Two execution models are supported:
//!
//! * **reasoning-only** backends (`Mock`, `OpenAiCompatible`) — Swarm owns
//!   the tool-use loop and executes validated tool requests itself;
//! * **managed agents** (`ManagedAgent`) — the backend owns its tool loop;
//!   Swarm manages the process, workspace, and captured output only.

pub mod browser_chat;
pub mod error;
pub mod managed;
pub mod mock;
pub mod openai;
pub mod pricing;
pub mod provider;
pub mod quota;
pub mod registry;

// Convenience re-exports.
pub use error::ProviderError;
pub use browser_chat::{BrowserChatConfig, BrowserChatProvider, ChatSite};
pub use managed::{ManagedAgentConfig, ManagedAgentProvider};
pub use mock::{coding_script, MockProvider, MockStep};
pub use openai::{OpenAiConfig, OpenAiProvider};
pub use pricing::ModelPricing;
pub use provider::{
    FinishReason, InferenceResponse, NativeToolCall, Provider, ProviderStream, SendRequest,
    Session, SessionConfig, ToolProtocol,
};
pub use quota::{ProviderGateStatus, QuotaGate};
pub use registry::ProviderRegistry;
