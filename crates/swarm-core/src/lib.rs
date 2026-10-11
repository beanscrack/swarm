//! Domain model for Swarm: identifiers, lifecycle state machines, tasks, agents,
//! runs, budgets, persistent events, typed tool requests, the agent reply
//! protocol and configuration.
//!
//! This crate performs no I/O. Every invariant that can be checked without a
//! database, process or network lives here and is covered by unit tests.

/// Declares a string-backed enum whose textual form is used in SQLite, events
/// and the agent protocol. Serde uses the same snake_case spelling, and a test
/// asserts that the two agree for every variant.
macro_rules! string_enum {
    ($(#[$meta:meta])* $vis:vis enum $name:ident { $( $(#[$variant_doc:meta])* $variant:ident => $text:literal ),+ $(,)? }) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
        #[serde(rename_all = "snake_case")]
        $vis enum $name {
            $($variant),+
        }

        impl $name {
            /// Every variant in declaration order.
            pub const ALL: &'static [$name] = &[$($name::$variant),+];

            /// Stable textual form.
            pub const fn as_str(self) -> &'static str {
                match self {
                    $($name::$variant => $text),+
                }
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl std::str::FromStr for $name {
            type Err = $crate::CoreError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                match value {
                    $($text => Ok($name::$variant),)+
                    other => Err($crate::CoreError::UnknownValue {
                        kind: stringify!($name),
                        value: other.to_string(),
                    }),
                }
            }
        }
    };
}

pub mod agent;
pub mod artifact;
pub mod budget;
pub mod config;
pub mod error;
pub mod event;
pub mod ids;
pub mod message;
pub mod protocol;
pub mod provider;
pub mod research;
pub mod run;
pub mod state;
pub mod task;
pub mod time;
pub mod tool;

pub use error::{CoreError, CoreResult};
pub use ids::{AgentId, ArtifactId, HypothesisId, MessageId, ProviderId, RunId, SessionId, TaskId};
pub use message::MessageToolCall;
pub use task::AgentRole;
pub use run::RunMode;
pub use state::{AgentState, RunStatus, StateMachine, TaskState};
