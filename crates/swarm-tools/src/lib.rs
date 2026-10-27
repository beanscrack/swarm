//! Tool execution for Swarm.
//!
//! [`ToolExecutor`] resolves, validates, and executes the tools an agent asks
//! for inside a [`SandboxConfig`]: file read/write/edit, directory listing,
//! search, directory creation, shell commands, tests, Python, and git. Every
//! path is confined to the sandbox workspace and every command runs with that
//! workspace as its working directory. Tool outcomes are [`ToolResult`]s —
//! data an agent can read back — not panics.
//!
//! This crate is transport- and model-agnostic: it consumes plain tool names
//! and JSON arguments and produces results plus durable
//! [`swarm_core::tool::ToolCallRecord`]s.

mod catalog;
mod error;
mod executor;
mod sandbox;

pub use catalog::builtin_catalog;
pub use error::{FileChange, ToolError, ToolResult};
pub use executor::{ToolCall, ToolExecutor};
pub use sandbox::{
    DEFAULT_COMMAND_TIMEOUT, DEFAULT_OUTPUT_CAP, SandboxConfig, cap_output, resolve_within,
};
