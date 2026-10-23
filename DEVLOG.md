# DEVLOG

Cross-session development log. Newest first. Keep entries short: what shipped, what was verified, what's open.

## 2026-10-23 — v0.1.0 — Repo CLAUDE.md, health check

- Repo `CLAUDE.md`: build traps (desktop crate outside `default-members`, the `libc` 0.2.186 pin, private tmux sockets, the remote attach script, the commit gate), crate boundaries, and the identity, attach, argv, SSH, hook and versioning invariants.
- `repo-health-check` audit; nothing applied yet. Open findings:
  - High: rustfmt runs at the default 100 columns instead of the 120 baseline (no `rustfmt.toml`; 214 hunks).
  - Medium: production `unwrap`/`expect` in `hooks.rs`, `desktop/main.rs` and `install.rs`.
  - Medium: the CLI and desktop decide which candidate "open" acts on in different ways (`open.rs` vs `view.rs`); this belongs in `ghostmc-host::desktop`.
  - Medium: `ghostmc-protocol` depends on `ghostmc-platform` (`Capabilities`, `GhosttyTerminal`).
  - Medium: the envelope guard is duplicated in `cli/main.rs` and `server.rs`.
  - Medium: `view.rs`, `service.rs` and `store.rs` are oversized.
  - Medium: config `kind` and `linux_mode` are free strings.
  - Medium: no `docs/architecture.md` or `spec/ui.md`.
- Session moved from the cloud container to the Mac with `claude --teleport`. The cloud-only state did not come along: the loopback sshd, the smoke-test registry and the cloud `~/.claude`.

## 2026-10-23 — v0.1.0 — First vertical slice: registry, tmux, host, CLI, hooks, SSH remote, GPUI desktop

- Rust 2024 workspace with seven crates: `core`, `tmux`, `protocol`, `platform`, `host`, `cli`, and `desktop` (the GPUI app, excluded from the default members). Architecture, identities and the security model are in `docs/adr/0001-architecture.md`.
- **Registry.** SQLite (rusqlite, bundled) holds projects, device-specific locations, remote links and git/worktree detection. Migrations run through `PRAGMA user_version`, and the store refuses a database with a newer schema.
- **tmux.** A session is identified by socket + server pid + start time + `$id`, never by name. On create, the location, project and token tags are set atomically in the same command list as `new-session`. Creation is idempotent under a per-location flock: six concurrent creators produce one session. A cwd match is only a candidate the user must choose; the CLI exits with code 2 when a choice is needed. Observe uses `-f read-only,ignore-size`.
- **Agents.** `hook install` and `hook uninstall` handle Claude Code and Codex hooks without touching other handlers: they back up the config first and refuse invalid JSON. `hook ingest` attributes events by session token, then tmux pane, then cwd; states run from running and needs-approval through ended, disconnected and stale. Prompts and assistant text are never stored.
- **Host.** `ghostmc host run` serves a Unix socket and checks the peer uid. The CLI falls back to direct mode when no host is running. Remote access uses `ssh -T -o BatchMode=yes … ghostmc api`, which is read-only unless `--allow-write`; remote attach goes through `ssh -t`.
- **Ghostty.** On macOS, AppleScript lists, focuses and launches terminals, taking values only through argv. A terminal is linked to a tmux client only by an exact tty match. On Linux, `ghostty +new-window` is used; Linux has no list, focus or new-tab, so those report as unsupported.
- **Desktop.** The GPUI app shows projects, sessions, agents, the git diff and files, and updates live from hook events.
- Validation: `cargo test --workspace` (77 tests) and clippy clean. `scripts/verify_remote_attach.py --device loop` passed 12/12 over loopback SSH. The desktop app ran under Xvfb (screenshots in `docs/images/`). The host idles at about 16 MB RSS and 0 % CPU. All of this ran on an Ubuntu cloud container. Not run yet: macOS Ghostty and AppleScript, GPUI on macOS, Fedora/Wayland, real Claude Code and Codex binaries firing hooks, and a Tailscale run between two real machines. Details are in `MVP.md`.
