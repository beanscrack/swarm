# Task: make `swarm run` work end to end (runtime + CLI)

> Implementation status (2026-10-13): steps 0a through 6 are implemented and
> covered by workspace tests, including the optional planner decomposition.
> The HTTP path for the local preset is tested against an OpenAI-compatible
> server fixture. The manual llama-server smoke test below still needs a
> running local model.

Goal: `swarm run "<goal>" --provider mock` creates a run, executes agents through the scheduler, tools and
store, and exits 0 with a persisted, inspectable result. Then the same with `--provider local`
(llama-server). Today `swarm-runtime` is a 6-line doc comment and `swarm-cli` is an empty `main`; everything
they need underneath exists and is tested (see `docs/API_SUMMARY.md`).

**Read `docs/API_SUMMARY.md` first. It replaces reading the crates.** Do the steps in order. One step =
one commit; each step has a "Done when" you can run.

## Working rules (the last three hours were lost to breaking these)

1. **Write early.** At most 5 read/search calls before the first file write in a step. The signatures you need
   are in `API_SUMMARY.md`. For anything missing: `grep -n "pub fn NAME" -A6 <file>`, never open a whole file.
2. **Never read a file larger than ~150 lines in full.** Use `offset`/`limit`. Never re-read a file you wrote.
3. **Let the compiler drive.** Write the file, run `cargo check -p <crate>`, fix the errors it names. Do not
   re-plan between edits; do not re-derive designs that are written below.
4. **Context hygiene.** Pipe cargo output through `tail -40`/`grep -E "^(error|warning: unused)|test result"`.
   Never `cat` a lock file, `target/` or a long test log.
5. **Keep thinking short.** The design is below; implement it.
6. **New files:** create with `write_file`. **Existing files** (`runtime/src/lib.rs`, `cli/src/main.rs` are tiny):
   read once in full, then overwrite. Earlier runs wasted calls on "refusing to overwrite ... not seen its full content".
7. **Windows:** the `bash` tool is `cmd /C`. In tests script tools via `write_file`/`read_file`, not shell one-liners.
   Cargo needs `export PATH="$HOME/.cargo/bin:$PATH"` in the bash tool.
8. **Commit after each step passes** (`git add -A && git commit -m "turn N/20: step X ..."`). Do not edit
   public signatures in other crates beyond what a step says; if you are blocked, append one line to
   `docs/BLOCKERS.md` and work around it.
9. Do not implement the TUI, research mode, managed-agent providers, or interactive approvals. Out of scope.

## Step 0a: native tool calls must round-trip (core + providers, ~60 lines)

Problem: see trap 7 in `API_SUMMARY.md`. Without this the agent loop breaks on the first real model reply.

- `swarm-core/src/message.rs`: add `pub struct MessageToolCall { pub id: String, pub name: String, pub arguments: serde_json::Value }`
  (Debug, Clone, PartialEq, Serialize, Deserialize), re-export it from `lib.rs`. Add
  `#[serde(default, skip_serializing_if = "Vec::is_empty")] pub tool_calls: Vec<MessageToolCall>` to `ChatMessage`
  (default empty in `base`) and `ChatMessage::assistant_with_tool_calls(content, Vec<MessageToolCall>)`.
  Old stored checkpoints must still deserialize. Fix any struct-literal constructions the compiler reports.
- `swarm-providers/src/openai.rs` `build_body`: for an assistant message with tool calls emit
  `{"role":"assistant","content":<text or null>,"tool_calls":[{"id","type":"function","function":{"name","arguments":<JSON-encoded STRING>}}]}`;
  for a `tool` message add `"tool_call_id"`. All other messages unchanged.
- Tests: (a) core serde round trip incl. an old JSON without `tool_calls`; (b) a wiremock test in `openai.rs`
  that sends `[user, assistant_with_tool_calls, tool]` and asserts the request body has `tool_calls[0].function.arguments`
  as a string and `tool_call_id` on the tool message.

Done when: `cargo test -p swarm-core -p swarm-providers` passes.

## Step 0b: persist agent state (store, ~25 lines)

`Store` can `create_agent` but cannot update one. Add `pub async fn update_agent(&self, agent: &Agent) -> Result<()>`
in `swarm-store/src/agents.rs` (`UPDATE agents SET state, steps, updated_at WHERE id`; error if no row) and a test
that creates, mutates state/steps, reloads. Look at `create_agent` in the same file for column names.

Done when: `cargo test -p swarm-store` passes.

