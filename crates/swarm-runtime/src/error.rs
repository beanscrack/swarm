//! Runtime error type.
//!
//! Wraps the crate-local error types into one typed error the CLI can display
//! and the driver can branch on.

use swarm_core::CoreError;
use swarm_providers::ProviderError;
use thiserror::Error;

/// A runtime failure. Each variant maps to a distinct condition.
#[derive(Debug, Error)]
pub enum RuntimeError {
    /// A problem with the run/provider configuration.
    #[error("configuration error: {0}")]
    Config(String),

    /// A provider rejected the request (classified for retry elsewhere).
    #[error(transparent)]
    Provider(ProviderError),

    /// A store/persistence failure.
    #[error(transparent)]
    Store(anyhow::Error),

    /// A core domain invariant was violated.
    #[error(transparent)]
    Core(CoreError),

    /// A feature the runtime does not (yet) implement.
    #[error("unsupported: {0}")]
    Unsupported(String),
}

impl From<ProviderError> for RuntimeError {
    fn from(e: ProviderError) -> Self {
        Self::Provider(e)
    }
}

impl From<CoreError> for RuntimeError {
    fn from(e: CoreError) -> Self {
        Self::Core(e)
    }
}

impl From<anyhow::Error> for RuntimeError {
    fn from(e: anyhow::Error) -> Self {
        Self::Store(e)
    }
}
