# swarm

A Rust workspace for orchestrating swarms of LLM agents: scheduling tasks, tracking budgets and usage, and persisting runs, events, findings, and artifacts.

> Status: early development. The CLI runs goals end to end; automatic task
> decomposition and a terminal UI are still planned.

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
```

`run` writes to `.swarm/` by default and gives agents a `./work` workspace.
Use `--store DIR` on every command to choose another store. Inspection
commands accept an optional run ID; otherwise they show the latest run.

To use a running OpenAI-compatible llama-server on `127.0.0.1:8080`:

```sh
cargo run -p swarm-cli -- run "create hello.txt containing hello and verify it" --provider local --approve-all
```

The `local` preset uses one concurrent request and a 600-second HTTP timeout.
Use `--model` and `--base-url` to override its defaults. A named provider
other than `mock` or `local` must be defined in a TOML file passed with
`--config`. Runs using paid providers must explicitly disable the zero-cost
policy in that configuration.

By default, tools marked as needing approval return an approval-required
message to the agent; interactive approval is not yet supported. Use
`--approve-all` only for a workspace where the agent may run those tools.

Local state (`.swarm/`, `*.db`) and build output (`target/`) are git-ignored.
