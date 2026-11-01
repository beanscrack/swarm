//! `swarm-runtime` — the agent runtime.
//!
//! Implemented in stages: the provider factory and config glue, prompts, the
//! agent loop, the runtime driver and the CLI turn models, tools and state
//! into work.

mod agent;
mod approval;
mod error;
mod evidence;
mod factory;
mod hooks;
mod prompt;
pub mod repo_map;
mod reply;
mod runtime;
pub mod review;

pub use agent::{AgentOutcome, AgentRunner};
pub use approval::{ApprovalHandler, ApprovalRequest};
pub use hooks::{HookCall, ToolHooks};
pub use error::RuntimeError;
pub use factory::{build_registry, chatgpt_web_provider_config, gemini_web_provider_config, local_provider_config, scheduler_config};
pub use prompt::{
    initial_messages, initial_messages_with, project_instructions, system_prompt, system_prompt_with,
};
pub use reply::parse_plan;
pub use runtime::Runtime;
