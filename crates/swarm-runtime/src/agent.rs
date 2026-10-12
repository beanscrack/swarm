//! The agent turn loop — the heart of the runtime.
//!
//! [`AgentRunner::run`] drives one agent on one task to a terminal outcome: it
//! opens a *single* provider session (kept across the turn so a stateful or
//! scripted provider sees one continuous conversation), then loops on
//! inferences — running native tool calls through the sandboxed executor,
//! recording usage/spend and emitting events, and persisting a checkpoint after
//! every step — until the agent completes, fails, or a budget/quota gate trips.
//!
//! The registry's [`ProviderRegistry::dispatch`] is intentionally not used for
//! the repeated sends: it opens a fresh session per call, which would reset a
//! per-session scripted provider to step 0 every turn. Instead this loop owns
//! the session and reproduces the zero-cost + quota-gate checks `dispatch`
//! performs, so the same invariants hold across a multi-step turn.

use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use tokio::time::sleep;

use swarm_core::agent::{Agent, AgentCheckpoint};
use swarm_core::state::AgentState;
use swarm_core::budget::BudgetUsage;
use swarm_core::config::ApprovalPolicy;
use swarm_core::event::{EventKind, RunEvent};
use swarm_core::message::{ChatMessage, MessageToolCall};
use swarm_core::run::Run;
use swarm_core::task::Task;
use swarm_core::tool::ToolRequestValidation;
use swarm_core::{ProviderId, StateMachine};
use swarm_providers::{
    FinishReason, InferenceResponse, NativeToolCall, ProviderError, ProviderRegistry,
    SendRequest, Session, SessionConfig, ToolProtocol,
};
use swarm_store::Store;
use swarm_tools::{ToolCall, ToolExecutor};

use crate::prompt::initial_messages;

/// How a single agent turn ends.
#[derive(Debug)]
pub enum AgentOutcome {
    /// The agent produced a final answer.
    Completed { summary: String },
    /// The agent could not complete; `retryable` says whether it may be requeued.
    Failed { reason: String, retryable: bool },
}

/// The per-agent runtime: a durable store, the provider registry, and the
/// sandboxed tool executor.
pub struct AgentRunner {
    pub store: Arc<Store>,
    pub registry: Arc<ProviderRegistry>,
    pub executor: Arc<ToolExecutor>,
    pub approval: ApprovalPolicy,
}

/// Tool output fed back to the model is capped this large (8 KiB).
const MODEL_OUTPUT_CAP: usize = 8 * 1024;

/// Inference retries per step, after the first attempt.
const MAX_RETRIES: u32 = 3;

/// A fresh `AgentCheckpoint` seeded with the run/task prompt.
fn fresh_checkpoint(run: &Run, task: &Task) -> AgentCheckpoint {
    let mut cp = AgentCheckpoint::new();
    cp.messages = initial_messages(run, task);
    cp
}

impl AgentRunner {
    /// Runs `agent` on `task` to a terminal [`AgentOutcome`].
    pub async fn run(
        &self,
        run: &Run,
        task: &Task,
        agent: &mut Agent,
        provider: &ProviderId,
    ) -> anyhow::Result<AgentOutcome> {
        let now = now_ms();

        // Resume from a durable checkpoint if one exists; otherwise seed the
        // conversation with the run/task prompts.
        let mut cp = match self.store.load_checkpoint(&agent.id).await? {
            Some(existing) => existing,
            None => fresh_checkpoint(run, task),
        };

        // Move the agent Created -> Ready -> Running (idempotent on resume).
        let from = agent.state;
        for target in [AgentState::Ready, AgentState::Running] {
            if agent.state.can_transition_to(target) {
                agent.transition(target, now).map_err(|e| anyhow::anyhow!("{e}"))?;
            }
        }
        self.emit(
            run,
            agent,
            task,
            EventKind::AgentStateChanged,
            json!({ "from": from.to_string(), "to": agent.state.to_string() }),
        )
        .await?;
        self.store.update_agent(agent).await?;

        // Resolve the provider and open a session kept for the whole turn.
        let provider_entry = self
            .registry
            .find(provider)
            .ok_or_else(|| anyhow::anyhow!("unknown provider: {provider}"))?;
        if run.zero_cost && !provider_entry.billing().is_zero_cost() {
            return self
                .finish_failed(
                    run,
                    agent,
                    task,
                    "zero-cost run selected a non-zero-cost provider".to_string(),
                    false,
                )
                .await;
        }
        let key = provider_entry.id().as_str().to_string();
        let mut session: Box<dyn Session> = provider_entry
            .start_session(SessionConfig::default())
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;

        loop {
            // Budget + quota gates, checked before spending an inference.
            let stats = self.store.load_stats(&run.id).await?;
            if let Err(e) = run.budget.check_steps(cp.steps) {
                return self
                    .finish_failed(run, agent, task, format!("budget: {e}"), false)
                    .await;
            }
            if let Err(e) = run.budget.check_inference(&stats.usage) {
                return self
                    .finish_failed(run, agent, task, format!("budget: {e}"), false)
                    .await;
            }
            if !self.registry.gate.can_dispatch(&key) {
                return self
                    .finish_failed(
                        run,
                        agent,
                        task,
                        "provider throttled by quota gate".to_string(),
                        true,
                    )
                    .await;
            }

            self.emit(
                run,
                agent,
                task,
                EventKind::InferenceStarted,
                json!({ "step": cp.steps }),
            )
            .await?;
            let started = Instant::now();
            let resp = match self
                .infer(&mut session, &key, run, agent, task, &cp.messages)
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    let retryable = e.classify().is_retryable();
                    return self
                        .finish_failed(run, agent, task, e.to_string(), retryable)
                        .await;
                }
            };
            let latency_ms = started.elapsed().as_millis() as u64;

