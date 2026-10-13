# swarm

A Rust workspace for orchestrating swarms of LLM agents: scheduling tasks, tracking budgets and usage, and persisting runs, events, findings, and artifacts.

> Status: early development. The CLI runs goals end to end and can decompose
> multi-agent runs. A read-only terminal dashboard shows live run state.

## Crates

| Crate | Purpose |
| --- | --- |
| `swarm-core` | Shared types: ids, agents, tasks, runs, events, messages, budgets, config, provider/tool traits |
| `swarm-store` | SQLite persistence (sqlx) with migrations for runs, tasks, agents, events, artifacts, findings, hypotheses, usage |
| `swarm-providers` | LLM provider abstraction, OpenAI client, mock provider, pricing, quota, registry |
| `swarm-scheduler` | Task scheduling |
| `swarm-tools` | Tool implementations available to agents |
| `swarm-runtime` | Agent runtime tying scheduler, providers, tools, and store together |
| `swarm-cli` | Command-line interface |

## Building

Requires Rust 1.94+ (edition 2024).

```sh
cargo build
cargo test
```

## Quick start

From the repository root, run a fully offline mock task and inspect its
persisted result:

```sh
cargo run -p swarm-cli -- run "say hello" --provider mock
cargo run -p swarm-cli -- status
cargo run -p swarm-cli -- tasks
cargo run -p swarm-cli -- agents
cargo run -p swarm-cli -- logs
cargo run -p swarm-cli -- tui
```

`run` writes to `.swarm/` by default and gives agents a `./work` workspace.
Use `--store DIR` on every command to choose another store. Inspection
commands accept an optional run ID; otherwise they show the latest run.
`tui` requires an interactive terminal, refreshes twice per second, and shows
run usage, tasks, agents, and recent events. Press Tab or the arrow keys to
switch runs, `r` to refresh, and `q` to quit. Pass a run ID to keep it focused
on one run.
Pass `--agents N` with `N > 1` to have one planner create up to `N - 1`
worker tasks. The planner can declare dependencies between tasks so a worker
waits for prerequisite work to finish. The mock provider simulates their
replies; a real provider can execute the workers' tool calls.

To use a running OpenAI-compatible llama-server on `127.0.0.1:8080`:

```sh
cargo run -p swarm-cli -- run "create hello.txt containing hello and verify it" --provider local --approve-all
```

The `local` preset uses one concurrent request and a 600-second HTTP timeout.
Use `--model` and `--base-url` to override its defaults. A named provider
other than `mock` or `local` must be defined in a TOML file passed with
`--config`. Runs using paid providers must explicitly disable the zero-cost
policy in that configuration.
For a managed-agent provider, set `kind = "managed_agent"`, a `command` array,
and optionally `workspace_dir` and `timeout_secs` in its provider entry.
The CLI uses `--workspace` when `workspace_dir` is omitted. Swarm sends the
task prompt to the process on stdin, or replaces `{prompt}` in a command
argument. The external agent runs its own tools; Swarm captures stdout as its
result. Managed agents that do not report token usage show zero tokens.
To generate a complete transport test config, run
`cargo run -q -p swarm-cli --example managed_echo_config > swarm.toml`.
That example only echoes the prompt; replace its `command` with your agent CLI
before using it for actual work.
The local preset has been smoke-tested with the Qwen3.8 27B HauhauCS IQ3_M
model through llama-server, including a file write and read-back verification.

By default, tools marked as needing approval prompt in the terminal with the
tool name, arguments, and risk. Only `y` approves that exact call; all other
answers deny it. In a noninteractive session the call is denied. Set
`tools.approval = "ask_all"` in a TOML config to prompt for every tool. Use
`--approve-all` only for a workspace where the agent may run those tools.

Local state (`.swarm/`, `*.db`) and build output (`target/`) are git-ignored.
