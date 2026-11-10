# SwayRTC

Ultra low latency browser remote desktop for Sway: a minimal Bun server for
authentication + WebRTC signaling + UI, and a native Rust host that captures a
Sway output and injects real Wayland input.

Capture is a custom wlr-screencopy client with three encode paths: software
x264 (the default, latency first), VA-API, and a **cross-GPU zero-copy
pipeline** — the AMD GPU's VPP converts the captured dma-buf to NV12, the
Intel GPU encodes it with VA-API, and no pixel ever touches the CPU
(~4% CPU at 1080p60; see *Zero copy: working, as an option* below).

Replaces the WayVNC + noVNC stack (see *Relationship to WayVNC / noVNC*).

```
 browser (HTTPS/WSS)                    host (same machine, runs as the service user)
 ┌──────────────────┐   signaling   ┌──────────────────────────────────────┐
 │ login  ── PAM ────────────────▶  │ Bun 127.0.0.1:8777                   │
 │ UI + WebRTC      │◀── SDP/ICE ──▶│  · PAM auth (pamtester)              │
 └────────┬─────────┘               │  · /api/ice (TURN credentials)       │
          │                         │  · /ws/host, /ws/viewer              │
          │  DTLS-SRTP media        └───────────┬──────────────────────────┘
          │  (UDP, TURN by default)             │ local WS
          ▼                                     ▼
 ┌────────────────────────┐          ┌──────────────────────────────────────┐
 │ Cloudflare Realtime    │◀────────▶│ swayrtc-host (Rust)                  │
 │ TURN  turn:…:3478/5349 │          │  · WebRTC peer, H.264 RTP            │
 └────────────────────────┘          │  · capture (wlr-screencopy → H.264)  │
                                     │  · Wayland virtual keyboard/pointer  │
                                     └──────────────────────────────────────┘
```

## What works (measured on the reference machine)

| Item | Result |
| --- | --- |
| Video | 1920×1080, **61 fps**, H.264 Constrained Baseline |
| Zero copy | cross-GPU pipeline: AMD VPP → Intel VA-API over dma-buf, **~4% CPU**, 39 ms capture+encode |
| Path | `UDP · RELAY` through Cloudflare TURN, `relay → relay` pair |
| Latency | ICE RTT **~39 ms**, frame jitter ~2 ms |
| First frame | ~0.27 s after the peer connects |
| Reload / reconnect | video back in ~0.8 s |
| Input | physical keyboard, absolute pointer, drag, scroll, modifiers, key hold/repeat |
| Input latency | DataChannel send -> host receive: **p50 19-20 ms** (p90 25 ms) |
| Presentation | media element vs WebCodecs canvas: canvas **6.4 ms** earlier |
| Idle bandwidth | static desktop ~1.05 Mbps; hidden tab and 30 min idle release it entirely |
| Mouse feel | pointer-lock mode: relative motion, no OS cursor chasing the video |
| Unicode text | composed IME / Japanese text pasted through the clipboard (Ctrl+V) |
| Mobile | iPhone emulation: video, layout without overflow, soft keyboard + special keys |
| Child process hygiene | 0 capture processes left after disconnect |

Reference machine: Debian sid, SwayFX 1.12 (Sway 1.12 base), Intel Core i5-8400
(UHD 630 / Quick Sync on `/dev/dri/renderD128`), AMD RX 6400
(`/dev/dri/renderD129`, **no** H.264 encoder), Sway headless output
`HEADLESS-1` 1920×1080.

## Running it

### Prerequisites

```bash
sudo apt install wf-recorder pamtester wtype wlrctl
cd native && cargo build --release
```

### Secrets

`.secrets/` is git-ignored and mode 700:

| File | Meaning |
| --- | --- |
| `host-token` | bearer token the Rust host uses for `/ws/host` and `/api/ice` |
| `turn-key` | Cloudflare Realtime TURN key id + secret, one per line |

Create the TURN key with:

```bash
cf realtime turn keys create --name swayrtc   # keep uid + secret
```

### systemd (user units)

```bash
systemctl --user enable --now swayrtc-bun.service swayrtc-host.service swayrtc-tunnel.service
systemctl --user status swayrtc-{bun,host,tunnel}
journalctl --user -u swayrtc-host -f
```

`swayrtc-host.service` needs the Wayland environment of the session:

```ini
Environment=XDG_RUNTIME_DIR=/run/user/1000
Environment=WAYLAND_DISPLAY=wayland-1
Environment=SWAYRTC_CAPTURE_BIN=/path/to/swayrtc/capture-sway.sh
```

### Cloudflare Tunnel

`~/.cloudflared/swayrtc.yml` publishes `swayrtc.example.com` to
`http://127.0.0.1:8777` on its own tunnel id. Cloudflare Access is **not** used —
the application has its own PAM login. Only signaling/UI travel through the
tunnel; media never does.

