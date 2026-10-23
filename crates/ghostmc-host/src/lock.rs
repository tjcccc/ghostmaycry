//! Advisory file locks (flock) shared by the host and direct-mode CLIs, so
//! concurrent "create session" requests for one location serialize even
//! across processes.

use std::fs::File;
use std::os::fd::AsRawFd;
use std::path::Path;

pub struct FileLock {
    file: File,
}

impl FileLock {
    /// Block until the exclusive lock on `path` is held.
    pub fn acquire(path: &Path) -> std::io::Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path)?;
        loop {
            let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
            if rc == 0 {
                return Ok(Self { file });
            }
            let err = std::io::Error::last_os_error();
            if err.kind() != std::io::ErrorKind::Interrupted {
                return Err(err);
            }
        }
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
    }
}
