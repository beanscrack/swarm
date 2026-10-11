# Swarm API summary (what `swarm-runtime` and `swarm-cli` may call)

Snapshot of the public surface at commit `e22fbd1` ("turn 2/20"), verified against the source.
**Use this instead of reading the crates.** If something you need is not listed, run
`grep -n "pub fn NAME" -A6 crates/<crate>/src/<file>.rs` rather than reading the whole file.
Everything below compiles and is covered by tests (107 passing).

Crate dependency order: `core` <- `store`, `providers`, `scheduler`, `tools` <- `runtime` <- `cli`.

## Traps (read these first)

1. **Two `SchedulerConfig` types.** `swarm_core::config::SchedulerConfig` (run-file policy:
   `max_active_inferences: u32`, `default_provider_concurrency: u32`, ...) is NOT
   `swarm_scheduler::SchedulerConfig` (`max_active: usize`, `provider_limits: BTreeMap<String, usize>`,
   `default_provider_limit: usize`, `max_per_group: usize`). No conversion exists; the runtime writes one.
2. **Two `ProviderRegistry` types.** `swarm_core::provider::ProviderRegistry` is a metadata catalogue
   (`ProviderEntry`). `swarm_providers::ProviderRegistry` holds `Arc<dyn Provider>` and dispatches
   inference. The runtime uses the **providers** one.
3. **No provider factory exists.** Nothing turns a `core::config::ProviderConfig` into an
   `Arc<dyn Provider>`. The runtime must write `build_registry`.
4. **No reply parser exists.** `core::protocol` documents a fenced ```` ```swarm ```` block but there is
   no parser. Native tool calls (`ToolProtocol::Native`) need none.
5. **The `bash` tool runs `cmd /C` on Windows** (`/bin/sh -c` elsewhere). Do not use
   `mock::coding_script()` as-is on Windows (`test -f ... && cat` is POSIX). Script mocks with
   `write_file` / `read_file` instead.
6. **`Store::open(root)`** creates `<root>/swarm.db` and `<root>/artifacts/`. Default root: `.swarm/` (git-ignored).
7. **Native tool calls do not round-trip yet.** `ChatMessage` has no `tool_calls` field, and
   `OpenAiSession::build_body` (`providers/src/openai.rs`, ~line 250) serializes every message as
   `{"role", "content"}` only: a `tool` message goes out WITHOUT `tool_call_id` and the assistant's
   `tool_calls` are never echoed back. Strict OpenAI-style endpoints reject that; `llama-server --jinja`
   mis-renders it. Tests pass only because none sends a multi-turn tool conversation.
   Fixing this is step 0 of `docs/TASK_runtime_cli.md`; the agent loop (step 3) depends on it.

## swarm-core (no I/O)

IDs (all `new()`, `parse(&str) -> CoreResult<Self>`, `as_str()`, Display, serde):
`RunId, TaskId, AgentId, MessageId, SessionId, ArtifactId, HypothesisId`. `ProviderId::new(impl Into<String>) -> CoreResult<Self>`.
`swarm_core::time::now_millis() -> i64`.

State machines (`StateMachine` trait: `can_transition_to`, `transition_to`, `is_terminal`):
- `RunStatus`: Created, Running, Paused, Completed, Failed, Cancelled
- `TaskState`: Pending, Ready, Running, Waiting, Completed, Failed, Cancelled
- `AgentState`: Created, Ready, Running, Waiting, Paused, Completed, Failed, Cancelled
- Legal task moves: Pending->Ready|Failed|Cancelled; Ready->Running|Failed|Cancelled;
  Running->Waiting|Ready|Completed|Failed|Cancelled; Waiting->Ready|Failed|Cancelled.
- Legal agent moves: Created->Ready; Ready->Running; Running->Waiting|Ready|Paused|Completed|Failed|Cancelled; ...

```rust
pub enum AgentRole { Planner, Worker, Reviewer }          // string_enum: as_str(), FromStr, ALL
pub enum RunMode { General, Coding, Research, Benchmark }

pub struct Run { id: RunId, objective: String, status: RunStatus, mode: RunMode, provider: String,
  model: Option<String>, root_task_id: Option<TaskId>, agent_target: u32, zero_cost: bool,
  budget: ResourceBudget, created_at: i64, updated_at: i64 }
Run::new(objective, provider, model: Option<String>, agent_target: u32, zero_cost: bool, budget, now) -> Run // status Created
run.transition(RunStatus, now) -> CoreResult<()>

