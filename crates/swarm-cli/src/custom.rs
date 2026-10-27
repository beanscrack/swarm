//! User-defined slash commands: a markdown file whose body is a prompt.
//!
//! `.swarm/commands/fix-tests.md` becomes `/fix-tests`; a subdirectory becomes a
//! namespace (`review/api.md` is `/review:api`). Directories, most specific
//! first:
//!
//! * `<store>/commands` (default `./.swarm/commands`), then `<workspace>/.swarm/commands`
//! * `<workspace>/.claude/commands`, so a project's Claude Code commands just work
//! * `~/.swarm/commands`, then `~/.claude/commands`
//!
//! A file may start with `---` front matter holding `description:` and
//! `argument-hint:`. In the body, `$ARGUMENTS` is everything typed after the
//! command, `$1`..`$9` are its words, and `@path` attaches a file as it does in
//! the prompt. A command whose body uses none of them gets the arguments
//! appended. Built-in commands always win a name clash.

use std::path::{Path, PathBuf};
use std::sync::RwLock;

use crate::ui::registry::{self, Group, SlashSpec};

const MAX_COMMANDS: usize = 300;
const MAX_FILE_BYTES: u64 = 64 * 1024;

/// A command found on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command {
    pub name: String,
    pub description: String,
    pub hint: String,
    pub path: PathBuf,
    /// "project" or "user", as the menu labels it.
    pub scope: &'static str,
}

/// What a scan found.
#[derive(Debug, Default)]
pub struct Scan {
    pub commands: Vec<Command>,
    /// Files that were ignored, and why.
    pub skipped: Vec<(String, String)>,
}

static LOADED: RwLock<Vec<Command>> = RwLock::new(Vec::new());

/// The directories to scan, most specific first.
pub fn dirs(store: &Path, workspace: &Path) -> Vec<(PathBuf, &'static str)> {
    let mut out = vec![
        (store.join("commands"), "project"),
        (workspace.join(".swarm/commands"), "project"),
        (workspace.join(".claude/commands"), "project"),
    ];
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        out.push((home.join(".swarm/commands"), "user"));
        out.push((home.join(".claude/commands"), "user"));
    }
    let mut seen = Vec::new();
    out.retain(|(dir, _)| {
        let key = dir.canonicalize().unwrap_or_else(|_| dir.clone());
        if seen.contains(&key) {
            false
        } else {
            seen.push(key);
            true
        }
    });
    out
}

