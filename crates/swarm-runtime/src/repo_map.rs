//! Bounded cached navigation context, never verification evidence.
use serde_json::json;
use std::{
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
    path::Path,
    process::Command,
};

pub fn repository_map(root: &Path, max_chars: usize) -> Option<String> {
    if max_chars == 0 {
        return None;
    }
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["ls-files", "-z"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let files: Vec<_> = output
        .stdout
        .split(|b| *b == 0)
        .filter(|b| !b.is_empty())
        .map(|b| String::from_utf8_lossy(b).into_owned())
        .collect();
    let mut hash = DefaultHasher::new();
    max_chars.hash(&mut hash);
    for file in &files {
        file.hash(&mut hash);
        if let Ok(meta) = std::fs::metadata(root.join(file)) {
            meta.len().hash(&mut hash);
            meta.modified().ok().hash(&mut hash);
        }
    }
    let key = hash.finish().to_string();
    let cache = root.join(".swarm/repo-map.json");
    if let Some(value) = std::fs::read(&cache)
        .ok()
        .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
    {
        if value["key"] == key {
            return value["map"].as_str().map(str::to_string);
        }
    }
    let canonical = std::fs::canonicalize(root).ok()?;
    let mut map = String::from(
        "Repository map (cached navigation aid; inspect actual files before making claims):\n",
    );
    for file in files {
        if map.chars().count() >= max_chars {
            break;
        }
        if file
            .split('/')
            .any(|p| matches!(p, "vendor" | "node_modules" | "target" | ".swarm"))
        {
            continue;
        }
        map.push_str(&file);
        map.push('\n');
        let path = root.join(&file);
        if std::fs::symlink_metadata(&path)
            .ok()
            .is_none_or(|m| m.file_type().is_symlink())
        {
            continue;
        }
        if !std::fs::canonicalize(&path)
            .ok()
            .is_some_and(|p| p.starts_with(&canonical))
        {
            continue;
        }
        if std::fs::metadata(&path)
            .ok()
            .is_some_and(|m| m.len() > 128_000)
        {
            continue;
        }
        if let Ok(text) = std::fs::read_to_string(path) {
            for (index, line) in text
                .lines()
                .enumerate()
                .filter(|(_, l)| {
                    let l = l.trim_start();
                    [
                        "pub fn ",
                        "pub struct ",
                        "pub enum ",
                        "fn ",
                        "func ",
                        "type ",
                        "class ",
                        "def ",
                        "export function ",
                        "export class ",
                    ]
                    .iter()
                    .any(|prefix| l.starts_with(prefix))
                })
                .take(5)
            {
                map.push_str(&format!(
                    "  {}: {}\n",
                    index + 1,
                    line.trim().chars().take(120).collect::<String>()
                ));
            }
        }
    }
    let map: String = map.chars().take(max_chars).collect();
    if let Some(parent) = cache.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(cache, json!({"key":key,"map":map}).to_string());
    Some(map)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn map_is_bounded_cached_and_refreshed_after_edits() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            Command::new("git")
                .arg("init")
                .arg(dir.path())
                .output()
                .unwrap()
                .status
                .success()
        );
        std::fs::write(dir.path().join("a.rs"), "pub fn first() {}\n").unwrap();
        assert!(
            Command::new("git")
                .arg("-C")
                .arg(dir.path())
                .args(["add", "."])
                .output()
                .unwrap()
                .status
                .success()
        );
        let first = repository_map(dir.path(), 1000).unwrap();
        assert!(first.contains("first"));
        assert_eq!(first, repository_map(dir.path(), 1000).unwrap());
        std::fs::write(dir.path().join("a.rs"), "pub fn second_longer() {}\n").unwrap();
        let changed = repository_map(dir.path(), 1000).unwrap();
        assert!(changed.contains("second_longer"));
        assert!(repository_map(dir.path(), 100).unwrap().chars().count() <= 100);
    }
}
