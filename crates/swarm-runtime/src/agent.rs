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
use swarm_core::tool::{ToolKind, ToolRequestValidation};
use swarm_core::protocol::AgentPlan;
use swarm_core::{AgentRole, ProviderId, StateMachine, TaskId, TaskState};
use swarm_providers::{
    FinishReason, InferenceResponse, NativeToolCall, Provider, ProviderError, ProviderRegistry,
    SendRequest, Session, SessionConfig, ToolProtocol,
};
use swarm_store::Store;
use swarm_tools::{ToolCall, ToolExecutor, ToolResult};

use crate::prompt::initial_messages_with;
use crate::reply::parse_plan;
use crate::approval::{ApprovalHandler, ApprovalRequest};
use crate::hooks::{HookCall, ToolHooks};

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
    pub approval_handler: Option<Arc<dyn ApprovalHandler>>,
    /// The host's look at each call before and after it runs.
    pub hooks: Option<Arc<dyn ToolHooks>>,
    /// Project instructions (`AGENTS.md` / `CLAUDE.md`) for the system prompt.
    pub instructions: Option<String>,
}

/// Tool output fed back to the model is capped this large (8 KiB).
const MODEL_OUTPUT_CAP: usize = 8 * 1024;

/// Read-only calls from one reply that may run at the same time.
const MAX_PARALLEL_TOOLS: usize = 4;

/// One tool call after it has been announced, validated and (if needed) approved,
/// before it runs. `denial` is why it will not run, when `approved` is false.
struct PreparedTool {
    tc: NativeToolCall,
    name: String,
    approved: bool,
    denial: Option<String>,
    hook_call: HookCall,
}

/// Splits the calls of one reply into runs. A `true` marks a call that may share
/// a run with its neighbours (up to `max` of them); a `false` call is a run on its
/// own. The runs cover every index, in order, so effects keep the order they were
/// requested in.
fn plan_batches(concurrent: &[bool], max: usize) -> Vec<std::ops::Range<usize>> {
    let max = max.max(1);
    let mut runs = Vec::new();
    let mut start = 0;
    while start < concurrent.len() {
        let mut end = start + 1;
        if concurrent[start] {
            while end < concurrent.len() && concurrent[end] && end - start < max {
                end += 1;
            }
        }
        runs.push(start..end);
        start = end;
    }
    runs
}

/// Inference retries per step, after the first attempt.
const MAX_RETRIES: u32 = 3;

