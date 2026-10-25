# CLAUDE.md

Ghost May Cry (`ghostmc`): Rust workspace for a local-first control center over tmux sessions, Ghostty and CLI coding agents. Architecture, identities and the security model are in `docs/adr/0001-architecture.md`; read it before changing session matching, attach, hooks, IPC or remote access. Keep `docs/MVP.md` evidence and `README.md` claims in sync with behavior. `DEVLOG.md` is the newest-first change log: read its top entries for recent history and add one per checkpoint (what shipped, what was verified, what's open). `CHANGELOG.md` is the user-facing summary per version: update it when users would notice a change.

## Build and test

- `cargo build` / `cargo test` cover the default members only. The GPUI app is excluded on purpose; use `cargo build -p ghostmc-desktop` or `--workspace` when touching it.
- `Cargo.lock` pins `libc` to 0.2.186: GPUI 0.2.2's `xattr` breaks on newer `libc`. Never run a blanket `cargo update`; after any dependency change, re-run `cargo update -p libc --precise 0.2.186` if needed and confirm `cargo build -p ghostmc-desktop`.
- tmux tests use private sockets and must never touch the user's default server. Manual runs: set `GHOSTMC_HOME` and `TMUX_TMPDIR` to scratch dirs.
- Remote attach changes: run `scripts/verify_remote_attach.py --device <name>` (works against `ssh localhost`) and record the result in `docs/MVP.md`.
- Gate before committing: `cargo fmt --all --check`, `cargo clippy --workspace --all-targets --locked -- -D warnings`, `cargo test --workspace --locked`.

## Crate boundaries

- `ghostmc-core` and `ghostmc-tmux` depend on no other workspace crate.
- `ghostmc-platform` (Ghostty, editor, notifications) is GUI-session code: only the CLI, desktop and desktop-side helpers may call it, never the host's request handling. `ghostmc-protocol` should not depend on it (currently violated via `Capabilities`/`GhosttyTerminal`; see the health-check plan).
- Request logic lives once in `ghostmc-host::Service`; the CLI and desktop are clients. Policy shared by CLI and desktop (for example which candidate "open" acts on) belongs in `ghostmc-host::desktop` (or `ghostmc-host::workspace` for the panel's grouping), not in either UI.

## Invariants

- Identity: a tmux session is `SessionRef` (socket + server pid + start time + `$id`). Never identify sessions by name or index; a cwd match is only a candidate the user must choose; only the `@ghostmc_location` tag or a managed record is verified. Ghostty terminals link to tmux only by exact `tty` match.
- Never create a session or start a command unless the user asked; never restart an agent; never use `send-keys`. Observe means `-f read-only,ignore-size`.
- Pass external values as argv. Shell strings exist only for the remote side of `ssh` and Ghostty's `command`, built with `ghostmc_core::shell::quote`/`join`. AppleScript takes values via `argv`, never interpolation.
- SSH: never relax host-key checking; metadata calls use `BatchMode=yes`; reject targets starting with `-`. `ghostmc api` stays read-only unless `--allow-write`.
- `ghostmc hook ingest` must never write to stdout and always exits 0. Never persist prompts or assistant messages; notification text stays truncated; the end-of-turn brief lives only in host memory and never leaves the device through `ghostmc api`.
- Hook config edits touch only handlers containing `hook ingest --agent`, back up first, and refuse invalid JSON.
- Schema changes: append a migration in `store.rs` and bump `SCHEMA_VERSION`; protocol changes: same major stays wire-compatible, otherwise bump the major.
