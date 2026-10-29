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

## Driving swarm from another agent

An orchestrating agent (Hermes, Claude Code, Codex) can launch workers in the
background and collect structured results, with `swarm job ...` (JSON on stdout) or
`swarm mcp` (the same operations as MCP tools). Each worker can get its own git
worktree and ends by reporting status, metrics and artifacts; the result carries a
patch. See [docs/ORCHESTRATION.md](docs/ORCHESTRATION.md) and the ready-made skill in
`skills/swarm-orchestrator/`.

```sh
swarm job submit --provider chatgpt-web --repo ./kernels --goal "Optimise matmul.cu with tiling; bench: ..."
swarm job wait job_... --timeout 300
swarm job result job_...
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
so a session (and an active `/goal`) survives restarts; `--new` starts fresh.

```
╭──────────────────────────────────────────────────────────╮
│ ✻ Welcome to swarm!                                      │
│                                                          │
│   /help for help, /status for your current setup         │
│                                                          │
│   cwd: /home/me/project/work                             │
│   chatgpt-web · auto · 3 agents · approvals ask_dangerous│
╰──────────────────────────────────────────────────────────╯

❯ make the login test pass
● Read(app/login.py)
  ⎿  Read 42 lines
● Update(app/login.py)
  ⎿  Updated app/login.py with 1 addition and 1 removal
      12 -     if user.password == given:
      12 +     if verify(user.password, given):
● Bash(pytest -q)
  ⎿  3 passed in 0.41s
● Fixed the check in `login.py`; the test passes.
✻ Cogitated for 26.4s · edited 1 file +1 -1 · read 1 file · ran 1 command · 2.7k tokens

────────────────────────────────────────────────────────────────
❯ Try "write tests for the parser"
────────────────────────────────────────────────────────────────
  ? for shortcuts                     chatgpt-web · auto · 2.7k tokens
