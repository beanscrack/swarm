//! The shell that runs the `bash` / `run_tests` tools, and everything the CLI
//! reuses from it (`!` escapes, hooks, custom provider commands).
//!
//! Both tools are named and documented as shells, and models write POSIX for
//! them (`pwd; ls -la`, `grep -q`, `echo x > out.json`). Unix has `/bin/sh`.
//! Windows does not, but git-bash ships `sh.exe` and puts it on `PATH` on most
//! dev machines — that is the shell the copied-one-liner examples expect, and
//! picking `cmd /C` there turns every model-authored command into
//! `'pwd' is not recognized as an internal or external command` (exit 255) and
//! burns a turn per attempt. So: prefer a POSIX shell, and fall back to
//! `cmd /C` only when the machine truly has none, so the feature degrades to
//! the platform shell instead of dying.
//!
//! `SWARM_SHELL` overrides the choice, e.g. `SWARM_SHELL=bash`.

use std::path::{Path, PathBuf};

/// A program plus the flag that takes a script string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shell {
    pub program: String,
    pub flag: String,
    /// True when the program speaks POSIX (`;` chains, `>&2`, `grep`, `cat`).
    pub posix: bool,
}

impl Shell {
    /// `argv` for running `script`: `[program, flag, script]`.
    pub fn argv(&self, script: &str) -> Vec<String> {
        vec![self.program.clone(), self.flag.clone(), script.to_string()]
    }

    /// A spawned-ready command with the script wired in. The caller sets the
    /// working directory and the stdio.
    pub fn command(&self, script: &str) -> tokio::process::Command {
        let mut parts = self.argv(script);
        let mut cmd = tokio::process::Command::new(&parts[0]);
        cmd.args(&parts[1..]);
        #[cfg(windows)]
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW: never open a terminal window
        cmd
    }

    /// A short label for messages and tests.
    pub fn label(&self) -> String {
        if self.posix { "sh".to_string() } else { "cmd".to_string() }
    }
}

static SHELL: std::sync::OnceLock<Shell> = std::sync::OnceLock::new();

/// The shell for this process, resolved once.
pub fn shell() -> &'static Shell {
    SHELL.get_or_init(detect)
}

/// Whether shell scripts written for a POSIX shell can run here. Tests and
/// features that depend on `;`, `>&2` or `grep` check this first.
pub fn posix_available() -> bool {
    shell().posix
}

fn detect() -> Shell {
    if let Ok(custom) = std::env::var("SWARM_SHELL") {
        let custom = custom.trim();
        if !custom.is_empty() {
            return Shell {
                program: custom.to_string(),
                flag: "-c".to_string(),
                posix: posix_name(custom),
            };
        }
    }
    if cfg!(windows) {
        // Git for Windows' sh first. `bash.exe` on a Windows PATH is usually the WSL
        // launcher in System32: with no distro installed it answers every command
        // with "execvpe(/bin/bash) failed", and with one it runs a different
        // filesystem from the workspace. Neither is the shell a model's POSIX
        // one-liner expects, so it is never picked.
        for candidate in git_shells() {
            if candidate.is_file() {
                return Shell { program: candidate.display().to_string(), flag: "-c".to_string(), posix: true };
            }
        }
        for name in ["sh", "bash", "sh.exe", "bash.exe"] {
            if let Some(found) = which(name) {
                if is_wsl_launcher(&found) {
                    continue;
                }
                return Shell { program: found.display().to_string(), flag: "-c".to_string(), posix: true };
            }
        }
        return Shell { program: "cmd".to_string(), flag: "/C".to_string(), posix: false };
    }
    for candidate in ["/bin/sh", "/bin/bash", "/usr/bin/sh", "/usr/bin/bash"] {
        if Path::new(candidate).is_file() {
            return Shell {
                program: candidate.to_string(),
                flag: "-c".to_string(),
                posix: true,
            };
        }
    }
    Shell { program: "sh".to_string(), flag: "-c".to_string(), posix: true }
}

/// Where Git for Windows keeps its `sh`: next to whatever `git` is on PATH, then
/// the usual install directories.
fn git_shells() -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();
    if let Some(root) = which("git").as_deref().and_then(Path::parent).and_then(Path::parent) {
        roots.push(root.to_path_buf());
    }
    roots.push(PathBuf::from(r"C:\Program Files\Git"));
    roots.push(PathBuf::from(r"C:\Program Files (x86)\Git"));
    roots
        .into_iter()
        .flat_map(|root| [root.join("bin").join("sh.exe"), root.join("usr").join("bin").join("sh.exe")])
        .collect()
}

/// The WSL launcher that Windows puts on PATH as `bash.exe` (System32, or the
/// Store alias under WindowsApps).
pub fn is_wsl_launcher(path: &Path) -> bool {
    let text = path.to_string_lossy().to_ascii_lowercase().replace('/', "\\");
    text.contains("\\windows\\system32\\bash") || text.contains("\\windowsapps\\bash")
}

