#!/usr/bin/env bash
# SwayRTC capture: Sway output -> H.264 Annex-B on stdout.
#
# Primary path: our own wlr-screencopy client (native/target/release/swayrtc-capture).
# Measured capture+encode latency with a full-screen stimulus: 21 ms median
# (7 ms floor) versus 35 ms for the old wf-recorder pipeline - wf-recorder kept
# several frames in its own pool. It also gives exact access-unit boundaries and
# removes one process from the tree.
#
# Fallback: wf-recorder, used only if the binary has not been built, so the host
# keeps working during an upgrade.
#
# Encoder selection (SWAYRTC_ENCODER):
#   unset / "auto"  ask the planner (swayrtc-probe, DESIGN.md §2): zero-copy
#                   plans run the C pipeline with the plan's device roles,
#                   CPU/VA-API plans run the native capture, which re-plans
#                   its own encoder the same way.
#   "zerocopy"      the C GPU pipeline (native/gpu/zerocopy-capture).
#   "x264" / "qsv"  the native capture with that encoder, as before.
# SWAYRTC_DRY_RUN=1 prints the decision instead of exec'ing anything.
set -uo pipefail
: "${XDG_RUNTIME_DIR:=/run/user/1000}"
: "${WAYLAND_DISPLAY:=wayland-1}"
export XDG_RUNTIME_DIR WAYLAND_DISPLAY

ROOT="$(dirname "$(readlink -f "$0")")"
# Zero-copy path: screencopy into a dma-buf, the capture GPU's VPP converts
# to NV12, the encode GPU encodes. On the reference machine that is AMD
# converting and Intel encoding (~4% CPU against ~160% for the x264
# pipeline, because no pixels are touched on the CPU at all); which devices
# play which role comes from the planner, not from literals.
ZEROCOPY="$ROOT/native/gpu/zerocopy-capture"
# Which output to capture: the physical session's connector, resolved by the
# host wrapper (HDMI-A-4 here); the old headless name stays as a fallback.
OUTPUT="${SWAYRTC_OUTPUT:-HEADLESS-1}"
NATIVE="$ROOT/native/target/release/swayrtc-capture"
PROBE="$ROOT/native/target/release/swayrtc-probe"
FPS="${SWAYRTC_FPS:-60}"
BITRATE="${SWAYRTC_BITRATE:-6M}"
QP="${SWAYRTC_QP:-24}"
GOP="${SWAYRTC_GOP:-$FPS}"
REQUESTED="${SWAYRTC_ENCODER:-auto}"
export SWAYRTC_FPS SWAYRTC_BITRATE SWAYRTC_QP SWAYRTC_GOP

# /dev/dri is needed for VA-API; the systemd user manager starts without the
# render group, so re-exec through sg when the device is not accessible.
run_gated() {
  if [ -r "${SWAYRTC_VA_DEVICE:-/dev/dri/renderD128}" ]; then
    exec "$@"
  fi
  exec sg render -c "exec $(printf '%q ' "$@")"
}

dry_run() {
  if [ "${SWAYRTC_DRY_RUN:-0}" = "1" ]; then
    echo "capture-sway: dry-run: would exec: $*" >&2
    exit 0
  fi
}

# --- planner consultation (auto mode only) ----------------------------------
PLAN_STRATEGY="" PLAN_ENCODER="" PLAN_CAPTURE_DEVICE="" PLAN_ENCODE_DEVICE=""
if [ "$REQUESTED" = "auto" ]; then
  if [ -x "$PROBE" ]; then
    while IFS='=' read -r k v; do
      case "$k" in
        SWAYRTC_PLAN_STRATEGY) PLAN_STRATEGY="$v" ;;
        SWAYRTC_PLAN_ENCODER) PLAN_ENCODER="$v" ;;
        SWAYRTC_PLAN_CAPTURE_DEVICE) PLAN_CAPTURE_DEVICE="$v" ;;
        SWAYRTC_PLAN_ENCODE_DEVICE) PLAN_ENCODE_DEVICE="$v" ;;
      esac
    done < <("$PROBE" --format env 2>/dev/null)
    echo "capture-sway: auto plan: ${PLAN_STRATEGY:-unknown} (encoder: ${PLAN_ENCODER:-unknown})" >&2
  else
    echo "capture-sway: auto plan: unavailable (swayrtc-probe not built); native capture will plan" >&2
  fi