pub struct Task { id, run_id, parent_id: Option<TaskId>, depth: u32, title, objective, role: AgentRole,
  group: String, priority: i32, depends_on: Vec<TaskId>, state: TaskState, attempts: u32, created_at, updated_at }
pub struct TaskSpec { title, objective, role, group, priority, depends_on: Vec<TaskId> }
task.transition(TaskState, now); topological_order(&[(TaskId, Vec<TaskId>)]) -> CoreResult<Vec<TaskId>>;
dependencies_satisfied(&[TaskId], &HashSet<TaskId>) -> bool
// NOTE: there is no Task::new(); build the struct literally (TaskId::new(), state Pending/Ready, attempts 0).

pub struct Agent { id, run_id, task_id, parent_id: Option<AgentId>, role, depth, state, steps: u32, created_at, updated_at }
Agent::new(run_id, task_id, parent_id, role, depth, now) -> Agent  // state Created
agent.transition(AgentState, now)

pub struct AgentCheckpoint { version: u32, steps: u32, format_retries: u32, messages: Vec<ChatMessage>,
  tool_calls: Vec<ToolCallRecord>, child_task_ids: Vec<TaskId>, usage: BudgetUsage }
AgentCheckpoint::new()

pub struct ChatMessage { role: ChatRole, kind: ChatMessageKind, content: String, tool_call_id: Option<String>,
  artifact_refs: Vec<ArtifactRef>, created_at: i64 }
ChatMessage::{system, user, assistant}(content); ChatMessage::tool(call_id, content)
// ChatRole: System|User|Assistant|Tool.  No `tool_calls` field yet (added in brief step 0).

pub struct ResourceBudget { max_steps_per_agent: u32 (16), max_depth: u32 (3), max_children_per_task: u32 (16),
  max_pending_tasks: u32, max_inference_calls: u64, max_total_tokens: u64, max_spend_micros: u64 (0 = no spend),
  max_wall_clock_secs: u64, tool_timeout_ms: u64, max_tool_output_bytes: usize, tool_memory_limit_mb: u64,
  max_format_retries: u32 (2), max_task_attempts: u32 (2) }   // Default impl; serde(default)
budget.check_inference(&BudgetUsage) / check_spend(&BudgetUsage, add_micros) / check_depth(u32) /
  check_children(usize) / check_pending_tasks(pending, new) / check_steps(steps_taken)  -> CoreResult<()>
pub struct BudgetUsage { inference_calls, input_tokens, output_tokens, spend_micros, tool_calls: u64 }  // Copy, Default
usage.add(&other); usage.total_tokens()
pub struct TokenUsage { input_tokens, output_tokens, cached_input_tokens: u64 }   // in swarm_core::budget

pub enum EventKind { AgentStateChanged, TaskStateChanged, RunStateChanged, InferenceStarted, InferenceCompleted,
  InferenceFailed, ToolRequested, ToolResult, FindingSubmitted, ArtifactCreated, ResearchUpdated, CheckpointSaved,
  Message, Log }
RunEvent::new(kind, run_id, payload: serde_json::Value).with_agent(AgentId).with_task(TaskId)   // seq assigned by store

// tools (core::tool)
pub struct ToolSpec { name, kind: ToolKind, description, input_schema: ToolSchema, sandboxed: bool,
  max_output_bytes, requires_approval: bool }
pub struct ToolCatalog { pub specs: Vec<ToolSpec> }  // new(), register(spec), get(name), len()
pub enum ToolRequestValidation { AutoApprove{reason}, NeedsApproval{risk}, Deny{reason} }  // is_auto()
pub struct ToolCallRecord { call_id, name, input: Value, output_ref: Option<ArtifactRef>, success: bool,
  error: Option<String>, duration_ms: u64, created_at: i64 }

// protocol (data only; no parser)
ToolRequest{id,name,input,artifact_refs,created_at}; PlanStep{description,kind,priority}; AgentPlan{steps,expected_artifacts,success_criteria};
FindingSubmission::new(title, content) + .validate(); AgentReply::new(MessageId, text){tool_requests,finding,plan,done,done_reason,raw}

// config (core::config) -- serde, loaded from swarm.toml
RunConfig { objective, provider: ProviderId, model: String, agents: u32, zero_cost: bool, budget: ResourceBudget,
  providers: Vec<ProviderConfig>, scheduler: core SchedulerConfig, memory, research, tools: ToolSandboxConfig }