            // Record usage + spend and announce the completed step.
            let spend = provider_entry.pricing().estimate(&resp.usage);
            let delta = BudgetUsage {
                inference_calls: 1,
                input_tokens: resp.usage.input_tokens,
                output_tokens: resp.usage.output_tokens,
                spend_micros: spend,
                tool_calls: 0,
            };
            self.store.record_usage(&run.id, &delta, now).await?;
            cp.usage.add(&delta);
            cp.steps += 1;
            self.emit(
                run,
                agent,
                task,
                EventKind::InferenceCompleted,
                json!({
                    "step": cp.steps,
                    "input_tokens": resp.usage.input_tokens,
                    "output_tokens": resp.usage.output_tokens,
                    "spend_micros": spend,
                    "finish_reason": format!("{:?}", resp.finish_reason),
                    "latency_ms": latency_ms,
                }),
            )
            .await?;

            // Append the assistant reply and branch on whether it asked for tools.
            if resp.tool_calls.is_empty() {
                if resp.finish_reason == FinishReason::Length {
                    cp.messages.push(ChatMessage::user("Continue."));
                    cp.format_retries += 1;
                    if cp.format_retries > run.budget.max_format_retries {
                        return self
                            .finish_failed(
                                run,
                                agent,
                                task,
                                "max format retries exceeded".to_string(),
                                false,
                            )
                            .await;
                    }
                    self.save(run, agent, task, &cp, now).await?;
                } else {
                    self.save(run, agent, task, &cp, now).await?;
                    return self
                        .finish_completed(run, agent, task, resp.text.clone())
                        .await;
                }
            }

            let calls: Vec<MessageToolCall> = resp
                .tool_calls
                .iter()
                .map(|tc| MessageToolCall {
                    id: tc.id.clone(),
                    name: tc.name.clone(),
                    arguments: tc.arguments.clone(),
                })
                .collect();
            cp.messages
                .push(ChatMessage::assistant_with_tool_calls(resp.text.clone(), calls));

