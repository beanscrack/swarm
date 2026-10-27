//! `@path` mentions: attach workspace files to a request.
//!
//! The words stay in the message as typed; the files' contents are appended
//! under a heading so the agent has them without spending a tool call. Paths
//! are resolved inside the workspace only, and obviously secret files are
//! refused rather than quietly sent to a model.

use std::path::Path;

use swarm_tools::resolve_within;

/// Mentions attached per message.
const MAX_FILES: usize = 8;
/// Bytes read from any one file.
const MAX_FILE_BYTES: usize = 64 * 1024;
/// Entries listed for a mentioned directory.
const MAX_DIR_ENTRIES: usize = 60;

/// The result of expanding a message.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Expansion {
    /// The message with attachments appended (the text when nothing attached).
    pub text: String,
    /// One line per mention that was attached, or refused.
    pub notes: Vec<String>,
    /// How many files or folders were attached.
    pub attached: usize,
}

/// Files that must never be attached on a mention.
fn is_secret(path: &str) -> bool {
    let lower = path.to_lowercase();
    let name = lower.rsplit('/').next().unwrap_or(&lower);
    name == ".env"
        || name.starts_with(".env.")
        || name == "id_rsa"
        || name == "id_ed25519"
        || name.ends_with(".pem")
        || name.ends_with(".key")
        || name == "credentials"
        || name == ".netrc"
        || lower.contains(".ssh/")
        || lower.contains(".aws/")
        || lower.contains(".git/")
}

/// The `@word` tokens in `text` (a mention starts a word, so an email address
/// is not one).
fn mentions(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for word in text.split_whitespace() {
        let Some(rest) = word.strip_prefix('@') else { continue };
        let path = rest.trim_end_matches([',', ';', ':', ')', '.', '?', '!', '"', '\'']);
        if path.is_empty() || out.iter().any(|seen| seen == path) {
            continue;
        }
        out.push(path.to_string());
    }
    out
}