RunConfig::mock(objective, agents) -> RunConfig;  .provider_config(&ProviderId) -> Option<ProviderConfig>;  .may_run(&ProviderConfig) -> bool
ProviderConfig { name: ProviderId, kind: ProviderKind (Mock|OpenAiCompatible|ManagedAgent), model, base_url: Option<String>,
  api_key, api_key_env, max_output_tokens: u32, temperature: Option<f32>, workspace_dir: Option<PathBuf>,
  extra_args: Vec<String>, zero_cost: bool, billing: BillingMode, max_concurrency: Option<u32> }
ProviderConfig::mock(name, model); ProviderConfig::openai_compatible(name, model); .api_key_resolved() -> Option<String>
ToolSandboxConfig { workspace_root, blocked_paths, max_output_bytes: u64, command_timeout_secs: u64, memory_limit_mb, allow_network, approval: ApprovalPolicy }
```

`CoreError`, `CoreResult<T>` are in `swarm_core`. `BillingMode` (`swarm_core::budget`) has `.is_zero_cost()`.

## swarm-store (`Store`, all methods `async`, return `anyhow::Result`)

```rust
Store::open(root: &Path) -> Result<Store>;  store.root(); store.artifacts_dir(); DEFAULT_LEASE_TTL_MS = 120_000
// runs
create_run(&Run); load_run(&RunId) -> Option<Run>; load_run_required(&RunId) -> Run; list_runs() -> Vec<Run>;
update_run(&Run); set_run_root_task(&RunId, &TaskId, now)
// tasks  (claim sets state=running, attempts+=1, lease; complete/fail require the lease token)
create_task(&Task); load_task(&TaskId) -> Option<Task>; list_tasks(&RunId) -> Vec<Task>;
tasks_by_state(&RunId, TaskState) -> Vec<Task>; finished_task_count(&RunId) -> u32;
claim_task(&TaskId, token: &str, now, ttl_ms) -> bool;  renew_lease(&TaskId, token, now, ttl_ms) -> bool;
release_lease(&TaskId, token, now) -> bool (-> ready);  reclaim_expired_leases(now) -> Vec<Task> (-> ready);
complete_task(&TaskId, token, now);  fail_task(&TaskId, token, now, retry: bool) (retry => ready, else failed);
set_task_state(&TaskId, TaskState, now)   // no lease; for Pending->Ready promotion and failure cascades
// agents
create_agent(&Agent); load_agent(&AgentId) -> Option<Agent>; agents_for_run(&RunId) -> Vec<Agent>;
save_checkpoint(&AgentId, &RunId, &AgentCheckpoint, now); load_checkpoint(&AgentId) -> Option<AgentCheckpoint>
// events
append_event(&RunEvent) -> u64 (seq); last_seq(&RunId) -> u64; events_since(&RunId, since: u64, limit: u32); latest_events(&RunId, limit: u32)
// usage (record_usage ADDS the delta)
record_usage(&RunId, &BudgetUsage, now) -> RunStats; load_stats(&RunId) -> RunStats{usage, verified_results, wasted_work, cache_reuse};
increment(&RunId, "verified_results"|"wasted_work"|"cache_reuse", n, now)
// recovery
recover_run(&RunId, now) -> RecoveryReport{released_tasks, reset_agents};  live_agents(&RunId) -> Vec<Agent>
// artifacts / findings / hypotheses
store_artifact(&RunId, &Artifact, now); artifact_path(&ArtifactId); load_artifact(&ArtifactId); list_artifacts(&RunId) -> Vec<ArtifactRef>
submit_finding(&RunId, title, content, confidence: f64, tags: &[String], json: Value, ...) ; search_findings(&RunId, query, limit); recent_findings(&RunId, limit)
upsert_hypothesis(&Hypothesis); load_hypothesis(&HypothesisId); list_hypotheses(&RunId)
```

## swarm-providers

```rust
#[async_trait] trait Provider: Send+Sync { id() -> &ProviderId; kind(); model() -> &str; capabilities() -> ProviderCapabilities;
  billing() -> BillingMode; pricing() -> ModelPricing; async start_session(SessionConfig) -> Result<Box<dyn Session>, ProviderError> }
#[async_trait] trait Session { async stream(&SendRequest) -> ProviderStream; async send(SendRequest) -> Result<InferenceResponse, ProviderError>; async close() }

