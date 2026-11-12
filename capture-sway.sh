#!/usr/bin/env bash
# SwayRTC capture: Sway output -> H.264 Annex-B on stdout.
#
# Capture is no longer built in this repository. It lives in the external
# zerocap project (https://github.com/souta-lab/zerocap): a wlr-screencopy
# client with a probe/planner that picks x264, VA-API or the cross-GPU
# zero-copy pipeline at start (DESIGN.md §6). This script is only the seam
# between the host and zerocap: it finds the zerocap binary, translates the
# SWAYRTC_* settings into the ZEROCAP_* environment zerocap reads, and execs
# it, so zerocap writes straight to the host's pipe.
#
# Arguments: swayrtc-host spawns $SWAYRTC_CAPTURE_BIN with wl-screenrec-style
# argv (-o HEADLESS-1 -m 60 --codec avc --dri-device ... -f -), so that any
# wl-screenrec-compatible binary can be plugged in directly. This script is
# configured from the environment instead and accepts and ignores that argv:
# the hard-coded "-o HEADLESS-1" in particular would override the session's
# real connector, which host-start.sh passes as SWAYRTC_OUTPUT.
#
# Locating zerocap: $ZEROCAP_BIN, else `zerocap` on PATH, else
# ~/.local/bin/zerocap. Zero-copy plans additionally need zerocap's
# `zerocopy-capture` helper next to the zerocap binary, on PATH, or named by
# ZEROCAP_ZEROCOPY_BIN; without it zerocap falls back to its CPU path.
#
# Environment translation (an explicit ZEROCAP_* value always wins):
#   SWAYRTC_OUTPUT                    -> ZEROCAP_OUTPUT (default HEADLESS-1)
#   SWAYRTC_FPS                       -> ZEROCAP_FPS
#   SWAYRTC_ENCODER                   -> ZEROCAP_ENCODER ("qsv" -> "vaapi")
#   SWAYRTC_VA_DEVICE                 -> ZEROCAP_VA_DEVICE
#   SWAYRTC_QP / SWAYRTC_GOP          -> ZEROCAP_QP / ZEROCAP_GOP
#   SWAYRTC_BITRATE (e.g. 6M, 800k)   -> ZEROCAP_BITRATE (as is, CPU path)
#                                        ZEROCAP_BITRATE_KBPS (integer kbps)
#   SWAYRTC_BITRATE_KBPS              -> ZEROCAP_BITRATE_KBPS (beats the above)
#   SWAYRTC_ZEROCOPY_CAPTURE_DEVICE   -> ZEROCAP_ZEROCOPY_CAPTURE_DEVICE
#   SWAYRTC_ZEROCOPY_ENCODE_DEVICE    -> ZEROCAP_ZEROCOPY_ENCODE_DEVICE
#   SWAYRTC_CAPTURE_DEVICE, SWAYRTC_RC, SWAYRTC_VBV, SWAYRTC_TIMING,
#   SWAYRTC_CAPTURE_LOG               -> the same name with ZEROCAP_
#     (knobs the removed in-tree capture honoured, kept working)
# Unset SWAYRTC_* values are not translated: zerocap's own defaults apply.
#
# SWAYRTC_DRY_RUN=1 prints the resolved zerocap command instead of exec'ing it.
set -uo pipefail
: "${XDG_RUNTIME_DIR:=/run/user/$(id -u)}"
: "${WAYLAND_DISPLAY:=wayland-1}"
export XDG_RUNTIME_DIR WAYLAND_DISPLAY

die() {
  echo "capture-sway: $*" >&2
  exit 1
}

# --- locate zerocap -----------------------------------------------------------
if [ -n "${ZEROCAP_BIN:-}" ]; then
  [ -x "$ZEROCAP_BIN" ] || die "ZEROCAP_BIN=$ZEROCAP_BIN is not an executable file"
  ZEROCAP="$ZEROCAP_BIN"
elif ZEROCAP="$(command -v zerocap 2>/dev/null)" && [ -n "$ZEROCAP" ]; then
  :
elif [ -x "$HOME/.local/bin/zerocap" ]; then
  ZEROCAP="$HOME/.local/bin/zerocap"
else
  die "zerocap is required but was not found (checked \$ZEROCAP_BIN, PATH, \$HOME/.local/bin/zerocap).
capture-sway: build and install it from https://github.com/souta-lab/zerocap (cargo build --release, then copy
capture-sway: target/release/zerocap - and gpu/zerocopy-capture for zero-copy plans - to ~/.local/bin),
capture-sway: or set ZEROCAP_BIN to its path."
fi

