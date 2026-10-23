//! System notifications.
//!
//! Linux uses `notify-send`; with `--wait` and an action it reports a click,
//! which we turn into navigation. macOS uses `osascript display
//! notification`, which cannot report clicks; navigation there goes through
//! the desktop app instead.

use std::process::{Command, Stdio};

pub fn available() -> bool {
    let bin = if cfg!(target_os = "macos") {
        "osascript"
    } else {
        "notify-send"
    };
    std::env::var_os("PATH")
        .is_some_and(|paths| std::env::split_paths(&paths).any(|d| d.join(bin).is_file()))
}

/// Fire-and-forget notification.
pub fn notify(title: &str, body: &str) {
    let _ = if cfg!(target_os = "macos") {
        Command::new("osascript")
            .args(["-e", "on run argv\ndisplay notification (item 2 of argv) with title (item 1 of argv)\nend run"])
            .args([title, body])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
    } else {
        Command::new("notify-send")
            .args(["--app-name=Ghost May Cry", "--", title, body])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
    };
}

/// Notification with an "Open" action. `on_open` runs on a background thread
/// if the user clicks it (Linux with a libnotify that supports `--wait`).
/// Elsewhere this degrades to [`notify`].
pub fn notify_actionable(title: &str, body: &str, on_open: impl FnOnce() + Send + 'static) {
    if cfg!(target_os = "macos") {
        notify(title, body);
        return;
    }
    let (title, body) = (title.to_owned(), body.to_owned());
    std::thread::spawn(move || {
        let out = Command::new("notify-send")
            .args([
                "--app-name=Ghost May Cry",
                "--wait",
                "--action=open=Open",
                "--",
                &title,
                &body,
            ])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output();
        if let Ok(o) = out
            && String::from_utf8_lossy(&o.stdout).trim() == "open"
        {
            on_open();
        }
    });
}
