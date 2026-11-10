#!/usr/bin/env bash
set -euo pipefail
# Diagnostic source only: synthetic 1080p60, hardware H.264 on Intel.
# Does not capture Sway and does not prove zero-copy screen capture.
exec ffmpeg -nostdin -hide_banner -loglevel warning \
  -init_hw_device vaapi=qs:/dev/dri/renderD128 \
  -filter_hw_device qs \
  -re -f lavfi -i "testsrc2=size=1920x1080:rate=60" \
  -vf "format=nv12,hwupload" \
  -c:v h264_vaapi -bf 0 -g 60 -b:v 5M \
  -f h264 pipe:1
