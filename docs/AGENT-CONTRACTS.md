# Agent execution and verification contracts

The thirteen requested changes are implemented in the existing runtime. Browser-bridge
improvements are outside this work. The live Swift/F12 experiment, token savings, and
throughput measurements still need the actual deployment and benchmark task.

## Provider configuration and transport (1, 2, 7, 10)

`--tool-protocol auto|native|text`, `SWARM_TOOL_PROTOCOL`, or top-level TOML
`tool_protocol = "text"` selects the transport. Auto follows declared capabilities;
it does not probe the server. Jobs preserve the selected transport through worker
rounds and resume. Run/chat/resume and MCP accept the selection.

Generate a complete starting config with:

```sh
cargo run -p swarm-cli --example swift_pool_config > swift.toml
```

Replace the example model and URLs with the deployment. The relevant provider fields are:

```toml
[[providers]]
# Other provider fields are present in the generated complete config.
name = "swift"
max_concurrency = 4 # cap across this pool

[providers.openai]
min_output_tokens = 4096
chat_template_kwargs = { enable_thinking = false }
streaming = true
headers = { "X-Deployment" = "swift" }

[[providers.endpoints]]
base_url = "http://127.0.0.1:8080/v1"
max_concurrency = 2

[[providers.endpoints]]
base_url = "http://127.0.0.1:8081/v1"
max_concurrency = 2
```

The output-token floor applies after request/session/provider token selection. SSE
requests send an Accept header and request usage in both native and text mode. A
server returning JSON despite a streaming request is normalized too. No Python shim
is needed for these options. This does not prove that an arbitrary server honors them.

The pool rotates among available endpoints and enforces endpoint and pool capacities
with OS file locks shared across processes on the same host. A single configured URL
uses the same limiter. `SWARM_POOL_DIR` selects the shared lock directory (default:
the OS temporary directory). Processes sharing an endpoint must configure its capacity
consistently and use the same lock directory. Duplicate URLs are rejected. Locks release
on cancellation/process death; queue admission does not hold an endpoint lane. Pooling
does not add GPU capacity or coordinate different hosts. Failed requests follow the
runtime's bounded retry policy.

`job status/result` retain `tokens` and expose `total_tokens`. Provider-reported usage
is recorded; a missing/zero usage payload gets a character-based budget estimate and
`usage_estimated: true`. Inference events identify `usage_source`. Estimates are not
provider billing records. Usage is for the selected worker run; verification rounds
have separate stores and budgets.

## Reviews and evidence (3, 4)

Submit reviews using `--preset reviewer` (MCP: `preset: "reviewer"`). Reviewers are
read-only in purpose: they need not change files. They must inspect relevant files or
diffs, attempt checks, and call `report_result` with the usual status/summary plus:

```json
{
  "status": "success",
  "summary": "Found an independently reproducible defect",
  "verdict": "REQUEST_CHANGES",
  "blockers": [{
    "file": "src/parser.rs",
    "line": 42,
    "claim": "The empty-input case panics",
    "verification": "executed",
    "repro_command": "cargo test empty_input",
    "observed_output": "test empty_input ... FAILED"
  }],
  "checks": [{
    "command": "cargo test empty_input",
    "observed_output": "test empty_input ... FAILED"
  }]
}
```

Verdicts are `APPROVE`, `REQUEST_CHANGES`, or `INCONCLUSIVE`. Verification is `executed`,
`code_inspection`, or `unverified`; the latter two may use null reproduction fields.
An approval cannot contain blockers, and a request for changes needs a blocker.

The runtime rejects unsupported reviews and offers two correction attempts. The job
gate additionally enforces `--min-tool-calls` (default/minimum 2). Planning/reporting
do not count. File locations must exist within the workspace and have recorded
inspection. Every check and executed reproduction must match a recorded command and
nonempty output excerpt, including checks that ran and failed. Command exit codes are
persisted in tool-result events. A declared unverified finding, fabricated output,
missing check, or empty review yields `unverified_review`, never a trusted verdict.
An accepted completed review yields `review_verified` with `review_evidence` details.

This establishes evidence provenance, not the semantic truth of every claim. Code
inspection is explicitly distinct from experimental reproduction. Human/independent
review remains appropriate. Oversized outputs are capped; choose an excerpt the model
actually received and paginate source inspection when needed.

## Baselines, freshness and dispatch (5, 9, 12, 13)

Jobs default to the builder preset. Builders and test writers require `--verify`;
the command must run and fail before inference starts. Green checks, missing commands,
and timeouts are refused. Baseline command, exit code and output are retained in the
job spec/result. `--preset general` is the explicit exploration/no-red-gate mode;
reviewers instead satisfy the review evidence contract.