## Configuration

| Variable | Default | Meaning |
| --- | --- | --- |
| `PORT` / `BIND` | `8777` / `127.0.0.1` | Bun listener |
| `SWAYRTC_ACCOUNT` | `s0u7a` | PAM account, must equal the service user |
| `SWAYRTC_ALLOWED_ORIGINS` | – | extra accepted `Origin` hosts (comma separated) |
| `SWAYRTC_DEBUG` | off | verbose signaling / ICE / input tracing |
| `SWAYRTC_SIGNALING_URL` | `ws://127.0.0.1:8777/ws/host` | host → Bun WS |
| `SWAYRTC_CAPTURE_BIN` | `capture-sway.sh` | capture command |
| `SWAYRTC_PUBLIC_IP` | `auto` | extra direct candidates: `auto`, `off`, or an explicit IPv4 |
| `SWAYRTC_INPUT_BACKEND` | `native` | `legacy` forces wtype/wlrctl |
| `SWAYRTC_INPUT_RELATIVE` | off | relative instead of absolute pointer motion |
| `SWAYRTC_PASTE_SHIFT` | off | paste Unicode text with Ctrl+Shift+V (terminals) |

## Design decisions

### Media path: TURN is the default

Both ends of the reference deployment sit behind carrier grade NAT (CGNAT).
Measured NAT behaviour:

* host side: endpoint independent mapping, **port preserving** (external port ==
  local source port), but address/port dependent **filtering**;
* phone side: **symmetric** NAT (the same socket maps to different external
  ports per destination).

That combination cannot be traversed with STUN alone, and the carrier also
rotates the public IPv4 over time. Direct UDP therefore stays available but is
*not* the default: `iceTransportPolicy` is `relay`, so media always goes through
Cloudflare's TURN relay. DTLS-SRTP is end to end, so the relay only forwards
encrypted packets — it cannot read the screen.

`PATH POLICY` in the UI offers `TURN relay` (default), `Auto` (direct first,
relay as fallback) and `Direct only` (advanced, for LAN / non-CGNAT use).

TURN credentials are minted on demand by the Bun server from
`.secrets/turn-key` (a Cloudflare Realtime TURN key) and handed to authenticated
clients only, via `GET /api/ice`, with a 6 hour TTL.

### One session: the physical one

SwayRTC attaches to the Sway session that is driving the monitor - there is no
second compositor. `tools/resolve-session.sh` finds it (wayland-2 / HDMI-A-4 here)
by matching sway IPC sockets against wl_output connector names, because the
wayland socket moves between restarts; `host-start.sh` exports it, passes
`SWAYRTC_OUTPUT` to the capture and restarts the host when the session changes.
`sway-headless.service` is retired. The capture overlays the cursor and the
viewer hides the local one while a stream is live, so the remote picture is what
the monitor shows and pointer-lock mode still has a cursor.

### Capture: our own wlr-screencopy client

`native/src/bin/swayrtc-capture.rs` captures `HEADLESS-1` itself through
`zwlr_screencopy_manager_v1`, converts the XRGB buffer to NV12 across a few
threads, and drives FFmpeg over a pipe. It replaces `wf-recorder`, which kept
several captured frames in its own pool: with a full-screen stimulus the
pipeline now needs **21 ms** (7 ms floor) for capture+encode instead of 35 ms.
Having the capture in-process also gives exact access-unit boundaries and one
process less to supervise. `capture-sway.sh` prefers the binary and falls back
to wf-recorder if it has not been built.

The encoder is picked at start:

* **x264** (default) - lowest latency: 21 ms capture+encode, ~109 ms end to end.
* **qsv** (`SWAYRTC_ENCODER=qsv`) - FFmpeg uploads the NV12 with `hwupload` and
  encodes with `h264_vaapi` (Constrained Baseline, CQP). Half the CPU (79% vs
  159%) but **82 ms** capture+encode: the Intel LP encoder (the only H.264
  entrypoint iHD exposes on Gen9) keeps several frames in flight at 60 fps.
  Handing it one frame at a time takes 6 ms, so this is its steady-state
  pipeline, not a queue we can drain; `-async_depth 1`, `-threads 1`,
  `-filter_threads 1` and a larger pipe all made no difference.

Three constraints, all measured:

1. **Never hand keyboard events to an unreliable channel.** A lost keyup leaves
   the key held and the compositor repeats it (a tap becomes a burst); a lost
   keydown drops a character. Keys, buttons, wheel and text use a reliable
   ordered DataChannel, pointer motion uses an unreliable unordered one.
2. **H.264 must be Constrained Baseline** - Chrome decodes 2 of 250 Main/High
   frames and floods the host with PLIs.