/// Attach the files `text` mentions. Mentions that are not files in the
/// workspace are left alone: `@alice` is probably a person.
pub fn expand(workspace: &Path, text: &str) -> Expansion {
    let mut expansion = Expansion { text: text.to_string(), ..Expansion::default() };
    let mut blocks = String::new();
    for path in mentions(text) {
        if expansion.attached >= MAX_FILES {
            expansion.notes.push(format!("skipped @{path}: at most {MAX_FILES} attachments per message"));
            continue;
        }
        let Ok(abs) = resolve_within(workspace, &path) else { continue };
        let Ok(meta) = std::fs::metadata(&abs) else { continue };
        if is_secret(&path) {
            expansion.notes.push(format!("refused @{path}: it looks like a secret"));
            continue;
        }
        if meta.is_dir() {
            let mut names: Vec<String> = std::fs::read_dir(&abs)
                .map(|read| {
                    read.filter_map(Result::ok)
                        .map(|entry| {
                            let name = entry.file_name().to_string_lossy().into_owned();
                            if entry.file_type().is_ok_and(|kind| kind.is_dir()) { format!("{name}/") } else { name }
                        })
                        .collect()
                })
                .unwrap_or_default();
            names.sort();
            let total = names.len();
            names.truncate(MAX_DIR_ENTRIES);
            blocks.push_str(&format!("\n--- {path} (directory, {total} entries) ---\n{}\n", names.join("\n")));
            expansion.notes.push(format!("attached @{path} ({total} entries)"));
            expansion.attached += 1;
            continue;
        }
        let Ok(bytes) = std::fs::read(&abs) else { continue };
        if bytes.contains(&0) {
            expansion.notes.push(format!("skipped @{path}: it is a binary file"));
            continue;
        }
        let cut = bytes.len() > MAX_FILE_BYTES;
        let body = String::from_utf8_lossy(&bytes[..bytes.len().min(MAX_FILE_BYTES)]).into_owned();
        let lines = body.lines().count();
        blocks.push_str(&format!("\n--- {path} ---\n{body}"));
        if !body.ends_with('\n') {
            blocks.push('\n');
        }
        if cut {
            blocks.push_str("[file truncated]\n");
        }
        expansion.notes.push(format!(
            "attached @{path} ({lines} line{}{})",
            if lines == 1 { "" } else { "s" },
            if cut { ", truncated" } else { "" }
        ));
        expansion.attached += 1;
    }
    if expansion.attached > 0 {
        expansion.text = format!("{text}\n\nAttached files (from @ mentions):\n{blocks}");
    }
    expansion
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/main.rs"), "fn main() {}\n").unwrap();
        std::fs::write(dir.path().join("notes.md"), "no trailing newline").unwrap();
        std::fs::write(dir.path().join(".env"), "SECRET=1\n").unwrap();
        std::fs::write(dir.path().join("blob.bin"), [0u8, 1, 2, 3]).unwrap();
        dir
    }

    #[test]
    fn a_mentioned_file_is_attached_after_the_message() {
        let dir = workspace();
        let got = expand(dir.path(), "explain @src/main.rs please");
        assert_eq!(got.attached, 1);
        assert!(got.text.starts_with("explain @src/main.rs please\n\nAttached files"), "{}", got.text);
        assert!(got.text.contains("--- src/main.rs ---\nfn main() {}\n"), "{}", got.text);
        assert_eq!(got.notes, vec!["attached @src/main.rs (1 line)"]);
    }

    #[test]
    fn punctuation_after_a_mention_is_not_part_of_the_path() {
        let dir = workspace();
        let got = expand(dir.path(), "look at @notes.md, then @src/main.rs.");
        assert_eq!(got.attached, 2, "{:?}", got.notes);
        assert!(got.text.contains("no trailing newline\n"), "a missing final newline is added: {}", got.text);
    }

    #[test]
    fn unknown_mentions_are_left_alone() {
        let dir = workspace();
        let got = expand(dir.path(), "ask @alice about it, mail bob@example.com");
        assert_eq!(got, Expansion { text: "ask @alice about it, mail bob@example.com".into(), notes: vec![], attached: 0 });
    }

    #[test]
    fn secrets_are_refused_and_said_so() {
        let dir = workspace();
        let got = expand(dir.path(), "read @.env");
        assert_eq!(got.attached, 0);
        assert_eq!(got.text, "read @.env");
        assert_eq!(got.notes, vec!["refused @.env: it looks like a secret"]);
        for secret in ["id_rsa", "deploy.pem", "x/.ssh/config", ".aws/credentials", "a/.git/config", ".env.production"] {
            assert!(is_secret(secret), "{secret}");
        }
        assert!(!is_secret("src/environment.rs") && !is_secret("keys.md"));
    }

    #[test]
    fn paths_cannot_escape_the_workspace() {
        let dir = workspace();
        let got = expand(dir.path(), "read @../../etc/hostname and @/etc/hostname");
        assert_eq!(got.attached, 0, "{:?}", got.notes);
    }

    #[test]
    fn binary_files_are_skipped() {
        let dir = workspace();
        let got = expand(dir.path(), "see @blob.bin");
        assert_eq!(got.attached, 0);
        assert_eq!(got.notes, vec!["skipped @blob.bin: it is a binary file"]);
    }

    #[test]
    fn big_files_are_cut_and_marked() {
        let dir = workspace();
        std::fs::write(dir.path().join("big.txt"), "line\n".repeat(40_000)).unwrap();
        let got = expand(dir.path(), "@big.txt");
        assert!(got.text.contains("[file truncated]"));
        assert!(got.text.len() < 70_000, "{} bytes", got.text.len());
        assert!(got.notes[0].contains("truncated"));
    }

    #[test]
    fn a_directory_is_listed_not_dumped() {
        let dir = workspace();
        let got = expand(dir.path(), "what is in @src");
        assert_eq!(got.attached, 1);
        assert!(got.text.contains("--- src (directory, 1 entries) ---\nmain.rs"), "{}", got.text);
    }

    #[test]
    fn there_is_a_cap_on_attachments_and_repeats_count_once() {
        let dir = workspace();
        for n in 0..12 {
            std::fs::write(dir.path().join(format!("f{n}.txt")), "x").unwrap();
        }
        let message: String = (0..12).map(|n| format!("@f{n}.txt ")).collect();
        let got = expand(dir.path(), &message);
        assert_eq!(got.attached, MAX_FILES);
        assert!(got.notes.iter().any(|note| note.contains("at most 8")));
        let repeated = expand(dir.path(), "@notes.md and again @notes.md");
        assert_eq!(repeated.attached, 1);
    }
}
