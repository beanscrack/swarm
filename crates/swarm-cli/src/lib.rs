//! Command-line entry points for running and inspecting durable Swarm runs.

mod approval;
mod chat;
mod style;
mod shell;
mod commands;
mod custom;
mod hooks;
mod job;
mod job_policy;
mod live;
mod mcp;
mod transcript;
mod ui;
mod tui;

pub use commands::{Cli, CommandOutcome, execute};
