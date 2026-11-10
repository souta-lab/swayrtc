#!/usr/bin/env python3
"""Print "<WAYLAND_DISPLAY> <OUTPUT>" for the Sway session attached to the seat.

SwayRTC attaches to the session that drives the monitor, so the remote view is
the desktop the person at the keyboard sees and no second compositor exists.

Two wrinkles:
  * the wayland socket name moves between restarts (wayland-0/1/2), so it is read
    from the compositor's open file descriptors;
  * sometimes no monitor is plugged in at all (every DRM connector reports
    disconnected). Then the session has no output and there is nothing to
    capture, so one headless output is created *inside the same session* - the
    session count still stays at one. A lock keeps concurrent callers (the host
    watchdog runs this every few seconds) from creating more than one.
"""

import fcntl
import json
import re
import os
import subprocess
import sys
import tempfile
from pathlib import Path

RUNTIME = os.environ.get("XDG_RUNTIME_DIR", f"/run/user/{os.getuid()}")
PHYSICAL = ("HDMI", "DP", "eDP")


def sway(sock, args):
    env = dict(os.environ, SWAYSOCK=sock, XDG_RUNTIME_DIR=RUNTIME)
    try:
        return subprocess.run(["swaymsg", *args], env=env, capture_output=True, text=True, timeout=4).stdout
    except Exception:
        return ""


def outputs(sock):
    raw = sway(sock, ["-t", "get_outputs", "-r"])
    try:
        return json.loads(raw)
    except Exception:
        return []


def active_names(sock):
    return [o.get("name") or "?" for o in outputs(sock) if o.get("active")]


def pick(sock):
    names = active_names(sock)
    for name in names:
        if name.startswith(PHYSICAL):
            # Physical connector is back: drop virtual outputs so windows cannot
            # land on them.
            for extra in [n for n in names if n.startswith("HEADLESS-")]:
                sway(sock, ["output", extra, "unplug"])
            return name
    headless = sorted([n for n in names if n.startswith("HEADLESS-")], key=lambda n: int(n.split("-")[-1] or 0))
    if headless:
        return headless[0]
    return None


def ensure_output(sock):
    with tempfile.NamedTemporaryFile(prefix="swayrtc-resolve-", dir=RUNTIME, delete=False) as lock:
        try:
            fcntl.flock(lock.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
        except OSError:
            return None  # another caller is creating it right now
        # Re-check under the lock: nothing to do if an output appeared meanwhile.
        name = pick(sock)
        if name:
            return name
        sway(sock, ["create_output"])
        return pick(sock)


def session_socket():
    for sock in sorted(Path(RUNTIME).glob("sway-ipc.*.sock")):
        pid = sock.name.split(".")[-2]
        try:
            cgroup = Path(f"/proc/{pid}/cgroup").read_text().splitlines()[0]
        except Exception:
            continue
        if "session-" not in cgroup:  # seat sessions only, not user-unit compositors
            continue
        wd = None
        try:
            for fd in Path(f"/proc/{pid}/fd").iterdir():
                target = os.readlink(fd)
                candidate = target.rsplit("/", 1)[-1]
                # The compositor keeps the socket and its lock file open; either
                # identifies the display.
                match = re.fullmatch(r"(wayland-\d+)(\.lock)?", candidate)
                if match:
                    wd = match.group(1)
                    break
        except Exception:
            pass
        if not wd:
            continue
        yield str(sock), wd


def main():
    for sock, wd in session_socket():
        name = pick(sock) or ensure_output(sock)
        if name:
            print(wd, name)
            return 0
    return 1


if __name__ == "__main__":
    sys.exit(main())
