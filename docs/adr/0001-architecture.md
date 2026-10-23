# ADR 0001 — Architecture of the first vertical slice

- Status: accepted (kickoff, 2026-10-23)
- Scope: identities, process boundaries, tmux ownership, Ghostty integration,
  agent observation, remote access and its security model.

## Context

Ghost May Cry (`ghostmc`) is a local-first control center for projects,
terminal sessions and CLI coding agents (Claude Code, Codex). It must find the
*actual* running sessions, focus or reattach them instead of restarting work,
and let the same work continue from another device, without a cloud service.

The kickoff named four traps. This ADR records how the design avoids each one,
and what was checked against real software instead of assumed.

## What was verified at kickoff

| Topic | Source | Finding that shaped the design |
|---|---|---|
| GPUI | crates.io `gpui 0.2.2`, Apache-2.0 | Builds on Linux (X11/Wayland, Vulkan via `blade`). Its transitive `xattr 0.2.3` does not compile against `libc ≥ 0.2.187`; the lockfile pins `libc 0.2.186`. `gpui-component` was not adopted: too large for what we draw. |
| Ghostty AppleScript | `macos/Ghostty.sdef` from upstream source | Terminals expose stable `id`, `working directory`, foreground `pid` and **`tty`**. `new surface configuration` has `command`, `initial working directory`, `environment variables`, `wait after command`. Commands: `focus`, `new window`, `new tab in <window>`, `activate window`, `select tab`, `input text`. |
| Ghostty on Linux | `src/cli/new_window.zig` | `ghostty +new-window` talks to a running GTK instance over D-Bus and accepts `--working-directory` and `-e`; GTK only. No API for listing or focusing windows. |
| tmux | tmux 3.4, tested live | Formats give `#{pid}`/`#{start_time}` (server epoch), `$`/`@`/`%` ids, `#{client_tty}`, session user options (`@name`). `attach -f read-only,ignore-size` blocks input and keeps the size. Control characters in format output are printed as octal escapes (`\037`). `tmux -S` does **not** create a missing socket directory and still exits 0. |
| Claude Code hooks | code.claude.com/docs/en/hooks | `~/.claude/settings.json` → `hooks.<Event>[].hooks[] {type: "command", command, timeout}`; stdin JSON has `session_id`, `cwd`, `hook_event_name`; `Notification` has `message`, `notification_type` (`permission_prompt`, `idle_prompt`, …). Exit code 2 blocks; stdout may be parsed as JSON. |
| Codex hooks | `openai/codex` `codex-rs/hooks` source and schemas | `~/.codex/hooks.json` (or `[hooks]` in `config.toml`), same shape as Claude. Events: `SessionStart`, `UserPromptSubmit`, `PreToolUse`, `PermissionRequest`, `PostToolUse`, `Stop`, `SessionEnd`, … — no `Notification`/`StopFailure`. Non-managed hooks must be trusted with `/hooks` before they run. |

## Decisions

### 1. Identities are explicit and never inferred from paths or names

| Entity | Identity | Lifetime |
|---|---|---|
| `Device` | generated `dev_…` id | durable (SQLite) |
| `Project` | generated `prj_…` id; name unique per registry | durable |
| `ProjectLocation` | `loc_…`; (project, device, normalized path) | durable |
| tmux session | `SessionRef` = socket path + server pid + server start time + `$id` | one tmux server lifetime |
| Ghostty terminal | AppleScript `id` | one Ghostty process lifetime |
| `AgentRun` | (agent kind, agent `session_id`) | until the agent session ends |

- A project is not a path. The same path may belong to two projects only
  through two explicit locations.
- tmux ids are only meaningful with the server epoch. A key from a previous
  server lifetime is rejected as stale (`ResolveSession` → `not_found`), and
  agent panes on a restarted or vanished server count as gone.
- Session names are never identity: the same name on two servers is reported
  as ambiguous (verified in the CLI smoke test).
- Registries are per device and not merged. A remote location is linked
  explicitly (`ghostmc project link … --device`), and remote queries identify
  it by path (`PathPlan`), so no cross-device id coupling exists.

