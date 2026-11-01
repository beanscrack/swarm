//! `swarm job`, driven the way an orchestrating agent drives it: as a subprocess,
//! reading JSON. The mock provider keeps it offline; its script creates
//! `hello.txt` and finishes.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

struct Jobs {
    dir: tempfile::TempDir,
}

#[test]
fn builders_require_a_red_baseline_and_keep_the_gate_evidence() {
    let jobs = Jobs::new();
    let binary = env!("CARGO_BIN_EXE_swarm");
    let submit = |extra: &[&str]| {
        Command::new(binary).args(["job","submit","--provider","mock","--goal","create hello.txt"])
            .arg("--jobs").arg(jobs.root()).args(extra).env("SWARM_MOCK_SCRIPT","coding").output().unwrap()
    };
    let missing = submit(&[]);
    assert!(!missing.status.success());
    assert!(String::from_utf8_lossy(&missing.stderr).contains("require --verify"));
    let green = submit(&["--verify","true"]);
    assert!(!green.status.success());
    assert!(String::from_utf8_lossy(&green.stderr).contains("baseline gate is green"));
    let red = submit(&["--verify","test -f hello.txt"]);
    assert!(red.status.success(),"{}",String::from_utf8_lossy(&red.stderr));
    let value: Value = serde_json::from_slice(&red.stdout).unwrap();
    let id = value["job_id"].as_str().unwrap(); jobs.finish(id);
    let result = jobs.ok(&["result",id]);
    assert_eq!(result["baseline_gate"]["red"],true);
    assert_eq!(result["outcome"],"verified");
    assert_eq!(result["token_limit"],14_000_000);
}

#[test]
fn upstream_movement_is_reported_without_polluting_the_worker_patch() {
    let jobs = Jobs::new(); let repo = jobs.dir.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap(); git(&repo,&["init"]);
    std::fs::write(repo.join("base.txt"),"base\n").unwrap(); git(&repo,&["add","."]);
    git(&repo,&["commit","-m","base"]);
    let base = git(&repo,&["rev-parse","HEAD"]);
    git(&repo,&["update-ref","refs/remotes/origin/main","HEAD"]);
    let submitted = jobs.submit(&["--repo",repo.to_str().unwrap()]);
    std::fs::write(repo.join("upstream.txt"),"upstream\n").unwrap(); git(&repo,&["add","."]);
    git(&repo,&["commit","-m","upstream"]); git(&repo,&["update-ref","refs/remotes/origin/main","HEAD"]);
    let id = submitted["job_id"].as_str().unwrap(); jobs.finish(id);
    let result = jobs.ok(&["result",id]);
    assert_eq!(result["base_drift"],true);
    let patch = std::fs::read_to_string(result["patch"]["path"].as_str().unwrap()).unwrap();
    assert!(patch.contains("hello.txt")); assert!(!patch.contains("upstream.txt"));
    let stale = Command::new(env!("CARGO_BIN_EXE_swarm"))
        .args(["job","submit","--preset","general","--provider","mock","--goal","explore","--repo",repo.to_str().unwrap(),"--base",&base])
        .arg("--jobs").arg(jobs.root()).output().unwrap();
    assert!(!stale.status.success());
    assert!(String::from_utf8_lossy(&stale.stderr).contains("stale base"));
}

impl Jobs {
    fn new() -> Self {
        Self { dir: tempfile::tempdir().unwrap() }
    }

    fn root(&self) -> PathBuf {
        self.dir.path().join("jobs")
    }

    /// Runs `swarm job <args>`; returns the parsed stdout and whether it exited 0.
    fn run(&self, args: &[&str]) -> (Value, bool) {
        let output = Command::new(env!("CARGO_BIN_EXE_swarm"))
            .arg("job")
            .args(args)
            .arg("--jobs")
            .arg(self.root())
            .env_remove("SWARM_PROVIDER")
            .env("SWARM_MOCK_SCRIPT", "coding")
            .output()
            .expect("run swarm");
        let text = String::from_utf8_lossy(&output.stdout).into_owned();
        let value = serde_json::from_str(&text).unwrap_or_else(|error| {
            panic!(
                "stdout is not JSON ({error}): {text:?}\nstderr: {}",
                String::from_utf8_lossy(&output.stderr)
            )
        });
        (value, output.status.success())
    }

