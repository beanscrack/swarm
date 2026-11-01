//! `swarm mcp`: the job layer as MCP tools over stdio, so an agent that speaks
//! MCP (Hermes, Claude Code, Codex, ...) can launch swarm workers the way it calls
//! any other tool, with no shell and no prose to parse.
//!
//! Transport: newline-delimited JSON-RPC 2.0 on stdin/stdout (the MCP stdio
//! transport). Only what a tool server needs is implemented: `initialize`,
//! `ping`, `tools/list`, `tools/call`, and the notifications a client may send.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use anyhow::Result;
use clap::Args;
use serde_json::{Value, json};

use crate::job::{self, SubmitArgs};

#[derive(Debug, Args)]
pub struct McpArgs {
    /// Where jobs live (default: $SWARM_JOBS, else ./.swarm-jobs).
    #[arg(long)]
    jobs: Option<PathBuf>,
}

/// The longest a single `swarm_wait` call blocks. MCP hosts time tool calls out,
/// so a longer wait is made of several calls, not one.
const MAX_WAIT_SECS: u64 = 45;

const INSTRUCTIONS: &str = "Swarm runs coding workers in the background. Launch each independent piece of \
work with swarm_submit (give every worker the goal, how to verify success, and a repo so it gets its own \
git worktree), keep working or launch more, then swarm_wait for them and read swarm_result: the worker's \
structured report (status, metrics, artifacts), its summary, and a patch for repo jobs. Compare results \
across workers by their metrics and apply the best patch yourself. Builder jobs (the default) and test-writer jobs require a verify command that fails before dispatch. Use preset reviewer for independent read-only reviews with structured verdict/blockers/checks, or preset general for exploration. Endpoint pools, protocol selection and native options can be supplied in a config TOML.";

pub async fn serve(args: McpArgs) -> Result<()> {
    let jobs = job::jobs_dir(args.jobs.as_deref());
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let reply = match serde_json::from_str::<Value>(&line) {
            Ok(message) => handle(&jobs, &message).await,
            Err(error) => Some(error_reply(Value::Null, -32700, &format!("parse error: {error}"))),
        };
        if let Some(reply) = reply {
            let mut out = stdout.lock();
            writeln!(out, "{reply}")?;
            out.flush()?;
        }
    }
    Ok(())
}

fn error_reply(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

/// One JSON-RPC message in, at most one reply out (notifications get none).
pub async fn handle(jobs: &Path, message: &Value) -> Option<Value> {
    let method = message.get("method").and_then(Value::as_str).unwrap_or("");
    let Some(id) = message.get("id").cloned() else {
        return None; // a notification: initialized, cancelled, ...
    };
    let result = match method {
        "initialize" => json!({
            "protocolVersion": message["params"]["protocolVersion"].as_str().unwrap_or("2024-11-05"),
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "swarm", "version": env!("CARGO_PKG_VERSION")},
            "instructions": INSTRUCTIONS,
        }),
        "ping" => json!({}),
        "tools/list" => json!({"tools": tool_list()}),
        "tools/call" => {
            let name = message["params"]["name"].as_str().unwrap_or("");
            let arguments = message["params"].get("arguments").cloned().unwrap_or_else(|| json!({}));
            match call_tool(jobs, name, &arguments).await {
                Ok(value) => tool_result(&value, false),
                Err(error) => tool_result(&json!({"error": format!("{error:#}")}), true),
            }
        }
        other => return Some(error_reply(id, -32601, &format!("method not found: {other}"))),
    };
    Some(json!({"jsonrpc": "2.0", "id": id, "result": result}))
}

fn tool_result(value: &Value, is_error: bool) -> Value {
    json!({
        "content": [{"type": "text", "text": serde_json::to_string_pretty(value).unwrap_or_default()}],
        "isError": is_error,
    })
}

fn text(arguments: &Value, key: &str) -> Option<String> {
    arguments.get(key).and_then(Value::as_str).map(str::to_string).filter(|s| !s.is_empty())
}

fn number(arguments: &Value, key: &str) -> Option<u64> {
    arguments.get(key).and_then(Value::as_u64)
}

fn strings(arguments: &Value, key: &str) -> Vec<String> {
    arguments
        .get(key)
        .and_then(Value::as_array)
        .map(|items| {
            items.iter().filter_map(Value::as_str).map(str::to_string).filter(|s| !s.is_empty()).collect()
        })
        .unwrap_or_default()
}

