# Generalizing SwayRTC: capability probe, planner, portable capture

Status: design + phase 1 prototype (`native/src/bin/swayrtc-probe.rs`).
Target: SwayRTC should run on any Sway machine, not only the reference
machine it was developed on. The capture pipeline in particular must adapt
to whatever GPUs are installed instead of assuming one specific pair.

## 1. What is machine-specific today

The reference machine has two GPUs with complementary VA-API capabilities:

- AMD RX 6400 (amdgpu): exposes a video processor (VPP) but **no H.264
  encoder** (Navi 24 has no encode block; VA-API shows decode entrypoints
  only).
- Intel UHD 630 (i915 / iHD): exposes an H.264 encoder but **no VPP
  entrypoint at all** on this driver. Its encoder is the low-power one:
  the only encode entrypoint is `VAEntrypointEncSliceLP`.

The zero-copy pipeline exists because of this split (AMD converts, Intel
encodes), but the split is baked into the code as literal device paths and
prose, and the remaining pipelines are selected by a human setting env
vars. Concretely:

| Assumption | Where |
| --- | --- |
| `/dev/dri/renderD129` is the AMD GPU, `/dev/dri/renderD128` is the Intel GPU | `native/gpu/zerocopy-capture.c` (`open_va` calls at the end of `main`), `native/gpu/zerocopy-encode.c` (same) |
| The vendor split itself ("only AMD exposes a video processor... only Intel exposes an H.264 encoder") | header comments of both C files; the layout only works for this exact pair |
| Encode device defaults to `renderD128` | `SWAYRTC_VA_DEVICE` default in `capture-sway.sh` (the `run_gated` check and the ffmpeg `h264_vaapi` fallback), `native/src/bin/swayrtc-capture.rs` (`va_device`), `native/src/main.rs` (passed to the capture child) |
| Encoder is chosen manually; default is software x264 | `SWAYRTC_ENCODER` in `capture-sway.sh` and `native/src/bin/swayrtc-capture.rs` |
| The shared NV12 buffer is allocated on a specific device because of which driver can mmap/import it | comments and allocation order in `native/gpu/zerocopy-capture.c` |
| Output defaults to `HEADLESS-1`; the physical connector is `HDMI-A-4` here | `SWAYRTC_OUTPUT` default in `capture-sway.sh` and `native/src/bin/swayrtc-capture.rs` |
| Session/output discovery lives outside the binaries | `host-start.sh` shells out to `tools/resolve-session.sh` → `tools/resolve-session.py`, which reads the compositor's fds, matches sway IPC sockets against output names, prefers connectors starting with HDMI/DP/eDP, and creates a headless output inside the session when no monitor is attached |
| Media goes through a TURN relay by default (the reference deployment sits behind CGNAT on both ends) | `SWAYRTC_RELAY_ONLY` handling in `native/src/main.rs`; `PATH POLICY` default in the web UI |