    fn ok(&self, args: &[&str]) -> Value {
        let (value, success) = self.run(args);
        assert!(success, "swarm job {args:?} failed: {value}");
        value
    }

    fn submit(&self, extra: &[&str]) -> Value {
        let mut args = vec!["submit", "--preset", "general", "--provider", "mock", "--goal", "create hello.txt and say done"];
        args.extend_from_slice(extra);
        self.ok(&args)
    }

    fn finish(&self, id: &str) -> Value {
        let waited = self.ok(&["wait", id, "--timeout", "90"]);
        assert_eq!(waited["done"], true, "{waited}");
        waited
    }
}

fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=t", "-c", "user.email=t@example.com", "-c", "commit.gpgsign=false"])
        .args(args)
        .output()
        .expect("git");
    assert!(output.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&output.stderr));
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

#[test]
fn a_job_runs_in_the_background_and_its_result_is_collected() {
    let jobs = Jobs::new();
    let submitted = jobs.submit(&["--label", "t1"]);
    let id = submitted["job_id"].as_str().unwrap().to_string();
    assert_eq!(submitted["reused"], false);
    assert!(id.starts_with("job_"));

    let waited = jobs.finish(&id);
    assert_eq!(waited["timed_out"], false);
    assert_eq!(waited["jobs"][0]["state"], "completed");

    let result = jobs.ok(&["result", &id]);
    assert_eq!(result["state"], "completed");
    assert_eq!(result["label"], "t1");
    assert!(result["files_changed"].as_array().unwrap().iter().any(|f| f == "hello.txt"), "{result}");
    assert!(result["summary"].as_str().unwrap_or("").len() > 3, "a final summary: {result}");
    // The mock worker never calls report_result; the result says so plainly.
    assert!(result["report"].is_null());
    assert!(result["note"].as_str().unwrap().contains("report_result"));
    let workspace = PathBuf::from(result["workspace"].as_str().unwrap());
    assert_eq!(std::fs::read_to_string(workspace.join("hello.txt")).unwrap(), "hello, swarm\n");
    assert!(result["inference_calls"].as_u64().unwrap() >= 1);
    assert!(result["tool_calls"].as_u64().unwrap() >= 1);

    // The report instruction rode along with the goal.
    let spec = std::fs::read_to_string(jobs.root().join(&id).join("job.json")).unwrap();
    assert!(spec.contains("create hello.txt and say done") && !spec.contains("report_result"));
}

#[test]
fn the_same_key_returns_the_same_job_and_list_filters_by_label() {
    let jobs = Jobs::new();
    let first = jobs.submit(&["--key", "exp-1", "--label", "alpha"]);
    let again = jobs.submit(&["--key", "exp-1", "--label", "alpha"]);
    assert_eq!(again["reused"], true);
    assert_eq!(again["job_id"], first["job_id"]);
    let other = jobs.submit(&["--label", "beta"]);
    jobs.finish(first["job_id"].as_str().unwrap());
    jobs.finish(other["job_id"].as_str().unwrap());

    let all = jobs.ok(&["list"]);
    assert_eq!(all["jobs"].as_array().unwrap().len(), 2);
    let alpha = jobs.ok(&["list", "--label", "alpha"]);
    assert_eq!(alpha["jobs"].as_array().unwrap().len(), 1);
    assert_eq!(alpha["jobs"][0]["job_id"], first["job_id"]);
    let done = jobs.ok(&["list", "--state", "completed"]);
    assert_eq!(done["jobs"].as_array().unwrap().len(), 2);
    let none = jobs.ok(&["list", "--state", "running"]);
    assert!(none["jobs"].as_array().unwrap().is_empty());
}