# --- translate SWAYRTC_* -> ZEROCAP_* -------------------------------------------
# set_from <ZEROCAP_NAME> <value>: export the value unless the operator already
# set ZEROCAP_NAME explicitly, or the value is empty.
set_from() {
  local name="$1" value="$2"
  [ -n "$value" ] || return 0
  [ -n "${!name+x}" ] && return 0
  export "$name=$value"
}

# "6M" -> 6000, "800k" -> 800, "2.5M" -> 2500, "6000000" (bit/s) -> 6000.
# Prints nothing for a value it cannot parse.
bitrate_kbps() {
  local v="$1"
  [[ "$v" =~ ^([0-9]+(\.[0-9]+)?)([kKmM]?)$ ]] || return 0
  awk -v n="${BASH_REMATCH[1]}" -v u="${BASH_REMATCH[3]}" 'BEGIN {
    m = (u == "k" || u == "K") ? 1 : (u == "m" || u == "M") ? 1000 : 0.001
    printf "%d\n", n * m + 0.5
  }'
}

# Operator-explicit ZEROCAP_BITRATE_KBPS must beat SWAYRTC_BITRATE_KBPS, which
# must beat the value derived from SWAYRTC_BITRATE; remember which was explicit
# before anything is exported.
explicit_kbps="${ZEROCAP_BITRATE_KBPS+x}"

set_from ZEROCAP_OUTPUT "${SWAYRTC_OUTPUT:-HEADLESS-1}"
set_from ZEROCAP_FPS "${SWAYRTC_FPS:-}"
encoder="${SWAYRTC_ENCODER:-}"
[ "$encoder" = "qsv" ] && encoder="vaapi" # zerocap's spelling of the same path
set_from ZEROCAP_ENCODER "$encoder"
set_from ZEROCAP_VA_DEVICE "${SWAYRTC_VA_DEVICE:-}"
set_from ZEROCAP_QP "${SWAYRTC_QP:-}"
set_from ZEROCAP_GOP "${SWAYRTC_GOP:-}"
set_from ZEROCAP_ZEROCOPY_CAPTURE_DEVICE "${SWAYRTC_ZEROCOPY_CAPTURE_DEVICE:-}"
set_from ZEROCAP_ZEROCOPY_ENCODE_DEVICE "${SWAYRTC_ZEROCOPY_ENCODE_DEVICE:-}"
for knob in CAPTURE_DEVICE RC VBV TIMING CAPTURE_LOG; do
  src="SWAYRTC_$knob"
  set_from "ZEROCAP_$knob" "${!src:-}"
done

if [ -n "${SWAYRTC_BITRATE:-}" ]; then
  # zerocap's CPU path hands the string to FFmpeg (-maxrate); its zero-copy
  # pipeline reads an integer kbps. Translate to both.
  set_from ZEROCAP_BITRATE "$SWAYRTC_BITRATE"
  kbps="$(bitrate_kbps "$SWAYRTC_BITRATE")"
  if [ -z "$kbps" ]; then
    echo "capture-sway: SWAYRTC_BITRATE=$SWAYRTC_BITRATE is not of the form 6M / 800k / 6000000; ZEROCAP_BITRATE_KBPS not set" >&2
  elif [ -z "$explicit_kbps" ] && [ -z "${SWAYRTC_BITRATE_KBPS:-}" ]; then
    export ZEROCAP_BITRATE_KBPS="$kbps"
  fi
fi
[ -z "$explicit_kbps" ] && [ -n "${SWAYRTC_BITRATE_KBPS:-}" ] && export ZEROCAP_BITRATE_KBPS="$SWAYRTC_BITRATE_KBPS"

# --- run ------------------------------------------------------------------------
if [ "${SWAYRTC_DRY_RUN:-0}" = "1" ]; then
  echo "capture-sway: dry-run: would exec: $(env | grep '^ZEROCAP_' | sort | tr '\n' ' ')$ZEROCAP" >&2
  exit 0
fi

# VA-API needs the render node; the systemd user manager starts without the
# render group, so re-exec through sg when the device exists but is not
# accessible. exec in both branches keeps this process's pid and process
# group, which swayrtc-host uses to signal and reap the capture.
device="${ZEROCAP_VA_DEVICE:-/dev/dri/renderD128}"
if [ -e "$device" ] && [ ! -r "$device" ] && command -v sg >/dev/null 2>&1; then
  exec sg render -c "exec $(printf '%q' "$ZEROCAP")"
fi
exec "$ZEROCAP"
