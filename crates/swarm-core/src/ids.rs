use crate::{CoreError, CoreResult};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

/// Longest identifier accepted from any source.
const MAX_ID_LEN: usize = 96;

fn is_id_char(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-'
}

/// Declares an opaque, prefixed, random identifier such as `task_<uuid>`.
macro_rules! prefixed_id {
    ($(#[$meta:meta])* $name:ident, $prefix:literal) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(try_from = "String", into = "String")]
        pub struct $name(String);

        impl $name {
            /// Prefix shared by every identifier of this kind.
            pub const PREFIX: &'static str = $prefix;

            /// Creates a fresh random identifier.
            pub fn new() -> Self {
                Self(format!("{}_{}", $prefix, uuid::Uuid::new_v4().simple()))
            }

            /// Validates and wraps an identifier read from storage or input.
            pub fn parse(value: &str) -> crate::CoreResult<Self> {
                let expected_prefix = concat!($prefix, "_");
                if !value.starts_with(expected_prefix)
                    || value.len() > super::ids::MAX_ID_LEN
                    || !value.bytes().all(super::ids::is_id_char)
                {
                    return Err(crate::CoreError::InvalidId {
                        kind: stringify!($name),
                        value: value.to_string(),
                    });
                }
                Ok(Self(value.to_string()))
            }

            /// Borrowed textual form.
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl FromStr for $name {
            type Err = CoreError;

            fn from_str(value: &str) -> CoreResult<Self> {
                Self::parse(value)
            }
        }

        impl TryFrom<String> for $name {
            type Error = CoreError;

            fn try_from(value: String) -> CoreResult<Self> {
                Self::parse(&value)
            }
        }

        impl From<$name> for String {
            fn from(value: $name) -> String {
                value.0
            }
        }
    };
}

prefixed_id!(
    /// Identifier of a run: one user objective executed to completion.
    RunId,
    "run"
);
prefixed_id!(
    /// Identifier of a task node in the hierarchical task graph.
    TaskId,
    "task"
);
prefixed_id!(
    /// Identifier of a logical agent executing a task.
    AgentId,
    "agent"
);
prefixed_id!(
    /// Identifier of a provider session (conversation or managed process).
    SessionId,
    "sess"
);
prefixed_id!(
    /// Identifier of a stored artifact.
    ArtifactId,
    "art"
);
prefixed_id!(
    /// Identifier of an inter-agent message.
    MessageId,
    "msg"
);
prefixed_id!(
    /// Identifier of a research hypothesis.
    HypothesisId,
    "hyp"
);

/// Name of a configured provider, such as `mock` or `gemini`.
///
/// Provider names are human-chosen, so they are not random: lowercase ASCII
/// letters, digits, `-` and `_`, 1 to 64 characters.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ProviderId(String);

impl ProviderId {
    /// Validates a provider name.
    pub fn new(value: impl Into<String>) -> CoreResult<Self> {
        let value = value.into();
        let valid = !value.is_empty()
            && value.len() <= 64
            && value
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_');
        if !valid {
            return Err(CoreError::InvalidId {
                kind: "ProviderId",
                value,
            });
        }
        Ok(Self(value))
    }

    /// Borrowed textual form.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ProviderId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for ProviderId {
    type Err = CoreError;

    fn from_str(value: &str) -> CoreResult<Self> {
        Self::new(value)
    }
}

impl TryFrom<String> for ProviderId {
    type Error = CoreError;

    fn try_from(value: String) -> CoreResult<Self> {
        Self::new(value)
    }
}

impl From<ProviderId> for String {
    fn from(value: ProviderId) -> String {
        value.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_ids_round_trip_through_parse() {
        let id = TaskId::new();
        assert!(id.as_str().starts_with("task_"));
        assert_eq!(TaskId::parse(id.as_str()).unwrap(), id);
    }

    #[test]
    fn wrong_prefix_is_rejected() {
        let run = RunId::new();
        assert!(matches!(
            TaskId::parse(run.as_str()),
            Err(CoreError::InvalidId { kind: "TaskId", .. })
        ));
    }

    #[test]
    fn injection_characters_are_rejected() {
        assert!(RunId::parse("run_abc'; DROP TABLE runs;--").is_err());
        assert!(RunId::parse("run_").is_ok());
        assert!(RunId::parse(&format!("run_{}", "a".repeat(200))).is_err());
    }

    #[test]
    fn provider_ids_are_validated() {
        assert!(ProviderId::new("mock").is_ok());
        assert!(ProviderId::new("local-openai_2").is_ok());
        assert!(ProviderId::new("").is_err());
        assert!(ProviderId::new("Mock").is_err());
        assert!(ProviderId::new("a b").is_err());
    }

    #[test]
    fn serde_uses_plain_strings() {
        let id = AgentId::new();
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, format!("\"{}\"", id));
        let back: AgentId = serde_json::from_str(&json).unwrap();
        assert_eq!(back, id);
        assert!(serde_json::from_str::<AgentId>("\"task_x\"").is_err());
    }
}