#[test]
fn a_repo_job_gets_its_own_worktree_and_returns_a_patch() {
    let jobs = Jobs::new();
    let repo = jobs.dir.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("kernel.cu"), "// baseline\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "baseline"]);
    let base = git(&repo, &["rev-parse", "HEAD"]);

    let repo_arg = repo.to_string_lossy().into_owned();
    let a = jobs.submit(&["--repo", &repo_arg]);
    let b = jobs.submit(&["--repo", &repo_arg]);
    let (a_id, b_id) = (a["job_id"].as_str().unwrap(), b["job_id"].as_str().unwrap());
    assert_ne!(a["workspace"], b["workspace"], "each job has its own worktree");
    jobs.finish(a_id);
    jobs.finish(b_id);

    // The workers wrote into their worktrees, not into the repo.
    assert!(!repo.join("hello.txt").exists(), "the original checkout is untouched");
    let result = jobs.ok(&["result", a_id]);
    let patch = result["patch"]["path"].as_str().expect("a patch for a repo job");
    let text = std::fs::read_to_string(patch).unwrap();
    assert!(text.contains("hello.txt") && text.contains("+hello, swarm"), "{text}");
    assert!(result["patch"]["stat"].as_str().unwrap().contains("1 file changed"), "{result}");
    assert_eq!(git(&repo, &["rev-parse", "HEAD"]), base, "the base commit did not move");

    // The patch applies to a fresh checkout of the base.
    let fresh = jobs.dir.path().join("fresh");
    git(&repo, &["worktree", "add", "-q", "--detach", &fresh.to_string_lossy(), &base]);
    git(&fresh, &["apply", patch]);
    // git on Windows may convert line endings (core.autocrlf); the content is what matters.
    assert_eq!(std::fs::read_to_string(fresh.join("hello.txt")).unwrap().replace("\r\n", "\n"), "hello, swarm\n");

    // Clean removes the worktree and branch and keeps the patch.
    let cleaned = jobs.ok(&["clean", a_id]);
    assert_eq!(cleaned["patch_kept"], true, "{cleaned}");
    assert!(!PathBuf::from(a["workspace"].as_str().unwrap()).exists());
    assert!(!git(&repo, &["branch", "--list", &format!("codex/{a_id}")]).contains(a_id));
    assert!(Path::new(patch).is_file());
    assert!(PathBuf::from(b["workspace"].as_str().unwrap()).exists(), "the other job is not touched");
}

// A caller that captures our output (a shell tool, an MCP host) must get its
// answer when `submit` returns, not when the worker ends. On Windows the worker
// used to inherit the caller's pipe, which held it open for the whole job.
#[test]
fn submit_returns_at_once_even_when_the_callers_output_is_captured() {
    let jobs = Jobs::new();
    let started = std::time::Instant::now();
    // `output()` captures stdout and stderr through pipes and waits for them to close.
    let output = Command::new(env!("CARGO_BIN_EXE_swarm"))
        .args(["job", "submit", "--preset", "general", "--provider", "mock", "--goal", "create hello.txt and say done", "--jobs"])
        .arg(jobs.root())
        .env("SWARM_MOCK_SCRIPT", "coding")
        .env("SWARM_MOCK_LATENCY_MS", "6000") // three model calls: about 18 seconds of work
        .env_remove("SWARM_PROVIDER")
        .output()
        .unwrap();
    let took = started.elapsed();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert!(took < std::time::Duration::from_secs(9), "submit blocked for {took:?}");
    let id = serde_json::from_slice::<Value>(&output.stdout).unwrap()["job_id"].as_str().unwrap().to_string();

    // The worker really is still running, and cancel stops it.
    let status = jobs.ok(&["status", &id]);
    assert!(matches!(status["state"].as_str(), Some("starting" | "running")), "{status}");
    let cancelled = jobs.ok(&["cancel", &id]);
    assert_eq!(cancelled["state"], "cancelled", "{cancelled}");
    assert_eq!(jobs.ok(&["status", &id])["state"], "cancelled");
}