### 2. Association rules: verified link vs. candidate

`ghostmc_core::matching` ranks live tmux sessions against a location:

1. **Tagged** — the session carries `@ghostmc_location=<loc id>` (set
   atomically at creation) or matches a managed-session record. This is the
   only *verified* association.
2. **PathExact / PathDescendant** — some pane's cwd is the location or inside
   it (component-wise, so `/src/app2` never matches `/src/app`). These are
   *candidates* only; the user must pick.

A session tagged for another location is never claimed on a path match. One
verified candidate is opened directly; anything else asks the user (CLI exits
with code 2 when non-interactive).

### 3. tmux owns sessions; ghostmc uses the tmux CLI

- Discovery: one `tmux -S <socket> list-panes -a -F … ; list-clients -F …`
  call per server. Sockets inspected: every socket owned by us in
  `$TMUX_TMPDIR/tmux-<uid>/` (never other users'), plus configured extras.
- Creation: one tmux command list —
  `new-session -d -P -F … -s NAME -c PATH -e GHOSTMC_SESSION_TOKEN=… [-- argv…] ; set-option @ghostmc_location … ; …`
  — so tags can only land on the new session. All values are argv, never
  shell text (tested with hostile paths and option values).
- Idempotence: a per-location `flock` serializes creates across processes
  (host and direct-mode CLIs). `Reuse` returns the existing ghostmc session;
  `Additional` is an explicit second session. Verified with six concurrent
  creators → exactly one session.
- Nothing is started automatically. Creating a session starts the user's
  shell (or an explicit command the user passed); an unmanaged running agent
  is never restarted.
- **Control mode is deliberately not used yet.** No feature in this slice
  needs streamed pane output: discovery, attach and creation are request/response
  CLI calls, and agent state comes from hooks. Control mode (with its
  `%begin/%end/%error` framing, escaped non-UTF-8 `%output`, per-session
  attachment, flow control and `capture-pane` resync) is reserved for a future
  in-app live view, where it becomes necessary.

### 4. Attach semantics and resize policy

| Action | What happens |
|---|---|
| Focus | AppleScript `focus` on an exact terminal id. No input, no new process. |
| Attach (writable) | `tmux attach-session -t <id>` in a new Ghostty window/tab. Other clients are not detached. Sizing follows the user's `window-size`. |
| Attach `--keep-size` | `-f ignore-size`: input allowed, session size untouched (phones, small windows). |
| Observe | `-f read-only,ignore-size`: no input, no resize. |
| Create | Only on explicit request; then attach. |

Verified with real ptys and `window-size smallest`: a 50×12 writable client
shrinks the window to 50×11 (control), while observer and `--keep-size`
clients leave it unchanged. (tmux 3.4 already excludes read-only clients from
sizing; `ignore-size` is passed explicitly so behavior does not depend on
version.) ghostmc never uses `send-keys` on a session it presents as observed.

### 5. Ghostty: real terminal, verified association

- No libghostty, no terminal rendering inside the dashboard.
- macOS: AppleScript, with every value passed through `argv` (no
  interpolation into script source). A Ghostty terminal is tied to a tmux
  session **only** when its `tty` equals a tmux `client_tty` — exact, not a
  cwd guess. Ghostty's own `working directory` is shown for unmanaged
  terminals but never used to identify a tmux pane.
- Linux: launch only (`ghostty +new-window … -e argv`, falling back to a new
  `ghostty` process). Listing/focus/new-tab are reported as unsupported and
  disabled in the UI instead of faked.
- `terminal.kind = "custom"` launches any terminal with an argv prefix.

### 6. Process boundaries

```
 ghostmc-desktop (GPUI)  ──┐                         ┌── tmux servers (CLI)
 ghostmc CLI ──────────────┼─ UDS, JSON lines ─ host ┼── SQLite registry (WAL)
 agent hooks (ghostmc hook ingest) ─┘                └── git CLI, filesystem
        │
        └── GUI actions stay here: Ghostty AppleScript/launch, editor, file manager
```

- **Host** (`ghostmc host run`): unprivileged user process; owns the
  registry, tmux coordination, event ingestion, notifications and periodic
  reconciliation (prunes records of dead sessions, bounds event storage).
  Installable as a systemd `--user` unit or a LaunchAgent; start-at-login is
  opt-in. No root, no system-wide daemon.
- **Direct mode**: when no host runs, the CLI executes the same `Service`
  in-process against the same SQLite file. This keeps `ghostmc` usable over
  SSH and in scripts without a daemon.
- **GUI actions** run in the desktop app or the interactive CLI, inside the
  user's graphical session, never in the headless host. Without Automation
  permission or Ghostty, the action fails with a clear error and everything
  else keeps working.
- Local IPC: Unix socket in a 0700 directory we own (refusing directories
  owned by others), socket mode 0600, and a peer-uid check on every
  connection. A second host refuses to start on a live socket; a stale socket
  is replaced.

### 7. Protocol and compatibility

`ghostmc-protocol`: `{"id", "protocol": {"major","minor"}, "method", "params"}`
requests and `{"id","protocol","status","result"|"error"}` responses. Same
major ⇒ compatible; unknown fields are ignored. An incompatible major gets
`incompatible_protocol` and nothing is executed. The SQLite schema is versioned
separately (`PRAGMA user_version`); a newer database is refused rather than
modified.

### 8. Agent observation

- Hooks call `ghostmc hook ingest --agent claude|codex`. It reads at most
  1 MiB of stdin, never writes to stdout (agents may parse it), always exits 0,
  and sends the event to the host (or writes SQLite directly).
- Stored per event: event name, agent session/turn ids, cwd, tmux socket,
  server pid and pane from `$TMUX`/`$TMUX_PANE`, the session token from
  `$GHOSTMC_SESSION_TOKEN`, a detail (notification type, tool name) and the
  notification message truncated to 200 chars. **Prompts and assistant
  messages are never stored** (tested).
- Attribution evidence, strongest first: session token → tmux pane from the
  hook environment → cwd only (shown as "unverified"). Weaker evidence never
  overwrites stronger evidence.
- States: running, needs approval, idle, **turn ended** (not "succeeded"),
  failed, ended, disconnected (pane verified gone), unknown (no event for
  30 min while "running"). Out-of-order and duplicate events are ignored.
- Hook installation edits only handlers containing `hook ingest --agent`,
  preserves all other keys in order, backs the file up before the first
  change, refuses to touch invalid JSON, and uninstalls back to the original
  document (tested: the result parses equal to the original).

### 9. Remote access: SSH, no new listener

- Metadata: `ssh -T -o BatchMode=yes -- <target> 'ghostmc api'`, one JSON
  request on stdin. `BatchMode` makes unknown host keys fail instead of
  prompting; ghostmc never sets `StrictHostKeyChecking=no`. Targets starting
  with `-` or containing whitespace are rejected.
- Attach: `ssh -t -- <target> '<abs tmux> -S <socket> attach-session -t <id> [flags]'`
  in the local terminal. The remote command is quoted with a POSIX quoter that
  is round-trip tested through `sh`. The tmux path is absolute because a
  non-interactive SSH `PATH` often lacks it. The only new process on the host
  is the tmux client; the session and its programs are unchanged (verified).
- `ghostmc api` answers read-only methods unless started with
  `--allow-write`; remote session creation is not offered in this slice.

**Trust boundary.** Anyone who can SSH into the account can do anything the
account can, including typing into any tmux session. The read-only guard and
`Observe` are protections against accidents, not a sandbox: `attach -r` makes
*that client* read-only, nothing more. A future TCP/WebSocket endpoint (for a
mobile app) needs its own device pairing, authentication and authorization;
reachability over Tailscale is not authorization.

## Consequences

- The slice works today with nothing but tmux, git, SSH and Ghostty.
- Remote use is as safe as the user's SSH setup and no safer; documented.
- Linux users get launch-only Ghostty integration until Ghostty or the
  desktop offer a supported focus API.
- A live terminal view inside the dashboard will need tmux control mode and a
  renderer; that is intentionally deferred.
