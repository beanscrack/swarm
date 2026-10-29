//! `swarm job`, driven the way an orchestrating agent drives it: as a subprocess,
//! reading JSON. The mock provider keeps it offline; its script creates
//! `hello.txt` and finishes.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

struct Jobs {
    dir: tempfile::TempDir,
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
        let mut args = vec!["submit", "--provider", "mock", "--goal", "create hello.txt and say done"];
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
    assert!(!git(&repo, &["branch", "--list", &format!("swarm/{a_id}")]).contains(a_id));
    assert!(Path::new(patch).is_file());
    assert!(PathBuf::from(b["workspace"].as_str().unwrap()).exists(), "the other job is not touched");
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
        .args(["job", "submit", "--goal", "x", "--jobs"])
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