#[test]
fn the_outcome_tells_apart_a_job_that_did_work_from_one_that_only_ended() {
    let jobs = Jobs::new();
    let id = jobs.submit(&[])["job_id"].as_str().unwrap().to_string();
    jobs.finish(&id);
    let done = jobs.ok(&["result", &id]);
    // Changed files but no report_result call.
    assert_eq!(done["outcome"], "changed_without_report", "{done}");

    // A worker that does nothing (the plain mock only replies) ends as `completed`
    // too, and the outcome says no work was done.
    let idle = Command::new(env!("CARGO_BIN_EXE_swarm"))
        .args(["job", "submit", "--preset", "general", "--provider", "mock", "--goal", "do nothing at all", "--jobs"])
        .arg(jobs.root())
        .env_remove("SWARM_PROVIDER")
        .env_remove("SWARM_MOCK_SCRIPT")
        .output()
        .unwrap();
    let idle_id = serde_json::from_slice::<Value>(&idle.stdout).unwrap()["job_id"].as_str().unwrap().to_string();
    jobs.finish(&idle_id);
    let nothing = jobs.ok(&["result", &idle_id]);
    assert_eq!(nothing["state"], "completed");
    assert_eq!(nothing["outcome"], "no_work", "{nothing}");
}

#[test]
fn byproducts_of_running_things_are_not_part_of_the_patch() {
    let jobs = Jobs::new();
    let repo = jobs.dir.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("kernel.py"), "x = 1\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "baseline"]);
    let submitted = jobs.submit(&["--repo", &repo.to_string_lossy()]);
    let id = submitted["job_id"].as_str().unwrap().to_string();
    jobs.finish(&id);
    // What running a Python check inside the worktree leaves behind.
    let ws = PathBuf::from(submitted["workspace"].as_str().unwrap());
    std::fs::create_dir_all(ws.join("__pycache__")).unwrap();
    std::fs::write(ws.join("__pycache__").join("kernel.cpython-312.pyc"), b"\x00\x01").unwrap();
    std::fs::write(ws.join(".swarm_tmp_run.py"), "print(1)\n").unwrap();

    let result = jobs.ok(&["result", &id]);
    let patch = std::fs::read_to_string(result["patch"]["path"].as_str().unwrap()).unwrap();
    assert!(patch.contains("hello.txt"), "the real change is there");
    assert!(!patch.contains("pycache") && !patch.contains(".pyc") && !patch.contains("swarm_tmp_run"), "{patch}");
    assert!(result["patch"]["stat"].as_str().unwrap().contains("1 file changed"), "{result}");
}

// Found by an orchestrator whose workers died at 200000 tokens after 15 calls: a
// job's token budget has to be settable, generous by default, and its exhaustion has
// to read as exactly that, not as a worker that found nothing.
#[test]
fn the_token_budget_is_settable_generous_by_default_and_its_exhaustion_is_named() {
    let jobs = Jobs::new();
    let starved = jobs.submit(&["--max-tokens", "300"])["job_id"].as_str().unwrap().to_string();
    // `wait` exits non-zero when a job did not complete, which is the point here.
    let (waited, all_completed) = jobs.run(&["wait", &starved, "--timeout", "90"]);
    assert!(waited["done"] == true && !all_completed, "{waited}");
    let status = jobs.ok(&["status", &starved]);
    assert_eq!(status["state"], "failed", "{status}");
    assert_eq!(status["token_limit"], 300);
    let reason = status["reason"].as_str().unwrap_or_default();
    assert!(reason.contains("budget") && reason.contains("token"), "the reason names the budget: {status}");
    assert_eq!(jobs.ok(&["result", &starved])["outcome"], "failed");

    let normal = jobs.submit(&[])["job_id"].as_str().unwrap().to_string();
    jobs.finish(&normal);
    let status = jobs.ok(&["status", &normal]);
    assert_eq!(status["state"], "completed", "{status}");
    assert_eq!(status["token_limit"], 5_000_000, "a job's default is not the mock's 200000");
    assert!(status.get("reason").is_none() || status["reason"].is_null());
}

