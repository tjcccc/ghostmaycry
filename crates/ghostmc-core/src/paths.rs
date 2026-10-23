//! Path normalization and containment checks.

use std::path::{Component, Path, PathBuf};

/// Make `p` absolute and lexically clean; resolve symlinks when the path
/// exists so that `/tmp/x` and `/private/tmp/x` compare equal on macOS.
pub fn normalize(p: &Path) -> PathBuf {
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|c| c.join(p))
            .unwrap_or_else(|_| p.to_path_buf())
    };
    std::fs::canonicalize(&abs).unwrap_or_else(|_| lexical_clean(&abs))
}

/// Expand a leading `~/`.
pub fn expand_tilde(p: &str) -> PathBuf {
    if p == "~" {
        return ::dirs::home_dir().unwrap_or_else(|| PathBuf::from(p));
    }
    match (p.strip_prefix("~/"), ::dirs::home_dir()) {
        (Some(rest), Some(home)) => home.join(rest),
        _ => PathBuf::from(p),
    }
}

/// Remove `.`/`..` and duplicate separators without touching the filesystem.
pub fn lexical_clean(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// How a candidate directory relates to a project location.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "depth")]
pub enum PathRelation {
    Exact,
    /// Candidate is inside the location, `n` components deeper.
    Descendant(usize),
    /// Candidate contains the location (e.g. a shell at the repo parent).
    Ancestor(usize),
    Unrelated,
}

/// Compare by whole components so `/src/app` is not inside `/src/ap`.
pub fn relation(location: &Path, candidate: &Path) -> PathRelation {
    let loc: Vec<_> = location.components().collect();
    let cand: Vec<_> = candidate.components().collect();
    let common = loc.iter().zip(&cand).take_while(|(a, b)| a == b).count();
    match (common == loc.len(), common == cand.len()) {
        (true, true) => PathRelation::Exact,
        (true, false) => PathRelation::Descendant(cand.len() - loc.len()),
        (false, true) => PathRelation::Ancestor(loc.len() - cand.len()),
        _ => PathRelation::Unrelated,
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ContainError {
    #[error("path must be relative to the project root")]
    Absolute,
    #[error("path may not contain `..`")]
    ParentTraversal,
    #[error("path resolves outside the project root")]
    Escapes,
    #[error("{0}")]
    Io(String),
}

/// Resolve `rel` beneath `root`, refusing absolute paths, `..` and symlinks
/// whose target leaves `root`. Returns the canonical path.
pub fn resolve_within(root: &Path, rel: &str) -> Result<PathBuf, ContainError> {
    let rel_path = Path::new(rel);
    if rel_path.is_absolute() {
        return Err(ContainError::Absolute);
    }
    if rel_path
        .components()
        .any(|c| matches!(c, Component::ParentDir))
    {
        return Err(ContainError::ParentTraversal);
    }
    let root = std::fs::canonicalize(root).map_err(|e| ContainError::Io(e.to_string()))?;
    let joined = root.join(rel_path);
    let canon = std::fs::canonicalize(&joined).map_err(|e| ContainError::Io(e.to_string()))?;
    if !canon.starts_with(&root) {
        return Err(ContainError::Escapes);
    }
    Ok(canon)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relation_uses_components() {
        let loc = Path::new("/src/app");
        assert_eq!(relation(loc, Path::new("/src/app")), PathRelation::Exact);
        assert_eq!(
            relation(loc, Path::new("/src/app/crates/x")),
            PathRelation::Descendant(2)
        );
        assert_eq!(relation(loc, Path::new("/src")), PathRelation::Ancestor(1));
        assert_eq!(relation(loc, Path::new("/src/ap")), PathRelation::Unrelated);
        assert_eq!(
            relation(loc, Path::new("/src/application")),
            PathRelation::Unrelated
        );
    }

    #[test]
    fn lexical_clean_handles_dots() {
        assert_eq!(
            lexical_clean(Path::new("/a/./b/../c//d/")),
            PathBuf::from("/a/c/d")
        );
    }

    #[test]
    fn normalize_resolves_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert_eq!(normalize(&link), normalize(&real));
    }

    #[test]
    fn resolve_within_blocks_escapes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(dir.path().join("secret"), "x").unwrap();
        std::os::unix::fs::symlink(dir.path().join("secret"), root.join("leak")).unwrap();
        std::os::unix::fs::symlink(root.join("sub"), root.join("inner")).unwrap();

        assert!(resolve_within(&root, "sub").is_ok());
        assert!(resolve_within(&root, "").is_ok());
        assert!(resolve_within(&root, "inner").is_ok());
        assert_eq!(
            resolve_within(&root, "../secret"),
            Err(ContainError::ParentTraversal)
        );
        assert_eq!(
            resolve_within(&root, "/etc/passwd"),
            Err(ContainError::Absolute)
        );
        assert_eq!(resolve_within(&root, "leak"), Err(ContainError::Escapes));
    }
}
