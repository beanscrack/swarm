//! `swarm-runtime` — the agent runtime.
//!
//! Implemented in stages: the provider factory and config glue, prompts, the
//! agent loop, the runtime driver and the CLI turn models, tools and state
//! into work.

mod error;
mod factory;

pub use error::RuntimeError;
pub use factory::{build_registry, local_provider_config, scheduler_config};
