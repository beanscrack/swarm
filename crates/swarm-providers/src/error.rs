//! Provider errors and their retry classification.
//!
//! The split between `Transient` (safe to retry with backoff), `RateLimited`,
//! `QuotaExhausted` and `Permanent` (never retry) is what keeps quota errors
//! from triggering uncontrolled retry loops: the scheduler consults
//! [`ProviderError::classify`] and stops when the class is not retryable.

use std::time::Duration;

use swarm_core::provider::ProviderErrorClass;
use swarm_core::CoreError;
use thiserror::Error;

/// Errors returned by providers, classified for retry policy.
#[derive(Debug, Error, PartialEq)]
pub enum ProviderError {
    /// Domain-level violation (zero-cost policy, budget, invalid id).
    #[error("{0}")]
    Core(#[from] CoreError),

    /// The registry is refusing to dispatch to this provider right now
    /// (cooldown, quota exhausted, or disabled).
    #[error("provider {provider} is not available: {reason}")]
    Throttled {
        provider: String,
        reason: String,
        retry_after_ms: Option<u64>,
    },

    /// Network or I/O failure. Safe to retry with backoff.
    #[error("transport error: {0}")]
    Transport(String),

    /// HTTP error with a body from the provider; `class` is precomputed.
    #[error("provider HTTP error {status}: {message}")]
    Api {
        status: u16,
        message: String,
        class: ProviderErrorClass,
    },

    /// The provider returned a payload that could not be parsed.
    #[error("malformed provider response: {0}")]
    Malformed(String),

    /// The call exceeded its deadline.
    #[error("timeout after {0:?}")]
    Timeout(Duration),

    /// A managed-agent process failed to start or exited non-zero.
    #[error("process error: {0}")]
    Process(String),

    /// Session-level failure (unknown provider, script exhausted, closed).
    #[error("provider session error: {0}")]
    Session(String),
}

impl ProviderError {
    /// The retry classification of this error.
    pub fn classify(&self) -> ProviderErrorClass {
        use ProviderErrorClass::*;
        match self {
            Self::Core(_) => Permanent,
            Self::Throttled { retry_after_ms, .. } => RateLimited {
                retry_after_ms: *retry_after_ms,
            },
            Self::Transport(_) => Transient,
            Self::Api { class, .. } => class.clone(),
            Self::Malformed(_) => Permanent,
            Self::Timeout(_) => Transient,
            Self::Process(_) => Permanent,
            Self::Session(_) => Permanent,
        }
    }

    /// Whether a retry (with backoff) may succeed.
    pub fn is_retryable(&self) -> bool {
        self.classify().is_retryable()
    }

    /// The recommended wait before retrying, when the provider says so.
    pub fn retry_after_ms(&self) -> Option<u64> {
        self.classify().retry_after_ms()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ProviderErrorClass::*;

    #[test]
    fn classification_drives_retry_policy() {
        assert!(ProviderError::Transport("conn refused".into()).is_retryable());
        assert!(ProviderError::Timeout(Duration::from_secs(1)).is_retryable());
        assert!(!ProviderError::Process("exit 1".into()).is_retryable());
        assert!(!ProviderError::Malformed("bad json".into()).is_retryable());
        assert!(!ProviderError::Session("gone".into()).is_retryable());
        assert!(!ProviderError::Core(CoreError::BudgetExceeded {
            resource: "spend_micros",
            limit: 0,
            requested: 1
        })
        .is_retryable());
    }

    #[test]
    fn api_errors_carry_their_class() {
        let e = ProviderError::Api {
            status: 429,
            message: "slow down".into(),
            class: RateLimited {
                retry_after_ms: Some(1_500),
            },
        };
        assert_eq!(e.classify(), RateLimited {
            retry_after_ms: Some(1_500)
        });
        assert_eq!(e.retry_after_ms(), Some(1_500));
        assert!(e.is_retryable());
    }

    #[test]
    fn throttled_errors_report_retry_after() {
        let e = ProviderError::Throttled {
            provider: "mock".into(),
            reason: "cooling down".into(),
            retry_after_ms: Some(12),
        };
        assert_eq!(e.retry_after_ms(), Some(12));
        assert!(e.is_retryable());
    }
}
