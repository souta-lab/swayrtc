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
set -uo pipefail
: "${XDG_RUNTIME_DIR:=/run/user/1000}"
: "${WAYLAND_DISPLAY:=wayland-1}"
export XDG_RUNTIME_DIR WAYLAND_DISPLAY

ROOT="$(dirname "$(readlink -f "$0")")"
# Zero-copy path: screencopy into a dma-buf, AMD VPP converts to NV12, Intel
# encodes. Measured ~4% CPU against ~160% for the x264 pipeline, because no
# pixels are touched on the CPU at all.
ZEROCOPY="$ROOT/native/gpu/zerocopy-capture"
# Which output to capture: the physical session's connector, resolved by the
# host wrapper (HDMI-A-4 here); the old headless name stays as a fallback.
OUTPUT="${SWAYRTC_OUTPUT:-HEADLESS-1}"
NATIVE="$ROOT/native/target/release/swayrtc-capture"
FPS="${SWAYRTC_FPS:-60}"
BITRATE="${SWAYRTC_BITRATE:-6M}"
QP="${SWAYRTC_QP:-24}"
GOP="${SWAYRTC_GOP:-$FPS}"
ENCODER="${SWAYRTC_ENCODER:-x264}"
export SWAYRTC_FPS SWAYRTC_BITRATE SWAYRTC_QP SWAYRTC_GOP SWAYRTC_ENCODER

# /dev/dri is needed for VA-API; the systemd user manager starts without the
# render group, so re-exec through sg when the device is not accessible.
run_gated() {
  if [ -r "${SWAYRTC_VA_DEVICE:-/dev/dri/renderD128}" ]; then
    exec "$@"
  fi
  exec sg render -c "exec $(printf '%q ' "$@")"
}

# SWAYRTC_ENCODER=zerocopy: GPU-only pipeline. Measured 4% CPU against 160% for
# x264, but 54 ms capture+encode against 21 ms, because the Intel low-power
# encoder keeps a few frames in flight. Default stays x264 (latency first).
if [ "${SWAYRTC_ENCODER:-auto}" = "zerocopy" ] && [ -x "$ZEROCOPY" ]; then
  run_gated "$ZEROCOPY"
fi

if [ -x "$NATIVE" ]; then
  run_gated "$NATIVE"
fi

# ---- fallback: wf-recorder (previous implementation) ----------------------
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
