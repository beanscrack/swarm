# Orchestrating swarm from another agent

An agent such as Hermes, Claude Code or Codex can use swarm as a pool of workers:
launch several in the background, carry on, then collect structured results. The
worker is a swarm agent with file, shell, python and git tools. The orchestrator
decides what to try, compares what comes back, and keeps the best.

There are two interfaces to the same thing: `swarm job ...` (a shell command that
prints JSON) and `swarm mcp` (the same operations as MCP tools). Pick whichever the
orchestrator speaks; both are described below.

## The model

A **job** is a detached `swarm run` with a directory of its own:

```text
<jobs>/<id>/job.json     what was asked, and the process doing it
<jobs>/<id>/store/       the run's durable store (events, checkpoints)
<jobs>/<id>/run.log      the process output
<jobs>/<id>/work/        the workspace; a git worktree for --repo jobs
<jobs>/<id>/patch.diff   written by `result` for --repo jobs
```

`<jobs>` is `--jobs`, else `$SWARM_JOBS`, else `./.swarm-jobs`.

A worker ends by calling the **`report_result`** tool: a `status` (`success`,
`partial`, `failed`), a `summary`, numeric `metrics` (for example `speedup`,
`latency_us`, `tests_passed`) and `artifacts` (workspace files worth keeping). The
instruction to do so is appended to every goal (turn it off with
`--no-report-footer`). The call is validated, so a malformed report is rejected with
the reason and the worker fixes it. `swarm job result` reads the last accepted one
back, which is what lets an orchestrator compare workers by number instead of by
prose.

**Isolation.** `--repo PATH` gives the job its own git worktree and branch
(`swarm/<id>`), started from `--base` (default `HEAD`). Parallel workers on one
repository never touch each other's files, and `result` includes a patch against the
starting commit. `clean` removes the worktree and branch and keeps the patch.

## CLI

Every verb prints JSON. Exit status is 0 on success; for `wait`, 0 means every job
completed.

| Command | What it does |
| --- | --- |
| `swarm job submit --goal TEXT [--repo PATH] [--base REF] [--label L] [--key K] [--provider P] [--max-steps N]` | Starts a job, returns `{job_id, state, workspace, branch}` immediately. `--key` makes a repeat submit return the same job. `--workspace DIR` works in an existing directory instead. `--goal-file F` reads the goal from a file. |
| `swarm job status ID` | `state` (`starting`, `running`, `completed`, `failed`, `cancelled`, `interrupted`), elapsed seconds, model and tool calls, last action, and whether it has reported. |
| `swarm job wait ID... [--timeout S] [--any]` | Blocks until all jobs finish (first one with `--any`) or the timeout. Returns `done`, `timed_out`, `still_running` and every job's status. |
| `swarm job result ID` | `report` (the worker's own status, summary, metrics, artifacts, notes), `summary` (its final message), `files_changed`, and for repo jobs `patch` (`path`, `stat`, `apply`). |
| `swarm job cancel ID` | Stops the job's process tree. A finished job is left alone. |
| `swarm job list [--label L] [--state S]` | One status per job. |
| `swarm job resume ID` | Continues an `interrupted` job (its process died mid-run) with the same workspace and provider. Any other state is refused. |
| `swarm job clean ID` | Removes a repo job's worktree and branch; the patch stays. |

`--provider` and `--base-url` default to `$SWARM_PROVIDER` and `$SWARM_BASE_URL`, so
an orchestrator sets them once. A job started with no provider is refused rather
than silently run on the offline mock.

## MCP

`swarm mcp [--jobs DIR]` speaks MCP over stdio (newline-delimited JSON-RPC) and
exposes `swarm_submit`, `swarm_status`, `swarm_wait`, `swarm_result`, `swarm_cancel`,
`swarm_list`, `swarm_resume` and `swarm_clean`, with the same arguments and JSON results. A single
`swarm_wait` blocks at most 45 seconds, because MCP hosts time tool calls out; a
longer wait is several calls, and a timed-out call says so.

Register it with Hermes:

```powershell
hermes mcp add swarm `
  --env SWARM_PROVIDER=chatgpt-web SWARM_BASE_URL=http://127.0.0.1:8766 SWARM_JOBS=C:\swarm-jobs `
  --command C:\path\to\swarm.exe --args mcp
hermes mcp test swarm
```

Claude Code: `claude mcp add swarm -- C:\path\to\swarm.exe mcp`. Any MCP client works
with the command `swarm mcp`.

A ready-made skill for the orchestrator, with the loop below written out, is in
`skills/swarm-orchestrator/SKILL.md`. Copy it into the orchestrator's skills
directory.

## The orchestration loop

1. **Baseline.** Measure the starting point yourself and write the number down.
2. **Fan out.** One job per independent idea, each with `--repo`, the same
   verification command, and a different strategy named in the goal.
3. **Collect.** `wait` (with `--any` to start on the first result while the rest
   run), then `result` for each.
4. **Verify.** Do not trust a self-reported metric. Apply the best patch to your own
   checkout and re-run the benchmark and the correctness check there.
5. **Iterate.** Commit the winner, then fan out again with `--base` at the new commit.
6. **Clean up.** `clean` the jobs you are done with.

### A goal that works

A worker knows only the goal. State the task, the exact commands, and the bar:

```text
Optimise the matmul kernel in kernels/matmul.cu. Strategy: tile through shared memory
with 32x32 blocks. Correctness: `python check.py` must print PASS. Speed: `python
bench.py` prints `ms=<number>`; the baseline is 4.20. Work only in this repository.
Report the best `ms` you reach as the metric speedup (baseline divided by yours), and
list the patch file you made as an artifact.
```

## Things that matter

- **Workers run commands without asking.** Jobs start with `--approve-all`, inside
  their own worktree. That is the point of isolation, and also why the goal should
  name what is in bounds. A worker is not sandboxed from the rest of the machine.
- **Timing on a shared GPU is noisy.** Workers that benchmark at the same time slow
  each other down. Let workers implement and check correctness in parallel, and
  take the final timings yourself, one candidate at a time.
- **Parallelism is bounded by the provider.** Through the ChatGPT bridge every
  worker shares one account and a handful of tabs; see `docs/CHATGPT_WEB.md` for
  access points and `SWARM_BROWSER_CONCURRENCY`. A worker takes about 12 seconds per
  step, so budget with `--max-steps` and give them concrete goals.
- **Interrupted jobs.** A job whose process died before the run finished reports
  `interrupted`; `swarm job resume ID` continues it from its saved progress.
