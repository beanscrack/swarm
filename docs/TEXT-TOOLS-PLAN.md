# Text tool protocol: turning tool-call-free endpoints into agents

Status: prototype, phase 1 landed (parser + prompt, `crates/swarm-providers/src/text_tools.rs`).
Wiring is phases 2-3.

## The gap

swarm's agent loop is Hermes-shaped: it sends the OpenAI `tools` array on every request
(`swarm-runtime/src/agent.rs:430`) and parses `tool_calls` off the response. Any endpoint
that supports native function calling becomes an agent immediately - that is how the local
`swift` workers ran 36-162 real tool calls each.

Two endpoint classes break that:

1. **Tool-call-free servers** (small local models, quantized builds, servers with
   `--enable-auto-tool-choice` off). They accept the request and answer in prose. swarm's
   format-retry loop treats every non-call as a defect and kills the run at
   `max_format_retries`.
2. **Chat-UI bridges** (`chatgpt-web`). There is no `tool_calls` on that channel at all: the
   bridge shovels text through a web UI, the model does not know it is in a loop. Measured
   on Coalign #342: 1 tool call (a to-do list), 0 tests, `unverified_no_work` after the
   outcome fix landed.

swarm today handles class 1 by nagging ("your last call was not a tool call", `agent.rs:281-292`)
and dying. That is a repair layer pretending to be a protocol.

## Design

A first-class text protocol, selected by endpoint capability, not by retries:

    endpoint capability           protocol
    -----------------------       ------------------------------------
    native tool_calls             tools array (current path, unchanged)
    no tool_calls, real server    TEXT_PROTOCOL: fenced-JSON, tolerant parser
    chat-UI bridge                TEXT_PROTOCOL, with the honesty note below

- `TEXT_PROTOCOL` appends a compact instruction block to the system prompt: reply with
  exactly one fenced JSON object `{"tool": "...", "arguments": {...}}`, one call per reply.
- `parse_tool_calls` is deliberately forgiving: fenced blocks, bare objects, multiple blocks,
  key aliases (`tool`/`name`/`function`, `arguments`/`params`/`input`/`args`), trailing commas,
  unterminated fences, prose around the block. It returns a `reason` for every rejection, so
  the retry prompt can name the defect instead of saying "malformed".
- Capability probe: send one tiny request with `tools` set; if the response carries
  `tool_calls`, the endpoint is native. Cache per endpoint; `SWARM_TOOLS_PROTOCOL=auto|native|text`
  overrides.

## What this hopes to achieve

1. **Class 1 endpoints become usable agents without a bespoke proxy.** The vLLM
   `enable_thinking` shim is an endpoint-specific hack; this is the general mechanism. The
   test for success: a mock provider that can only emit text completes a tool-driven task
   (read a file, report a result) with zero format-retry deaths.
2. **Retry deaths become rare and diagnosable.** Today: "format retries exhausted". After:
   each rejection carries a reason, and the count of rejections per run is visible next to
   `worker_tool_calls`.
3. **A measurable comparison of engines on the same loop.** Same brief, same gate, three
   engines (`swift` native, a text-protocol-only model, the bridge). The metric is
   `worker_tool_calls` and gate outcome, not prose.
4. **Honest scope on chat-UI bridges.** The parser gives a bridge one real chance to call
   tools per turn; it does not make a web UI into an agent runtime. The model on the far side
   has no memory of the loop, no tool result plumbing guarantee, and its own connector
   affordances. Expected best case for the bridge: partial, verified by tool-call count, not
   by its summary. This is why the outcome field `unverified_no_work` matters more than the
   protocol.

Non-goals: no new tool surface, no change to native endpoints, no sandbox/permission change.

## Phases

- [x] P1 `text_tools.rs`: prompt builder + tolerant parser + reason codes + unit tests
      (self-contained, depends only on `serde_json`).
- [ ] P2 wire into `swarm-providers`: capability probe, `tools_protocol` on `ProviderConfig`
      (`auto|native|text`), response mapping `ParsedCall -> ToolCall`.
- [ ] P3 agent loop: text path bypasses `format_retries`; rejection reasons into the retry
      prompt; `usage.text_protocol_rejections` surfaced in `job status`.
- [ ] P4 test matrix: mock provider emitting only prose; a real tool-call-free local model if
      one is loaded; the bridge as a documented negative control.

## Measurement

Per run, report: tool calls, text-protocol rejections, gate outcome, `verify.worker_tool_calls`.
Success is a tool-call-free server finishing a gated task with `outcome: verified` and
rejections per accepted call below ~0.2.