fi

# --- zero-copy path ----------------------------------------------------------
# Explicit SWAYRTC_ENCODER=zerocopy keeps the pre-planner behaviour; in auto
# mode the path is taken only when the plan is a zero-copy strategy. A
# missing binary falls through to the native capture in both cases.
# SWAYRTC_ENCODER=zerocopy measured 54 ms capture+encode against 21 ms for
# x264 on the reference machine, because the Intel low-power encoder keeps a
# few frames in flight; it trades that latency for ~4% CPU.
if [ "$REQUESTED" = "zerocopy" ] || { [ "$REQUESTED" = "auto" ] && [ "$PLAN_ENCODER" = "zerocopy" ]; }; then
  if [ -x "$ZEROCOPY" ]; then
    if [ "$REQUESTED" = "auto" ]; then
      export SWAYRTC_ZEROCOPY_CAPTURE_DEVICE="${PLAN_CAPTURE_DEVICE:-/dev/dri/renderD129}"
      export SWAYRTC_ZEROCOPY_ENCODE_DEVICE="${PLAN_ENCODE_DEVICE:-/dev/dri/renderD128}"
      echo "capture-sway: zero-copy roles from plan: capture/convert=$SWAYRTC_ZEROCOPY_CAPTURE_DEVICE encode=$SWAYRTC_ZEROCOPY_ENCODE_DEVICE" >&2
    fi
    dry_run "$ZEROCOPY"
    run_gated "$ZEROCOPY"
  fi
  echo "capture-sway: zero-copy binary not built; falling back to the native capture" >&2
fi

# --- native capture ----------------------------------------------------------
if [ -x "$NATIVE" ]; then
  if [ "$REQUESTED" = "auto" ]; then
    export SWAYRTC_ENCODER=auto
    # Align the binary's own planning with the shell-side plan (its planner
    # would pick the same device; this just skips the disagreement).
    if [ "$PLAN_ENCODER" = "vaapi" ] && [ -n "$PLAN_ENCODE_DEVICE" ] && [ -z "${SWAYRTC_VA_DEVICE:-}" ]; then
      export SWAYRTC_VA_DEVICE="$PLAN_ENCODE_DEVICE"
    fi
  else
    export SWAYRTC_ENCODER="$REQUESTED"
  fi
  dry_run "$NATIVE (SWAYRTC_ENCODER=$SWAYRTC_ENCODER)"
  run_gated "$NATIVE"
fi

# ---- fallback: wf-recorder (previous implementation) ----------------------
ENCODER="$REQUESTED"
[ "$ENCODER" = "auto" ] && ENCODER="x264"
if [ "$ENCODER" = "qsv" ]; then
  run_gated bash -c "wf-recorder -y -o "$OUTPUT" -D -c rawvideo -x nv12 -m rawvideo -f /dev/stdout 2>/dev/null |
    ffmpeg -nostdin -y -hide_banner -loglevel error -filter_threads 1 -threads 1 \
      -init_hw_device vaapi=qs:${SWAYRTC_VA_DEVICE:-/dev/dri/renderD128} -filter_hw_device qs \
      -f rawvideo -pixel_format nv12 -video_size 1920x1080 -framerate $FPS -i pipe:0 \
      -vf hwupload -c:v h264_vaapi -rc_mode CQP -qp $QP -async_depth 1 -profile:v constrained_baseline \
      -bf 0 -g $GOP -maxrate $BITRATE -bufsize 1M -f h264 /dev/stdout"
fi
exec wf-recorder -y -o "$OUTPUT" -c libx264 \
  -p preset=ultrafast -p tune=zerolatency -p profile=baseline \
  -p bframes=0 -p keyint="$GOP" -p min-keyint="$GOP" -p scenecut=0 -p aud=1 \
  -b "$BITRATE" -p maxrate="$BITRATE" -p bufsize=1M \
  -r "$FPS" -D -m h264 -f /dev/stdout
