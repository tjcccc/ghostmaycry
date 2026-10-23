#!/usr/bin/env python3
"""Verify remote continuation of a pre-existing tmux session over SSH.

Runs on the *client* device. Uses only the Python standard library.

What it proves, against a real tmux server on the target device:
  1. A long-running program in a tmux session keeps running (same PID) while
     clients attach and detach over SSH.
  2. `ghostmc attach --device ... --observe` gives a read-only client that
     cannot inject input and does not resize the session.
  3. `ghostmc attach --device ...` (writable) delivers input to the same pane,
     and an already-attached "original" client sees it.
  4. Detaching does not end the session.

The test uses its own tmux server (`tmux -L ghostmc-verify`) so it never
touches your real sessions.

Usage:
  scripts/verify_remote_attach.py --device NAME [--ghostmc PATH]

NAME must be registered with `ghostmc device add NAME --ssh TARGET`, and
`ssh TARGET` must work non-interactively (keys + known host).
"""

import argparse
import json
import os
import pty
import fcntl
import termios
import struct
import select
import shlex
import subprocess
import sys
import time

SOCKET = "ghostmc-verify"
SESSION = "gmc-verify"
# A stand-in for a coding agent: a long-lived process that echoes input.
PROGRAM = "echo READY; while IFS= read -r l; do echo \"GOT:$l\"; done"


def ssh(target_argv, remote_cmd):
    return subprocess.run(target_argv + [remote_cmd], capture_output=True, text=True)


class Pty:
    def __init__(self, argv, cols, rows):
        self.pid, self.fd = pty.fork()
        if self.pid == 0:
            os.execvp(argv[0], argv)
        fcntl.ioctl(self.fd, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
        self.buf = b""

    def pump(self, seconds):
        end = time.time() + seconds
        while time.time() < end:
            r, _, _ = select.select([self.fd], [], [], 0.05)
            if r:
                try:
                    data = os.read(self.fd, 65536)
                except OSError:
                    return
                if not data:
                    return
                self.buf += data

    def type(self, text):
        os.write(self.fd, text.encode())

    def close(self):
        try:
            os.kill(self.pid, 15)
        except ProcessLookupError:
            pass
        try:
            os.waitpid(self.pid, 0)
        except ChildProcessError:
            pass


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--device", required=True)
    ap.add_argument("--ghostmc", default="ghostmc", help="local ghostmc binary")
    args = ap.parse_args()

    devices = json.loads(subprocess.check_output([args.ghostmc, "--json", "devices"]))
    dev = next((d for d in devices if d["name"].lower() == args.device.lower()), None)
    if not dev or dev["is_local"]:
        sys.exit(f"unknown remote device {args.device!r}")
    ssh_opts = os.environ.get("GHOSTMC_SSH_OPTS", "").split()
    base = ["ssh", "-T", "-o", "BatchMode=yes", *ssh_opts, "--", dev["ssh_target"]]
    tty_base = ["ssh", "-t", *ssh_opts, "--", dev["ssh_target"]]
    tmux = f"tmux -L {SOCKET} -f /dev/null"

    results = []

    def check(name, ok, detail=""):
        results.append((name, bool(ok), detail))
        print(f"{'PASS' if ok else 'FAIL'}  {name}{('  — ' + detail) if detail else ''}")

    ssh(base, f"{tmux} kill-server 2>/dev/null")
    r = ssh(base, f"{tmux} new-session -d -s {SESSION} -x 120 -y 40 {shlex.quote(PROGRAM)}")
    check("create test session on target", r.returncode == 0, r.stderr.strip())
    if r.returncode != 0:
        return 1

    def info():
        out = ssh(base, f"{tmux} display -p -t {SESSION} '#{{pane_pid}} #{{window_width}}x#{{window_height}}'").stdout.split()
        return (out[0], out[1]) if len(out) == 2 else (None, None)

    def pane_text():
        return ssh(base, f"{tmux} capture-pane -p -t {SESSION}").stdout

    # Adversarial sizing: with `smallest`, any client that counts toward the
    # size would shrink the window.
    ssh(base, f"{tmux} set -g window-size smallest")

    # The "original" display (stands in for the Ghostty window on the host).
    original = Pty(tty_base + [f"{tmux} attach -t {SESSION}"], 120, 40)
    original.pump(1.5)

    pid0, size0 = info()
    check("program running", pid0 is not None, f"pane pid {pid0}, size {size0} with original client attached")

    # Discovery through ghostmc over SSH.
    sessions = json.loads(subprocess.check_output([args.ghostmc, "--json", "sessions", "--device", args.device]))
    found = [s for s in sessions if s["name"] == SESSION and s["ref"]["socket_path"].endswith("/" + SOCKET)]
    check("ghostmc lists the pre-existing session over SSH", len(found) == 1)
    key = found[0]["key"] if found else SESSION

    def attach_argv(observe, keep_size=False):
        cmd = [args.ghostmc, "--json", "attach", key, "--device", args.device, "--here", "--dry-run"]
        if observe:
            cmd.append("--observe")
        if keep_size:
            cmd.append("--keep-size")
        return json.loads(subprocess.check_output(cmd))["argv"]

    # Observer: small viewport, read-only.
    obs_argv = attach_argv(True)
    check("observe argv is read-only + ignore-size", "read-only,ignore-size" in obs_argv[-1], obs_argv[-1])
    observer = Pty(obs_argv, 50, 12)
    observer.pump(2)
    observer.type("OBSERVER_INPUT\r")
    observer.pump(1)
    _, size1 = info()
    check("observer did not resize the session", size1 == size0, f"{size0} -> {size1}")
    check("observer input was not delivered", "OBSERVER_INPUT" not in pane_text())

    # Writable remote client.
    wr = Pty(attach_argv(False), 120, 40)
    wr.pump(2)
    wr.type("REMOTE_INPUT\r")
    wr.pump(1.5)
    original.pump(1)
    check("writable input reached the pane", "GOT:REMOTE_INPUT" in pane_text())
    check("original client sees the remote input", b"GOT:REMOTE_INPUT" in original.buf)

    # Small writable client that must not resize (e.g. a phone).
    small = Pty(attach_argv(False, keep_size=True), 50, 12)
    small.pump(2)
    small.type("SMALL_INPUT\r")
    small.pump(1.5)
    _, size2 = info()
    check("--keep-size client delivers input", "GOT:SMALL_INPUT" in pane_text())
    check("--keep-size client did not resize the session", size2 == size0, f"{size0} -> {size2}")

    # Detach the new clients with the tmux prefix (C-b d).
    for p in (wr, observer, small):
        p.type("\x02d")
        p.pump(1)
        p.close()
    pid1, _ = info()
    check("session survives detach", pid1 is not None)
    check("program was never restarted", pid1 == pid0, f"pid {pid0} -> {pid1}")

    original.close()
    ssh(base, f"{tmux} kill-server")
    failed = [r for r in results if not r[1]]
    print(f"\n{len(results) - len(failed)}/{len(results)} checks passed")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
