//! Workspace file index for `@` mentions: a bounded, cached walk with a
//! forgiving ranker (basename prefix, then substring, then abbreviation).

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const MAX_ENTRIES: usize = 8_000;
const MAX_DEPTH: usize = 8;
/// A huge tree (a home directory) must not freeze the prompt.
const WALK_BUDGET: Duration = Duration::from_millis(150);
const STALE_AFTER: Duration = Duration::from_secs(30);

/// Directories never worth offering.
const SKIP: &[&str] = &[
    "node_modules", "target", "__pycache__", "venv", "dist", "build", "out", "vendor",
];

pub struct FileIndex {
    root: PathBuf,
    entries: Vec<String>,
    built: Option<Instant>,
}

impl FileIndex {
    pub fn new(root: PathBuf) -> Self {
        Self { root, entries: Vec::new(), built: None }
    }

    /// Entries best matching `query`, at most `limit`. Directories end in `/`.
    pub fn search(&mut self, query: &str, limit: usize) -> Vec<String> {
        if self.built.is_none_or(|at| at.elapsed() > STALE_AFTER) {
            self.entries = walk(&self.root);
            self.built = Some(Instant::now());
        }
        rank(&self.entries, query, limit)
    }

    /// Forget the cache (after the agent changed files, say).
    #[cfg(test)]
    pub fn invalidate(&mut self) {
        self.built = None;
    }
}

fn walk(root: &Path) -> Vec<String> {
    let started = Instant::now();
    let mut out = Vec::new();
    let mut queue: VecDeque<(PathBuf, usize)> = VecDeque::from([(root.to_path_buf(), 0)]);
    while let Some((dir, depth)) = queue.pop_front() {
        let Ok(read) = std::fs::read_dir(&dir) else { continue };
        let mut names: Vec<(String, bool)> = read
            .filter_map(Result::ok)
            .filter_map(|entry| {
                let name = entry.file_name().to_string_lossy().into_owned();
                let is_dir = entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false);
                Some((name, is_dir))
            })
            .collect();
        names.sort();
        for (name, is_dir) in names {
            if out.len() >= MAX_ENTRIES || started.elapsed() > WALK_BUDGET {
                return out;
            }
            if is_dir && (name.starts_with('.') || SKIP.contains(&name.as_str())) {
                continue;
            }
            let relative = dir
                .strip_prefix(root)
                .unwrap_or(Path::new(""))
                .join(&name)
                .to_string_lossy()
                .into_owned();
            if is_dir {
                out.push(format!("{relative}/"));
                if depth + 1 < MAX_DEPTH {
                    queue.push_back((dir.join(&name), depth + 1));
                }
            } else {
                out.push(relative);
            }
        }
    }
    out
}

fn rank(entries: &[String], query: &str, limit: usize) -> Vec<String> {
    let query = query.to_lowercase();
    if query.is_empty() {
        let mut shallow: Vec<&String> = entries.iter().collect();
        shallow.sort_by_key(|path| (path.matches('/').count(), path.len()));
        return shallow.into_iter().take(limit).cloned().collect();
    }
    let mut scored: Vec<(u8, usize, &String)> = entries
        .iter()
        .filter_map(|path| {
            let lower = path.to_lowercase();
            let trimmed = lower.trim_end_matches('/');
            let base = trimmed.rsplit('/').next().unwrap_or(trimmed);
            let score = if base.starts_with(&query) {
                0
            } else if base.contains(&query) {
                1
            } else if lower.starts_with(&query) {
                2
            } else if lower.contains(&query) {
                3
            } else if is_subsequence(&query, &lower) {
                4
            } else {
                return None;
            };
            Some((score, path.len(), path))
        })
        .collect();
    scored.sort();
    scored.into_iter().take(limit).map(|(_, _, path)| path.clone()).collect()
}

fn is_subsequence(needle: &str, haystack: &str) -> bool {
    let mut chars = haystack.chars();
    needle.chars().all(|want| chars.any(|have| have == want))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for path in [
            "README.md",
            "src/main.rs",
            "src/ui/editor.rs",
            "src/ui/screen.rs",
            "docs/GUIDE.md",
            "target/debug/junk.o",
            "node_modules/pkg/index.js",
            ".git/config",
            ".hidden/secret",
        ] {
            let file = dir.path().join(path);
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(file, "x").unwrap();
        }
        dir
    }

    #[test]
    fn it_lists_project_files_and_skips_noise() {
        let dir = tree();
        let mut index = FileIndex::new(dir.path().to_path_buf());
        let all = index.search("", 100);
        assert!(all.contains(&"README.md".to_string()), "{all:?}");
        assert!(all.contains(&"src/main.rs".to_string()));
        assert!(all.contains(&"src/".to_string()), "directories end in a slash: {all:?}");
        for noise in ["target/debug/junk.o", "node_modules/pkg/index.js", ".git/config", ".hidden/secret"] {
            assert!(!all.iter().any(|path| path == noise), "{noise} must be skipped: {all:?}");
        }
    }

    #[test]
    fn an_empty_query_prefers_shallow_paths() {
        let dir = tree();
        let mut index = FileIndex::new(dir.path().to_path_buf());
        let first = index.search("", 3);
        assert!(first.iter().all(|path| path.matches('/').count() <= 1), "{first:?}");
    }

    #[test]
    fn ranking_prefers_basename_prefix_over_substring_over_abbreviation() {
        let dir = tree();
        let mut index = FileIndex::new(dir.path().to_path_buf());
        assert_eq!(index.search("edi", 5)[0], "src/ui/editor.rs");
        assert_eq!(index.search("main", 5)[0], "src/main.rs");
        let abbreviated = index.search("sue", 5);
        assert!(abbreviated.contains(&"src/ui/editor.rs".to_string()), "{abbreviated:?}");
        assert!(index.search("README", 5)[0] == "README.md");
        assert!(index.search("zzzzz", 5).is_empty());
    }

    #[test]
    fn matching_ignores_case_and_honours_the_limit() {
        let dir = tree();
        let mut index = FileIndex::new(dir.path().to_path_buf());
        assert_eq!(index.search("guide", 5), vec!["docs/GUIDE.md"]);
        assert_eq!(index.search("", 2).len(), 2);
    }

    #[test]
    fn an_unreadable_root_is_just_empty() {
        let mut index = FileIndex::new(PathBuf::from("/definitely/not/here"));
        assert!(index.search("x", 5).is_empty());
    }

    #[test]
    fn the_cache_is_reused_until_invalidated() {
        let dir = tree();
        let mut index = FileIndex::new(dir.path().to_path_buf());
        index.search("", 1);
        std::fs::write(dir.path().join("new_file.txt"), "x").unwrap();
        assert!(index.search("new_file", 5).is_empty(), "cached");
        index.invalidate();
        assert_eq!(index.search("new_file", 5), vec!["new_file.txt"]);
    }
}