3. **Rate control must be CQP, not a bitrate target.** `-b:v` + `maxrate` makes
   VA-API pad an unchanged screen up to the target (measured 5.75 Mbps on 60
   identical frames); CQP costs 1.05 Mbps for the same static desktop.

### Zero copy: working, as an option

`native/gpu/zerocopy-capture.c` is a complete GPU-only pipeline:

```
wlr-screencopy -> ARGB dma-buf (AMD, where the monitor hangs)
               -> AMD VPP colour conversion -> linear NV12 dma-buf
               -> Intel VA-API h264_vaapi (libavcodec, CQP, async_depth 1)
```

No pixel ever touches the CPU. Measured **~4% CPU** against ~160% for the x264
pipeline, at 60 fps, Constrained Baseline, colours matching a grim screenshot.

Three things had to be true, and each was verified before the code was written:

* Intel iHD exposes the H.264 encoder but **no video processor**; AMD exposes a
  VPP but **no H.264 encoder**. The split is forced by the hardware.
* A **linear** NV12 buffer is the only layout both drivers import. Intel cannot
  import AMD's tiled export ("resource allocation failed"), and AMD cannot
  allocate a linear NV12 buffer through gbm, so the buffer is allocated on AMD
  as a plain R8 surface and imported by both.
* libavcodec will encode an imported surface directly if the frame carries the
  surface id in `data[3]` plus a dummy `buf[0]`; `vaCopy` and `vaPutImage`
  between an imported and a pooled surface both fail.

Measured with the periodic keyframe disabled and an instant stimulus (solid
colour swap), so the numbers are the pipeline itself:

| | capture+encode | end to end | CPU |
|---|---|---|---|
| zero copy | **39 ms** | **95 ms** | **~4%** |
| x264 | 46 ms | 106 ms | ~160% |

Stability matters as much as the median, so the pipeline is shaped like a game
streaming host:

* **bounded frames** - VBR with a 100 KB VBV and `max_frame_size`. CQP let a
  full-screen change reach 190 KB, which occupies the uplink for ~250 ms and
  queues every frame behind it (p99 frame size 167 KB -> 25 KB).
* **newest wins** - the frame queue holds one frame, so a stale one is dropped
  instead of being sent late.
* **warm start** - the capture runs before the first viewer connects; the cold
  start used to stall the first second (worst gap 529 ms -> 34 ms).
* **trailing AUD** - access units close immediately instead of waiting for the
  next frame.

Measured spread over ten rounds: 89, 90, 91, 91, 92 ms (3 ms spread, was 29 ms).

The last 16 ms came from closing access units: the host's Annex-B parser can only
finish a frame when the next one starts, so the capture writes an access unit
delimiter right after the slices. The encoder itself answers in 2-3 ms and the
capture loop is 16 ms per frame (10 ms compositor copy + 3 ms VPP + 3 ms encode).

```
SWAYRTC_ENCODER=zerocopy   # cross-GPU pipeline: 4% CPU, 39 ms capture+encode
SWAYRTC_ENCODER=x264       # default: software encode, latency first
```

### Zero copy: the earlier dead end

The working pipeline above is custom code. The stock-tooling route was tried
first: cross GPU DMA-BUF was measured, not assumed. Even with the compositor forced
onto the Intel GPU (`WLR_DRM_DEVICES=/dev/dri/card1`) and the headless output
rendered there, `wl-screenrec --codec avc` still fails with

    Failed to create processing pipeline config: 12 (the requested VAProfile is not supported)

in `scale_vaapi`. So the obstacle is not only the AMD -> Intel import: Intel's
iHD cannot run the **RGBA -> NV12 video processing** that a wlroots capture
buffer needs. Encoding NV12 with `h264_vaapi` works fine (that is the path the
current pipeline uses, after converting on the CPU). Getting to zero copy would
need a custom EGL/Vulkan compute pass that writes NV12 into a dma-buf VA-API
accepts - a research sized change for a few CPU percent.

The shipped pipeline sidesteps exactly this: the colour conversion runs on the
AMD GPU's VPP instead of Intel's video processor, and the Intel side only
encodes the NV12 surface it is handed (see *Zero copy: working, as an option*).

### Renderer: media element by default, WebCodecs optional

The viewer paints through a `<video>` element by default. An optional WebCodecs
renderer installs `RTCRtpScriptTransform` on the video receiver, decodes the RTP
stream in a worker (`web/transform-worker.js`) and paints a canvas over the
element, which stays in place so size, focus, pointer lock and the input path
are untouched (the worker forwards every frame downstream as well).

Measured with a full-screen stimulus, both surfaces sampled in the same rAF
loop: the canvas becomes visible **6.4 ms earlier** (p50) than the element. That
is the media element's presentation queue - the jitter buffer sits before the
transform and cannot be skipped from script. Pick it in "PATH POLICY" next to
the ICE policy; the toggle persists per tab.

