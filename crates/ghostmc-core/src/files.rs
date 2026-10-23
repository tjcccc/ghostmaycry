//! One-level directory listing for the file browser. Listing is lazy (one
//! directory per request) so huge trees never block, and every path is
//! confined to the project root.

use crate::paths::{ContainError, resolve_within};
use serde::{Deserialize, Serialize};
use std::path::Path;

/// Cap per directory so a pathological folder cannot flood a client.
pub const MAX_ENTRIES: usize = 5000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEntry {
    pub name: String,
    /// Relative to the project root, `/`-separated.
    pub rel_path: String,
    pub is_dir: bool,
    pub is_symlink: bool,
    pub size: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Listing {
    pub entries: Vec<FileEntry>,
    pub truncated: bool,
}

/// List `rel` under `root`. Hidden files and `.gitignore`d entries are
/// skipped unless `show_hidden`. `.git` is always skipped.
pub fn list_dir(root: &Path, rel: &str, show_hidden: bool) -> Result<Listing, ContainError> {
    let root = std::fs::canonicalize(root).map_err(|e| ContainError::Io(e.to_string()))?;
    let dir = resolve_within(&root, rel)?;
    let walker = ignore::WalkBuilder::new(&dir)
        .max_depth(Some(1))
        .hidden(!show_hidden)
        .git_ignore(!show_hidden)
        .git_exclude(!show_hidden)
        .git_global(!show_hidden)
        .ignore(!show_hidden)
        .parents(true)
        .follow_links(false)
        .build();
    let mut entries = Vec::new();
    let mut truncated = false;
    for item in walker.flatten() {
        if item.depth() == 0 {
            continue;
        }
        let name = item.file_name().to_string_lossy().into_owned();
        if name == ".git" {
            continue;
        }
        if entries.len() >= MAX_ENTRIES {
            truncated = true;
            break;
        }
        let ft = item.file_type();
        let is_symlink = ft.is_some_and(|t| t.is_symlink());
        // For symlinks, report the target kind only if it stays in the root.
        let (is_dir, size) = if is_symlink {
            match std::fs::canonicalize(item.path()) {
                Ok(t) if t.starts_with(&root) => {
                    let m = std::fs::metadata(&t).ok();
                    (
                        m.as_ref().is_some_and(|m| m.is_dir()),
                        m.filter(|m| m.is_file()).map(|m| m.len()),
                    )
                }
                _ => (false, None),
            }
        } else {
            let m = item.metadata().ok();
            (
                ft.is_some_and(|t| t.is_dir()),
                m.filter(|m| m.is_file()).map(|m| m.len()),
            )
        };
        let rel_path = item
            .path()
            .strip_prefix(&root)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| name.clone());
        entries.push(FileEntry {
            name,
            rel_path,
            is_dir,
            is_symlink,
            size,
        });
    }
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    Ok(Listing { entries, truncated })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_respecting_gitignore_and_hidden() {
        let d = tempfile::tempdir().unwrap();
        let r = d.path();
        std::process::Command::new("git")
            .arg("init")
            .arg("-q")
            .arg(r)
            .status()
            .unwrap();
        std::fs::write(r.join(".gitignore"), "target/\n*.log\n").unwrap();
        std::fs::create_dir_all(r.join("src/inner")).unwrap();
        std::fs::create_dir_all(r.join("target")).unwrap();
        std::fs::write(r.join("src/main.rs"), "fn main(){}").unwrap();
        std::fs::write(r.join("debug.log"), "x").unwrap();
        std::fs::write(r.join(".env"), "SECRET=1").unwrap();
        std::fs::write(r.join("README.md"), "hi").unwrap();

        let l = list_dir(r, "", false).unwrap();
        let names: Vec<_> = l.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["src", "README.md"]);

        let l = list_dir(r, "", true).unwrap();
        let names: Vec<_> = l.entries.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"target") && names.contains(&".env") && !names.contains(&".git"));

        let l = list_dir(r, "src", false).unwrap();
        assert_eq!(l.entries[0].rel_path, "src/inner");
        assert!(l.entries[0].is_dir);
        assert!(list_dir(r, "../", false).is_err());
    }

    #[test]
    fn escaping_symlinks_are_not_followed() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().join("root");
        std::fs::create_dir(&root).unwrap();
        std::os::unix::fs::symlink("/etc", root.join("etc")).unwrap();
        let l = list_dir(&root, "", true).unwrap();
        assert!(l.entries[0].is_symlink && !l.entries[0].is_dir);
        assert!(list_dir(&root, "etc", true).is_err());
    }
}