fn job_id(arguments: &Value) -> Result<String> {
    text(arguments, "job_id").ok_or_else(|| anyhow::anyhow!("job_id is required"))
}

async fn call_tool(jobs: &Path, name: &str, arguments: &Value) -> Result<Value> {
    match name {
        "swarm_submit" => job::submit_job(
            jobs,
            SubmitArgs {
                policy: serde_json::from_value(arguments.clone())?,
                config: text(arguments, "config").map(PathBuf::from),
                goal: text(arguments, "goal"),
                workspace: text(arguments, "workspace").map(PathBuf::from),
                repo: text(arguments, "repo").map(PathBuf::from),
                base: text(arguments, "base"),
                label: text(arguments, "label"),
                key: text(arguments, "key"),
                provider: text(arguments, "provider"),
                base_url: text(arguments, "base_url"),
                tool_protocol: text(arguments, "tool_protocol").map(|p| p.parse()).transpose()?,
                model: text(arguments, "model"),
                agents: number(arguments, "agents").map(|n| n as u32),
                max_steps: number(arguments, "max_steps").map(|n| n as u32),
                max_tokens: number(arguments, "max_tokens"),
                max_inference_calls: number(arguments, "max_inference_calls"),
                max_wall_clock_secs: number(arguments, "max_wall_clock_secs"),
                verify: text(arguments, "verify"),
                max_rounds: number(arguments, "max_rounds").map(|n| n as u32),
                verify_timeout_secs: number(arguments, "verify_timeout_secs"),
                allow_push: strings(arguments, "allow_push"),
                no_report_footer: arguments.get("no_report_footer").and_then(Value::as_bool).unwrap_or(false),
                ..Default::default()
            },
        ),
        "swarm_status" => job::job_status(jobs, &job_id(arguments)?).await,
        "swarm_wait" => {
            let ids: Vec<String> = arguments
                .get("job_ids")
                .and_then(Value::as_array)
                .map(|list| list.iter().filter_map(Value::as_str).map(str::to_string).collect())
                .unwrap_or_default();
            anyhow::ensure!(!ids.is_empty(), "job_ids is required");
            let timeout = number(arguments, "timeout_secs").unwrap_or(30).min(MAX_WAIT_SECS);
            let any = arguments.get("any").and_then(Value::as_bool).unwrap_or(false);
            let (mut value, _) = job::wait_jobs(jobs, &ids, timeout, any).await?;
            if value["timed_out"] == true {
                value["next"] = json!("not finished yet: call swarm_wait again, or swarm_status to see progress");
            }
            Ok(value)
        }
        "swarm_result" => job::job_result(jobs, &job_id(arguments)?).await,
        "swarm_cancel" => job::cancel_job(jobs, &job_id(arguments)?).await,
        "swarm_list" => job::list_jobs(jobs, text(arguments, "label").as_deref(), text(arguments, "state").as_deref()).await,
        "swarm_resume" => job::resume_job(
            jobs,
            &job_id(arguments)?,
            job::BudgetRaise {
                max_tokens: number(arguments, "max_tokens"),
                max_steps: number(arguments, "max_steps").map(|n| n as u32),
                max_inference_calls: number(arguments, "max_inference_calls"),
                max_wall_clock_secs: number(arguments, "max_wall_clock_secs"),
            },
        )
        .await,
        "swarm_clean" => job::clean_job(jobs, &job_id(arguments)?),
        other => anyhow::bail!("unknown tool `{other}`"),
    }
}

