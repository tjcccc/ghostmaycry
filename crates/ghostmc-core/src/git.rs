//! Git via the `git` CLI. Read-only: we never take the index lock
//! (`GIT_OPTIONAL_LOCKS=0`) so we cannot interfere with a running agent.

use crate::model::GitIdentity;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::process::Command;

/// Upper bound on diff text returned to clients.
pub const MAX_DIFF_BYTES: usize = 2 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum GitError {
    #[error("git is not installed or not runnable: {0}")]
    Spawn(std::io::Error),
    #[error("git failed: {0}")]
    Failed(String),
    #[error("not a git repository")]
    NotARepo,
    #[error("{0}")]
    Path(#[from] crate::paths::ContainError),
}

fn git(dir: &Path) -> Command {
    let mut c = Command::new("git");
    c.arg("-C")
        .arg(dir)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("LC_ALL", "C")
        .env("GIT_TERMINAL_PROMPT", "0");
    c
}

fn run(mut c: Command) -> Result<Vec<u8>, GitError> {
    let out = c.output().map_err(GitError::Spawn)?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_owned();
        if err.contains("not a git repository") {
            return Err(GitError::NotARepo);
        }
        return Err(GitError::Failed(err));
    }
    Ok(out.stdout)
}

/// Detect repository facts for `dir`; `Ok(None)` when it is not in a repo.
pub fn detect(dir: &Path) -> Result<Option<GitIdentity>, GitError> {
    let mut c = git(dir);
    c.args([
        "rev-parse",
        "--path-format=absolute",
        "--show-toplevel",
        "--git-dir",
        "--git-common-dir",
    ]);
    let out = match run(c) {
        Ok(o) => o,
        Err(GitError::NotARepo) => return Ok(None),
        Err(e) => return Err(e),
    };
    let text = String::from_utf8_lossy(&out);
    let mut lines = text.lines();
    let (Some(root), Some(git_dir), Some(common)) = (lines.next(), lines.next(), lines.next())
    else {
        return Err(GitError::Failed("unexpected rev-parse output".into()));
    };
    let mut c = git(dir);
    c.args(["symbolic-ref", "--quiet", "--short", "HEAD"]);
    let branch = run(c)
        .ok()
        .map(|b| String::from_utf8_lossy(&b).trim().to_owned())
        .filter(|b| !b.is_empty());
    let mut c = git(dir);
    c.args(["rev-parse", "--short", "HEAD"]);
    let head = run(c)
        .ok()
        .map(|b| String::from_utf8_lossy(&b).trim().to_owned())
        .filter(|b| !b.is_empty());
    Ok(Some(GitIdentity {
        root: root.to_owned(),
        common_dir: common.to_owned(),
        is_linked_worktree: crate::paths::normalize(Path::new(git_dir))
            != crate::paths::normalize(Path::new(common)),
        branch,
        head,
    }))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangedFile {
    /// Path relative to the repository root.
    pub path: String,
    /// Original path for renames/copies.
    pub orig_path: Option<String>,
    /// Two-letter XY status as in `git status --short` (`.` = unchanged).
    pub xy: String,
    pub kind: ChangeKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    Modified,
    Added,
    Deleted,
    Renamed,
    Untracked,
    Conflicted,
    Ignored,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitStatus {
    pub branch: Option<String>,
    pub upstream: Option<String>,
    pub ahead: i64,
    pub behind: i64,
    /// Repository-wide working tree changes. Attribution to a particular
    /// agent is *not* implied.
    pub files: Vec<ChangedFile>,
}

fn kind_for(xy: &str) -> ChangeKind {
    let b = xy.as_bytes();
    let has = |c: u8| b.contains(&c);
    if has(b'U') || xy == "AA" || xy == "DD" {
        ChangeKind::Conflicted
    } else if has(b'R') || has(b'C') {
        ChangeKind::Renamed
    } else if has(b'D') {
        ChangeKind::Deleted
    } else if has(b'A') {
        ChangeKind::Added
    } else {
        ChangeKind::Modified
    }
}

/// Parse `git status --porcelain=v2 --branch -z`.
pub fn parse_status_v2(raw: &[u8]) -> GitStatus {
    let mut st = GitStatus::default();
    let mut fields = raw
        .split(|&b| b == 0)
        .map(|f| String::from_utf8_lossy(f).into_owned());
    while let Some(rec) = fields.next() {
        if rec.is_empty() {
            continue;
        }
        if let Some(h) = rec.strip_prefix("# ") {
            if let Some(b) = h.strip_prefix("branch.head ") {
                st.branch = (b != "(detached)").then(|| b.to_owned());
            } else if let Some(u) = h.strip_prefix("branch.upstream ") {
                st.upstream = Some(u.to_owned());
            } else if let Some(ab) = h.strip_prefix("branch.ab ") {
                for part in ab.split_whitespace() {
                    if let Some(a) = part.strip_prefix('+') {
                        st.ahead = a.parse().unwrap_or(0);
                    } else if let Some(b) = part.strip_prefix('-') {
                        st.behind = b.parse().unwrap_or(0);
                    }
                }
            }
            continue;
        }
        let mut it = rec.splitn(2, ' ');
        let tag = it.next().unwrap_or("");
        let rest = it.next().unwrap_or("");
        match tag {
            "1" => {
                // 1 XY sub mH mI mW hH hI path
                let parts: Vec<&str> = rest.splitn(8, ' ').collect();
                if parts.len() == 8 {
                    st.files.push(ChangedFile {
                        path: parts[7].to_owned(),
                        orig_path: None,
                        xy: parts[0].to_owned(),
                        kind: kind_for(parts[0]),
                    });
                }
            }
            "2" => {
                // 2 XY sub mH mI mW hH hI Xscore path NUL origPath
                let parts: Vec<&str> = rest.splitn(9, ' ').collect();
                if parts.len() == 9 {
                    let orig = fields.next();
                    st.files.push(ChangedFile {
                        path: parts[8].to_owned(),
                        orig_path: orig,
                        xy: parts[0].to_owned(),
                        kind: ChangeKind::Renamed,
                    });
                }
            }
            "u" => {
                // u XY sub m1 m2 m3 mW h1 h2 h3 path
                let parts: Vec<&str> = rest.splitn(10, ' ').collect();
                if parts.len() == 10 {
                    st.files.push(ChangedFile {
                        path: parts[9].to_owned(),
                        orig_path: None,
                        xy: parts[0].to_owned(),
                        kind: ChangeKind::Conflicted,
                    });
                }
            }
            "?" => st.files.push(ChangedFile {
                path: rest.to_owned(),
                orig_path: None,
                xy: "??".into(),
                kind: ChangeKind::Untracked,
            }),
            "!" => st.files.push(ChangedFile {
                path: rest.to_owned(),
                orig_path: None,
                xy: "!!".into(),
                kind: ChangeKind::Ignored,
            }),
            _ => {}
        }
    }
    st
}

pub fn status(root: &Path) -> Result<GitStatus, GitError> {
    let mut c = git(root);
    c.args([
        "status",
        "--porcelain=v2",
        "--branch",
        "-z",
        "--untracked-files=all",
    ]);
    Ok(parse_status_v2(&run(c)?))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diff {
    pub text: String,
    pub truncated: bool,
}

fn bounded(mut out: Vec<u8>) -> Diff {
    let truncated = out.len() > MAX_DIFF_BYTES;
    out.truncate(MAX_DIFF_BYTES);
    Diff {
        text: String::from_utf8_lossy(&out).into_owned(),
        truncated,
    }
}

/// Unified diff of the working tree against `HEAD` (or the index when
/// `staged`). `path` limits the diff to one repository-relative file and must
/// stay within the repository. Untracked files are diffed against nothing.
pub fn diff(root: &Path, path: Option<&str>, staged: bool) -> Result<Diff, GitError> {
    if let Some(p) = path {
        // Validate containment (also for deleted files, whose parent must exist).
        let rel = Path::new(p);
        if rel.is_absolute()
            || rel
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err(crate::paths::ContainError::ParentTraversal.into());
        }
        if root.join(rel).exists() {
            crate::paths::resolve_within(root, p)?;
        }
        let mut c = git(root);
        c.args(["ls-files", "--error-unmatch", "--"]).arg(p);
        let tracked = c.output().map(|o| o.status.success()).unwrap_or(false);
        if !tracked && !staged && root.join(rel).is_file() {
            let mut c = git(root);
            c.args([
                "diff",
                "--no-color",
                "--no-ext-diff",
                "--no-index",
                "--",
                "/dev/null",
            ])
            .arg(p);
            // --no-index exits 1 when files differ; that is success here.
            let out = c.output().map_err(GitError::Spawn)?;
            return Ok(bounded(out.stdout));
        }
    }
    let mut c = git(root);
    c.args(["diff", "--no-color", "--no-ext-diff"]);
    if staged {
        c.arg("--cached");
    } else {
        // Compare to HEAD when it exists so staged + unstaged both show.
        let mut probe = git(root);
        probe.args(["rev-parse", "--verify", "--quiet", "HEAD"]);
        if probe.output().map(|o| o.status.success()).unwrap_or(false) {
            c.arg("HEAD");
        }
    }
    c.arg("--");
    if let Some(p) = path {
        c.arg(p);
    }
    Ok(bounded(run(c)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(dir: &Path, args: &[&str]) {
        let ok = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args([
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .output()
            .unwrap();
        assert!(
            ok.status.success(),
            "{:?}",
            String::from_utf8_lossy(&ok.stderr)
        );
    }

    fn repo() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        sh(d.path(), &["init", "-q", "-b", "main"]);
        std::fs::write(d.path().join("a.txt"), "one\n").unwrap();
        sh(d.path(), &["add", "."]);
        sh(d.path(), &["commit", "-q", "-m", "init"]);
        d
    }

    #[test]
    fn parses_porcelain_v2() {
        let raw = b"# branch.oid abc\0# branch.head main\0# branch.upstream origin/main\0# branch.ab +2 -1\0\
1 .M N... 100644 100644 100644 abc abc src/a b.rs\0\
2 R. N... 100644 100644 100644 abc abc R100 new.rs\0old.rs\0\
? untracked.txt\0";
        let s = parse_status_v2(raw);
        assert_eq!(s.branch.as_deref(), Some("main"));
        assert_eq!((s.ahead, s.behind), (2, 1));
        assert_eq!(s.files.len(), 3);
        assert_eq!(s.files[0].path, "src/a b.rs");
        assert_eq!(s.files[1].orig_path.as_deref(), Some("old.rs"));
        assert_eq!(s.files[2].kind, ChangeKind::Untracked);
    }

    #[test]
    fn detects_repo_and_worktree() {
        let d = repo();
        let id = detect(d.path()).unwrap().unwrap();
        assert!(!id.is_linked_worktree);
        assert_eq!(id.branch.as_deref(), Some("main"));
        let wt = d.path().join("wt");
        sh(
            d.path(),
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "feature",
                wt.to_str().unwrap(),
            ],
        );
        let wid = detect(&wt).unwrap().unwrap();
        assert!(wid.is_linked_worktree);
        assert_eq!(wid.common_dir, id.common_dir);
        assert_ne!(wid.root, id.root);
        assert_eq!(wid.branch.as_deref(), Some("feature"));
        let plain = tempfile::tempdir().unwrap();
        assert!(detect(plain.path()).unwrap().is_none());
    }

    #[test]
    fn status_and_diff() {
        let d = repo();
        std::fs::write(d.path().join("a.txt"), "two\n").unwrap();
        std::fs::write(d.path().join("new.txt"), "fresh\n").unwrap();
        let s = status(d.path()).unwrap();
        assert_eq!(s.files.len(), 2);
        let all = diff(d.path(), None, false).unwrap();
        assert!(all.text.contains("+two"));
        let untracked = diff(d.path(), Some("new.txt"), false).unwrap();
        assert!(untracked.text.contains("+fresh"));
        assert!(diff(d.path(), Some("../etc/passwd"), false).is_err());
    }
}
