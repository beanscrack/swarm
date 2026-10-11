use thiserror::Error;

/// Result alias used throughout the domain model.
pub type CoreResult<T> = Result<T, CoreError>;

/// Violations of domain invariants. Every variant is a caller error that can be
/// reported to a user or fed back to a model; none of them are I/O failures.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum CoreError {
    #[error("invalid {kind} identifier `{value}`")]
    InvalidId { kind: &'static str, value: String },

    #[error("unknown {kind} value `{value}`")]
    UnknownValue { kind: &'static str, value: String },

    #[error("invalid {entity} state transition from `{from}` to `{to}`")]
    InvalidTransition {
        entity: &'static str,
        from: String,
        to: String,
    },

    #[error("duplicate task `{0}` in dependency graph")]
    DuplicateTask(String),

    #[error("task `{task}` depends on unknown task `{dependency}`")]
    UnknownDependency { task: String, dependency: String },

    #[error("dependency cycle involving task `{0}`")]
    DependencyCycle(String),

    #[error("budget exceeded for {resource}: requested {requested} against limit {limit}")]
    BudgetExceeded {
        resource: &'static str,
        limit: u64,
        requested: u64,
    },

    #[error("invalid tool request: {0}")]
    InvalidToolRequest(String),

    #[error("invalid finding: {0}")]
    InvalidFinding(String),

    #[error("malformed agent reply: {0}")]
    Protocol(String),

    #[error("invalid configuration: {0}")]
    Config(String),

    #[error("zero-cost mode forbids provider `{provider}` (billing: {billing})")]
    ZeroCostViolation {
        provider: String,
        billing: &'static str,
    },
}