fn tool_list() -> Value {
    json!([
        {
            "name": "swarm_submit",
            "description": "Start a swarm worker in the background and return its job_id at once. The worker is a \
coding agent with file, shell, python and git tools. Give it a goal that states the task and how it will know it \
succeeded (for example the benchmark command and the number to beat). Pass `repo` so it works in its own git \
worktree and branch: parallel workers on one repo never collide, and the result carries a patch. The worker ends \
by reporting structured status and metrics, which swarm_result returns.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "goal": {"type": "string", "description": "The task, with how to verify it."},
                    "repo": {"type": "string", "description": "Path of a git repository; the job gets its own worktree."},
                    "base": {"type": "string", "description": "Commit, branch or tag to start from (default HEAD)."},
                    "workspace": {"type": "string", "description": "An existing directory to work in instead of repo."},
                    "label": {"type": "string", "description": "Groups jobs, e.g. one label per experiment."},
                    "key": {"type": "string", "description": "Idempotency key: the same key returns the same job."},
                    "provider": {"type": "string", "description": "Provider (mock, local, chatgpt-web, ...); default SWARM_PROVIDER."},
                    "model": {"type": "string"},
                    "base_url": {"type": "string", "description": "Provider endpoint override, e.g. http://127.0.0.1:8790/v1 for a local OpenAI-compatible server. Without it the job inherits $SWARM_BASE_URL, which pins every MCP job to the host's configured provider (a browser bridge on some hosts, which stalls long runs)."},
                    "tool_protocol": {"type": "string", "enum": ["auto", "native", "text"], "description": "Tool transport override. Use text for endpoints without native function calling; persisted for retries and resume."},
                    "config": {"type":"string", "description":"Run TOML path, including native provider options and endpoint pools."},
                    "preset": {"type":"string", "enum":["general","builder","reviewer","test-writer"], "default":"builder", "description":"Builder/test-writer require a red baseline --verify gate. Reviewer requires structured evidence. Explicit budgets override preset ceilings."},
                    "min_tool_calls":{"type":"integer", "minimum":2},
                    "freshness_ref":{"type":"string"},
                    "max_active_jobs":{"type":"integer", "minimum":1},
                    "priority":{"type":"integer", "description":"Higher goes first; queued jobs age one point per minute. Reviewers default to 20, builders 0."},
                    "sandbox_image":{"type":"string", "description":"Preinstalled Docker image for network-free command isolation."},
                    "agents": {"type": "integer", "description": "Agents inside the job (default 1)."},
                    "max_steps": {"type": "integer", "description": "Model turns the worker may take (default 40)."},
                    "max_inference_calls": {"type": "integer", "description": "Inference calls the worker may make. Defaults to max-steps x agents, so raising max-steps alone is enough; set this only to give a retrying worker more calls than its step budget."},
                    "max_wall_clock_secs": {"type": "integer", "description": "Wall-clock limit for the job in seconds (default 3600). Raise it for repositories where builds and tests are slow; the run fails when it is exceeded."},
                    "max_tokens": {"type": "integer", "description": "Tokens the job may use in total, input plus output on every call (default 5000000). A job over a large repository can spend 200000 in a few calls, so raise it for big trees; a job that hits it fails with the reason in swarm_status."},
                    "verify": {"type": "string", "description": "A command run in the workspace when the worker stops; exit code 0 means the work holds. If it fails, its output goes to a fresh worker on the same workspace (up to max_rounds). Result outcome is `verified` only when it passes, `verify_failed` otherwise. Use your repo's test/build gate, e.g. `go test ./...`."},
                    "max_rounds": {"type": "integer", "description": "Worker runs a verify job may use, the first included (default 3)."},
                    "verify_timeout_secs": {"type": "integer", "description": "Seconds one verify run may take (default 1800)."},
                    "allow_push": {"type": "array", "items": {"type": "string"}, "description": "Refs the worker's shell may `git push` to: branch names, `remote/branch`, or `*` for anything. Omit (the default) to refuse every push from the worker - the orchestrator keeps remote writes. A guardrail on the command string, not a sandbox."},
                    "no_report_footer": {"type": "boolean"}
                },
                "required": ["goal"]
            }
        },
        {
            "name": "swarm_status",
            "description": "Where one job is: state (starting, running, completed, failed, cancelled, interrupted), \
elapsed time, model and tool calls, and its last action.",
            "inputSchema": {"type": "object", "properties": {"job_id": {"type": "string"}}, "required": ["job_id"]}
        },
        {
            "name": "swarm_wait",
            "description": "Wait for jobs to finish, up to timeout_secs (at most 45 per call). Returns every job's \
status and which are still running. With any=true it returns as soon as one finishes, so you can start on its \
result while the others run. If it times out, call it again.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "job_ids": {"type": "array", "items": {"type": "string"}},
                    "timeout_secs": {"type": "integer", "description": "Default 30, maximum 45."},
                    "any": {"type": "boolean"}
                },
                "required": ["job_ids"]
            }
        },
        {
            "name": "swarm_result",
            "description": "What a job produced: `report` (the worker's own status, summary, numeric metrics, \
artifacts and notes), `summary` (its final message), `files_changed`, and for repo jobs `patch` (path to a diff \
against the starting commit, and the git apply command). Compare metrics across workers to pick the best.",
            "inputSchema": {"type": "object", "properties": {"job_id": {"type": "string"}}, "required": ["job_id"]}
        },
        {
            "name": "swarm_cancel",
            "description": "Stop a running job and its processes. A job that already finished is left as it is. \
Use it when a worker is stuck or its approach has stopped being worth the time.",
            "inputSchema": {"type": "object", "properties": {"job_id": {"type": "string"}}, "required": ["job_id"]}
        },
        {
            "name": "swarm_list",
            "description": "List jobs, optionally only one label or state.",
            "inputSchema": {
                "type": "object",
                "properties": {"label": {"type": "string"}, "state": {"type": "string"}}
            }
        },
        {
            "name": "swarm_resume",
            "description": "Continue a job whose state is `interrupted` (its process died before the run finished): \
same workspace, same provider, picking up from its saved progress. Works for an `interrupted` job \
with no extra arguments, and for a `failed` job whose reason is budget exhaustion - which needs a \
raised ceiling, since usage is cumulative: pass max_tokens / max_steps / max_inference_calls / \
max_wall_clock_secs above what the job already spent (swarm_status reports them). A completed, \
cancelled, or genuinely failed job is refused.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "job_id": {"type": "string"},
                    "max_tokens": {"type": "integer", "description": "New whole-job token ceiling; must exceed the tokens already spent."},
                    "max_steps": {"type": "integer", "description": "New per-agent step ceiling; also raises inference calls in step."},
                    "max_inference_calls": {"type": "integer"},
                    "max_wall_clock_secs": {"type": "integer"}
                },
                "required": ["job_id"]
            }
        },
        {
            "name": "swarm_clean",
            "description": "Remove a finished repo job's worktree and branch; the patch and logs are kept.",
            "inputSchema": {"type": "object", "properties": {"job_id": {"type": "string"}}, "required": ["job_id"]}
        }
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_handshake_lists_every_tool_with_a_schema() {
        let dir = tempfile::tempdir().unwrap();
        let init = handle(dir.path(), &json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {"protocolVersion": "2025-03-26"}})).await.unwrap();
        assert_eq!(init["id"], 1);
        assert_eq!(init["result"]["protocolVersion"], "2025-03-26", "the client's version is echoed");
        assert!(init["result"]["capabilities"]["tools"].is_object());
        assert!(init["result"]["instructions"].as_str().unwrap().contains("swarm_submit"));
        assert!(handle(dir.path(), &json!({"jsonrpc": "2.0", "method": "notifications/initialized"})).await.is_none());

        let list = handle(dir.path(), &json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"})).await.unwrap();
        let names: Vec<&str> = list["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(
            names,
            ["swarm_submit", "swarm_status", "swarm_wait", "swarm_result", "swarm_cancel", "swarm_list", "swarm_resume", "swarm_clean"]
        );
        for tool in list["result"]["tools"].as_array().unwrap() {
            assert_eq!(tool["inputSchema"]["type"], "object", "{tool}");
            assert!(tool["description"].as_str().unwrap().len() > 20);
        }
        // The submit schema must advertise every knob an MCP client needs: without
        // base_url every MCP job is pinned to the host's $SWARM_BASE_URL provider.
        let submit = list["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["name"] == "swarm_submit")
            .unwrap();
        let props = submit["inputSchema"]["properties"].as_object().unwrap();
        for key in ["base_url", "max_tokens", "max_inference_calls", "max_wall_clock_secs", "verify", "allow_push"] {
            assert!(props.contains_key(key), "swarm_submit schema must advertise {key}");
        }
    }

    #[tokio::test]
    async fn errors_come_back_as_tool_errors_and_unknown_methods_as_rpc_errors() {
        let dir = tempfile::tempdir().unwrap();
        let bad = handle(dir.path(), &json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call",
            "params": {"name": "swarm_status", "arguments": {"job_id": "job_missing"}}})).await.unwrap();
        assert_eq!(bad["result"]["isError"], true);
        assert!(bad["result"]["content"][0]["text"].as_str().unwrap().contains("no such job"));
        let unknown = handle(dir.path(), &json!({"jsonrpc": "2.0", "id": 4, "method": "tools/call",
            "params": {"name": "swarm_nope", "arguments": {}}})).await.unwrap();
        assert_eq!(unknown["result"]["isError"], true);
        let rpc = handle(dir.path(), &json!({"jsonrpc": "2.0", "id": 5, "method": "resources/list"})).await.unwrap();
        assert_eq!(rpc["error"]["code"], -32601);
        let wait = handle(dir.path(), &json!({"jsonrpc": "2.0", "id": 6, "method": "tools/call",
            "params": {"name": "swarm_wait", "arguments": {}}})).await.unwrap();
        assert_eq!(wait["result"]["isError"], true, "job_ids is required");
    }
}
