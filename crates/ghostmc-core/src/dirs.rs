//! Filesystem locations. `GHOSTMC_HOME` overrides everything (tests, multiple
//! profiles); otherwise platform conventions are used.

use std::io;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

fn home_override() -> Option<PathBuf> {
    std::env::var_os("GHOSTMC_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// Durable data (SQLite registry).
pub fn data_dir() -> PathBuf {
    home_override().unwrap_or_else(|| {
        ::dirs::data_dir()
            .unwrap_or_else(|| PathBuf::from(".")) // no home: degrade to cwd
            .join("ghostmc")
    })
}

/// User-edited preferences.
pub fn config_dir() -> PathBuf {
    home_override().unwrap_or_else(|| {
        ::dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("ghostmc")
    })
}

/// Private runtime directory holding the host socket.
pub fn runtime_dir() -> PathBuf {
    if let Some(h) = home_override() {
        return h.join("run");
    }
    if let Some(xdg) = std::env::var_os("XDG_RUNTIME_DIR").filter(|v| !v.is_empty()) {
        return PathBuf::from(xdg).join("ghostmc");
    }
    data_dir().join("run")
}

pub fn db_path() -> PathBuf {
    data_dir().join("ghostmc.sqlite3")
}

pub fn config_path() -> PathBuf {
    config_dir().join("config.toml")
}

pub fn host_socket_path() -> PathBuf {
    runtime_dir().join("host.sock")
}

/// Create `dir` (mode 0700) and verify it is owned by us and not accessible
/// to group/other. Refuses to use a directory someone else controls.
pub fn ensure_private_dir(dir: &Path) -> io::Result<()> {
    if !dir.exists() {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
    }
    let meta = std::fs::symlink_metadata(dir)?;
    if !meta.is_dir() {
        return Err(io::Error::other(format!(
            "{} is not a directory",
            dir.display()
        )));
    }
    let uid = unsafe { libc::geteuid() };
    if meta.uid() != uid {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "{} is owned by uid {}, not {uid}",
                dir.display(),
                meta.uid()
            ),
        ));
    }
    if meta.mode() & 0o077 != 0 {
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}