```

Finished output flows into your terminal's own scrollback; only the spinner,
the prompt box and the footer are redrawn in place. Replies are rendered as
markdown, file edits show a numbered diff, and a permission prompt shows the
exact command or diff before anything runs:

```
╭──────────────────────────────────────────────────────────────╮
│ Bash command                                                 │
│                                                              │
│   rm -rf build                                               │
│                                                              │
│   tool `bash` may act outside the sandbox                    │
│                                                              │
│ Do you want to proceed?                                      │
│ ❯ 1. Yes                                                     │
│   2. Yes, and don't ask again for bash this session          │
│   3. No, deny this call (esc)                                │
╰──────────────────────────────────────────────────────────────╯
```

### Keys

| Key | Does |
| --- | --- |
| `enter` | send (a partial `/command` is completed first) |
| `shift+enter`, `ctrl+j`, `\` then `enter` | newline (`shift+enter` needs a terminal with the kitty keyboard protocol; the others work everywhere) |
| `esc` | stop the running turn; closes a menu first; pressed twice it clears a typed prompt (the draft stays in history), or on an empty prompt opens `/rewind` |
| `ctrl+c` | clear the prompt, else stop the turn, else press twice to exit |
| `ctrl+d` | delete forward, or exit on an empty prompt (press twice) |
| `shift+tab` | cycle permission mode: default → ask every tool → auto → plan |
| `up` / `down` | move through a multi-line prompt, then through history (saved across sessions) |
| `ctrl+r` | search prompt history |
| `tab` | complete a `/command` or `@file`; queue what you typed while a turn runs |
| `ctrl+o` | toggle full transcript detail |
| `ctrl+g` | edit the prompt in `$VISUAL` / `$EDITOR` |
| `ctrl+l` | clear the screen |
| `ctrl+z` | suspend (`fg` resumes and repaints) |
| `?` on an empty prompt | shortcut sheet |
| `ctrl+a/e/b/f/k/u/w/y`, `alt+b/f/d`, `ctrl+_` | emacs line editing, kill ring, undo |

Pasting a large block collapses it to `[Pasted text #1 +41 lines]` in the prompt;
the whole text is sent. Typing `/` opens a command menu, `@` a file menu.

### Prompt prefixes

* `@path` attaches a workspace file (or lists a folder) to the request. Paths
  stay inside the workspace, and secrets such as `.env` or `*.pem` are refused.
* `!command` runs a shell command in the workspace yourself, not through the
  agent, and keeps its output as context for the next goal.
* `#note` saves a line to `AGENTS.md`, which every agent reads.

### Commands

| Session | Work | Setup | Inspect |
| --- | --- | --- | --- |
| `/help [filter]` | `/goal [text\|status\|pause\|resume\|clear]` | `/model [name]` | `/status [run]` |
| `/clear` `/new` | `/subgoal [text\|rm n\|clear]` | `/permissions [ask\|all\|auto\|plan]` | `/cost` |
| `/compact` | `/plan <task>` | `/allow [tool]` `/allow-all` | `/context` |
| `/resume [run]` | `/review [what]` `/security-review [what]` | `/verbose` `/focus` | `/todos` |
| `/rewind [n] [code\|chat\|both]` | `/loop [5m <prompt>\|stop]` | `/redraw` | `/tasks [run]` `/logs [run]` |
| `/undo [n]` `/retry` | `/init` | `/reload` | `/runs` `/history` |
| `/btw <question>` | `/queue [list\|rm n\|clear]` | `/memory [edit]` | `/hooks` |
| `/copy` `/export [file]` | `/busy [queue\|interrupt]` | `/config` | `/tools` `/diff` `/doctor` |
| `/exit` | `/agents [n]` | | |

An unambiguous prefix works (`/comp`), and a typo gets a suggestion
(`/hepl` → did you mean `/help`).

* **`/goal <condition>`** keeps working across turns until the condition is met.
  After each turn the configured model is asked, in one JSON line, whether the
  goal is done, should continue, or is blocked. It pauses on `blocked`, after
  20 turns, on a failed or interrupted turn, or if the judge gives no usable
  answer three times; `/goal resume` restarts the budget. Anything you queue
  takes priority over the next continuation.
* **`/plan <task>`** (or `shift+tab` to plan mode) lets the agent read, list and
  search but refuses every write and command, then asks whether to proceed.
* **While a turn runs**, what you type is queued and shown above the prompt;
  `up` takes the newest queued message back to edit, `/busy interrupt` makes
  `enter` stop the turn and send the message instead.
* **`/undo`** takes the last request out of the conversation and puts it back in
  the prompt; **`/retry`** sends it again. The files the agent changed are not
  reverted, only what the session remembers.
* **`/copy`** puts the last result on the clipboard with an OSC 52 escape, so it
  works over SSH and inside tmux.

For scripts and CI, `swarm chat --oneshot` reads one goal from stdin and exits,
and piped stdin feeds one input per line in order (a line starting with `/` is a
command there too). The final result is printed to stdout. Colours are dropped
for pipes, `NO_COLOR` and `TERM=dumb`; `SWARM_UI=always` forces them and
`SWARM_UI=never` disables them. `SWARM_KEYBOARD=legacy` skips the keyboard
protocol query for terminals that mishandle it.

### Rewind

`/rewind` (or `esc esc` on an empty prompt) goes back to before one of your
requests: pick the request, then whether to restore the code, the conversation,
or both. Every file the agent wrote or edited is recorded with what it held
before, so restoring puts those files back, and removes files the agent created.
A file you (or a shell command) changed since the agent wrote it is left alone
and listed, never overwritten. Shell commands the agent ran are not undone.
Without a terminal: `/rewind 2 code` goes back two requests, files only
(`code`, `chat` or `both`, default both). The request comes back in the prompt.
A turn you interrupted counts too, since it may already have edited files.

### The agent's checklist

For work with several steps the agent keeps a checklist (`todo_write`), drawn as
it changes: `☒` done, `◐` in progress (the status line names it while it
runs), `☐` to do. `/todos` shows the current list. It is carried into your next
request, so the agent picks up where it left off. Nothing needs approval for it.

### Custom commands

A markdown file is a command: `.swarm/commands/fix-tests.md` becomes
`/fix-tests`, and a folder is a namespace (`review/api.md` is `/review:api`).
Looked up in `<store>/commands` (default `./.swarm/commands`),
`<workspace>/.swarm/commands`, `<workspace>/.claude/commands` (so a project's
Claude Code commands work), `~/.swarm/commands` and `~/.claude/commands`, in
that order. They appear in the `/` menu and `/help`, marked `(project)` or
`(user)`.

```markdown
---
description: Fix a failing test
argument-hint: <test name>
---
Run `$1`, find why it fails, fix the code (not the test), and run it again.
Anything else I typed: $ARGUMENTS
```

`$ARGUMENTS` is everything after the command, `$1`..`$9` its words (quote a
phrase to keep it together), and `@path` attaches a file. With none of them in
the body, the arguments are appended. The file is read each time you use it, so
edits apply at once; `/reload` picks up new files. A built-in command always
wins a name clash.

### Hooks

Shell commands that run at fixed points, as in Claude Code. In
`~/.swarm/settings.json` or `.swarm/settings.json`:

```json
{ "hooks": {
    "PreToolUse":  [{ "matcher": "bash|write_file",
                      "hooks": [{ "type": "command", "command": "./check.sh", "timeout": 10 }] }],
    "PostToolUse": [{ "matcher": "Edit", "hooks": [{ "type": "command", "command": "cargo fmt" }] }],
    "UserPromptSubmit": [{ "hooks": [{ "type": "command", "command": "git branch --show-current" }] }],
    "Stop": [{ "hooks": [{ "type": "command", "command": "notify-send 'swarm is done'" }] }]
} }
```

The command receives the event as JSON on stdin (`hook_event_name`,
`session_id`, `cwd`, and `tool_name` / `tool_input` / `tool_response` or
`prompt`). Exit `0` passes; exit `2` blocks, with stderr as the reason (a
blocked tool call never reaches the approval prompt, and the model is told why;
after a tool call, stderr is passed to the model as feedback); any other exit is
noted and ignored. It may also print `{"decision": "block", "reason": "…"}`.
For `UserPromptSubmit`, stdout is added to your prompt as context. A `matcher`
is tool names separated by `|` (swarm's or Claude Code's, so `bash` and `Bash`
both work), or `*` for all. `Stop` runs once after a request and its `/goal`
turns. `/hooks` lists what is in force.

A project's own `settings.json` can come from whoever wrote the project, so its
hooks run only after you approve the exact commands in a dialog at startup.
The approval is stored in `~/.swarm/trusted-hooks` against the file's path and
content, so changing a command asks again. Without a terminal, project hooks are
skipped.

### Also

* `/btw <question>` answers from the conversation so far without joining it,
  and works while a turn is running (one model call, no tools).
* `/subgoal <condition>` adds a condition the active `/goal` must also meet; the
  judge and every continuation prompt include it. `/subgoal rm 2`, `/subgoal clear`.
* `/loop 5m <prompt>` runs a prompt every 5 minutes (`10s`..`24h`) while the
  session is idle, up to 50 times, and stops itself if a run fails or is
  interrupted. What you type always goes first. The prompt may be a `/command`.
* `/focus` hides tool activity that went well, leaving your prompts, the
  answers, failures and denials. The footer's file counts stay right.
* `/review [what]` and `/security-review [what]` ask for a read-only review of
  the uncommitted changes (or what you name), with findings by severity.
* `/vim` turns on vim keys in the prompt (`SWARM_VIM=1` starts with them):
  `esc` for normal mode, `i a I A o O` to insert, `h l w b e 0 ^ $` and
  counts to move, `d c y` with a motion (`dw`, `c$`, `3dd`, `yy`), `x X D C s S`,
  `p P`, `u`. `enter` still sends, and `esc` in normal mode keeps its usual
  jobs (stop a turn, clear, rewind). The mode shows in the footer.
* `/redraw` repaints the prompt area and takes the terminal's input modes back,
  if something scrambled the screen. It works while a turn runs.

### How it stays unbroken

The terminal has exactly one owner: a UI task (`crates/swarm-cli/src/ui`). The
engine, the transcript tail and the permission prompt never print; they send
messages to it, so a spinner, a dialog and streaming output can no longer fight
over a row. It keeps the live region at the bottom with relative cursor
arithmetic (no queries to the terminal), draws each frame inside a synchronized
update with the cursor hidden, and restores the terminal on exit, on panic, and
on SIGTERM/SIGINT/SIGHUP. Text from models and tools is sanitised, so an escape
sequence in a file the agent `cat`s cannot repaint or retitle your terminal.
Tool subprocesses get no stdin (they cannot steal your keystrokes) and are
killed when a turn is interrupted.

## Live view

`swarm run` uses the same terminal UI (spinner, permission dialog, Esc to stop)
and prints the transcript as the run happens:

```
● swarm
  provider chatgpt-web · model auto · 3 agents · approvals ask_dangerous
  goal create hello.txt containing hello and verify it
  workspace ./work · run run_1f2e…

● Write(hello.txt)
  ⎿  Wrote 1 line to hello.txt
      1 + hello
● Bash(cat hello.txt)
  ⎿  hello
● Created hello.txt and verified it contains `hello`.
✻ completed in 26.4s · 1 task · 2 inferences · 1 tool calls · 2671 tokens
```

`●` is an action, `⎿` its result, `✗` a failure. The transcript is rendered from
the persisted event log, so a run that was interrupted can be replayed later:

```sh
cargo run -p swarm-cli -- logs <run-id> --transcript
```

Flags: `--plain` prints only the final one-line summary, and `--verbose` adds
low-signal rows (swarm's own tool names and arguments, agent states,
checkpoints, per-step usage).


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
