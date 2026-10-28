//! The shell resolver lives in `swarm_tools::shell`: the agent's `bash` tool
//! spawns there, and hooks, `!` escapes and custom provider commands have to
//! agree with it, so the CLI re-exports it instead of resolving a second answer.
//!
//! Hooks and shell escapes are shell scripts (`echo a; echo b >&2; exit 2`,
//! `grep -q`), the `bash` tool is documented as a shell and models write POSIX
//! for it, so one resolution rule has to cover both. `SWARM_SHELL` overrides it.

pub use swarm_tools::shell::{posix_available, shell};

// Test-only helpers: `which` proves the resolved program is really on PATH.
#[cfg(test)]
use swarm_tools::shell::which;

#[cfg(test)]
use std::path::Path;

/// A path in the form a shell script understands: forward slashes, drive
/// letter kept. MSYS `sh` writes `cat > C:/Users/x/out.json` to the right file
/// (a `C:\Users\...` argument becomes a filename full of backslashes there),
/// and `cmd` accepts forward slashes in redirections too.
#[cfg(test)]
pub fn script_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn script_paths_use_forward_slashes() {
        assert_eq!(script_path(Path::new(r"C:\Users\x\out.json")), "C:/Users/x/out.json");
        assert_eq!(script_path(Path::new("/tmp/out.json")), "/tmp/out.json");
    }

    #[test]
    fn the_cli_sees_the_same_shell_as_the_bash_tool() {
        let shell = shell();
        assert_eq!(shell.posix, posix_available());
        assert!(shell.program == "cmd" || which(&shell.program).is_some(), "{shell:?}");
    }
}
