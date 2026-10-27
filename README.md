# swarm

A Rust workspace for orchestrating swarms of LLM agents: scheduling tasks, tracking budgets and usage, and persisting runs, events, findings, and artifacts.

> Status: early development. The CLI runs goals end to end and can decompose
> multi-agent runs. A read-only terminal dashboard shows live run state.

## Crates

| Crate | Purpose |
| --- | --- |
| `swarm-core` | Shared types: ids, agents, tasks, runs, events, messages, budgets, config, provider/tool traits |
| `swarm-store` | SQLite persistence (sqlx) with migrations for runs, tasks, agents, events, artifacts, findings, hypotheses, usage |
| `swarm-providers` | LLM provider abstraction, OpenAI client, browser-chat provider (ChatGPT and Gemini web bridges), mock provider, pricing, quota, registry |
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

## Interactive session

Plain `swarm` (no subcommand) on a terminal opens a Claude Code-style session.
`swarm chat` (alias `c`) is the same thing with arguments:

```sh
cargo run -p swarm-cli --                       # session on ./.swarm + ./work
cargo run -p swarm-cli -- chat "add a login page" --provider chatgpt-web
cargo run -p swarm-cli -- chat --continue       # keep the last run's result as context
cargo run -p swarm-cli -- chat --resume run_1f2e…
```

Every goal you type is a normal durable run, so `status`, `tasks`, `logs`,
`--transcript`, budgets, checkpoints and approvals all keep working; the session
is the layer on top. It is stored under the `chat` namespace of the same store,
so a session survives restarts (`--new` starts a fresh one).

```
⏺ swarm
  provider chatgpt-web · model auto · 1 agent · approvals on-write
  session chat-1f2e… · turn 3 · 41.2k tokens · 12 inferences · 9 tool calls

❯ add a login page
⏺ [a1 t1] write_file("app/login.py")
  ⎿ wrote 42 lines to app/login.py
  ⎿ step 1 · 4.2s · 1.2k → 96 tokens · auto
✻ writing · 26s · ↓ 2.7k tokens
✻ completed in 26.4s · 1 task · 2 inferences · 1 tool calls · 2.7k tokens
❯
```

The renderer (`transcript.rs`) and its palette (`style.rs`) follow Claude Code:
the `⏺` action bullet in terracotta on a light cell, tool results as `⎿` preview
lines at a four-space gutter, `+N` for what was cut, `↓ 1.2k` token counts, the
`✻` working spinner, and a bordered permission panel:

```
╭─ ⚠  Allow bash? ─────────────────────────────╮
│ bash({"command": "rm -rf build"})            │
│                                              │
│ ❯ 1. Yes, allow once                         │
│   2. Yes, allow for this session             │
│   3. No, deny this call                      │
│                                              │
│ runs a shell command in the workspace        │
╰──────────────────────────────────────────────╯
  1/2/3 to choose · y/a/n shorthand · Esc denies
```

Colours are truecolor when the terminal offers them and are dropped for pipes,
`NO_COLOR`, `TERM=dumb` and `SWARM_UI=never`; `SWARM_UI=always` forces them for
captures. `ctrl+o` widens previews.

