#!/usr/bin/env bash
# Starts the SwayRTC host attached to the *physical* Sway session.
#
# There is no second compositor any more: the host captures the monitor's output
# and injects input into the same seat the person at the keyboard uses. The
# wayland socket name changes when the session restarts, so it is resolved at
# start and re-checked while running; if the session goes away the host is
# restarted so it can attach to the new socket.
set -uo pipefail
ROOT="$(dirname "$(readlink -f "$0")")"
RUNTIME="/run/user/$(id -u)"
export XDG_RUNTIME_DIR="$RUNTIME"

resolve() { "$ROOT/tools/resolve-session.sh" 2>/dev/null; }

# Wait for a session that is driving a real connector.
for _ in $(seq 60); do
  info=$(resolve) && break
  sleep 2
done
if [ -z "${info:-}" ]; then
  echo "swayrtc-host: no physical Sway session found" >&2
  exit 1
fi
WD=${info%% *}
OUT=${info##* }
export WAYLAND_DISPLAY="$WD" SWAYRTC_OUTPUT="$OUT"
echo "swayrtc-host: attached to $WD ($OUT)" >&2

"$ROOT/native/target/release/swayrtc-host" &
HOST=$!
trap 'kill "$HOST" 2>/dev/null' TERM INT
while kill -0 "$HOST" 2>/dev/null; do
  sleep 3
  now=$(resolve) || true
  if [ "${now:-}" != "$info" ]; then
    echo "swayrtc-host: session changed (${now:-gone}); restarting" >&2
    kill "$HOST" 2>/dev/null
    break
  fi
done
wait "$HOST" 2>/dev/null
exit 1   # let systemd bring it back with a freshly resolved session