Nothing here is wrong for the reference machine — the measured numbers in
the README come from exactly these choices. The problem is that the
*knowledge* ("which GPU can convert", "which can encode", "the LP encoder
behaves like this") lives in comments and in the operator's head, so the
same binaries silently do the wrong thing, or fail, on different hardware:
a single-Intel laptop, an AMD APU, a machine where the render nodes are
numbered the other way round.

## 2. Architecture: probe → model → planner → plan

Three stages, each independently testable:

```
  sysfs + libva          capability            strategy
 ───────────────▶ probe ─────────▶ model ──────▶ planner ──▶ plan
 (per render node)      (per device)           (one output)
```

### 2.1 Probe

For every `/dev/dri/renderD*` node, collect:

- PCI vendor/device IDs and the kernel driver, from
  `/sys/class/drm/<node>/device/{vendor,device,driver}`.
- Whether the device owns a connected physical connector
  (`/sys/class/drm/cardN-<conn>/status`) and whether it is the
  `boot_vga` device.
- VA-API capabilities, by linking libva directly:
  `vaGetDisplayDRM(fd)` + `vaInitialize`, then
  `vaQueryConfigProfiles` / `vaQueryConfigEntrypoints` per profile, plus
  `vaQueryVendorString`. The phase 1 prototype does exactly this (no
  `vainfo` subprocess; `vainfo`'s human-oriented output is not a stable
  interface, and the probe must run where a human is not watching).

Two capability subtleties the probe has to get right:

- **Video processing is queried as `VAEntrypointVideoProc` on
  `VAProfileNone`.** A device either lists it or not; there is no per-format
  answer at this level.
- **An H.264 encoder may appear as `VAEntrypointEncSlice` or
  `VAEntrypointEncSliceLP`.** The Intel Gen9 low-power encoder exposes
  *only* the LP form. A probe that looks for `EncSlice` alone reports the
  reference machine's Intel GPU as "no encoder" and the whole pipeline
  design collapses — this is the single easiest detection bug to write.

The probe is a hypothesis, not a proof: an advertised entrypoint does not
guarantee a specific conversion works (the README documents
`scale_vaapi` failing on Intel with "the requested VAProfile is not
supported" for a path its entrypoints seemed to allow). The planner
therefore emits not just a plan but an ordered fallback chain, and the
runtime (phase 2) must validate the chosen pipeline at startup and walk
down the chain if initialisation fails, instead of dying.

### 2.2 Capability model

Per device:

```
Device {
  render_node   /dev/dri/renderD128
  pci           vendor 0x8086, device 0x3e92
  kernel_driver i915
  va_driver     "Intel iHD driver for Intel(R) Gen Graphics - 26.3.5"
  vpp           false                       // VideoProc on VAProfileNone
  h264_encode   { ConstrainedBaseline: [EncSliceLP], Main: [EncSliceLP],
                  High: [EncSliceLP] }      // encode entrypoints only
  connected_output  Some("HDMI-A-4") | None
  boot_vga      bool
}
```

The plan only depends on `vpp`, `h264_encode` (ConstrainedBaseline is the
profile that matters — browsers will not decode the others reliably in
this pipeline, per the README's design notes), and which device is the
capture source.

### 2.3 Capture-source determination

The compositor's frames originate on one GPU: the one driving the output
being captured. The probe decides which, in this order:

1. `SWAYRTC_CAPTURE_DEVICE` override (explicit operator choice wins).
2. The device owning a connected physical connector — definitive when a
   monitor is attached.
3. The `boot_vga` device — when every connector is disconnected the
   session is headless, and wlroots renders headless buffers on its
   primary GPU, which in practice is the boot/display GPU.
4. The first render node (last resort).

(Scanning `/proc` for the compositor's open DRM fds does **not**
disambiguate: wlroots multi-GPU opens every card. It is not used.)

### 2.4 Planner

A strategy is a path `(capture device → convert location → encode
location)`. Selection order, first match wins:

| # | Strategy | Condition | Path |
| --- | --- | --- | --- |
| 1 | zero-copy, single-device | capture device has VPP **and** H.264 encode | capture → same device VPP → same device encode |
| 2 | zero-copy, cross-device | capture device has VPP; some other device has H.264 encode | capture → capture-device VPP → other device encode (dma-buf handoff) |
| 3 | CPU convert + VA-API encode | some device has H.264 encode (prefer the capture device) | capture → CPU XRGB→NV12 → that device's encode |
| 4 | CPU convert + x264 | always available | capture → CPU → libx264 |

Tie-breaking inside a strategy: prefer the capture device itself, then
the lowest render-node number. Strategy 1 is the common laptop case
(single Intel iGPU, or an AMD APU whose VPP and VCN encoder live on the
same device) and is worth stating explicitly: most machines are not the
reference machine's two-vendor split, they are this.

The planner prints its rejects as well as its choice — "strategy 1
rejected: capture GPU has no H.264 encode entrypoint" — because on an
unknown machine the reasoning *is* the debugging output.

### 2.5 Doctor UX

`swayrtc-probe` with no arguments prints the device table and the plan.
This is the `doctor` surface: run it on any machine and it states what
SwayRTC would do there and why. Real output on the reference machine
(no monitor attached at the time, so the capture source came from rule
3):

```
$ swayrtc-probe
...
Capture source: /dev/dri/renderD129 (AMD, driver amdgpu) — boot_vga GPU (no connected connector found)
Plan: zero-copy, cross-device
  capture  /dev/dri/renderD129 (AMD) — compositor dma-buf
  convert  /dev/dri/renderD129 (AMD) — VPP
  encode   /dev/dri/renderD128 (Intel) — h264_vaapi, ConstrainedBaseline via EncSliceLP
```

(The full output of the prototype is in the phase 1 commit message /
PR description.) A `--json` form of the model is planned for phase 2 so
the capture binary and SwayRTC can consume the plan mechanically; phase 1
keeps the plan human-readable and encodes the same decisions the shell
scripts already express through `SWAYRTC_ENCODER` / `SWAYRTC_VA_DEVICE`,
so adopting it changes no runtime behaviour on the reference machine.

## 3. Splitting capture into its own project

`native/src/bin/swayrtc-capture.rs` (wlr-screencopy client) and
`native/gpu/` (the VA-API pipelines) are a self-contained tool already:
frames in from the compositor, H.264 Annex-B out on stdout. They move to
a separate repository (name to be decided; `wl-screenrec` is taken by the
existing project it was compared against in the README):

- CLI: `--output <name>`, `--fps <n>`,
  `--encoder auto|x264|vaapi|zerocopy`, `--probe` (print the plan and
  exit). `auto` is the planner of §2.4.
- The probe/planner code moves with it — capability knowledge belongs to
  the component that opens the GPUs.
- Contract with SwayRTC stays what it already is: Annex-B H.264 on
  stdout, diagnostics on stderr, configuration by flags/env. SwayRTC
  keeps spawning it through `SWAYRTC_CAPTURE_BIN`
  (`native/src/main.rs`), so the split needs no protocol work.
- The existing `SWAYRTC_*` env names are honoured during the transition
  (`SWAYRTC_ENCODER`, `SWAYRTC_VA_DEVICE`, `SWAYRTC_OUTPUT`,
  `SWAYRTC_FPS`, ...) and deprecated in favour of flags.

## 4. Phasing

1. **Probe (this branch).** `swayrtc-probe` enumerates devices, builds
   the capability model via libva, runs the planner rules, prints the
   plan. Acceptance: on the reference machine it selects strategy 2 —
   convert on the AMD device, encode on Intel `renderD128` — matching
   the hand-tuned configuration. No changes to the capture binaries,
   the host, or the running services.
2. **Planner wiring + capture split.** The capture binaries consume the
   plan (strategy attempt + startup validation + fallback chain of
   §2.1); the C zero-copy program takes its two device roles as
   parameters instead of literals; capture moves to its own repo with
   the CLI of §3.
3. **SwayRTC-side generalization.** Output/session discovery absorbed
   from `tools/resolve-session.py` into the host (or kept as a helper
   shipped with the project): env > focused output > first active
   output, headless creation when nothing is attached. TURN becomes a
   policy choice instead of the default assumption (`Auto` path policy
   already exists in the UI; `relay` stops being the default on
   networks where direct UDP works). Packaging so a fresh machine needs
   no reference-machine folklore.

## 5. Non-goals (for now)

- **NVIDIA zero-copy.** There is no VA-API encode path to plan around;
  NVENC through ffmpeg remains reachable via strategy 3/4 variants but
  is not designed here.
- **Non-wlroots compositors.** Capture is `zwlr_screencopy_manager_v1`;
  GNOME/KDE would need different protocols and are out of scope.
- **Multi-output capture and per-output plans.** One output, one plan.
- **Replacing PAM authentication or the signaling design.** They are not
  machine-specific in the way the capture pipeline is.

## 6. Status addendum (2026-11-10, phase 2)

Phase 2 is implemented on branch `feat/planner-integration`:

- The probe/planner lives in the shared module `native/src/caps.rs`
  (crate lib target), used by both `swayrtc-probe` and `swayrtc-capture`.
  `swayrtc-probe` additionally prints the plan machine-readably
  (`--format env` / `--format json`).
- `swayrtc-capture` with `SWAYRTC_ENCODER` unset/`auto` consults the
  planner for the CPU strategies; `capture-sway.sh` in auto mode reads
  the probe plan and takes the zero-copy C path only for zero-copy
  plans, exporting the plan device roles
  (`SWAYRTC_ZEROCOPY_CAPTURE_DEVICE` / `SWAYRTC_ZEROCOPY_ENCODE_DEVICE`,
  now honoured by the C pipeline). Auto-chosen VA-API pipelines are
  gated by a one-frame startup validation and fall back to x264 with
  the reason logged.
- **Capture has a new home: [zerocap](https://github.com/souta-lab/zerocap)**
  (public). The split project is wlroots-generic rather than
  Sway-specific — registry-based output enumeration, `ZEROCAP_*`
  environment, `zerocap` / `zerocap-probe` binaries — and contains the
  capture bin, the probe, the caps module and the `gpu/` zero-copy
  pipeline with the CLI of §3.
- The in-tree capture code remains in place and production wiring is
  unchanged: `SWAYRTC_CAPTURE_BIN` is the seam through which the host
  can later point at a zerocap build. Removing the in-tree copy is
  phase 3, to be done only after such a switch is proven in production.
