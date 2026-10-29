---
name: swarm-orchestrator
description: "Run swarm coding workers in parallel and pick the best result: fan out variants, collect structured reports, verify the winner. For kernel optimisation, refactors, and any task with several independent attempts."
version: 1.0.0
license: MIT
metadata:
  hermes:
    tags: [swarm, orchestration, parallel-agents, optimization]
prerequisites:
  commands: [swarm]
---

# Orchestrating swarm workers

Swarm workers are coding agents with file, shell, python and git tools. You launch
them in the background, keep working, and read back a structured report from each.
Use them when a task splits into independent attempts, such as several optimisation
strategies for one kernel.

Tools (MCP: `swarm_*`; shell: `swarm job ...`):

- `swarm_submit` starts a worker and returns a `job_id` at once.
- `swarm_wait` waits for jobs (`any: true` returns on the first to finish).
- `swarm_result` returns the worker's `report` (status, summary, numeric `metrics`,
  `artifacts`), `files_changed`, and for repo jobs a `patch`.
- `swarm_status`, `swarm_list`, `swarm_cancel`, `swarm_resume`, `swarm_clean` do what they say.

## The loop

1. **Measure the baseline yourself** and write the number down. Everything is
   compared against it.
2. **Fan out.** One `swarm_submit` per independent idea, all with the same `repo`
   (each gets its own worktree and branch, so they cannot collide) and a `label`
   for the experiment. Use 3 to 6 workers; more only slows each of them down.
3. **Write the goal as if for a stranger.** It must contain: the file or area, the
   one strategy this worker should try, the exact correctness command and what a
   pass looks like, the exact benchmark command and how to read its number, the
   baseline, and what to put in `metrics`. A vague goal gets a vague result.
4. **Wait.** `swarm_wait` with the job ids; call it again on a timeout. With
   `any: true` you can read the first finished result while the others run.
5. **Read every result**, including failures: a `failed` or `partial` report with
   notes tells you what not to try again.
6. **Verify the winner yourself.** Apply its `patch` to your checkout, then run the
   correctness check and the benchmark there. Workers benchmark while other workers
   run on the same machine, so their numbers are noisy and self-reported; yours is
   the one that counts.
7. **Iterate.** Commit the winner, then fan out a new round with `base` set to the
   new commit and strategies that build on it. Stop when a round gains nothing.
8. **Clean up** with `swarm_clean` for finished jobs.

## Goal template

```text
Optimise <function> in <file>. Strategy: <the one idea for this worker>.
Correctness: `<command>` must <expected output>.
Speed: `<command>` prints <how to read the number>; the baseline is <number>.
Work only inside this repository. When finished call report_result with status,
a summary of what you changed and why it was faster or not, metrics {speedup:
<baseline / yours>, <other numbers worth comparing>}, and the patch as an artifact.
```

## Rules of thumb

- Give each worker a different strategy; ten copies of the same prompt find the same
  thing ten times. Typical axes for a GPU kernel: tiling and shared memory, memory
  coalescing and vector loads, occupancy and launch configuration, instruction-level
  tricks, fusing neighbouring kernels.
- Workers run commands without asking, so name what is in bounds in the goal.
- Use `key` when a submit might be retried, so it cannot start the same worker twice.
- A job that reports `interrupted` lost its process; `swarm_resume` continues it.
- A worker that never calls `report_result` still leaves `summary` and
  `files_changed`; read those instead, and tighten the goal next time.
- Read `outcome`, not `state`. A job can end `completed` having done nothing:
  `outcome` is `no_work` then (resubmit under a new `key`; a key stays with a failed
  job), or `changed_without_report` when it edited files without reporting.
