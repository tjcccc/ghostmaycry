# Changelog

User-facing changes in each version of Ghost May Cry, newest first. The detailed development log is [DEVLOG.md](DEVLOG.md); what is verified, and how, is in [docs/MVP.md](docs/MVP.md).

The `ghostmc` CLI, the host and the desktop app share one registry, so update them together.

## [0.2.0] - 2026-10-25

### Added

- On macOS, the desktop app lives in the menu bar: a ghost icon opens a panel under it, with no Dock icon. Clicking elsewhere closes it, and right-clicking the icon offers Quit.
- Workspaces. Each saved project is a workspace, and every other open Ghostty window shows up as one named after its first tab. Rows show the tab's folder, git branch and a Claude Code (✳) or Codex (◆) mark. A 🔔 and a dot on the menu-bar icon show when an agent finished or needs approval.
- Clicking a workspace opens a second card with its agents and the start of each agent's last reply. Click an agent to jump to its tab. **Open / Focus terminal** and **Open Folder** act on the whole workspace.
- Right-click a workspace to Pin, Rename (with input-method support) or Remove it. Pinning or renaming an open Ghostty window saves it as a project.
- **+** opens a new Ghostty window in a folder you choose under Settings.
- On macOS 26 and later the panel uses Liquid Glass, and the right-click menu is a native macOS menu.
- `scripts/build_macos_app.sh` builds a `Ghost May Cry.app` bundle.

### Changed

- The desktop app's four-tab window (projects, sessions, agents, diff and files) is replaced by the panel. The CLI still covers those views.
- Agent hooks record which terminal an agent runs in, so an agent can be matched to its Ghostty tab. An agent whose terminal has closed shows as disconnected.

### Fixed

- Listing Ghostty terminals failed on Ghostty 1.3.1, which doesn't report a terminal's tty or pid.

### Upgrading

- The first 0.2.0 run upgrades the registry in place. Version 0.1 binaries refuse it afterwards, so reinstall the CLI and the desktop app together (`cargo install --path crates/ghostmc-cli` and `--path crates/ghostmc-desktop`), and restart a running `ghostmc host`.
- An agent's last reply appears in the panel only while `ghostmc host run` is running. It is kept in memory and never written to disk.
- With Ghostty 1.3.1, agents are matched to tabs by folder, and only when exactly one tab has that folder.

## [0.1.0] - 2026-10-23

First version.

### Added

- `ghostmc` CLI: a project registry with locations on several devices, plus tmux session discovery, creation, focus and reattach. A session is never matched by its name.
- Agent status for Claude Code and Codex through hooks (`ghostmc hook install`), with states from running and needs-approval to ended. Prompts and replies are never stored.
- A background host (`ghostmc host run`, or as a LaunchAgent or systemd user service) that keeps the CLI and desktop app up to date.
- Continuing work on another device over SSH: `ghostmc attach --device`, read-only with `--observe`.
- Git status and diff, file listing, and opening files in your editor or file manager.
- The GPUI desktop app with projects, sessions, agents, diff and files.