#[test]
fn unknown_jobs_bad_input_and_finished_jobs_are_handled() {
    let jobs = Jobs::new();
    let output = Command::new(env!("CARGO_BIN_EXE_swarm"))
        .args(["job", "status", "job_nope", "--jobs"])
        .arg(jobs.root())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("no such job"));

    let no_provider = Command::new(env!("CARGO_BIN_EXE_swarm"))
        .args(["job", "submit", "--preset", "general", "--goal", "x", "--jobs"])
        .arg(jobs.root())
        .env_remove("SWARM_PROVIDER")
        .output()
        .unwrap();
    assert!(!no_provider.status.success());
    assert!(String::from_utf8_lossy(&no_provider.stderr).contains("no provider"));

    let id = jobs.submit(&[])["job_id"].as_str().unwrap().to_string();
    jobs.finish(&id);
    let cancelled = jobs.ok(&["cancel", &id]);
    assert_eq!(cancelled["state"], "completed", "a finished job stays finished: {cancelled}");

    // Only an interrupted job can be resumed; a completed one is refused.
    let resume = Command::new(env!("CARGO_BIN_EXE_swarm"))
        .args(["job", "resume", &id, "--jobs"])
        .arg(jobs.root())
        .output()
        .unwrap();
    assert!(!resume.status.success());
    assert!(String::from_utf8_lossy(&resume.stderr).contains("only an interrupted job"));
}

fn needs_posix_shell() -> bool {
    let posix = swarm_tools::shell::posix_available();
    if !posix {
        eprintln!("skipped: the verify commands in this test are POSIX shell");
    }
    posix
}

#[test]
fn a_verify_job_that_passes_is_verified() {
    if !needs_posix_shell() {
        return;
    }
    let jobs = Jobs::new();
    let submitted = jobs.submit(&["--verify", "test -f hello.txt"]);
    assert_eq!(submitted["verify"], "test -f hello.txt");
    let id = submitted["job_id"].as_str().unwrap().to_string();
    jobs.finish(&id);

    let result = jobs.ok(&["result", &id]);
    assert_eq!(result["outcome"], "verified", "{result}");
    assert_eq!(result["verify"]["passed"], true, "{result}");
    assert_eq!(result["verify"]["rounds"].as_array().unwrap().len(), 1, "{result}");
    assert_eq!(result["verify"]["phase"], "done");
}

#[test]
fn a_failed_check_sends_a_new_worker_back_and_the_second_round_can_pass() {
    if !needs_posix_shell() {
        return;
    }
    let jobs = Jobs::new();
    // Fails the first time it runs, passes after: the retry is what makes it pass.
    let check = "if [ -f .seen ]; then exit 0; fi; touch .seen; echo first-round-failure; exit 1";
    let submitted = jobs.submit(&["--verify", check, "--max-rounds", "3"]);
    let id = submitted["job_id"].as_str().unwrap().to_string();
    jobs.finish(&id);

    let result = jobs.ok(&["result", &id]);
    assert_eq!(result["outcome"], "verified", "{result}");
    let rounds = result["verify"]["rounds"].as_array().unwrap();
    assert_eq!(rounds.len(), 2, "{result}");
    assert_eq!(rounds[0]["passed"], false);
    assert!(rounds[0]["output"].as_str().unwrap().contains("first-round-failure"), "{result}");
    assert_eq!(rounds[1]["passed"], true);

    // The second worker ran in its own store and was told why the first one failed.
    let job_dir = jobs.root().join(&id);
    assert!(job_dir.join("store.r2").join("swarm.db").is_file());
    let log = std::fs::read_to_string(job_dir.join("run.log")).unwrap();
    assert!(log.contains("[verify] round 1/3: worker starting"), "{log}");
    assert!(log.contains("[verify] round 2: check passed"), "{log}");
}