## Step 1: provider factory and config glue (`swarm-runtime/src/factory.rs`, `error.rs`)

- `RuntimeError` (thiserror): `Config(String)`, `Provider(ProviderError)`, `Store(anyhow::Error)`, `Core(CoreError)`, `Unsupported(String)`.
- `build_registry(cfg: &RunConfig) -> Result<ProviderRegistry, RuntimeError>`: for each `ProviderConfig`:
  `Mock` -> `Arc::new(MockProvider::unowned())` (with `id` set from the config name); `OpenAiCompatible` ->
  `OpenAiConfig::new(base_url, model)` then set `api_key = cfg.api_key_resolved()`, `temperature`, `max_tokens =
  Some(max_output_tokens)`, `billing = cfg.billing`, **`timeout_ms = 600_000`** (the default 60 s kills a local
  reasoning model whose calls take 100-265 s) -> `OpenAiProvider::new`; `ManagedAgent` -> `Unsupported`.
  Skip providers that fail `cfg.may_run(..)` only if the run is zero-cost and the provider is not.
- `scheduler_config(cfg: &RunConfig, registry: &ProviderRegistry) -> swarm_scheduler::SchedulerConfig`: map
  `max_active_inferences -> max_active`, `default_provider_concurrency -> default_provider_limit`, and
  `provider_limits[name] = max_concurrency` from each `ProviderConfig`. `max_per_group = 2`.
- `local_provider_config() -> ProviderConfig` preset for the llama-server: kind OpenAiCompatible, name `local`, model
  `qwen3.8-27b-hauhau`, `base_url = Some("http://127.0.0.1:8080/v1")`, `api_key_env = None`, `zero_cost = true`,
  `billing = BillingMode::ZeroCost`, `max_concurrency = Some(1)` (server runs `--parallel 1`), `max_output_tokens = 8192`.

Tests: mock config builds 1 provider; `local_provider_config()` builds without network; scheduler mapping honours
`max_concurrency = 1`.

Add to `swarm-runtime/Cargo.toml`: `uuid = { workspace = true }`; dev-dependency `tempfile = { workspace = true }`.
Done when: `cargo test -p swarm-runtime` passes.

## Step 2: prompts (`swarm-runtime/src/prompt.rs`, pure functions, ~100 lines)

- `system_prompt(role: AgentRole, mode: RunMode) -> String`: short and deterministic. Worker: "You are a coding agent. Use the
  provided tools to complete the task inside the workspace. Verify your work (run tests/read files back). When
  finished, reply with a brief summary and NO tool calls." Planner/Reviewer: one-paragraph variants.
- `initial_messages(run: &Run, task: &Task) -> Vec<ChatMessage>`: `[system(system_prompt), user("Objective: ..\nTask: <title>\n<objective>")]`.

Tests: contains the objective and task title; identical input gives identical output.
Done when: `cargo test -p swarm-runtime prompt` passes.

## Step 3: the agent loop (`swarm-runtime/src/agent.rs`) , the core of the project

```rust
pub struct AgentRunner { pub store: Arc<Store>, pub registry: Arc<ProviderRegistry>, pub executor: Arc<ToolExecutor> }
pub enum AgentOutcome { Completed { summary: String }, Failed { reason: String, retryable: bool } }
impl AgentRunner { pub async fn run(&self, run: &Run, task: &Task, agent: &mut Agent, provider: &ProviderId) -> anyhow::Result<AgentOutcome> }
```
Algorithm (implement exactly; each bullet is a few lines):

1. `cp = store.load_checkpoint(&agent.id)` or `AgentCheckpoint::new()` + `initial_messages`. Move agent to `Running`
   (Created->Ready->Running, or Paused->Ready->Running on resume), persisting with `update_agent` and emitting
   `AgentStateChanged`.