Two things that cost an hour and are worth knowing:

* `createEncodedStreams()` on the receiver throws `InvalidStateError: Too late`
  as soon as anything has started the receiver (attaching `srcObject` counts),
  so the transform must be armed right after `addTransceiver`, before signalling.
* The frame handed to the transform is detached by `writer.write()`. Copy
  `value.data` **before** forwarding it, or the decoder receives empty chunks
  and silently produces nothing.

### Frame policy: newest wins

The capture sink publishes into a **3 frame** ring. If the pump cannot keep up
the broadcast channel drops the oldest frames and the receiver jumps to the
newest one, so a stall appears as one dropped frame instead of a delay that
keeps growing. The GOP cache used for joining mid-stream is separate, so a new
viewer still starts on a keyframe. Since the ring was tightened no `pump.lagged`
events appear in a normal session.

### Where the latency actually goes

Measured on this machine (viewer and host share a clock, so the numbers are
direct one way measurements, not estimates):

| Leg | Value |
| --- | --- |
| Viewer DataChannel send -> host receive | 20 ms (p90 24 ms) |
| Network one way (ICE RTT 41 ms / 2) | 20.5 ms |
| Jitter buffer, `playoutDelayHint=0` applied | 7.4 ms (24.4 ms without the hint) |
| H.264 decode | 1.3 ms |
| End to end: pointer move -> moved cursor visible in the video (headless Chrome) | **~124 ms** |

Keyboard feels instant because typing tolerates 100 ms; pointing does not. The
round trip through the relay is the single biggest term, and headless Chrome's
video path inflates the last figure - a headed browser is faster.

Getting closer to a game streaming service needs, in order of impact:

1. **A direct path.** Cloudflare TURN costs two network legs (~40 ms round
   trip). Direct UDP or IPv6 would remove most of it. The UI's `Auto` policy will
   use a direct path when one exists; `relay` is the default because carrier
   grade NAT makes it fail for some clients.
2. **Decode without the media element.** WebCodecs plus a canvas removes the
   element's presentation buffering and the jitter buffer (tens of ms).
3. **Hardware encode.** Quick Sync would remove a few ms of encode time (see
   above for why it is blocked here).
4. **Shorter frame quantisation** is not available: capture already runs at
   60 fps continuously.

## Testing

All test tooling runs against isolated instances on their own port and a
throwaway Linux user (`swayrtctest`); production is never touched.

```bash
PORT=8792 bash tools/testenv.sh start     # isolated Bun + host (+ TURN key copy)
BASE=http://127.0.0.1:8792 node tools/diag.mjs        # quick connect check
BASE=http://127.0.0.1:8792 node tools/turn-test.mjs   # forced-relay reachability
BASE=http://127.0.0.1:8792 node tools/e2e.mjs         # full acceptance suite
PORT=8792 bash tools/testenv.sh logs|stop
```

`tools/e2e.mjs` checks login, video, the selected ICE path, live fps/latency,
input delivery to the host (via `swayrtc-input: event` debug lines), reload
recovery, the mobile layout, JS errors and capture child cleanup.

`tools/authcheck.sh` is the authentication regression suite (74 curl checks:
wrong password, rate limit, CSRF, traversal, session rotation, tunnel headers).

## Relationship to WayVNC / noVNC

This replaces them. The old `browser-vnc-*` units (wayvnc, websockify, nginx and
its tunnel) are stopped and disabled; nothing in this repository depends on them.

## Known limitations

* One viewer at a time; a second viewer takes over the session.
* Text injection goes through `wtype` and follows the host keyboard layout —
  CJK input needs an IME on the host, which is out of scope here.
* No audio.
* Media goes through Cloudflare TURN by default: expect one extra network hop and
  relay bandwidth accounting. Switch `PATH POLICY` to `Auto` on networks where
  direct UDP is possible.
* The default encoder is software x264 (~160% CPU at 1080p60). The VA-API and
  zero-copy paths cut CPU sharply but add encode latency (see above).
* The headless output can stop repainting after long idle periods, leaving stale
  content in the captured buffer. Continuous capture (`-D`) and a `swaybg`
  restart both force a full repaint.

## Layout

```
server.ts              Bun: PAM auth, sessions, /api/ice, WSS signaling, static UI
web/                   UI (no build step, no external CDN, strict CSP)
native/src/main.rs     WebRTC host, capture supervision, peer lifecycle
native/src/input.rs    Wayland virtual keyboard / pointer
native/gpu/            cross-GPU zero-copy capture (C; dma-buf: AMD VPP → Intel VA-API)
native/protocols/      vendored protocol XML for offline builds
capture-sway.sh        capture entry point: native client first, wf-recorder fallback
mock-capture.sh        synthetic 60 fps source for throughput tests
tools/                 test environment, E2E, auth regression suites
```