#[test]
fn a_check_that_never_passes_ends_as_verify_failed_with_its_output() {
    if !needs_posix_shell() {
        return;
    }
    let jobs = Jobs::new();
    let submitted = jobs.submit(&["--verify", "echo still-broken; exit 3", "--max-rounds", "2"]);
    let id = submitted["job_id"].as_str().unwrap().to_string();
    jobs.finish(&id);

    let result = jobs.ok(&["result", &id]);
    // The worker finished its runs, so the job is `completed`; the outcome is the check's verdict.
    assert_eq!(result["state"], "completed", "{result}");
    assert_eq!(result["outcome"], "verify_failed", "{result}");
    let rounds = result["verify"]["rounds"].as_array().unwrap();
    assert_eq!(rounds.len(), 2, "{result}");
    assert_eq!(rounds[1]["exit_code"], 3);
    assert!(rounds[1]["output"].as_str().unwrap().contains("still-broken"));
    assert!(result["note"].as_str().unwrap().contains("still failed"), "{result}");
}

#[test]
fn a_job_waiting_on_its_check_is_still_running() {
    if !needs_posix_shell() {
        return;
    }
    let jobs = Jobs::new();
    // Hold the check until status has observed it. Wall-clock sleeps can be
    // missed when many subprocess tests and Windows process queries compete.
    let submitted = jobs.submit(&["--verify", "echo waiting > verify-started; while [ ! -f verify-release ]; do sleep 0.1; done; test -f hello.txt"]);
    let id = submitted["job_id"].as_str().unwrap().to_string();
    let workspace = PathBuf::from(submitted["workspace"].as_str().unwrap());
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while !workspace.join("verify-started").exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let status = jobs.ok(&["status", &id]);
    std::fs::write(workspace.join("verify-release"), "release").unwrap();
    assert_eq!(status["verify"]["phase"], "verifying", "{status}");
    assert_eq!(status["state"], "running", "a verifying job is not finished: {status}");
    jobs.finish(&id);
    assert_eq!(jobs.ok(&["result", &id])["outcome"], "verified");
}

#[test]
fn a_verify_job_cannot_be_resumed() {
    if !needs_posix_shell() {
        return;
    }
    let jobs = Jobs::new();
    let id = jobs.submit(&["--verify", "true"])["job_id"].as_str().unwrap().to_string();
    jobs.finish(&id);
    let output = Command::new(env!("CARGO_BIN_EXE_swarm"))
        .args(["job", "resume", &id, "--jobs"])
        .arg(jobs.root())
        .output()
        .expect("run swarm");
    assert!(!output.status.success(), "resume must refuse a --verify job");
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("--verify loop") && error.contains("--workspace"), "{error}");
}

#[test]
fn a_passing_check_on_a_worker_that_used_no_tools_is_not_verified() {
    if !needs_posix_shell() {
        return;
    }
    let jobs = Jobs::new();
    // The plain mock only replies: no tool calls, no changes. The check passes on the untouched tree.
    let output = Command::new(env!("CARGO_BIN_EXE_swarm"))
        .args(["job", "submit", "--preset", "general", "--provider", "mock", "--goal", "do nothing at all", "--verify", "true", "--jobs"])
        .arg(jobs.root())
        .env_remove("SWARM_PROVIDER")
        .env_remove("SWARM_MOCK_SCRIPT")
        .output()
        .unwrap();
    let id = serde_json::from_slice::<Value>(&output.stdout).unwrap()["job_id"].as_str().unwrap().to_string();
    jobs.finish(&id);
    let result = jobs.ok(&["result", &id]);
    assert_eq!(result["verify"]["passed"], true, "{result}");
    assert_eq!(result["verify"]["worker_tool_calls"], 0, "{result}");
    assert_eq!(result["outcome"], "unverified_no_work", "{result}");
    assert!(result["note"].as_str().unwrap().contains("used no tools"), "{result}");
}
