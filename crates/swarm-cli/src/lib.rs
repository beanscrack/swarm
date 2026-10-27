//! Command-line entry points for running and inspecting durable Swarm runs.

mod approval;
mod chat;
mod commands;
mod live;
mod transcript;
mod tui;

pub use commands::{Cli, CommandOutcome, execute};