            for tc in &resp.tool_calls {
                self.process_tool(tc, run, agent, task, &mut cp, now).await?;
            }
            self.save(run, agent, task, &cp, now).await?;
        }
    }

    /// One inference call with retry on retryable provider errors (500ms·2^n).
    async fn infer(
        &self,
        session: &mut Box<dyn Session>,
        key: &str,
        run: &Run,
        agent: &Agent,
        task: &Task,
        messages: &[ChatMessage],
    ) -> Result<InferenceResponse, ProviderError> {
        let mut last: Option<ProviderError> = None;
        for attempt in 0..=MAX_RETRIES {
            let mut req = SendRequest::new(messages.to_vec());
            req.tools = self.executor.catalog().specs.clone();
            req.tool_protocol = ToolProtocol::Native;
            req.max_tokens = Some(8192);
            req.zero_cost = run.zero_cost;
            match session.send(req).await {
                Ok(resp) => {
                    self.registry.gate.observe_success(key);
                    return Ok(resp);
                }
                Err(e) => {
                    let class = e.classify();
                    self.registry.gate.observe_error(key, &class);
                    let _ = self.emit(
                        run,
                        agent,
                        task,
                        EventKind::InferenceFailed,
                        json!({
                            "class": format!("{class:?}"),
                            "message": e.to_string(),
                            "attempt": attempt,
                        }),
                    )
                    .await;
                    last = Some(e);
                    if !class.is_retryable() {
                        return Err(last.unwrap());
                    }
                    if attempt < MAX_RETRIES {
                        let delay_ms = (500u64 * 2u64.pow(attempt)).min(30_000);
                        sleep(Duration::from_millis(delay_ms)).await;
                    }
                }
            }
        }
        Err(last.unwrap())
    }

    /// Validates and executes one native tool call, appending the tool result
    /// (capped) to the conversation and recording it durably.
    async fn process_tool(
        &self,
        tc: &NativeToolCall,
        run: &Run,
        agent: &Agent,
        task: &Task,
        cp: &mut AgentCheckpoint,
        now: i64,
    ) -> anyhow::Result<()> {
        let name = tc.name.clone();
        self.emit(
            run,
            agent,
            task,
            EventKind::ToolRequested,
            json!({ "call_id": tc.id.clone(), "name": name.clone() }),
        )
        .await?;

        let validation = self.executor.validate(name.as_str(), &tc.arguments);
        let auto = matches!(validation, ToolRequestValidation::AutoApprove { .. });
        let approved = auto
            || (matches!(validation, ToolRequestValidation::NeedsApproval { .. })
                && self.approval == ApprovalPolicy::Never);

        let (success, model_output, record) = if approved {
            let call = ToolCall::new(tc.id.clone(), tc.name.clone(), tc.arguments.clone());
            let result = self.executor.execute(&call).await;
            let out = result.as_model_output();
            (result.success, out, Some(result.to_record(&tc.arguments)))
        } else {
            let reason = match &validation {
                ToolRequestValidation::Deny { reason } => {
                    format!("tool call denied: {reason}")
                }
                ToolRequestValidation::NeedsApproval { risk } => {
                    format!("tool `{name}` requires approval: {risk}")
                }
                _ => unreachable!("reached the unapproved branch"),
            };
            (false, reason, None)
        };

        let (model_text, truncated) = cap_for_model(&model_output);
        cp.messages.push(ChatMessage::tool(tc.id.clone(), model_text));

        if let Some(rec) = record {
            cp.tool_calls.push(rec);
            cp.usage.add(&BudgetUsage {
                tool_calls: 1,
                ..BudgetUsage::default()
            });
            self.store
                .record_usage(
                    &run.id,
                    &BudgetUsage {
                        tool_calls: 1,
                        ..BudgetUsage::default()
                    },
                    now,
                )
                .await?;
        }
        self.emit(
            run,
            agent,
            task,
            EventKind::ToolResult,
            json!({
                "call_id": tc.id.clone(),
                "name": name,
                "success": success,
                "truncated": truncated,
            }),
        )
        .await?;
        Ok(())
    }

    /// Persists the checkpoint and emits a [`EventKind::CheckpointSaved`].
    async fn save(
        &self,
        run: &Run,
        agent: &mut Agent,
        task: &Task,
        cp: &AgentCheckpoint,
        now: i64,
    ) -> anyhow::Result<()> {
        self.store.save_checkpoint(&agent.id, &run.id, cp, now).await?;
        self.emit(
            run,
            agent,
            task,
            EventKind::CheckpointSaved,
            json!({ "version": cp.version, "steps": cp.steps }),
        )
        .await?;
        Ok(())
    }

    async fn finish_completed(
        &self,
        run: &Run,
        agent: &mut Agent,
        task: &Task,
        summary: String,
    ) -> anyhow::Result<AgentOutcome> {
        let now = now_ms();
        let from = agent.state;
        agent
            .transition(AgentState::Completed, now)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        self.emit(
            run,
            agent,
            task,
            EventKind::AgentStateChanged,
            json!({ "from": from.to_string(), "to": "completed" }),
        )
        .await?;
        self.store.update_agent(agent).await?;
        Ok(AgentOutcome::Completed { summary })
    }

    async fn finish_failed(
        &self,
        run: &Run,
        agent: &mut Agent,
        task: &Task,
        reason: String,
        retryable: bool,
    ) -> anyhow::Result<AgentOutcome> {
        let now = now_ms();
        let from = agent.state;
        agent
            .transition(AgentState::Failed, now)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        self.emit(
            run,
            agent,
            task,
            EventKind::AgentStateChanged,
            json!({ "from": from.to_string(), "to": "failed", "reason": reason, "retryable": retryable }),
        )
        .await?;
        self.store.update_agent(agent).await?;
        Ok(AgentOutcome::Failed { reason, retryable })
    }

    /// Appends an event to the run's event stream.
    async fn emit(
        &self,
        run: &Run,
        agent: &Agent,
        task: &Task,
        kind: EventKind,
        payload: Value,
    ) -> anyhow::Result<()> {
        let event = RunEvent::new(kind, run.id.clone(), payload)
            .with_agent(agent.id.clone())
            .with_task(task.id.clone());
        self.store.append_event(&event).await?;
        Ok(())
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Caps tool output fed back to the model; reports whether it was truncated.
fn cap_for_model(out: &str) -> (String, bool) {
    if out.len() <= MODEL_OUTPUT_CAP {
        (out.to_string(), false)
    } else {
        let mut end = MODEL_OUTPUT_CAP;
        while end > 0 && !out.is_char_boundary(end) {
            end -= 1;
        }
        (format!("{}…[truncated]", &out[..end]), true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use swarm_core::budget::ResourceBudget;
    use swarm_core::provider::ProviderErrorClass;
    use swarm_core::{AgentRole, TaskId, TaskState};
    use swarm_providers::{coding_script, MockProvider, MockStep, Provider};
    use swarm_tools::SandboxConfig;

    /// Builds a wired runtime over a tempdir: a store, a mock registry, and a
    /// sandboxed tool executor rooted at `ws`.
    async fn fixture(script: Vec<MockStep>) -> (AgentRunner, Run, Task, Agent, ProviderId, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let provider = ProviderId::new("mock").unwrap();
        let mock = MockProvider::unowned().with_script(script);
        let provider_entry: Arc<dyn Provider> = Arc::new(mock);
        let mut registry = ProviderRegistry::new();
        registry.register(provider_entry);
        let store = Store::open(&dir.path().join("store")).await.unwrap();
        let run = Run::new(
            "objective",
            "mock",
            None,
            1,
            true,
            ResourceBudget::default(),
            now_ms(),
        );
        let task = Task {
            id: TaskId::new(),
            run_id: run.id.clone(),
            parent_id: None,
            depth: 0,
            title: "do the thing".to_string(),
            objective: "make it work".to_string(),
            role: AgentRole::Worker,
            group: "main".to_string(),
            priority: 0,
            depends_on: vec![],
            state: TaskState::Ready,
            attempts: 0,
            created_at: 0,
            updated_at: 0,
        };
        let agent = Agent::new(
            run.id.clone(),
            task.id.clone(),
            None,
            AgentRole::Worker,
            0,
            now_ms(),
        );
        // Persist the run, task and agent rows so the run_id foreign key on
        // events (and the agent row for checkpoints / update_agent) resolve.
        store.create_run(&run).await.unwrap();
        store.create_task(&task).await.unwrap();
        store.create_agent(&agent).await.unwrap();
        let executor = ToolExecutor::new(SandboxConfig::new(ws));
        let runner = AgentRunner {
            store: Arc::new(store),
            registry: Arc::new(registry),
            executor: Arc::new(executor),
            approval: ApprovalPolicy::Never,
        };
        (runner, run, task, agent, provider, dir)
    }

    #[tokio::test]
    async fn a_tool_loop_completes_and_persists() {
        let (runner, run, task, mut agent, provider, dir) =
            fixture(coding_script()).await;
        let ws = dir.path().join("ws");
        let result = runner.run(&run, &task, &mut agent, &provider).await.unwrap();
        assert!(
            matches!(result, AgentOutcome::Completed { .. }),
            "expected Completed, got {result:?}"
        );
        let content = std::fs::read_to_string(ws.join("hello.txt")).unwrap();
        assert_eq!(content, "hello, swarm\n");
        let loaded = runner.store.load_agent(&agent.id).await.unwrap().unwrap();
        assert_eq!(loaded.state, AgentState::Completed);
        // Usage and events were recorded durably.
        let usage = runner.store.load_stats(&run.id).await.unwrap().usage;
        assert!(usage.inference_calls >= 3, "expected >=3 inferences, got {}", usage.inference_calls);
        assert!(usage.tool_calls >= 2, "expected >=2 tool calls, got {}", usage.tool_calls);
    }

    #[tokio::test]
    async fn b_budget_max_steps_fails_not_retryable() {
        let endless = vec![
            MockStep::tool_calls(vec![NativeToolCall::new("c1", "bash", json!({"command": "true"}))]),
            MockStep::tool_calls(vec![NativeToolCall::new("c2", "bash", json!({"command": "true"}))]),
            MockStep::tool_calls(vec![NativeToolCall::new("c3", "bash", json!({"command": "true"}))]),
        ];
        let (runner, mut run, task, mut agent, provider, _dir) = fixture(endless).await;
        run.budget.max_steps_per_agent = 2;
        let result = runner.run(&run, &task, &mut agent, &provider).await.unwrap();
        assert!(
            matches!(result, AgentOutcome::Failed { retryable: false, .. }),
            "expected Failed non-retryable, got {result:?}"
        );
    }

    #[tokio::test]
    async fn c_permanent_fails_and_transient_retries_then_completes() {
        // A permanent failure fails immediately and is not retryable.
        let (runner, run, task, mut agent, provider, _dir) =
            fixture(vec![MockStep::failing(ProviderErrorClass::Permanent)]).await;
        let result = runner.run(&run, &task, &mut agent, &provider).await.unwrap();
        assert!(
            matches!(result, AgentOutcome::Failed { retryable: false, .. }),
            "expected permanent fail (not retryable), got {result:?}"
        );

        // A transient failure is retried and the turn then completes.
        let script = vec![
            MockStep::failing(ProviderErrorClass::Transient),
            MockStep::text("ok"),
        ];
        let (runner, run, task, mut agent, provider, _dir) = fixture(script).await;
        let result = runner.run(&run, &task, &mut agent, &provider).await.unwrap();
        assert!(
            matches!(result, AgentOutcome::Completed { .. }),
            "expected Completed after transient retry, got {result:?}"
        );
    }

    // (d) A tool call that would escape the sandbox is denied and never
    // executed; the denial is what the model sees, and the turn continues.
    #[tokio::test]
    async fn d_denied_path_loop_continues() {
        let script = vec![
            MockStep::tool_calls(vec![NativeToolCall::new(
                "c1",
                "write_file",
                json!({ "path": "../outside.txt", "content": "x" }),
            )]),
            MockStep::text("done"),
        ];
        let (runner, run, task, mut agent, provider, dir) = fixture(script).await;
        let result = runner.run(&run, &task, &mut agent, &provider).await.unwrap();
        assert!(
            matches!(result, AgentOutcome::Completed { .. }),
            "a denied tool call should not stop the turn, got {result:?}"
        );
        assert!(
            !dir.path().join("outside.txt").exists(),
            "the escaping path must be denied, not written"
        );
    }

    // (e) Crash-and-restart: resume from a persisted checkpoint and build on
    // the prior attempt's progress instead of restarting from step 0.
    #[tokio::test]
    async fn e_resume_continues_from_checkpoint() {
        let (runner, run, task, agent, provider, _dir) =
            fixture(vec![MockStep::text("done")]).await;
        // Persist a checkpoint as if one inference had already completed.
        let mut cp = AgentCheckpoint::new();
        cp.messages = initial_messages(&run, &task);
        let base_len = cp.messages.len();
        cp.messages.push(ChatMessage::assistant("step 1 done"));
        cp.steps = 1;
        runner
            .store
            .save_checkpoint(&agent.id, &run.id, &cp, now_ms())
            .await
            .unwrap();
        let mut agent = agent;
        let result = runner.run(&run, &task, &mut agent, &provider).await.unwrap();
        assert!(
            matches!(result, AgentOutcome::Completed { .. }),
            "expected Completed, got {result:?}"
        );
        // The run built on the checkpointed step (1) plus one new inference,
        // and continued the conversation rather than resetting it.
        let loaded = runner
            .store
            .load_checkpoint(&agent.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(loaded.steps, 2, "resume should continue from the checkpointed step");
        assert_eq!(
            loaded.messages.len(),
            base_len + 1,
            "resume should keep the checkpointed conversation (incl. step 1), not reset it"
        );
    }
}
