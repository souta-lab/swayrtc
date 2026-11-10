#!/usr/bin/env bash
# SwayRTC isolated test environment.
# Production (port 8777, systemd) is NEVER touched by this script.
# The Bun test server runs as the throwaway user "swayrtctest" so that PAM
# authentication is exercised exactly like production (pam_unix only lets a
# process authenticate its own account via the setgid unix_chkpwd helper).
set -uo pipefail

PORT="${PORT:-8778}"
# The Bun test server must run as the PAM account it authenticates, so tests use
# a throwaway local user. Create it on demand and never give it extra groups.
# Test account password: from the TEST_PASSWORD environment variable
# or .secrets/test-password (gitignored). Never hardcode it in this repo.
TEST_PASSWORD="${TEST_PASSWORD:-$(cat "$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/.secrets/test-password" 2>/dev/null || true)}"
if [ -z "$TEST_PASSWORD" ]; then
  echo "error: TEST_PASSWORD is not set and .secrets/test-password was not found" >&2
  exit 1
fi
PROJ=/home/s0u7a/src/swayrtc
DIR="/tmp/swayrtc-test-$PORT"
TESTUSER=swayrtctest
BUN=/home/s0u7a/.bun/bin/bun

kill_tree() {
  local p=$1
  [ -z "$p" ] && return 0
  for c in $(pgrep -P "$p" 2>/dev/null); do kill_tree "$c"; done
  kill "$p" 2>/dev/null
}

ensure_test_user() {
  if ! id "$TESTUSER" >/dev/null 2>&1; then
    echo "creating test user $TESTUSER (throwaway, password: $TEST_PASSWORD)" >&2
    sudo useradd -m -s /bin/bash "$TESTUSER"
    echo "$TESTUSER:$TEST_PASSWORD" | sudo chpasswd
  fi
}

sync_env() {
  ensure_test_user
  sudo rm -rf "$DIR"
  mkdir -p "$DIR/.secrets"
  cp "$PROJ/server.ts" "$DIR/"
  cp -r "$PROJ/web" "$DIR/web"
  cp "$PROJ/.secrets/host-token" "$DIR/.secrets/host-token"
  [ -f "$PROJ/.secrets/turn-key" ] && cp "$PROJ/.secrets/turn-key" "$DIR/.secrets/turn-key"
  cp "$BUN" "$DIR/bun"
  chmod 755 "$DIR" "$DIR/bun"
  chmod 700 "$DIR/.secrets"
  chmod 600 "$DIR/.secrets/host-token"
  # Only the files the server itself must own; DIR stays owned by the caller so
  # that log/pid files can be written by this script.
  sudo chown -R "$TESTUSER:$TESTUSER" "$DIR/bun" "$DIR/server.ts" "$DIR/web" "$DIR/.secrets"
}

stop_env() {
  for f in "$DIR/host.pid" "$DIR/bun.pid"; do
    [ -f "$f" ] || continue
    kill_tree "$(cat "$f")"
    rm -f "$f"
  done
  # The host is spawned with & and can be reparented to the systemd user manager,
  # losing the pid file. Match it by the signalling URL in its environment so a
  # stopped instance never leaves a virtual keyboard/pointer behind.
  local pid
  for pid in /proc/[0-9]*; do
    pid=${pid#/proc/}
    [ -r "/proc/$pid/environ" ] || continue
    if tr '\0' '\n' < "/proc/$pid/environ" 2>/dev/null | grep -qx "SWAYRTC_SIGNALING_URL=ws://127.0.0.1:$PORT/ws/host"; then
      kill_tree "$pid"
    fi
  done
  sleep 0.5
  return 0
}

start_env() {
  sync_env
  ( cd "$DIR" && sudo -u "$TESTUSER" env PORT="$PORT" BIND=127.0.0.1 SWAYRTC_ACCOUNT="$TESTUSER" \
      SWAYRTC_DEBUG=1 "$DIR/bun" run server.ts ) >"$DIR/bun.log" 2>&1 &
  echo $! > "$DIR/bun.pid"
  sleep 1
  env SWAYRTC_SIGNALING_URL="ws://127.0.0.1:$PORT/ws/host" SWAYRTC_DEBUG=1 \
      XDG_RUNTIME_DIR=/run/user/1000 WAYLAND_DISPLAY="${TEST_WAYLAND_DISPLAY:-$(/home/s0u7a/src/swayrtc/tools/resolve-session.sh 2>/dev/null | cut -d" " -f1)}" \
      SWAYRTC_OUTPUT="${TEST_OUTPUT:-$(/home/s0u7a/src/swayrtc/tools/resolve-session.sh 2>/dev/null | cut -d" " -f2)}" \
      SWAYRTC_CAPTURE_BIN="${SWAYRTC_CAPTURE_BIN:-$PROJ/capture-sway.sh}" \
      "$PROJ/native/target/release/swayrtc-host" >"$DIR/host.log" 2>&1 &
  echo $! > "$DIR/host.pid"
  sleep 2
  echo "--- bun.log ---"; cat "$DIR/bun.log"
  echo "--- host.log ---"; cat "$DIR/host.log"
}

case "${1:-start}" in
  sync) sync_env ;;
  start) start_env ;;
  stop) stop_env ;;
  restart) stop_env; sleep 1; start_env ;;
  logs) echo "--- bun.log (tail) ---"; tail -30 "$DIR/bun.log"; echo "--- host.log (tail) ---"; tail -30 "$DIR/host.log" ;;
  status)
    curl -s -o /dev/null -w "http:%{http_code}\n" "http://127.0.0.1:$PORT/health"
    [ -f "$DIR/host.pid" ] && echo "host pid $(cat "$DIR/host.pid")"
    ;;
  *) echo "usage: $0 {start|stop|restart|sync|logs|status}"; exit 1 ;;
esac