/// Read every command under `dirs`. Earlier directories win a name clash.
pub fn scan(dirs: &[(PathBuf, &'static str)]) -> Scan {
    let mut scan = Scan::default();
    for (dir, scope) in dirs {
        let mut files = Vec::new();
        collect(dir, dir, &mut files);
        files.sort();
        for file in files {
            let Some(name) = name_of(dir, &file) else { continue };
            let shown = format!("/{name}");
            if registry::COMMANDS.iter().any(|spec| spec.name == name || spec.aliases.contains(&name.as_str())) {
                scan.skipped.push((shown, "a built-in command has that name".into()));
                continue;
            }
            if scan.commands.iter().any(|existing| existing.name == name) {
                continue;
            }
            if scan.commands.len() >= MAX_COMMANDS {
                scan.skipped.push((shown, format!("more than {MAX_COMMANDS} commands")));
                continue;
            }
            match std::fs::metadata(&file) {
                Ok(meta) if meta.len() > MAX_FILE_BYTES => {
                    scan.skipped.push((shown, "file is over 64 KB".into()));
                    continue;
                }
                Err(error) => {
                    scan.skipped.push((shown, error.to_string()));
                    continue;
                }
                Ok(_) => {}
            }
            let Ok(text) = std::fs::read_to_string(&file) else {
                scan.skipped.push((shown, "not readable as text".into()));
                continue;
            };
            let (front, body) = split_front_matter(&text);
            let description = front
                .description
                .filter(|text| !text.is_empty())
                .or_else(|| first_line(body))
                .unwrap_or_else(|| "custom command".to_string());
            scan.commands.push(Command {
                name,
                description,
                hint: front.hint.unwrap_or_default(),
                path: file,
                scope,
            });
        }
    }
    scan.commands.sort_by(|a, b| a.name.cmp(&b.name));
    scan
}

fn collect(root: &Path, dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        // Depth is bounded by the path length; symlinked directories are not followed.
        match entry.file_type() {
            Ok(kind) if kind.is_dir() && path.starts_with(root) => collect(root, &path, out),
            Ok(kind) if kind.is_file() && path.extension().is_some_and(|ext| ext == "md") => out.push(path),
            _ => {}
        }
    }
}

/// `dir/review/api.md` -> `review:api`; `None` for a name that cannot be typed.
fn name_of(dir: &Path, file: &Path) -> Option<String> {
    let relative = file.strip_prefix(dir).ok()?.with_extension("");
    let parts: Vec<String> = relative.components().map(|part| part.as_os_str().to_string_lossy().to_lowercase()).collect();
    let name = parts.join(":");
    let valid = !name.is_empty()
        && name.chars().all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.' | ':'))
        && !name.starts_with(':')
        && !name.ends_with(':')
        && !name.contains("::");
    valid.then_some(name)
}

#[derive(Debug, Default)]
struct FrontMatter {
    description: Option<String>,
    hint: Option<String>,
}

/// Peel `---` front matter off a command file.
fn split_front_matter(text: &str) -> (FrontMatter, &str) {
    let mut front = FrontMatter::default();
    let Some(rest) = text.strip_prefix("---\n").or_else(|| text.strip_prefix("---\r\n")) else {
        return (front, text);
    };
    let mut consumed = 0;
    for line in rest.split_inclusive('\n') {
        consumed += line.len();
        let trimmed = line.trim();
        if trimmed == "---" {
            return (front, &rest[consumed..]);
        }
        if let Some((key, value)) = trimmed.split_once(':') {
            let value = value.trim().trim_matches(|ch| ch == '"' || ch == '\'').to_string();
            match key.trim() {
                "description" => front.description = Some(value),
                "argument-hint" | "argument_hint" => front.hint = Some(value),
                _ => {}
            }
        }
    }
    // No closing fence: it was not front matter after all.
    (FrontMatter::default(), text)
}

fn first_line(body: &str) -> Option<String> {
    let line = body.lines().map(|line| line.trim().trim_start_matches('#').trim()).find(|line| !line.is_empty())?;
    Some(crate::chat::clip(line, 70))
}

/// Make `commands` the custom commands the menu offers and `/name` runs.
pub fn install(commands: Vec<Command>) {
    let specs: Vec<&'static SlashSpec> = commands
        .iter()
        .map(|command| {
            let desc = format!("{} ({})", command.description, command.scope);
            // A handful of short strings per reload; the table wants 'static.
            let spec = SlashSpec {
                name: Box::leak(command.name.clone().into_boxed_str()),
                aliases: &[],
                args: Box::leak(command.hint.clone().into_boxed_str()),
                desc: Box::leak(desc.into_boxed_str()),
                group: Group::Custom,
            };
            &*Box::leak(Box::new(spec))
        })
        .collect();
    registry::set_custom(specs);
    if let Ok(mut loaded) = LOADED.write() {
        *loaded = commands;
    }
}

/// The installed command called `name`.
pub fn find(name: &str) -> Option<Command> {
    LOADED.read().ok()?.iter().find(|command| command.name == name).cloned()
}

/// The prompt template, without its front matter. Read fresh each time, so an
/// edit takes effect on the next use.
pub fn body_of(command: &Command) -> std::io::Result<String> {
    let text = std::fs::read_to_string(&command.path)?;
    Ok(split_front_matter(&text).1.trim().to_string())
}

/// Fill a template in: `$ARGUMENTS`, `$1`..`$9`. One pass, so text the user
/// typed is never itself expanded.
pub fn expand(body: &str, args: &str) -> String {
    let words = split_words(args);
    let mut out = String::with_capacity(body.len() + args.len());
    let mut used = false;
    let mut rest = body;
    while let Some(at) = rest.find('$') {
        out.push_str(&rest[..at]);
        let after = &rest[at + 1..];
        if let Some(tail) = after.strip_prefix("ARGUMENTS") {
            out.push_str(args);
            used = true;
            rest = tail;
        } else if let Some(digit) = after.chars().next().filter(|ch| ('1'..='9').contains(ch)) {
            out.push_str(words.get(digit as usize - '1' as usize).map_or("", String::as_str));
            used = true;
            rest = &after[1..];
        } else {
            out.push('$');
            rest = after;
        }
    }
    out.push_str(rest);
    if !used && !args.is_empty() {
        out.push_str(&format!("\n\nARGUMENTS: {args}"));
    }
    out
}

/// Split on whitespace, keeping `"quoted words"` together.
fn split_words(text: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut started = false;
    for ch in text.chars() {
        match quote {
            Some(open) if ch == open => quote = None,
            Some(_) => current.push(ch),
            None if ch == '"' || ch == '\'' => {
                quote = Some(ch);
                started = true;
            }
            None if ch.is_whitespace() => {
                if started || !current.is_empty() {
                    words.push(std::mem::take(&mut current));
                    started = false;
                }
            }
            None => current.push(ch),
        }
    }
    if started || !current.is_empty() {
        words.push(current);
    }
    words
}

/// The installed list is process-wide, so tests that install take this first.
#[cfg(test)]
pub(crate) static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, text: &str) {
        let path = dir.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    #[test]
    fn arguments_fill_the_template_once() {
        assert_eq!(expand("fix $ARGUMENTS now", "the bug"), "fix the bug now");
        assert_eq!(expand("compare $1 with $2", "a.rs b.rs"), "compare a.rs with b.rs");
        assert_eq!(expand("only $1 and $3", "x"), "only x and ");
        assert_eq!(expand("review the diff", ""), "review the diff");
        assert_eq!(expand("review the diff", "src/lib.rs"), "review the diff\n\nARGUMENTS: src/lib.rs");
        assert_eq!(expand("cost is $5.00 and $", ""), "cost is .00 and $", "$1..$9 are always placeholders");
        // What the user typed is not expanded a second time.
        assert_eq!(expand("run $ARGUMENTS", "$1 $ARGUMENTS"), "run $1 $ARGUMENTS");
        assert_eq!(expand("$1", "\"two words\" next"), "two words");
    }

    #[test]
    fn front_matter_gives_a_description_and_a_hint() {
        let (front, body) = split_front_matter("---\ndescription: \"Fix it\"\nargument-hint: [file]\nmodel: x\n---\nDo it.\n");
        assert_eq!(front.description.as_deref(), Some("Fix it"));
        assert_eq!(front.hint.as_deref(), Some("[file]"));
        assert_eq!(body, "Do it.\n");
        let (front, body) = split_front_matter("no fence\ndescription: nope\n");
        assert!(front.description.is_none() && body.starts_with("no fence"));
        let (front, body) = split_front_matter("---\ndescription: unclosed\nbody\n");
        assert!(front.description.is_none(), "an unclosed fence is just text");
        assert!(body.starts_with("---"));
    }

    #[test]
    fn files_become_commands_and_folders_become_namespaces() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "fix-tests.md", "---\ndescription: Make the tests pass\n---\nRun the tests and fix failures.");
        write(dir.path(), "review/api.md", "# Review the API\nLook at the public surface.");
        write(dir.path(), "notes.txt", "not a command");
        write(dir.path(), ".hidden.md", "hidden");
        write(dir.path(), "Bad Name.md", "spaces cannot be typed");
        write(dir.path(), "help.md", "shadows a built-in");
        let found = scan(&[(dir.path().to_path_buf(), "project")]);
        let names: Vec<&str> = found.commands.iter().map(|command| command.name.as_str()).collect();
        assert_eq!(names, ["fix-tests", "review:api"]);
        assert_eq!(found.commands[0].description, "Make the tests pass");
        assert_eq!(found.commands[1].description, "Review the API", "falls back to the first line");
        assert!(found.skipped.iter().any(|(name, why)| name == "/help" && why.contains("built-in")), "{:?}", found.skipped);
    }

    #[test]
    fn the_first_directory_wins_a_clash() {
        let project = tempfile::tempdir().unwrap();
        let user = tempfile::tempdir().unwrap();
        write(project.path(), "ship.md", "project version");
        write(user.path(), "ship.md", "user version");
        write(user.path(), "extra.md", "only in user");
        let found = scan(&[(project.path().to_path_buf(), "project"), (user.path().to_path_buf(), "user")]);
        assert_eq!(found.commands.len(), 2);
        let ship = found.commands.iter().find(|command| command.name == "ship").unwrap();
        assert_eq!(ship.scope, "project");
        assert_eq!(body_of(ship).unwrap(), "project version");
    }

    #[test]
    fn installed_commands_reach_the_menu_and_resolve() {
        let _lock = TEST_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "zzcustom-demo.md", "---\ndescription: A demo\nargument-hint: <thing>\n---\nDemo $ARGUMENTS");
        let found = scan(&[(dir.path().to_path_buf(), "project")]);
        install(found.commands);
        let spec = match registry::resolve("zzcustom-demo") {
            registry::Resolved::Exact(spec) => spec,
            other => panic!("{other:?}"),
        };
        assert_eq!(spec.group, Group::Custom);
        assert_eq!(spec.args, "<thing>");
        assert_eq!(spec.desc, "A demo (project)");
        assert!(registry::matching("zzcustom").iter().any(|spec| spec.name == "zzcustom-demo"));
        let command = find("zzcustom-demo").expect("installed");
        assert_eq!(expand(&body_of(&command).unwrap(), "now"), "Demo now");
        install(Vec::new());
        assert!(matches!(registry::resolve("zzcustom-demo"), registry::Resolved::Unknown(_)));
    }
}