2. Loop:
   - `run.budget.check_steps(cp.steps)` and `check_inference(&store.load_stats(run.id).usage)`; on error return
     `Failed { retryable: false }`.
   - Build `SendRequest { messages: cp.messages.clone(), tools: executor.catalog().specs.clone(), tool_protocol: Native,
     max_tokens: Some(8192), zero_cost: run.zero_cost, deadline: None, .. }`. Emit `InferenceStarted`.
   - `registry.dispatch(provider, req)`. On `Err(e)`: `class = e.classify()`; if `class.is_retryable()` retry up to 3
     times, sleeping `class.retry_after_ms().unwrap_or(500 * 2^n)` capped at 30 s; otherwise return
     `Failed { retryable: class.is_retryable() }`. Emit `InferenceFailed` on each error.
   - On success: delta = `BudgetUsage { inference_calls: 1, input_tokens, output_tokens, spend_micros: provider pricing estimate (0 if unknown), .. }`;
     `store.record_usage`; `cp.usage.add`; `cp.steps += 1`; emit `InferenceCompleted`. **Never put `resp.thinking` in the
     history** (it bloats context for nothing).
   - Push `assistant_with_tool_calls(resp.text, calls)` (or plain `assistant`). If there are **no tool calls**:
     `finish_reason == Length` -> push a user nudge "Continue." and count it against `max_format_retries`; otherwise
     save the checkpoint and return `Completed { summary: resp.text }`.
   - For each call: `executor.validate(name, &args)`. `AutoApprove` -> execute. `NeedsApproval` -> execute only when
     `RunConfig.tools.approval == Never` (CLI `--approve-all`), else a denial message ("approval required; interactive
     approval is not supported"). `Deny{reason}` -> denial message. Push `ChatMessage::tool(call.id, output)`, where
     `output = result.as_model_output()` **truncated to 8 KiB** (`cap_output`; never feed the model a giant tool result,
     it is what fills the context window). Push `result.to_record(&args)` into `cp.tool_calls`; `cp.usage.tool_calls += 1`;
     emit `ToolRequested` and `ToolResult`; `record_usage` for the tool call.
   - `store.save_checkpoint`; emit `CheckpointSaved`. Continue.
3. On `Completed` set agent `Completed`; on `Failed` set agent `Failed` (always `update_agent`).

Event payloads (keep these small and flat): `InferenceStarted {step}`; `InferenceCompleted {step, input_tokens, output_tokens, finish_reason, latency_ms}`;
`InferenceFailed {class, message}`; `ToolRequested {call_id, name}`; `ToolResult {call_id, name, success, truncated}`;
`AgentStateChanged {from, to}`; `CheckpointSaved {step}`. Always `.with_agent(..).with_task(..)`.

Tests (use `MockProvider::unowned().with_script(..)` in an `Arc`, a `tempfile` store and workspace; set approval Never):
a. script `[tool_calls(write_file hello.txt), tool_calls(read_file hello.txt), text("done")]` -> `Completed`, file exists,
   3 inferences, usage recorded, events include `ToolResult`, agent state `Completed` in the store.
b. `max_steps_per_agent = 2` with an endless tool-calling script -> `Failed`, not retryable.
c. `MockStep::failing(ProviderErrorClass::Permanent)` -> `Failed { retryable: false }`; `Transient` then success -> retries and completes.
d. a denied path (`write_file` to `..\\outside.txt`) -> loop continues, tool message says it was denied/failed.
e. resume: run (a) cut off after step 1 (script runs out is not enough; save a checkpoint manually), then `run` again
   -> continues from the checkpoint, does not repeat step 1.
Done when: `cargo test -p swarm-runtime agent` passes.

## Step 4: the runtime driver (`swarm-runtime/src/runtime.rs`)

```rust
pub struct Runtime { store: Arc<Store>, registry: Arc<ProviderRegistry>, executor: Arc<ToolExecutor>, scheduler: Scheduler }
impl Runtime {
  pub fn new(store, registry, executor, scheduler: Scheduler) -> Self
  pub async fn create_run(&self, cfg: &RunConfig, mode: RunMode) -> Result<Run, RuntimeError>
  pub async fn drive(&self, run_id: &RunId) -> Result<RunStatus, RuntimeError>
}
```
- `create_run`: `Run::new(..)` from `cfg`, `store.create_run`; root `Task` (built literally: `depth 0`, role Worker,
  group `"main"`, priority 0, `state Ready`, objective = run objective); `create_task`; `set_run_root_task`;
  transition run to `Running`; emit `RunStateChanged`.
- `drive`: `store.recover_run` first. Then loop (a `tokio::task::JoinSet` holds in-flight agents; keep an `Occupancy`):
  1. `list_tasks`; `completed` = ids in `Completed`; promote `Pending` tasks whose `depends_on` are all completed to `Ready`
     (`set_task_state`, emit `TaskStateChanged`); fail `Pending/Ready` tasks that depend on a `Failed/Cancelled` task.
  2. candidates = `Ready` tasks -> `SchedulableTask::from_task(t, run.provider, &completed)`;
     `scheduler.plan(&candidates, &occupancy, &BTreeMap::new())`.
  3. for each admitted: `token = uuid::Uuid::new_v4().to_string()`; `claim_task(id, &token, now, DEFAULT_LEASE_TTL_MS)`;
     if it returns `true`: create the `Agent` (`create_agent`), `occupancy.add(provider, 1)`, spawn `AgentRunner::run`.
  4. If nothing is in flight and nothing is admitted: break (every task terminal, or a stuck graph -> run `Failed`).
  5. Otherwise wait for the next finished agent (`join_next`) **or** a tick of `DEFAULT_LEASE_TTL_MS / 3` to `renew_lease`
     for in-flight tasks. On finish: `occupancy` -1; `Completed` -> `complete_task`; `Failed{retryable}` ->
     `fail_task(.., retry = retryable && task.attempts < budget.max_task_attempts)`.
  6. Stop with `Failed` when `budget.max_wall_clock_secs` has elapsed.
- End state: all tasks `Completed` -> run `Completed`; otherwise `Failed`. Persist with `update_run`, emit `RunStateChanged`.
  Return the final `RunStatus`.

Tests (mock provider, tempdir store + workspace): single-task run completes and `hello.txt` exists; run with two
independent tasks inserted by the test and `provider_limits = 1` never has more than 1 agent in flight (record max via an
`AtomicUsize` in a test-only provider wrapper or assert on event order); a task whose agent fails non-retryably fails the
run; an expired lease (claim with a past `now`) is recovered and re-run.
Done when: `cargo test -p swarm-runtime` passes and `lib.rs` re-exports `Runtime`, `AgentRunner`, `build_registry`, `RuntimeError`.

## Step 5: planner decomposition (optional stretch, only after step 6 works)

Skip until the CLI milestone is green. Needs a reply parser for a fenced ` ```swarm ` JSON block in
`swarm-runtime/src/reply.rs` (data types already in `swarm_core::protocol`) and creation of child tasks with
`check_depth/check_children/check_pending_tasks`. Everything in steps 0-4 and 6 works without it.

## Step 6: the CLI (`swarm-cli`)

Add to `swarm-cli/Cargo.toml`: `toml = { workspace = true }`, `serde = { workspace = true }`; dev: `tempfile`.
Library code in `src/lib.rs` + `src/commands.rs` (testable), `src/main.rs` stays thin (`#[tokio::main]`).

```
swarm run <GOAL> [--provider mock|local|<name>] [--agents N] [--store DIR=.swarm] [--workspace DIR=./work]
                 [--config swarm.toml] [--approve-all] [--model M] [--base-url URL]
swarm status [RUN_ID]   swarm tasks [RUN_ID]   swarm agents [RUN_ID]   swarm logs [RUN_ID] [--follow]
```
- `RUN_ID` defaults to the latest run (`list_runs`, newest `created_at`).
- `run`: build `RunConfig` (from `--config` TOML if given, else `RunConfig::mock(goal, agents)`; `--provider local` swaps in
  `local_provider_config()` and sets `cfg.provider`; `zero_cost` stays true), create the workspace dir, open the store,
  `build_registry`, `ToolExecutor::new(SandboxConfig::new(workspace))`, `Runtime::create_run` + `drive`, print a one-line
  result (`run <id> completed: N tasks, X tokens`). Exit code 0 if `Completed`, 1 otherwise.
- `status`/`tasks`/`agents`/`logs`: read-only store queries printed as aligned plain text. `logs` prints
  `latest_events`; `--follow` polls `events_since` every 500 ms until the run is terminal.
- Unit-test command logic against a tempdir store with the mock provider (no subprocess needed).

Done when: `cargo test --workspace` passes **and** this works from the repo root:
```
cargo run -p swarm-cli -- run "say hello" --provider mock --store .swarm-test --workspace .swarm-test/work
cargo run -p swarm-cli -- status --store .swarm-test       # shows: completed
cargo run -p swarm-cli -- tasks  --store .swarm-test       # one completed task
```
(`--store` is accepted by every subcommand. `.swarm-test` is git-ignored: add it to `.gitignore`.)

## Local-model smoke test (manual, after step 6)

`swarm run "create hello.txt containing hello and verify it" --provider local --approve-all`. The llama-server runs
`--parallel 1` with reasoning on, so expect minutes, not seconds, and exactly one request in flight. Do not put this in
the test suite. If it fails, check first: tool-call JSON shape (step 0a), request timeout (600 s), `max_concurrency = 1`.