/// Whether a program name is one of the sh family.
pub fn posix_name(program: &str) -> bool {
    let base = Path::new(program)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| program.to_string());
    let base = base.to_ascii_lowercase();
    base.starts_with("sh") || base.starts_with("bash") || base.starts_with("zsh") || base.starts_with("dash")
}

/// Locate a program on `PATH` (or a path given directly).
pub fn which(name: &str) -> Option<PathBuf> {
    // An absolute or relative path wins without a PATH search.
    if name.contains('/') || name.contains('\\') {
        let path = Path::new(name);
        return path.is_file().then(|| path.to_path_buf());
    }
    let separator = if cfg!(windows) { ';' } else { ':' };
    let extensions: &[&str] = if cfg!(windows) { &["", ".exe", ".cmd", ".bat"] } else { &[""] };
    for dir in std::env::var_os("PATH")?.to_string_lossy().split(separator) {
        if dir.is_empty() {
            continue;
        }
        for ext in extensions {
            let candidate = Path::new(dir).join(format!("{name}{ext}"));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_resolved_shell_is_a_real_program() {
        let shell = shell();
        assert!(!shell.program.is_empty());
        assert!(!shell.flag.is_empty());
        // `cmd` is a bare name, everything else resolves to a path we found.
        assert!(shell.program == "cmd" || which(&shell.program).is_some(), "{shell:?}");
    }

    #[test]
    fn argv_carries_the_script_verbatim() {
        let shell = Shell { program: "sh".into(), flag: "-c".into(), posix: true };
        assert_eq!(shell.argv("echo 'a; b'"), vec!["sh", "-c", "echo 'a; b'"]);
        assert_eq!(shell.label(), "sh");
        assert_eq!(Shell { program: "cmd".into(), flag: "/C".into(), posix: false }.label(), "cmd");
    }

    #[tokio::test]
    async fn the_shell_runs_a_script_and_reports_its_exit_code() {
        // Plain `echo`: every candidate shell (sh family, cmd) understands it.
        let output = shell()
            .command("echo swarm-shell-ok")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .expect("the resolved shell starts")
            .wait_with_output()
            .await
            .expect("the script finishes");
        let text = String::from_utf8_lossy(&output.stdout).into_owned();
        assert!(text.contains("swarm-shell-ok"), "{text:?} shell={:?}", shell());
    }

    #[tokio::test]
    async fn a_posix_shell_runs_the_posix_ones_the_model_writes() {
        if !posix_available() {
            eprintln!("no POSIX shell on this host: `;` chains and `>&2` need one");
            return;
        }
        // The exact shapes that came back as exit 255 under `cmd /C`.
        let output = shell()
            .command("pwd; ls -la >/dev/null; echo ok >&2; grep -q a <<< 'banana' && echo found")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .expect("sh starts")
            .wait_with_output()
            .await
            .expect("the script finishes");
        let out = String::from_utf8_lossy(&output.stdout).into_owned();
        let err = String::from_utf8_lossy(&output.stderr).into_owned();
        assert!(output.status.success(), "stdout={out:?} stderr={err:?}");
        assert!(out.contains("found") && err.contains("ok"), "stdout={out:?} stderr={err:?}");
    }

    #[test]
    fn posix_availability_agrees_with_the_resolved_shell() {
        assert_eq!(shell().posix, posix_available());
        #[cfg(unix)]
        assert!(posix_available(), "unix always has a shell");
    }

    #[test]
    fn the_wsl_launcher_is_recognised_and_git_bash_is_not() {
        assert!(is_wsl_launcher(Path::new(r"C:\WINDOWS\system32\bash.exe")));
        assert!(is_wsl_launcher(Path::new(r"C:\Windows\System32\bash.exe")));
        assert!(is_wsl_launcher(Path::new(r"C:\Users\x\AppData\Local\Microsoft\WindowsApps\bash.exe")));
        assert!(!is_wsl_launcher(Path::new(r"C:\Program Files\Git\bin\bash.exe")));
        assert!(!is_wsl_launcher(Path::new("/bin/bash")));
    }

    #[cfg(windows)]
    #[test]
    fn on_windows_the_chosen_shell_is_never_the_wsl_launcher() {
        // `SWARM_SHELL` is an explicit choice and is honoured as given.
        if std::env::var_os("SWARM_SHELL").is_none() {
            assert!(!is_wsl_launcher(Path::new(&shell().program)), "{:?}", shell());
        }
    }

    #[test]
    fn posix_names_are_recognised() {
        assert!(posix_name("sh"));
        assert!(posix_name("C:\\Program Files\\Git\\bin\\bash.exe"));
        assert!(posix_name("zsh"));
        assert!(!posix_name("cmd"));
        assert!(!posix_name("pwsh"));
    }
}
