#!/usr/bin/env bash
# Builds the zero-copy capture (screencopy -> AMD VPP -> Intel h264_vaapi).
set -euo pipefail
cd "$(dirname "$(readlink -f "$0")")"
wayland-scanner client-header proto/linux-dmabuf-unstable-v1.xml proto/linux-dmabuf-client.h
wayland-scanner private-code  proto/linux-dmabuf-unstable-v1.xml proto/linux-dmabuf-code.c
wayland-scanner client-header proto/wlr-screencopy-unstable-v1.xml proto/screencopy-client.h
wayland-scanner private-code  proto/wlr-screencopy-unstable-v1.xml proto/screencopy-code.c
gcc -O2 -o zerocopy-capture zerocopy-capture.c proto/linux-dmabuf-code.c proto/screencopy-code.c \
    $(pkg-config --cflags --libs libavcodec libavutil wayland-client) -lva -lva-drm -lgbm
echo "built $(pwd)/zerocopy-capture"
