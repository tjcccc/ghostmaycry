# MVP checklist

Legend: **✅ works** (verified, evidence given) · **🟡 partial** · **🧪 implemented, not
run** (code exists; the platform wasn't available) · **⏳ deferred**

Development environment for this kickoff: Ubuntu 24.04 (headless) in a cloud
container, Rust 1.97, tmux 3.4, OpenSSH 9.6. No macOS machine, no Fedora
desktop, no Ghostty install and no second physical device were available.
Evidence below says which environment it comes from. v0.2.0 checks were run on
macOS 27 (Apple silicon) with Ghostty 1.3.1, against a scratch `GHOSTMC_HOME`.

## Test inventory

`cargo test --workspace` runs **77 tests**, all passing:

| Crate | Tests | Covers |
|---|---|---|
| ghostmc-core | 45 | ids, POSIX quoting round-tripped through `sh`, path containment and symlink escapes, session matching, config, SQLite migrations and newer-schema refusal, project/device/location CRUD, agent state machine (dedup, out-of-order, evidence ranking, stale message), git (porcelain v2, worktrees, diff), file listing (gitignore, hidden, escapes), hook-config merge/uninstall |
| ghostmc-tmux | 9 | live tmux: discovery, epoch stability, atomic tagging (tags don't leak to other sessions), duplicate names, hostile paths/values, argv commands, private socket directory creation |
| ghostmc-host | 12 | live tmux end-to-end: plan → create → reuse → additional; **6 concurrent creators → exactly 1 session**; cwd candidates vs verified; hook pane attribution and *disconnected* on pane loss; stale keys after server restart; files/git confinement; `PathPlan`; Unix-socket server (0600/0700 modes, second host refused, protocol-major mismatch rejected, long-poll wake-up); read-only API guard; remote argv quoting; Ghostty↔tmux tty matching |
| ghostmc-protocol | 5 | wire format, key round-trip, forward-compatible parsing, read-only classification |
| ghostmc-platform | 5 | AppleScript output parsing, argv-only scripts, honest Linux capabilities, custom terminal receives hostile argv intact |
| ghostmc-cli | 1 | systemd unit / LaunchAgent quoting |

Lints: `cargo clippy --workspace --all-targets` is clean, and `cargo fmt` has
been applied. CI (`.github/workflows/ci.yml`) runs the same steps on Ubuntu and
macOS, but hasn't run yet because nothing has been pushed.

## 1. Workspace, desktop shell, host service

| Item | Status | Evidence |
|---|---|---|
| Rust workspace, host independent of UI | ✅ | `ghostmc host run` serves the Unix socket; the CLI falls back to direct mode when no host is running (smoke test) |
| GPUI desktop app on Linux | ✅ | Ran under Xvfb (X11, llvmpipe Vulkan) with real data: projects, sessions, diff, agents, live update with no user input after a hook event. Screenshots are in `docs/images/` |
| GPUI desktop app on macOS | ✅ | v0.2.0 menu-bar panel driven with real clicks and screenshots on macOS 27, Ghostty 1.3.1, scratch `GHOSTMC_HOME`. Checked: no Dock icon; the icon toggles the panel and an outside click closes it; a live Ghostty window appears as a workspace with its tabs and git branch; the right-click menu covers Pin, Rename (typed text including 测试, saved as a project) and Remove (with confirmation); the right card opens on a workspace click and closes on a second click; list and right card are separate cards sized to their content; the panel widens leftwards and shrinks back under the icon; a click in the gap between the cards closes the panel; on macOS 27 both cards render on Liquid Glass that follows them as they open, close and the window moves, and the right-click menu is a native `NSMenu` (system glass) whose Remove asks inline, Cancel tested (light appearance only; dark not checked); Settings; with the host running, a Claude row shows its state and end-of-turn brief, and the brief isn't in the DB or WAL. Not checked: `+` (opens a real window), IME composition, the icon dot, multiple displays |
| Workspace grouping (Ghostty windows ↔ projects, agent ↔ tab) | ✅ (logic) | `workspace::tests` (7): live windows, saved projects claiming tabs, tty-verified vs. folder-only agents (and the folder match allowed when Ghostty reports no ttys), tmux tabs using the session's location, ordering and dismissal |
| Fedora/Wayland run | 🧪 | Built and run on Ubuntu/X11 only |
| Host as user service | 🟡 | systemd `--user` unit and LaunchAgent are generated (quoting tested); `systemctl --user` / `launchctl` weren't exercised in the container |
| Resource use | ✅ (Linux) | Host idle: about 16 MB RSS and 0.0 % CPU over 10 minutes. Desktop figures are in "Performance" below |

## 2. Registry

| Item | Status | Evidence |
|---|---|---|
| Projects: create/rename/pin/notes/tags/remove | ✅ | store tests and CLI smoke test (`project add/edit/show/rm`) |
| Device-specific locations, remote links | ✅ | `project link … --device`, used by `open --device` over SSH |
| Git root, branch, linked-worktree detection | ✅ | `git::tests::detects_repo_and_worktree` |
| Schema versioning, refuse newer DB | ✅ | `store::tests::refuses_newer_schema` |
| Saving, renaming, pinning, removing workspaces in the desktop UI | 🟡 | implemented (text input with IME support); not yet exercised by hand. Adding folders, notes and tags stays CLI-only |
| Schema v2 (agent tty) upgrades a v1 database in place | ✅ | `store::tests::v1_database_upgrades_in_place` |

## 3. tmux sessions

| Item | Status | Evidence |
|---|---|---|
| Discovery across our sockets, ids + server epoch | ✅ | `live_tmux`, smoke test across two servers (`default`, `handmade`) |
| Verified vs candidate association, explicit choice | ✅ | `user_sessions_are_candidates_not_verified`; CLI exits 2 and asks for `--choose N` when not interactive |
| Same name on two servers → ambiguity error | ✅ | CLI smoke test (`attach byhand`) |
| Race-safe, idempotent create; explicit additional | ✅ | `concurrent_creates_yield_one_session`, `open_create_reuse_additional` |
| Never auto-create, never restart agents | ✅ | `open` without `--create` asks or exits; no `send-keys` anywhere in the codebase |
| Stale sessions after server restart | ✅ | `stale_keys_after_server_restart_are_rejected`; host reconcile removes the records |
| tmux control mode | ⏳ | deliberately not used yet (see ADR §3) |

## 4. Ghostty

| Item | Status | Evidence |
|---|---|---|
| macOS: list terminals (id, cwd, tty, pid), focus exact terminal, new window/tab with command + cwd | 🟡 | Ghostty 1.3.1: `ghostmc ghostty` lists window, tab, terminal id and cwd, and `ghostmc focus <id>` focuses that exact terminal. Ghostty 1.3.1's dictionary has no `tty` or `pid`; the list script now reads them optionally (before, the whole listing failed). New window/tab not run |
| tmux client ↔ Ghostty terminal by tty (no cwd guessing) | ✅ (logic) — inactive on Ghostty 1.3.1, which reports no tty | `desktop::tests::tty_match_links_terminal_to_tmux_session` |
| Linux: new window via `ghostty +new-window -e …`, fallback spawn | 🧪 | Ghostty is not installed here. The missing-Ghostty error path was verified in the desktop app (red status-bar message, nothing else breaks) |
| Linux: focus/list/new-tab disabled, not faked | ✅ | `linux_capabilities_are_honest`; UI shows the reason |
| Custom terminal launcher | ✅ | `custom_terminal_receives_argv` |

## 5. CLI

✅ `status`, `projects`, `project …`, `devices`, `device add/rm/check`,
`sessions [--device]`, `open [--device] [--choose] [--create] [--observe]
[--keep-size] [--here] [--tab] [--dry-run]`, `attach`, `new`, `focus`,
`ghostty`, `agents`, `events`, `git status/diff`, `files`, `edit`, `reveal`,
`hook install/uninstall/status/ingest`, `host run/install/uninstall/status`,
`api`. Every command supports `--json`.

## 6. Agents

| Item | Status | Evidence |
|---|---|---|
| Claude Code + Codex hook contracts checked | ✅ | Claude docs; Codex `codex-rs/hooks` schemas (ADR table) |
| Non-destructive install/uninstall, backup, invalid-JSON refusal | ✅ | `hooks::tests`; smoke test with a user `Stop` hook that is preserved through install and uninstall |
| Ingest runs inside a tmux pane, token/pane attribution | ✅ | smoke test: hook run inside the managed pane → "verified (ghostmc session token)", pane `%0` |
| States incl. needs-approval, turn-ended (≠ success), disconnected, stale | ✅ | `agent::tests`, `store::tests`, `hook_events_attach_agents_to_exact_panes` |
| Prompts/assistant text never stored | ✅ | unit tests plus `grep` of the SQLite files after ingesting a prompt containing a secret |
| End-of-turn brief held in host memory only | ✅ | `briefs_stay_in_memory_and_only_when_asked` (never in the database or WAL, cleared by a new prompt, absent after restart, only with `briefs: true`); direct-mode `hook ingest` with a brief left no trace on disk; `ghostmc api` clears `briefs` |
| Agent terminal (tty) captured from the hook's ancestors | ✅ | real `ghostmc hook ingest` run from inside Claude Code recorded `/dev/ttys004`, the agent's terminal; `agent::tests::ps_tty_column_maps_to_device`; a run whose tty owner exited is Disconnected (`run_whose_terminal_process_exited_is_disconnected`), so a reused tty can't claim it |
| Basic session/process info without hooks | ✅ | pane command and cwd shown for every session |
| System notifications | 🟡 | host notifier implemented (Linux clickable via `notify-send --action`, macOS plain); no notification daemon in the container to display them |
| Real Claude Code / Codex binaries firing hooks | 🧪 | payloads were simulated from the documented schemas |

## 7. Git and files

| Item | Status | Evidence |
|---|---|---|
| Changed files (porcelain v2), unified diff incl. untracked, size cap | ✅ | `git::tests`, desktop Changes tab screenshot |
| Lazy one-level file tree, gitignore/hidden, root confinement | ✅ | `files::tests`, `files_and_git_are_confined` |
| Open in editor / reveal | 🟡 | `open`/`xdg-open`/configured editor; argv tested, not launched here |
| Read-only git (no index lock) | ✅ | every git call sets `GIT_OPTIONAL_LOCKS=0` |
| Remote file editing | ⏳ | remote paths are never passed to a local editor |

## 8. Multi-device continuation

| Item | Status | Evidence |
|---|---|---|
| Remote metadata over SSH (`ghostmc api`, BatchMode, read-only by default) | ✅ (loopback) | `device check loop`, `sessions --device loop`, `open --device loop` |
| Unknown host key fails, never auto-accepted | ✅ | loopback sshd: `Host key verification failed.` under BatchMode |
| Attach a pre-existing session over SSH, input reaches original client, program never restarted | ✅ (loopback) | `scripts/verify_remote_attach.py --device loop`: **12/12 checks passed**, including observer cannot type, observer and `--keep-size` don't resize (under `window-size smallest`, with a writable 50×12 control client shrinking it to 50×11), same PID before and after |
| Real second machine over Tailscale | ⏳ pending | Run `scripts/verify_remote_attach.py --device <name>` between two real machines |
| Mobile | ⏳ | use any SSH client with the `--dry-run` attach command |

## Performance

Measured on the 4-vCPU container (no GPU, so the desktop uses software
Vulkan):

- Host: about 16 MB RSS and 0.0 % CPU at idle.
- Desktop (release build, llvmpipe, 1 project / 3 sessions): about 142 MB RSS,
  and 1.1 % CPU averaged over 30 s at idle. That includes a background refresh
  every 10 s. The numbers are pessimistic, because llvmpipe rasterizes in
  software on the CPU.
- `ghostmc open <project> --json` (release, direct mode, 2 tmux servers):
  about 15 ms wall time.
- Release binaries: `ghostmc` 10 MB, `ghostmc-desktop` 33 MB (not stripped).
- Known cost: on macOS each desktop refresh also runs one `osascript` call for
  Ghostty discovery. It hasn't been measured yet.

## Outstanding blockers

1. **No Mac in this environment.** The Ghostty AppleScript flows, the
   LaunchAgent and the GPUI macOS build need a first run on a real Mac. CI is
   set up to cover the build.
2. **No second physical device or Tailscale.** The cross-device check passed
   over loopback SSH. It still needs a run between two real machines.
3. **Real agents.** Install hooks with real Claude Code and Codex and watch
   the states change (approve the Codex hooks with `/hooks` first).
