//! Open files in the user's editor and reveal them in the file manager.

use crate::{PlatformError, Result};
use std::path::Path;
use std::process::{Command, Stdio};

fn spawn(argv: &[String]) -> Result<()> {
    use std::os::unix::process::CommandExt;
    Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .map_err(|e| PlatformError::Failed(format!("{}: {e}", argv[0])))?;
    Ok(())
}

fn system_open() -> &'static str {
    if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    }
}

/// argv used to open `path`: the configured editor or the system default.
pub fn open_argv(editor: &[String], path: &Path) -> Vec<String> {
    let mut v: Vec<String> = if editor.is_empty() {
        vec![system_open().into()]
    } else {
        editor.to_vec()
    };
    v.push(path.to_string_lossy().into_owned());
    v
}

/// Open a *local* path. Callers must not pass paths from remote devices.
pub fn open_in_editor(editor: &[String], path: &Path) -> Result<()> {
    if !path.exists() {
        return Err(PlatformError::Failed(format!(
            "{} does not exist",
            path.display()
        )));
    }
    spawn(&open_argv(editor, path))
}

/// Show `path` in Finder / the file manager.
pub fn reveal(path: &Path) -> Result<()> {
    if cfg!(target_os = "macos") {
        return spawn(&[
            "open".into(),
            "-R".into(),
            path.to_string_lossy().into_owned(),
        ]);
    }
    let dir = if path.is_dir() {
        path
    } else {
        path.parent().unwrap_or(path)
    };
    spawn(&["xdg-open".into(), dir.to_string_lossy().into_owned()])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn editor_argv() {
        let p = Path::new("/w/a b.rs");
        assert_eq!(open_argv(&["zed".into()], p), vec!["zed", "/w/a b.rs"]);
        assert_eq!(open_argv(&[], p).len(), 2);
    }
}
