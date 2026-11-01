//! Docker command boundary. No host credentials, sockets, environment or network
//! are forwarded. Image provisioning is an explicit host responsibility.
use crate::SandboxConfig;

pub struct ContainerCommand {
    pub command: tokio::process::Command,
    name: String,
}

impl Drop for ContainerCommand {
    fn drop(&mut self) {
        // Dropping the docker CLI alone does not kill its container.
        let _ = std::process::Command::new("docker")
            .args(["rm", "-f", &self.name])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output();
    }
}

pub fn container_command(
    config: &SandboxConfig,
    program: &str,
    args: &[String],
) -> Result<ContainerCommand, String> {
    let image = config
        .docker_image
        .as_deref()
        .ok_or("no sandbox image configured")?;
    if image.trim().is_empty() || image.starts_with('-') {
        return Err("invalid sandbox image".into());
    }
    if !config.allow_push.is_empty() {
        return Err(
            "isolated workers cannot receive push grants; the host orchestrator pushes".into(),
        );
    }
    let name = format!("swarm-{}-{}", std::process::id(), uuid::Uuid::new_v4());
    let root = std::fs::canonicalize(config.root()).map_err(|e| e.to_string())?;
    let mut command = tokio::process::Command::new("docker");
    command.args([
        "run",
        "--rm",
        "--pull=never",
        "--name",
        &name,
        "--network=none",
        "--read-only",
        "--cap-drop=ALL",
        "--security-opt=no-new-privileges",
        "--pids-limit=128",
        "--memory=2g",
        "--cpus=2",
        "--user=65534:65534",
        "--tmpfs=/tmp:rw,nosuid,nodev,size=512m",
        "--workdir=/workspace",
        "--env=HOME=/tmp",
        "--env=GIT_CONFIG_NOSYSTEM=1",
        "--env=GIT_TERMINAL_PROMPT=0",
    ]);
    if let Ok(job) = std::env::var("SWARM_JOB_ID") {
        command.arg("--label").arg(format!("swarm.job={job}"));
    }
    let root_text = root.to_string_lossy();
    // Docker's Windows client rejects Rust's canonical extended-length prefix.
    let mount_root = root_text.strip_prefix(r"\\?\").unwrap_or(&root_text);
    if mount_root.contains(',') {
        return Err("Docker workspace paths containing commas are unsupported".into());
    }
    command
        .arg("--mount")
        .arg(format!("type=bind,source={mount_root},target=/workspace"));
    if config.root().join(".swarm/sandbox-git").is_dir() {
        command.args([
            "--env=GIT_DIR=/workspace/.swarm/sandbox-git",
            "--env=GIT_WORK_TREE=/workspace",
            "--env=GIT_CONFIG_GLOBAL=/dev/null",
        ]);
    }
    command.args(["--entrypoint", program, image]).args(args);
    command.kill_on_drop(true);
    Ok(ContainerCommand { command, name })
}

/// Private repository metadata with no remotes or credential helpers. Worker git
/// commands see this snapshot; the orchestrator's actual .git stays authoritative.
pub fn prepare_repository(config: &SandboxConfig) -> Result<(), String> {
    if config.docker_image.is_none() || !config.root().join(".git").exists() {
        return Ok(());
    }
    let metadata = config.root().join(".swarm/sandbox-git");
    if metadata.exists() {
        return Ok(());
    }
    let run = |args: &[&std::ffi::OsStr]| -> Result<(), String> {
        let output = std::process::Command::new("git")
            .args(["-c", "commit.gpgsign=false"])
            .args(args)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env(
                "GIT_CONFIG_GLOBAL",
                if cfg!(windows) { "NUL" } else { "/dev/null" },
            )
            .current_dir(config.root())
            .output()
            .map_err(|e| e.to_string())?;
        if output.status.success() {
            Ok(())
        } else {
            Err(String::from_utf8_lossy(&output.stderr).into())
        }
    };
    run(&["init".as_ref(), "--bare".as_ref(), metadata.as_os_str()])?;
    std::fs::write(metadata.join("info/exclude"), ".swarm/\n").map_err(|e| e.to_string())?;
    for (key, value) in [
        ("user.name", "Swarm Sandbox"),
        ("user.email", "sandbox@localhost"),
        ("core.bare", "false"),
        ("safe.directory", "/workspace"),
    ] {
        run(&[
            "--git-dir".as_ref(),
            metadata.as_os_str(),
            "config".as_ref(),
            key.as_ref(),
            value.as_ref(),
        ])?;
    }
    run(&[
        "--git-dir".as_ref(),
        metadata.as_os_str(),
        "--work-tree".as_ref(),
        config.root().as_os_str(),
        "add".as_ref(),
        "-A".as_ref(),
    ])?;
    run(&[
        "--git-dir".as_ref(),
        metadata.as_os_str(),
        "--work-tree".as_ref(),
        config.root().as_os_str(),
        "commit".as_ref(),
        "--allow-empty".as_ref(),
        "-m".as_ref(),
        "Sandbox baseline".as_ref(),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn private_git_metadata_has_no_remote_and_preserves_the_host_repository() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            std::process::Command::new("git")
                .arg("init")
                .arg(dir.path())
                .output()
                .unwrap()
                .status
                .success()
        );
        std::fs::write(dir.path().join("source.txt"), "source\n").unwrap();
        let config = SandboxConfig::new(dir.path()).docker_image(Some("preinstalled-image".into()));
        prepare_repository(&config).unwrap();
        let metadata = dir.path().join(".swarm/sandbox-git");
        let private = std::process::Command::new("git")
            .arg("--git-dir")
            .arg(&metadata)
            .args(["log", "--oneline"])
            .output()
            .unwrap();
        assert!(private.status.success());
        let remotes = std::process::Command::new("git")
            .arg("--git-dir")
            .arg(&metadata)
            .arg("remote")
            .output()
            .unwrap();
        assert!(remotes.stdout.is_empty());
        assert!(
            !std::process::Command::new("git")
                .arg("-C")
                .arg(dir.path())
                .args(["rev-parse", "--verify", "HEAD"])
                .output()
                .unwrap()
                .status
                .success()
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("source.txt")).unwrap(),
            "source\n"
        );
    }
    #[tokio::test]
    #[ignore = "requires a running Docker daemon and preinstalled python:3.12-slim"]
    async fn docker_blocks_host_environment_and_network_but_can_write_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let config = SandboxConfig::new(dir.path()).docker_image(Some("python:3.12-slim".into()));
        let code = "import os,socket; assert 'SWARM_ISOLATION_CANARY' not in os.environ; assert not os.path.exists('/var/run/docker.sock'); s=socket.socket(); s.settimeout(1); result=s.connect_ex(('1.1.1.1',443)); assert result != 0; open('/workspace/proof.txt','w').write('isolated')";
        let mut container =
            container_command(&config, "python3", &["-c".into(), code.into()]).unwrap();
        container.command.env("SWARM_ISOLATION_CANARY", "host-only");
        let output = container.command.output().await.unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("proof.txt")).unwrap(),
            "isolated"
        );
    }
}