Keys: `enter` sends, `\` then `enter` inserts a newline, `up`/`down` and
`ctrl+r` walk the prompt history, `tab` completes slash commands,
`ctrl+a/e/b/f/k/u/w` do line editing, `ctrl+o` toggles transcript detail,
`ctrl+l` redraws, `ctrl+c` interrupts the running turn (the run stays in the
store and the message tells you how to resume it), `ctrl+d` leaves.

Slash commands: `/help`, `/status [run]`, `/tasks [run]`, `/logs [run]`,
`/runs`, `/history`, `/cost`, `/tools`, `/copy`, `/model <name>`,
`/agents <n>`, `/approve ask|danger|all`, `/allow [tool]`, `/allow-all`,
`/compact`, `/clear`, `/new`, `/exit`.

Goals typed while a turn is running are queued and run next, so you can steer
the session without waiting. Approvals print the exact call with its risk and
accept `y` (once), `n` (deny) or `a` (allow that tool for the rest of the
session, the same list `/allow` shows). `/approve all` and `/allow-all` are
per-session; nothing is written to disk.

For scripts and CI, `swarm chat --oneshot` reads one goal from stdin and exits,
and piped stdin feeds one input per line in order — a line starting with `/` is
a command there too, everything else is a goal.

## Live view

`run` prints a Claude Code-style transcript of the run as it happens:

```
⏺ swarm
  provider chatgpt-web · model auto · 3 agents · approvals on-write
  goal create hello.txt containing hello and verify it
  workspace ./work · run run_1f2e…

⏺ [a1 t1] asks for write_file
  ⎿ step 1 · 4.2s · 1204 → 96 tokens · gpt-5.1
⏺ [a1 t1] write_file(content=hello, path=hello.txt)
  ⎿ wrote 5 byte(s) to hello.txt
⏺ [a1 t1]
  Created hello.txt and verified it contains `hello`.
  ⎿ step 2 · 3.1s · 1330 → 41 tokens
⏺ [t1] result
  Created hello.txt and verified it contains `hello`.
✻ completed in 26.4s · 1 task · 2 inferences · 1 tool calls · 2671 tokens
```

`⏺` is an action, `⎿` its result, `✗` a failure, and a braille spinner with the
elapsed time, token count and current phase runs on the status row. A long
assistant reply streams line by line; tool output shows its first line.
The transcript is rendered from the persisted event log, so a run that was
interrupted can be replayed after the fact:

```sh
cargo run -p swarm-cli -- logs <run-id> --transcript
```

Flags: `--plain` prints only the final one-line summary, `--verbose` adds
low-signal rows (agent states, checkpoints, longer text), and `--json`
suppresses the transcript entirely. Colours are off for pipes, when
`NO_COLOR` is set, and under `TERM=dumb`. Force the live view in a pipe or CI
with `SWARM_UI=always` (or disable it with `SWARM_UI=never`).


On this workstation, the Qwen server can be started with
`C:\Users\fortn\llama.cpp\start-qwen38-27b.ps1`. Its declared alias is
`qwen3.8-27b-hauhau`, matching the `local` preset.

The `local` preset uses one concurrent request and a 600-second HTTP timeout.
Use `--model` and `--base-url` to override its defaults. A named provider
other than `mock`, `local`, `chatgpt-web`, or `gemini-web` must be defined in a TOML file passed with
`--config`. Runs using paid providers must explicitly disable the zero-cost
policy in that configuration.

To use ChatGPT web chat as Swarm's reasoning model, start the local Chrome
bridge and sign in, then run:

```sh
cargo run -p swarm-cli -- run "create hello.txt and verify it" --provider chatgpt-web --agents 3
```

The `chatgpt-web` preset uses the bridge at `127.0.0.1:8766` and leaves the
model at the account's own pick; pass `--model` to select one from ChatGPT's
model picker. Each session gets its own conversation and the bridge deletes it
when the session closes, so runs do not pile up in your chat history
(`--keep-chats` keeps them while debugging). The conversation API is not
available to every account, so run the bridge with `--no-api` for live use: it
reads replies from the rendered turns and reports token usage as zero. Setup,
model naming, sign-in through your own Chrome, concurrency across signed-in
profiles, rate limits, and troubleshooting are in
[ChatGPT web bridge](docs/CHATGPT_WEB.md). The bridge is verified offline against
a fixture site (`bridges/chatgpt-web/fixture.mjs`) and end to end with
`node e2e.mjs`.

Gemini web chat is the same bridge pattern on port `8765`
(`--provider gemini-web`, [docs/GEMINI_WEB.md](docs/GEMINI_WEB.md)).

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