SendRequest { messages: Vec<ChatMessage>, tools: Vec<ToolSpec>, tool_protocol: ToolProtocol (Native|Text|None),
  temperature: Option<f32>, max_tokens: Option<u32>, zero_cost: bool, deadline: Option<Duration> }   // SendRequest::new(messages)
InferenceResponse { text, thinking: String, tool_calls: Vec<NativeToolCall{id,name,arguments: Value}>,
  finish_reason: FinishReason (Stop|ToolCalls|Length|Other), usage: TokenUsage, provider, model, raw }   // has_tool_calls()

ProviderRegistry::new(); .register(Arc<dyn Provider>); .find(&ProviderId) -> Option<Arc<dyn Provider>>; .entries();
  .dispatch(&ProviderId, SendRequest) -> Result<InferenceResponse, ProviderError>
  // dispatch already enforces zero_cost, quota/cooldown gate, and records spend. It does NOT record run usage.
  .gate: QuotaGate { set_spend_cap(provider, Option<u64>), can_dispatch(provider), retry_after_ms(provider), snapshot() }

ProviderError { Core, Throttled{provider,reason,retry_after_ms}, Transport, Api{status,message,class}, Malformed, Timeout, Process, Session }
  .classify() -> ProviderErrorClass { Transient, RateLimited{retry_after_ms: Option<u64>}, QuotaExhausted, Auth, Unsupported, Permanent }
  class.is_retryable() is true for Transient | RateLimited | QuotaExhausted; class.retry_after_ms() -> Option<u64>

MockProvider::new() -> Arc<Self>;  MockProvider::unowned().with_script(Vec<MockStep>) -> MockProvider (wrap in Arc);
  MockStep::text("..") | MockStep::tool_calls(vec![NativeToolCall::new(id, name, json!({..}))]) | MockStep::failing(class)
OpenAiConfig::new(base_url, model) -> Result<_, CoreError>  { api_key, temperature, max_tokens, timeout_ms (60_000), billing, pricing, streaming }
OpenAiProvider::new(ProviderId, OpenAiConfig) -> Result<OpenAiProvider, ProviderError>
ManagedAgentConfig { name, command: Vec<String>, workdir, env, timeout, billing, pricing };  ManagedAgentProvider
ModelPricing::estimate(&TokenUsage) -> micro-USD
```

## swarm-scheduler (pure, no tokio)

```rust
SchedulerConfig { max_active, provider_limits, default_provider_limit, max_per_group }  // Default: 8, {}, 4, 2
Occupancy::new(); .get(p); .set(p, n); .add(p, delta); .total()
SchedulableTask::from_task(&Task, provider: &str, completed: &HashSet<TaskId>) -> SchedulableTask   // ready == Task.state==Ready && deps done
Scheduler::new(SchedulerConfig).plan(&[SchedulableTask], &Occupancy, boosts: &BTreeMap<String, i32>) -> SchedulePlan
SchedulePlan { admitted: Vec<PlannedDispatch{id, provider, group, priority, depth}>, deferred: Vec<(TaskId, DeferReason)> }  // admitted_ids()
```
`plan` does not mutate occupancy: the runtime adds to `Occupancy` when it dispatches and lowers it when a lane finishes.

## swarm-tools

```rust
SandboxConfig::new(workspace_root).command_timeout(Duration).output_cap_bytes(u64).allow_network(bool)
ToolExecutor::new(SandboxConfig) -> Self  // catalog = builtin_catalog(): bash, read_file, write_file, edit_file, list/search/mkdir, tests, python, git
  .catalog() -> &ToolCatalog;  .tool_names();  .spec(name);
  .validate(name: &str, input: &Value) -> ToolRequestValidation
  async .execute(&ToolCall{id,name,arguments}) -> ToolResult
ToolResult { call_id, name, success, content, stderr, exit_code: Option<i32>, truncated, error: Option<String>, duration_ms, output_ref }
  .as_model_output() -> String (what to feed back as ChatMessage::tool);  .to_record(&input: &Value) -> ToolCallRecord
```
Tool failures are data (`success == false`), never panics. Paths are confined to the sandbox workspace.

## swarm-runtime, swarm-cli (today)

`swarm-runtime/src/lib.rs` = a 6-line doc comment. `swarm-cli/src/main.rs` = `fn main() -> anyhow::Result<()> { Ok(()) }`.
`swarm-cli` binary name is `swarm` (`[[bin]] name = "swarm"`); library target is `swarm_cli`.