```sh
swarm job submit --config swift.toml --repo /repo --preset builder \
  --goal "Fix the failing empty-input case" --verify "cargo test empty_input"
swarm job submit --config swift.toml --repo /repo --preset reviewer \
  --goal "Independently review src/parser.rs and attempt cargo test empty_input"
```

Preset ceilings per worker run (explicit ceiling flags override these):

| Preset | Tokens | Steps | Default job priority |
| --- | ---: | ---: | ---: |
| builder | 14,000,000 | 260 | 0 |
| reviewer | 6,000,000 | 160 | 20 |
| test-writer | 8,000,000 | 200 | 10 |
| general | existing defaults | existing defaults | 0 |

`swarm run --preset` also applies role/budget defaults; the dispatch baseline policy
belongs to `swarm job submit`. Existing 60/80/95 percent budget-pressure steering remains.

Repository jobs must include the latest locally known `origin/main` when that ref
exists, or the explicitly selected `--freshness-ref`. The runner stores its observed
commit and compares it again when returning results: `base_drift` and `base_freshness`
show movement, or null when freshness cannot be established. This checks local refs;
fetch before dispatch if remote freshness is required. Patches always compare against
the immutable starting commit, excluding runtime caches; upstream movement is not
attributed to the worker.

`orchestrator/pre-dispatch.sh <owner/repo> <issue> --gate '<command>' --workspace <path>`
enforces the red baseline too, before its existing duplicate-PR checks. Reviews and
exploration use `--preset reviewer|general`.

All submitted/resumed jobs enter a shared queue in their jobs directory.
`--max-active-jobs` defaults to 4; use a consistent cap for that directory. Higher
`--priority` goes first, ties use submission age/id, and waiting jobs gain one priority
point per minute to prevent starvation. Running jobs are not preempted. Queued state
is visible in status and cancellation works while queued. Job admission and provider
inference capacity are independent limits. Verified jobs retain their existing bounded
worker/check retry loop.

## Repository context and resume (8, 11)

A cached map of tracked paths and selected symbol declarations is injected into the
system prompt. `repo_map_chars` defaults to 12,000; zero disables it. `repo_map_file`
can supply an existing map. The generated cache is `.swarm/repo-map.json`, refreshed
when tracked paths, file sizes or modification times change. It skips dependency/build
trees, binary/large files and symlinks. It is a navigation hint, not evidence, and its
token benefit still needs measurement on the target monorepo.

Task reclaim reuses the existing agent row, restoring its checkpointed conversation,
tool history, child task ids, step count and cumulative usage. Budget-death recovery
therefore continues the same transcript instead of creating a fresh agent. Jobs retain
provider config, protocol and sandbox selection. A direct `swarm resume` should supply
the original `--config`/overrides; verification-loop job resume retains its pre-existing
restriction and callers can submit a new job on the existing workspace.

## Command isolation (6)

The original push parser remains a convenience guardrail in host mode. To enforce an
OS boundary, supply `--sandbox-image <preinstalled-linux-image>` or
`tools.docker_image` in TOML. Shell/test/Python/git commands and verification commands
run in Docker with no network, no forwarded host credentials/environment/socket, a
read-only root, dropped capabilities, no new privileges, bounded memory/CPU/processes,
and a writable workspace mount. The image is never pulled implicitly and must contain
the required build/test tools. Python tools require python3. File tools reject symlink
escapes, and recursive traversal skips symlinks.

Isolated git uses private `.swarm/sandbox-git` metadata with no remotes or credential
helpers; source changes still appear in the orchestrator's patch. Push grants are
incompatible with this mode. The orchestrator owns remote writes. Container cleanup
runs on completion, timeout, and job cancellation; job labels permit targeted cleanup.
An abruptly killed standalone host may require removing its labeled/named containers.
Only the workspace is mounted, so provision dependencies in the image rather than
depending on host-global files. Secrets already stored in workspace files remain
workspace data; isolation does not redact them from the model.

## Validation

Automated coverage includes text-only streaming execution, native options/usage on the
wire, shared pool locks and release, false-review evidence, transcript-preserving
reclaim, red/green gates, upstream drift with immutable patches, queue priority/aging,
and repo-map cache invalidation. Run `cargo test --workspace`.

The Docker integration test is opt-in because it needs a daemon and a preinstalled
image. It has been run here with `python:3.12-slim` and checks workspace writes,
host-environment isolation, socket absence and blocked outbound connectivity:

```sh
cargo test -p swarm-tools docker_blocks_host --lib -- --ignored
```

Validation recorded on October 31, 2026: 655 workspace tests passed, with the one
Docker-dependent test excluded from the default suite and passed separately. The
pre-dispatch script passed bash syntax validation and rejected a green baseline.