/// A fresh `AgentCheckpoint` seeded with the run/task prompt.
fn fresh_checkpoint(run: &Run, task: &Task, instructions: Option<&str>) -> AgentCheckpoint {
    let mut cp = AgentCheckpoint::new();
    cp.messages = initial_messages_with(run, task, instructions);
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
        let cp = match self.store.load_checkpoint(&agent.id).await? {
            Some(existing) => existing,
            None => fresh_checkpoint(run, task, self.instructions.as_deref()),
        };
        agent.steps = cp.steps;

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
        let tool_protocol = if provider_entry.capabilities().tools {
            ToolProtocol::Native
        } else {
            ToolProtocol::Text
        };
        let mut session: Box<dyn Session> = provider_entry
            .start_session(SessionConfig::default())
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;

        let result = self.run_session(run, task, agent, provider_entry.as_ref(), &key,
            tool_protocol, &mut session, cp, now).await;
        let _ = session.close().await;
        result
    }

    async fn run_session(
        &self,
        run: &Run,
        task: &Task,
        agent: &mut Agent,
        provider_entry: &dyn Provider,
        key: &str,
        tool_protocol: ToolProtocol,
        session: &mut Box<dyn Session>,
        mut cp: AgentCheckpoint,
        now: i64,
    ) -> anyhow::Result<AgentOutcome> {
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
                .infer(session, key, run, agent, task, &cp.messages, tool_protocol)
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
            agent.steps = cp.steps;
            self.emit(
                run,
                agent,
                task,
                EventKind::InferenceCompleted,
                json!({
                    "step": cp.steps,
                    "model": provider_entry.model(),
                    "input_tokens": resp.usage.input_tokens,
                    "output_tokens": resp.usage.output_tokens,
                    "spend_micros": spend,
                    "finish_reason": format!("{:?}", resp.finish_reason),
                    "latency_ms": latency_ms,
                    // The live transcript renders the reply itself; the durable
                    // copy of the full turn stays in the agent checkpoint.
                    "text": resp.text.chars().take(4000).collect::<String>(),
                    "tool_calls": resp.tool_calls.iter()
                        .map(|call| call.name.clone()).collect::<Vec<_>>(),
                }),
            )
            .await?;

            // Append the assistant reply and branch on whether it asked for tools.
            if resp.tool_calls.is_empty() {
                if resp.finish_reason == FinishReason::MalformedTool {
                    cp.format_retries += 1;
                    if cp.format_retries > run.budget.max_format_retries {
                        return self.finish_failed(run, agent, task,
                            "Gemini did not provide a valid tool request after format retries".to_string(), false).await;
                    }
                    cp.messages.push(ChatMessage::assistant(resp.text.clone()));
                    cp.messages.push(ChatMessage::user(
                        "Your last tool request was malformed and no tool ran. Reply with only one fenced ```swarm JSON block: {\"tool\":{\"name\":\"exact_tool_name\",\"arguments\":{\"required_field\":\"value\"}}}. The arguments field must be a JSON object matching the selected tool schema."
                    ));
                    self.save(run, agent, task, &cp, now).await?;
                    continue;
                } else if resp.finish_reason == FinishReason::Length {
                    cp.messages.push(ChatMessage::assistant(resp.text.clone()));
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
                    continue;
                } else if task.role == AgentRole::Planner {
                    match parse_plan(&resp.text) {
                        Ok(Some(plan)) => {
                            let count = match self.create_children(run, task, &plan, &mut cp).await {
                                Ok(count) => count,
                                Err(e) => return self.finish_failed(run, agent, task, e.to_string(), false).await,
                            };
                            cp.messages.push(ChatMessage::assistant(resp.text));
                            self.save(run, agent, task, &cp, now).await?;
                            return self.finish_completed(run, agent, task, format!("Planned {count} worker tasks")).await;
                        }
                        invalid => {
                            cp.format_retries += 1;
                            if cp.format_retries > run.budget.max_format_retries {
                                return self.finish_failed(run, agent, task,
                                    format!("planner did not provide a valid swarm plan: {:?}", invalid.err()), false).await;
                            }
                            cp.messages.push(ChatMessage::assistant(resp.text));
                            cp.messages.push(ChatMessage::user(
                                "Reply with a valid fenced ```swarm JSON plan block containing a nonempty plan.steps array."
                            ));
                            self.save(run, agent, task, &cp, now).await?;
                            continue;
                        }
                    }
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

            self.process_tools(&resp.tool_calls, run, agent, task, &mut cp, now).await?;
            self.save(run, agent, task, &cp, now).await?;
        }
    }

    async fn create_children(
        &self,
        run: &Run,
        task: &Task,
        plan: &AgentPlan,
        cp: &mut AgentCheckpoint,
    ) -> anyhow::Result<usize> {
        let all = self.store.list_tasks(&run.id).await?;
        let existing: Vec<&Task> = all.iter()
            .filter(|child| child.parent_id.as_ref() == Some(&task.id))
            .collect();
        if !existing.is_empty() {
            cp.child_task_ids = existing.iter().map(|child| child.id.clone()).collect();
            return Ok(existing.len());
        }
        let count = plan.steps.len();
        anyhow::ensure!(count <= run.agent_target.saturating_sub(1) as usize,
            "plan has {count} workers, above the requested agent target {}", run.agent_target);
        run.budget.check_depth(task.depth + 1)?;
        run.budget.check_children(count)?;
        let pending = all.iter().filter(|t| matches!(t.state, TaskState::Pending | TaskState::Ready)).count();
        run.budget.check_pending_tasks(pending, count)?;
        let now = now_ms();
        let ids: Vec<TaskId> = (0..count).map(|_| TaskId::new()).collect();
        let children: Vec<Task> = plan.steps.iter().enumerate().map(|(index, step)| {
            anyhow::ensure!(step.depends_on.iter().all(|&dependency| dependency < index),
                "step {index} has a dependency that is not an earlier step");
            Ok(Task {
                id: ids[index].clone(),
                run_id: run.id.clone(),
                parent_id: Some(task.id.clone()),
                depth: task.depth + 1,
                title: step.description.chars().take(120).collect(),
                objective: step.description.clone(),
                role: AgentRole::Worker,
                group: format!("{}-{index}", task.id),
                priority: step.priority,
                depends_on: step.depends_on.iter().map(|&dependency| ids[dependency].clone()).collect(),
                state: TaskState::Pending,
                attempts: 0,
                created_at: now,
                updated_at: now,
            })
        }).collect::<anyhow::Result<Vec<_>>>()?;
        self.store.create_tasks(&children).await?;
        cp.child_task_ids = children.iter().map(|child| child.id.clone()).collect();
        for child in &children {
            self.store.append_event(&RunEvent::new(
                EventKind::TaskStateChanged,
                run.id.clone(),
                json!({ "state": "pending", "parent_task_id": task.id.as_str() }),
            ).with_task(child.id.clone())).await?;
        }
        Ok(count)
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
        tool_protocol: ToolProtocol,
    ) -> Result<InferenceResponse, ProviderError> {
        let mut last: Option<ProviderError> = None;
        for attempt in 0..=MAX_RETRIES {
            let mut req = SendRequest::new(messages.to_vec());
            req.tools = self.executor.catalog().specs.clone();
            req.tool_protocol = tool_protocol;
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
                        let delay_ms = class.retry_after_ms()
                            .unwrap_or(500u64 * 2u64.pow(attempt))
                            .min(30_000);
                        sleep(Duration::from_millis(delay_ms)).await;
                    }
                }
            }
        }
        Err(last.unwrap())
    }

    /// Validates and executes the tool calls of one model reply, appending each
    /// result (capped) to the conversation and recording it durably.
    ///
    /// A reply may hold several calls. They are asked about and validated in the
    /// order given, so approval prompts stay one at a time. Calls that only read
    /// (files, search, symbols) and sit next to each other then run together, up to
    /// [`MAX_PARALLEL_TOOLS`]; anything that writes or runs a command runs alone, in
    /// order, so the effects land as if the calls had been made one by one. Results
    /// are recorded in call order whatever order they finished in.
    async fn process_tools(
        &self,
        calls: &[NativeToolCall],
        run: &Run,
        agent: &Agent,
        task: &Task,
        cp: &mut AgentCheckpoint,
        now: i64,
    ) -> anyhow::Result<()> {
        let mut prepared = Vec::with_capacity(calls.len());
        for tc in calls {
            prepared.push(self.prepare_tool(tc, run, agent, task).await?);
        }
        let concurrent: Vec<bool> = prepared
            .iter()
            .map(|p| p.approved && self.is_read_only(&p.name))
            .collect();
        let mut results: Vec<Option<ToolResult>> = (0..prepared.len()).map(|_| None).collect();
        for range in plan_batches(&concurrent, MAX_PARALLEL_TOOLS) {
            let executions = range.clone().filter(|&i| prepared[i].approved).map(|i| {
                let tc = &prepared[i].tc;
                let call = ToolCall::new(tc.id.clone(), tc.name.clone(), tc.arguments.clone());
                async move { (i, self.executor.execute(&call).await) }
            });
            for (i, result) in futures::future::join_all(executions).await {
                results[i] = Some(result);
            }
        }
        for (p, result) in prepared.into_iter().zip(results) {
            self.finish_tool(p, result, run, agent, task, cp, now).await?;
        }
        Ok(())
    }

    /// Whether a tool only reads, judged by the kind it was registered with.
    fn is_read_only(&self, name: &str) -> bool {
        self.executor.catalog().specs.iter().any(|spec| {
            spec.name == name
                && matches!(
                    spec.kind,
                    ToolKind::Read | ToolKind::Search | ToolKind::Symbol | ToolKind::InspectArtifact
                )
        })
    }

    /// Everything about one call that must happen in order and before it runs:
    /// announce it, validate it, give hooks a chance to block it, and ask.
    async fn prepare_tool(
        &self,
        tc: &NativeToolCall,
        run: &Run,
        agent: &Agent,
        task: &Task,
    ) -> anyhow::Result<PreparedTool> {
        let name = tc.name.clone();
        self.emit(
            run,
            agent,
            task,
            EventKind::ToolRequested,
            json!({ "call_id": tc.id.clone(), "name": name.clone(), "arguments": tc.arguments.clone() }),
        )
        .await?;

        let validation = self.executor.validate(name.as_str(), &tc.arguments);
        let approval_risk = match &validation {
            ToolRequestValidation::AutoApprove { .. } if self.approval == ApprovalPolicy::AskAll => {
                Some("approval policy asks for every tool".to_string())
            }
            ToolRequestValidation::NeedsApproval { risk }
                if self.approval != ApprovalPolicy::Never => Some(risk.clone()),
            _ => None,
        };
        // Hooks see a call that is allowed to be made at all, before the user
        // is asked about it: a block is cheaper than a prompt.
        let hook_call = HookCall { run_id: run.id.clone(), name: name.clone(), arguments: tc.arguments.clone() };
        let hook_block = match (&self.hooks, &validation) {
            (_, ToolRequestValidation::Deny { .. }) | (None, _) => None,
            (Some(hooks), _) => hooks.before(&hook_call).await,
        };
        let mut approval_denial = None;
        let approved = if matches!(validation, ToolRequestValidation::Deny { .. }) || hook_block.is_some() {
            false
        } else if let Some(risk) = approval_risk {
            self.emit(run, agent, task, EventKind::ApprovalRequested,
                json!({ "call_id": tc.id, "name": name, "risk": risk })).await?;
            let decision = if let Some(handler) = &self.approval_handler {
                handler.approve(&ApprovalRequest {
                    run_id: run.id.clone(), task_id: task.id.clone(), agent_id: agent.id.clone(),
                    call_id: tc.id.clone(), name: name.clone(),
                    arguments: tc.arguments.clone(), risk,
                }).await
            } else {
                approval_denial = Some("approval required; no interactive handler is available".to_string());
                false
            };
            if !decision && approval_denial.is_none() {
                approval_denial = Some("tool call was not approved".to_string());
            }
            self.emit(run, agent, task, EventKind::ApprovalResolved,
                json!({ "call_id": tc.id, "name": name, "approved": decision })).await?;
            decision
        } else {
            true
        };

        let denial = if approved {
            None
        } else {
            Some(match &validation {
                _ if hook_block.is_some() => {
                    format!("blocked by a hook: {}", hook_block.as_deref().unwrap_or_default())
                }
                ToolRequestValidation::Deny { reason } => {
                    format!("tool call denied: {reason}")
                }
                ToolRequestValidation::NeedsApproval { risk } => {
                    format!("tool `{name}` requires approval: {risk}; {}",
                        approval_denial.as_deref().unwrap_or("approval was denied"))
                }
                ToolRequestValidation::AutoApprove { .. } => approval_denial
                    .unwrap_or_else(|| "tool call was not approved".to_string()),
            })
        };
        Ok(PreparedTool { tc: tc.clone(), name, approved, denial, hook_call })
    }

    /// Records one call's outcome: the hook note, the capped text the model sees,
    /// the durable record and the usage. `result` is `None` for a call that was
    /// denied and so never ran.
    #[allow(clippy::too_many_arguments)]
    async fn finish_tool(
        &self,
        prepared: PreparedTool,
        result: Option<ToolResult>,
        run: &Run,
        agent: &Agent,
        task: &Task,
        cp: &mut AgentCheckpoint,
        now: i64,
    ) -> anyhow::Result<()> {
        let PreparedTool { tc, name, denial, hook_call, .. } = prepared;
        let tc = &tc;
        let (success, model_output, record, change) = match result {
            Some(result) => {
                let mut out = result.as_model_output();
                if let Some(note) = match &self.hooks {
                    Some(hooks) => hooks.after(&hook_call, result.success, &out).await,
                    None => None,
                } {
                    out.push_str(&format!("\n\n[hook] {note}"));
                }
                let change = result.change.clone();
                (result.success, out, result.to_record(&tc.arguments), change)
            }
            None => {
                let reason = denial.unwrap_or_else(|| "tool call was not approved".to_string());
                let mut denied = ToolResult::failure(reason.clone());
                denied.call_id = tc.id.clone();
                denied.name = name.clone();
                (false, reason, denied.to_record(&tc.arguments), None)
            }
        };

        let (model_text, truncated) = cap_for_model(&model_output);
        cp.messages.push(ChatMessage::tool(tc.id.clone(), model_text));

        cp.tool_calls.push(record);
        let tool_delta = BudgetUsage { tool_calls: 1, ..BudgetUsage::default() };
        cp.usage.add(&tool_delta);
        self.store.record_usage(&run.id, &tool_delta, now).await?;
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
                "arguments": tc.arguments.clone(),
                "output": model_output.chars().take(1200).collect::<String>(),
                // What a file tool changed, captured when it ran, so a UI can
                // show an exact diff without racing the file system.
                "change": change,
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
        self.store.update_agent(agent).await?;
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
    use std::sync::atomic::{AtomicUsize, Ordering};
    use swarm_core::budget::ResourceBudget;
    use swarm_core::provider::ProviderErrorClass;
    use swarm_core::{AgentRole, TaskId, TaskState};
    use swarm_providers::{coding_script, BrowserChatConfig, BrowserChatProvider, MockProvider, MockStep, Provider};
    use swarm_tools::SandboxConfig;
    use wiremock::{Mock, MockServer, ResponseTemplate};
    use wiremock::matchers::{method, path};

    struct StaticApproval {
        decision: bool,
        calls: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl ApprovalHandler for StaticApproval {
        async fn approve(&self, request: &ApprovalRequest) -> bool {
            assert!(!request.call_id.is_empty());
            assert!(request.arguments.is_object());
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.decision
        }
    }

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
            approval_handler: None,
            hooks: None,
            instructions: None,
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
    async fn chatgpt_text_tool_call_runs_through_sandbox_and_closes_session() {
        let bridge = MockServer::start().await;
        Mock::given(method("POST")).and(path("/sessions"))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id":"remote-1"})))
            .expect(1).mount(&bridge).await;
        Mock::given(method("POST")).and(path("/sessions/remote-1/messages"))
            .respond_with(|request: &wiremock::Request| {
                let body: Value = serde_json::from_slice(&request.body).unwrap();
                let prompt = body["prompt"].as_str().unwrap();
                let answer = if prompt.contains("[TOOL RESULT") {
                    "verified".to_string()
                } else {
                    "```swarm\n{\"tool\":{\"name\":\"write_file\",\"arguments\":{\"path\":\"hello.txt\",\"content\":\"hello\"}}}\n```".to_string()
                };
                ResponseTemplate::new(200).set_body_json(json!({"text":answer,"chat_url":"https://chatgpt.com/c/fixture","usage":{"input_tokens":120,"output_tokens":18,"cached_input_tokens":0}}))
            })
            .expect(2).mount(&bridge).await;
        Mock::given(method("DELETE")).and(path("/sessions/remote-1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"closed":true})))
            .expect(1).mount(&bridge).await;
        let (mut runner, run, task, mut agent, _mock, dir) = fixture(vec![]).await;
        let provider_id = ProviderId::new("chatgpt-web").unwrap();
        let chatgpt = BrowserChatProvider::new(provider_id.clone(),
            BrowserChatConfig::chatgpt(bridge.uri(), "auto").billing(swarm_core::budget::BillingMode::ZeroCost)).unwrap();
        let mut registry = ProviderRegistry::new();
        registry.register(Arc::new(chatgpt));
        runner.registry = Arc::new(registry);
        let outcome = runner.run(&run, &task, &mut agent, &provider_id).await.unwrap();
        assert!(matches!(outcome, AgentOutcome::Completed { .. }));
        assert_eq!(std::fs::read_to_string(dir.path().join("ws/hello.txt")).unwrap(), "hello");
        // ChatGPT reports token counts on its conversation API, so Swarm's
        // budget ledger is fed real numbers through the bridge.
        let usage = runner.store.load_stats(&run.id).await.unwrap().usage;
        assert_eq!(usage.input_tokens, 240, "two inferences x 120 input tokens");
        assert_eq!(usage.output_tokens, 36, "two inferences x 18 output tokens");
        bridge.verify().await;
    }

    #[tokio::test]
    async fn malformed_text_tool_call_gets_one_correction_turn() {
        let bad = MockStep {
            text: "```swarm\n{\"tool\":{\"name\":\"write_file\",\"arguments\":\"bad\"}}\n```".into(),
            tool_calls: Vec::new(), finish: FinishReason::MalformedTool, error: None,
        };
        let good = MockStep::tool_calls(vec![NativeToolCall::new("fixed", "write_file",
            json!({"path":"repaired.txt","content":"ok"}))]);
        let (runner, run, task, mut agent, provider, dir) =
            fixture(vec![bad, good, MockStep::text("done")]).await;
        let outcome = runner.run(&run, &task, &mut agent, &provider).await.unwrap();
        assert!(matches!(outcome, AgentOutcome::Completed { .. }));
        assert_eq!(std::fs::read_to_string(dir.path().join("ws/repaired.txt")).unwrap(), "ok");
        assert_eq!(agent.steps, 3);
    }

    #[tokio::test]
    async fn planner_repairs_format_and_respects_agent_target() {
        let valid = "```swarm\n{\"plan\":{\"steps\":[{\"description\":\"Implement the first part\"}]}}\n```";
        let script = vec![MockStep::text("no plan"), MockStep::text(valid)];
        let (runner, mut run, mut task, mut agent, provider, _dir) = fixture(script).await;
        run.agent_target = 2;
        task.role = AgentRole::Planner;
        agent.role = AgentRole::Planner;
        let outcome = runner.run(&run, &task, &mut agent, &provider).await.unwrap();
        assert!(matches!(outcome, AgentOutcome::Completed { .. }), "{outcome:?}");
        let children: Vec<_> = runner.store.list_tasks(&run.id).await.unwrap().into_iter()
            .filter(|t| t.parent_id.as_ref() == Some(&task.id)).collect();
        assert_eq!(children.len(), 1);
        assert_eq!(agent.steps, 2);

        let dependent = "```swarm\n{\"plan\":{\"steps\":[{\"description\":\"Create the file\"},{\"description\":\"Verify the file\",\"depends_on\":[0]}]}}\n```";
        let (runner, mut run, mut task, mut agent, provider, _dir) =
            fixture(vec![MockStep::text(dependent)]).await;
        run.agent_target = 3;
        task.role = AgentRole::Planner;
        agent.role = AgentRole::Planner;
        let outcome = runner.run(&run, &task, &mut agent, &provider).await.unwrap();
        assert!(matches!(outcome, AgentOutcome::Completed { .. }), "{outcome:?}");
        let children: Vec<_> = runner.store.list_tasks(&run.id).await.unwrap().into_iter()
            .filter(|child| child.parent_id.as_ref() == Some(&task.id)).collect();
        let creator = children.iter().find(|child| child.objective == "Create the file").unwrap();
        let verifier = children.iter().find(|child| child.objective == "Verify the file").unwrap();
        assert_eq!(verifier.depends_on, vec![creator.id.clone()]);

        let too_many = "```swarm\n{\"plan\":{\"steps\":[{\"description\":\"One\"},{\"description\":\"Two\"}]}}\n```";
        let (runner, mut run, mut task, mut agent, provider, _dir) =
            fixture(vec![MockStep::text(too_many)]).await;
        run.agent_target = 2;
        task.role = AgentRole::Planner;
        agent.role = AgentRole::Planner;
        let outcome = runner.run(&run, &task, &mut agent, &provider).await.unwrap();
        assert!(matches!(outcome, AgentOutcome::Failed { retryable: false, .. }), "{outcome:?}");
        assert_eq!(runner.store.list_tasks(&run.id).await.unwrap().len(), 1);
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
        let checkpoint = runner.store.load_checkpoint(&agent.id).await.unwrap().unwrap();
        assert_eq!(checkpoint.tool_calls.len(), 1);
        assert!(!checkpoint.tool_calls[0].success);
        assert_eq!(runner.store.load_stats(&run.id).await.unwrap().usage.tool_calls, 1);
    }

    #[test]
    fn reads_share_a_run_and_everything_else_runs_alone_in_order() {
        let none: Vec<std::ops::Range<usize>> = Vec::new();
        assert_eq!(plan_batches(&[], 4), none);
        assert_eq!(
            plan_batches(&[true, true, false, true, true, true, true, true], 3),
            vec![0..2, 2..3, 3..6, 6..8]
        );
        assert_eq!(plan_batches(&[false, false], 4), vec![0..1, 1..2]);
        assert_eq!(
            plan_batches(&[true; 3], 0),
            vec![0..1, 1..2, 2..3],
            "a zero cap still makes progress, one call at a time"
        );
    }

    // One reply with many calls: results are recorded in call order, a write is
    // seen by a read that follows it, and a denied call in the middle of a batch
    // fails alone without stopping its neighbours.
    #[tokio::test]
    async fn one_reply_with_many_calls_records_results_in_call_order() {
        let script = vec![
            MockStep::tool_calls(vec![
                NativeToolCall::new("c1", "read_file", json!({ "path": "a.txt" })),
                NativeToolCall::new("c2", "read_file", json!({ "path": "b.txt" })),
                NativeToolCall::new("c3", "write_file", json!({ "path": "d.txt", "content": "WRITTEN-D" })),
                NativeToolCall::new("c4", "read_file", json!({ "path": "d.txt" })),
                NativeToolCall::new("c5", "read_file", json!({ "path": "../outside.txt" })),
                NativeToolCall::new("c6", "read_file", json!({ "path": "c.txt" })),
            ]),
            MockStep::text("done"),
        ];
        let (runner, run, task, mut agent, provider, dir) = fixture(script).await;
        let ws = dir.path().join("ws");
        for tool in ["read_file", "list_files", "search_files"] {
            assert!(runner.is_read_only(tool), "{tool} only reads, so it may run alongside others");
        }
        for tool in ["write_file", "edit_file", "bash"] {
            assert!(!runner.is_read_only(tool), "{tool} changes things, so it runs alone");
        }
        for (name, body) in [("a.txt", "AAA"), ("b.txt", "BBB"), ("c.txt", "CCC")] {
            std::fs::write(ws.join(name), body).unwrap();
        }
        let result = runner.run(&run, &task, &mut agent, &provider).await.unwrap();
        assert!(matches!(result, AgentOutcome::Completed { .. }), "got {result:?}");
        let checkpoint = runner.store.load_checkpoint(&agent.id).await.unwrap().unwrap();
        let results: Vec<(String, String)> = checkpoint
            .messages
            .iter()
            .filter(|m| m.role == swarm_core::message::ChatRole::Tool)
            .map(|m| (m.tool_call_id.clone().unwrap(), m.content.clone()))
            .collect();
        let ids: Vec<&str> = results.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(ids, ["c1", "c2", "c3", "c4", "c5", "c6"], "recorded in call order");
        assert!(results[0].1.contains("AAA") && results[1].1.contains("BBB"));
        assert!(results[3].1.contains("WRITTEN-D"), "a read after a write sees it: {}", results[3].1);
        assert!(results[5].1.contains("CCC"), "a call after a denied one still runs: {}", results[5].1);
        assert_eq!(checkpoint.tool_calls.len(), 6);
        assert!(!checkpoint.tool_calls[4].success, "the escaping path is denied");
        assert!(checkpoint.tool_calls[5].success);
        assert_eq!(runner.store.load_stats(&run.id).await.unwrap().usage.tool_calls, 6);
    }

    /// Blocks anything named `blocked`, and tacks a note on to everything else.
    struct TestHooks {
        before_seen: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl ToolHooks for TestHooks {
        async fn before(&self, call: &HookCall) -> Option<String> {
            self.before_seen.fetch_add(1, Ordering::SeqCst);
            (call.arguments["command"].as_str().is_some_and(|command| command.contains("forbidden")))
                .then(|| "no forbidden commands here".to_string())
        }

        async fn after(&self, call: &HookCall, success: bool, output: &str) -> Option<String> {
            assert_eq!(call.name, "bash");
            assert!(success, "the hook is told how the call went, got output {output:?}");
            Some("remember to run the linter".to_string())
        }
    }

    #[tokio::test]
    async fn a_hook_can_block_a_call_and_annotate_another() {
        let script = vec![
            MockStep::tool_calls(vec![NativeToolCall::new(
                "c1", "bash", json!({ "command": "echo forbidden > blocked.txt" }),
            )]),
            MockStep::tool_calls(vec![NativeToolCall::new(
                "c2", "bash", json!({ "command": "echo fine > fine.txt" }),
            )]),
            MockStep::text("done"),
        ];
        let (mut runner, run, task, mut agent, provider, dir) = fixture(script).await;
        let hooks = Arc::new(TestHooks { before_seen: AtomicUsize::new(0) });
        runner.hooks = Some(hooks.clone());
        let outcome = runner.run(&run, &task, &mut agent, &provider).await.unwrap();
        assert!(matches!(outcome, AgentOutcome::Completed { .. }), "a blocked call does not end the turn");
        assert!(!dir.path().join("ws/blocked.txt").exists(), "the blocked command never ran");
        assert!(dir.path().join("ws/fine.txt").exists(), "the other one did");
        assert_eq!(hooks.before_seen.load(Ordering::SeqCst), 2);
        let events = runner.store.latest_events(&run.id, 100).await.unwrap();
        let results: Vec<&serde_json::Value> =
            events.iter().filter(|event| event.kind == EventKind::ToolResult).map(|event| &event.payload).collect();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0]["success"], false);
        assert!(
            results[0]["output"].as_str().unwrap().contains("blocked by a hook: no forbidden commands here"),
            "the model is told why: {}", results[0]["output"]
        );
        assert!(results[1]["output"].as_str().unwrap().contains("[hook] remember to run the linter"));
    }

    #[tokio::test]
    async fn a_hook_blocks_before_anyone_is_asked_to_approve() {
        let script = vec![
            MockStep::tool_calls(vec![NativeToolCall::new(
                "c1", "bash", json!({ "command": "echo forbidden" }),
            )]),
            MockStep::text("done"),
        ];
        let (mut runner, run, task, mut agent, provider, _dir) = fixture(script).await;
        runner.approval = ApprovalPolicy::AskAll;
        let approval = Arc::new(StaticApproval { decision: true, calls: AtomicUsize::new(0) });
        runner.approval_handler = Some(approval.clone());
        runner.hooks = Some(Arc::new(TestHooks { before_seen: AtomicUsize::new(0) }));
        runner.run(&run, &task, &mut agent, &provider).await.unwrap();
        assert_eq!(approval.calls.load(Ordering::SeqCst), 0, "the user was not bothered with a call a hook stopped");
    }

    #[tokio::test]
    async fn process_tool_requires_approval_by_default() {
        let script = vec![
            MockStep::tool_calls(vec![NativeToolCall::new(
                "c1", "bash", json!({ "command": "echo hello > marker.txt" }),
            )]),
            MockStep::text("done"),
        ];
        let (mut runner, run, task, mut agent, provider, dir) = fixture(script).await;
        runner.approval = ApprovalPolicy::AskDangerous;
        let outcome = runner.run(&run, &task, &mut agent, &provider).await.unwrap();
        assert!(matches!(outcome, AgentOutcome::Completed { .. }));
        assert!(!dir.path().join("ws/marker.txt").exists());
        let checkpoint = runner.store.load_checkpoint(&agent.id).await.unwrap().unwrap();
        assert!(checkpoint.tool_calls[0].error.as_ref().unwrap().contains("requires approval"));

        let script = vec![
            MockStep::tool_calls(vec![NativeToolCall::new(
                "c1", "bash", json!({ "command": "echo hello > marker.txt" }),
            )]),
            MockStep::text("done"),
        ];
        let (mut runner, run, task, mut agent, provider, dir) = fixture(script).await;
        runner.approval = ApprovalPolicy::AskDangerous;
        let approval = Arc::new(StaticApproval { decision: true, calls: AtomicUsize::new(0) });
        runner.approval_handler = Some(approval.clone());
        let outcome = runner.run(&run, &task, &mut agent, &provider).await.unwrap();
        assert!(matches!(outcome, AgentOutcome::Completed { .. }));
        assert!(dir.path().join("ws/marker.txt").exists());
        assert_eq!(approval.calls.load(Ordering::SeqCst), 1);
        let events = runner.store.latest_events(&run.id, 100).await.unwrap();
        assert!(events.iter().any(|event| event.kind == EventKind::ApprovalResolved
            && event.payload["approved"] == true));

        let script = vec![
            MockStep::tool_calls(vec![NativeToolCall::new(
                "c1", "write_file", json!({ "path": "marker.txt", "content": "hello" }),
            )]),
            MockStep::text("done"),
        ];
        let (mut runner, run, task, mut agent, provider, dir) = fixture(script).await;
        runner.approval = ApprovalPolicy::AskAll;
        let approval = Arc::new(StaticApproval { decision: false, calls: AtomicUsize::new(0) });
        runner.approval_handler = Some(approval.clone());
        let outcome = runner.run(&run, &task, &mut agent, &provider).await.unwrap();
        assert!(matches!(outcome, AgentOutcome::Completed { .. }));
        assert!(!dir.path().join("ws/marker.txt").exists());
        assert_eq!(approval.calls.load(Ordering::SeqCst), 1);
    }

    // (e) Crash-and-restart: resume from a persisted checkpoint and build on
    // the prior attempt's progress instead of restarting from step 0.
    #[tokio::test]
    async fn e_resume_continues_from_checkpoint() {
        let (runner, run, task, agent, provider, _dir) =
            fixture(vec![MockStep::text("done")]).await;
        // Persist a checkpoint as if one inference had already completed.
        let mut cp = AgentCheckpoint::new();
        cp.messages = crate::prompt::initial_messages(&run, &task);
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
