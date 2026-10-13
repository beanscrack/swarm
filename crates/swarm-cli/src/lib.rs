//! Command-line entry points for running and inspecting durable Swarm runs.

mod commands;

pub use commands::{Cli, CommandOutcome, execute};
