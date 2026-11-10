mod input;
/// SwayRTC host: WebRTC P2P transport with GPU-encoded Annex-B H.264 capture.
///
/// Lifecycle overview:
///   signaling WS (exponential-backoff reconnect, never exits on drop)
///     -> one RTCPeerConnection at a time (generation tagged, stale events ignored)
///     -> one global capture process group, started ahead of negotiation,
///        killed (process group + /proc sweep) when the viewer is gone
///     -> encoded access units are broadcast to the active peer; the last GOP is
///        cached so a reconnecting viewer decodes video immediately.
use anyhow::{bail, Context, Result};
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::process::Command;
use tokio::sync::{broadcast, mpsc, watch, Notify};
use tokio_tungstenite::{
    connect_async,
    tungstenite::{client::IntoClientRequest, Message},
    MaybeTlsStream, WebSocketStream,
};
use webrtc::{
    api::{media_engine::MediaEngine, interceptor_registry::register_default_interceptors, API, APIBuilder},
    stats::StatsReportType,
    ice_transport::{ice_candidate::RTCIceCandidateInit, ice_server::RTCIceServer},
    interceptor::registry::Registry,
    media::Sample,
    peer_connection::{
        configuration::RTCConfiguration, peer_connection_state::RTCPeerConnectionState,
        sdp::session_description::RTCSessionDescription, RTCPeerConnection,
    },
    rtp_transceiver::rtp_codec::RTCRtpCodecCapability,
    track::track_local::{track_local_static_sample::TrackLocalStaticSample, TrackLocal},
};

// ---------------------------------------------------------------------------
// Logging.  Always-on lines are structured `key=value` on stderr (journald);
// SWAYRTC_DEBUG=1 adds SDP/candidate/ICE detail.
// ---------------------------------------------------------------------------
fn debug_enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("SWAYRTC_DEBUG").as_deref() == Ok("1"))
}
fn uptime_ms() -> u128 {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_millis()
}
macro_rules! log_info {
    ($($arg:tt)*) => {{ eprintln!("[swayrtc-host] +{:>7}ms {}", uptime_ms(), format!($($arg)*)); }};
}
macro_rules! log_debug {
    ($($arg:tt)*) => {{ if debug_enabled() { eprintln!("[swayrtc-host][debug] +{:>7}ms {}", uptime_ms(), format!($($arg)*)); } }};
}

// ---------------------------------------------------------------------------
// Tunables
// ---------------------------------------------------------------------------
// Frames buffered between the capture sink and the live pump. Small on
// purpose: if the pump falls behind, the broadcast channel drops the oldest
// frames and the receiver jumps to the newest one ("latest frame wins") instead
// of converging slowly while the user sees stale pixels.
// Newest wins: a deeper queue only adds latency when the sender falls behind.
// One slot means a stale frame is dropped instead of being sent late.
const FRAME_CHANNEL_CAPACITY: usize = 1;
const GOP_CACHE_MAX_BYTES: usize = 12 * 1024 * 1024;
const GOP_CACHE_MAX_FRAMES: usize = 240;
const H264_MAX_BUFFER_BYTES: usize = 12 * 1024 * 1024;
const SIGNAL_BACKOFF_MIN: Duration = Duration::from_millis(500);
const SIGNAL_BACKOFF_MAX: Duration = Duration::from_secs(8);
const WS_PING_INTERVAL: Duration = Duration::from_secs(25);
const MAX_CAPTURE_RESTARTS: u32 = 5;
const NEGOTIATION_TIMEOUT: Duration = Duration::from_secs(15);

fn env_ms(name: &str, default_ms: u64) -> Duration {
    match std::env::var(name).ok().and_then(|v| v.trim().parse::<u64>().ok()) {
        Some(ms) if ms > 0 && ms <= 600_000 => Duration::from_millis(ms),
        _ => Duration::from_millis(default_ms),
    }
}

// ---------------------------------------------------------------------------
// Process-tree helpers.  The capture backend is a shell pipeline
// (capture-sway.sh -> wl-screenrec | ffmpeg), so killing the direct child is
// not enough: the whole process group plus any reparented descendants must die.
// ---------------------------------------------------------------------------
mod procs {
    use std::collections::HashSet;
    use std::os::unix::fs::MetadataExt;

    pub const SIGTERM: i32 = 15;
    pub const SIGKILL: i32 = 9;
    const PR_SET_PDEATHSIG: i32 = 1;

    extern "C" {
        fn kill(pid: i32, signal: i32) -> i32;
        fn getuid() -> u32;
        fn prctl(option: i32, ...) -> i32;
    }

    pub fn uid() -> u32 {
        unsafe { getuid() }
    }

    /// Ask the kernel to SIGKILL the capture backend if this host process dies
    /// without a chance to clean up (SIGKILL, panic, OOM).  Must be async-signal
    /// safe: prctl only.
    pub fn die_with_parent() {
        unsafe {
            prctl(PR_SET_PDEATHSIG, SIGKILL, 0, 0, 0);
        }
    }

    fn signal_pid(pid: i32, signal: i32) {
        if pid > 1 {
            unsafe {
                kill(pid, signal);
            }
        }
    }

    /// (ppid, pgid) from /proc/<pid>/stat.
    fn stat(pid: i32) -> Option<(i32, i32)> {
        let raw = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let rest = &raw[raw.rfind(')')? + 1..];
        let mut fields = rest.split_whitespace();
        let _state = fields.next()?; // field 3
        let ppid: i32 = fields.next()?.parse().ok()?; // field 4
        let pgid: i32 = fields.next()?.parse().ok()?; // field 5
        Some((ppid, pgid))
    }

    fn owned_by_us(pid: i32) -> bool {
        std::fs::metadata(format!("/proc/{pid}"))
            .map(|m| m.uid() == uid())
            .unwrap_or(false)
    }

    /// Every live process owned by this uid as (pid, ppid, pgid).
    fn snapshot() -> Vec<(i32, i32, i32)> {
        let Ok(entries) = std::fs::read_dir("/proc") else {
            return Vec::new();
        };
        let me = std::process::id() as i32;
        entries
            .flatten()
            .filter_map(|entry| {
                let pid: i32 = entry.file_name().into_string().ok()?.parse().ok()?;
                if pid == me || !owned_by_us(pid) {
                    return None;
                }
                let (ppid, pgid) = stat(pid)?;
                Some((pid, ppid, pgid))
            })
            .collect()
    }

    /// Every process in `pgid` plus all of their descendants, with each
    /// process' own process group id.  Never includes the host itself, so a
    /// stray pgid collision cannot make us kill ourselves.
    pub fn tree_with_groups(pgid: i32) -> Vec<(i32, i32)> {
        if pgid <= 1 {
            return Vec::new();
        }
        if let Some((_, own_group)) = stat(std::process::id() as i32) {
            if own_group == pgid {
                return Vec::new();
            }
        }
        let snap = snapshot();
        let mut set: HashSet<i32> = snap.iter().filter(|(_, _, g)| *g == pgid).map(|(p, _, _)| *p).collect();
        if set.is_empty() {
            return Vec::new();
        }
        loop {
            let mut added = false;
            for (pid, ppid, _) in &snap {
                if set.contains(ppid) && !set.contains(pid) {
                    set.insert(*pid);
                    added = true;
                }
            }
            if !added {
                break;
            }
        }
        snap.iter()
            .filter(|(pid, _, _)| *pid > 1 && set.contains(pid))
            .map(|(pid, _, group)| (*pid, *group))
            .collect()
    }

    pub fn group_tree(pgid: i32) -> Vec<i32> {
        tree_with_groups(pgid).into_iter().map(|(pid, _)| pid).collect()
    }

    pub fn group_alive(pgid: i32) -> bool {
        !group_tree(pgid).is_empty()
    }

    /// SIGTERM/SIGKILL the process group AND every descendant that escaped it
    /// (re-parented / double-forked grandchildren).  Members of the group are
    /// signalled exactly once by killpg, so ffmpeg does not see a burst of
    /// duplicate signals.
    pub fn signal_group_tree(pgid: i32, signal: i32) {
        if pgid <= 1 {
            return;
        }
        unsafe {
            kill(-pgid, signal); // killpg: bash + wl-screenrec + ffmpeg
        }
        for (pid, pid_group) in tree_with_groups(pgid) {
            if pid_group != pgid {
                signal_pid(pid, signal);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Frame pipeline: capture -> broadcast -> per-peer RTP pump
// ---------------------------------------------------------------------------
struct Frame {
    data: Bytes,
    duration: Duration,
    is_idr: bool,
    /// Wall clock (ms since the unix epoch) when this access unit was read from
    /// the capture child. The viewer runs on the same machine in the reference
    /// setup, so it can be compared against viewer timestamps.
    read_at_ms: u64,
}

/// Rolling cache of the most recent IDR and everything after it, so a viewer
/// that connects mid-stream can start decoding from a keyframe immediately.
#[derive(Default)]
struct GopCache {
    frames: Vec<Arc<Frame>>,
    bytes: usize,
}

impl GopCache {
    fn push(&mut self, frame: Arc<Frame>) {
        if frame.is_idr {
            self.frames.clear();
            self.bytes = 0;
        } else if self.frames.is_empty() {
            return; // wait for a keyframe
        }
        self.bytes += frame.data.len();
        self.frames.push(frame);
        while self.frames.len() > GOP_CACHE_MAX_FRAMES || self.bytes > GOP_CACHE_MAX_BYTES {
            if self.frames.is_empty() {
                break;
            }
            let old = self.frames.remove(0);
            self.bytes = self.bytes.saturating_sub(old.data.len());
        }
    }

    fn snapshot(&self) -> Vec<Arc<Frame>> {
        self.frames.clone()
    }
}

#[derive(Default)]
struct CaptureStats {
    frames: AtomicU64,
    bytes: AtomicU64,
    write_errors: AtomicU64,
}

struct CaptureSink {
    frames: broadcast::Sender<Arc<Frame>>,
    gop: Arc<Mutex<GopCache>>,
    stats: Arc<CaptureStats>,
}

impl CaptureSink {
    fn emit(&self, data: Bytes, duration: Duration, is_idr: bool) {
        let frame = Arc::new(Frame {
            data,
            duration: duration.max(Duration::from_millis(1)),
            is_idr,
            read_at_ms: epoch_ms(),
        });
        self.stats.frames.fetch_add(1, Ordering::Relaxed);
        self.stats.bytes.fetch_add(frame.data.len() as u64, Ordering::Relaxed);
        if let Ok(mut gop) = self.gop.lock() {
            gop.push(frame.clone());
        }
        let _ = self.frames.send(frame); // Err simply means "no viewer attached"
    }
}

// --- Annex-B / H.264 access-unit reassembly -------------------------------
fn code_len(data: &[u8], offset: usize) -> usize {
    if data.get(offset..offset + 4) == Some(&[0, 0, 0, 1]) {
        4
    } else if data.get(offset..offset + 3) == Some(&[0, 0, 1]) {
        3
    } else {
        0
    }
}
fn start_code(data: &[u8], from: usize) -> Option<(usize, usize)> {
    (from..data.len().saturating_sub(2)).find_map(|i| {
        let len = code_len(data, i);
        if len > 0 { Some((i, len)) } else { None }
    })
}
/// Read first_mb_in_slice Exp-Golomb code (0 means first slice of an access unit).
fn first_mb_zero(nal: &[u8]) -> bool {
    if nal.len() < 2 {
        return false;
    }
    let mut zeros = 0usize;
    for &byte in &nal[1..] {
        for shift in (0..8).rev() {
            if byte & (1 << shift) != 0 {
                return zeros == 0;
            }
            zeros += 1;
            if zeros > 23 {
                return false;
            }
        }
    }
    false
}

struct AnnexBParser {
    buf: Vec<u8>,
    au: Vec<u8>,
    vcl: bool,
    au_idr: bool,
    last: Instant,
}

impl AnnexBParser {
    fn new() -> Self {
        Self {
            buf: Vec::with_capacity(1 << 20),
            au: Vec::with_capacity(200_000),
            vcl: false,
            au_idr: false,
            last: Instant::now(),
        }
    }

    fn flush(&mut self, sink: &CaptureSink) {
        if self.au.is_empty() {
            return;
        }
        let now = Instant::now();
        let duration = now.duration_since(self.last);
        self.last = now;
        let data = std::mem::take(&mut self.au);
        sink.emit(Bytes::from(data), duration, self.au_idr);
        self.vcl = false;
        self.au_idr = false;
    }

    fn feed(&mut self, chunk: &[u8], sink: &CaptureSink) {
        self.buf.extend_from_slice(chunk);
        while let Some((begin, header)) = start_code(&self.buf, 0) {
            let Some((end, _)) = start_code(&self.buf, begin + header) else {
                break;
            };
            if end <= begin + header {
                break;
            }
            let nal = self.buf[begin + header..end].to_vec();
            self.buf.drain(..end);
            if nal.is_empty() {
                continue;
            }
            let kind = nal[0] & 0x1f;
            if self.vcl && (kind == 9 || kind == 7 || ((kind == 1 || kind == 5) && first_mb_zero(&nal))) {
                self.flush(sink);
            }
            self.au.extend_from_slice(&[0, 0, 0, 1]);
            self.au.extend_from_slice(&nal);
            if kind == 1 || kind == 5 {
                self.vcl = true;
            }
            if kind == 5 {
                self.au_idr = true;
            }
        }
        if self.buf.len() > H264_MAX_BUFFER_BYTES {
            log_info!("event=capture.parser_overflow bytes={} action=reset", self.buf.len());
            self.buf.clear();
        }
    }
}

// ---------------------------------------------------------------------------
// Capture process manager: exactly one process group for the whole host, with
// start/stop requests serialised through a watch channel and a supervisor task.
// ---------------------------------------------------------------------------
#[derive(Default)]
struct CaptureInfo {
    pid: Option<i32>,
    gen: u64,
    started: Option<Instant>,
    wanted: bool,
}

#[derive(Clone)]
struct CaptureManager {
    desired: Arc<watch::Sender<bool>>,
    info: Arc<Mutex<CaptureInfo>>,
    frames: broadcast::Sender<Arc<Frame>>,
    gop: Arc<Mutex<GopCache>>,
    stats: Arc<CaptureStats>,
    generation: Arc<AtomicU64>,
}

impl CaptureManager {
    fn new(frames: broadcast::Sender<Arc<Frame>>, gop: Arc<Mutex<GopCache>>) -> (Self, watch::Receiver<bool>) {
        let (desired, rx) = watch::channel(false);
        (
            Self {
                desired: Arc::new(desired),
                info: Arc::new(Mutex::new(CaptureInfo::default())),
                frames,
                gop,
                stats: Arc::new(CaptureStats::default()),
                generation: Arc::new(AtomicU64::new(0)),
            },
            rx,
        )
    }

    /// Request that capture runs.  Idempotent, and returns immediately: the
    /// supervisor task owns the actual spawn so a dead process can be replaced
    /// without racing another start.  `wanted` and the watch send are updated
    /// under the same lock as stop() so the two can never interleave.
    fn start(&self) {
        let running = {
            let Ok(mut info) = self.info.lock() else {
                return;
            };
            if info.wanted {
                return;
            }
            info.wanted = true;
            let running = info.pid.is_some();
            self.desired.send_replace(true);
            running
        };
        log_info!("event=capture.request running={running}");
    }

    /// Kill the capture process group and wait until it is gone.  Safe to call
    /// concurrently with the supervisor: both only signal the same pgid.
    async fn stop(&self, reason: &str) {
        let pid = {
            let Ok(mut info) = self.info.lock() else {
                return;
            };
            if !info.wanted && info.pid.is_none() {
                return;
            }
            info.wanted = false;
            self.desired.send_replace(false);
            info.pid
        };
        match pid {
            Some(pid) => self.terminate_group(pid, reason).await,
            None => {
                // A spawn may still be in flight; give the supervisor a moment
                // and sweep whatever it produced.
                tokio::time::sleep(Duration::from_millis(120)).await;
                match self.info.lock().ok().and_then(|info| info.pid) {
                    Some(pid) => self.terminate_group(pid, reason).await,
                    None => log_info!("event=capture.stop reason={reason} pid=none"),
                }
            }
        }
    }

    async fn terminate_group(&self, pid: i32, reason: &str) {
        log_info!("event=capture.stop reason={reason} pid={pid}");
        procs::signal_group_tree(pid, procs::SIGTERM);
        let deadline = Instant::now() + Duration::from_millis(1500);
        while Instant::now() < deadline && procs::group_alive(pid) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        if procs::group_alive(pid) {
            log_info!("event=capture.stop_escalate pid={pid} reason=still_alive");
            procs::signal_group_tree(pid, procs::SIGKILL);
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
        let remaining = procs::group_tree(pid).len();
        if remaining > 0 {
            log_info!("event=capture.stop_incomplete pid={pid} remaining={remaining}");
        } else {
            log_info!("event=capture.stopped pid={pid} remaining=0");
        }
    }

    fn is_running(&self) -> bool {
        self.info.lock().map(|i| i.pid.is_some()).unwrap_or(false)
    }
}

/// Last-resort cleanup guard, created exactly once in main().  It is deliberately
/// not Clone: CaptureManager is cloned into callbacks and a Drop impl there would
/// kill the group whenever a transient clone goes away.  kill(2) is sync-safe, so
/// this also runs during panic unwinding.
struct CaptureKiller {
    capture: CaptureManager,
}

impl Drop for CaptureKiller {
    fn drop(&mut self) {
        if let Ok(info) = self.capture.info.lock() {
            if let Some(pid) = info.pid {
                log_info!("event=capture.guard_kill pid={pid}");
                procs::signal_group_tree(pid, procs::SIGKILL);
            }
        }
    }
}

fn spawn_capture() -> Result<(String, tokio::process::Child)> {
    let bin = std::env::var("SWAYRTC_CAPTURE_BIN").unwrap_or_else(|_| {
        match std::env::var("HOME") {
            Ok(home) => format!("{home}/.local/bin/wl-screenrec"),
            Err(_) => "wl-screenrec".into(),
        }
    });
    let wayland = std::env::var("WAYLAND_DISPLAY").unwrap_or_else(|_| "wayland-1".into());
    let dri = std::env::var("SWAYRTC_VA_DEVICE").unwrap_or_else(|_| "/dev/dri/renderD128".into());
    let mut cmd = Command::new(&bin);
    cmd.env("XDG_RUNTIME_DIR", format!("/run/user/{}", procs::uid()))
        .env("WAYLAND_DISPLAY", wayland)
        .arg("-o")
        .arg("HEADLESS-1")
        .arg("-m")
        .arg("60")
        .arg("--codec")
        .arg("avc")
        .arg("--dri-device")
        .arg(dri)
        .arg("--ffmpeg-muxer")
        .arg("h264")
        .arg("-f")
        .arg("-")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        // Own process group: killpg() then reaches the ffmpeg grandchild too.
        .process_group(0)
        .kill_on_drop(true);
    // Even a SIGKILLed host takes the capture pipeline down with it.
    unsafe {
        cmd.pre_exec(|| {
            procs::die_with_parent();
            Ok(())
        });
    }
    let child = cmd.spawn().with_context(|| format!("spawning capture backend {bin}"))?;
    Ok((bin, child))
}

async fn capture_supervisor(mut desired: watch::Receiver<bool>, capture: CaptureManager) {
    let mut restarts: u32 = 0;
    loop {
        if !*desired.borrow() {
            if desired.wait_for(|v| *v).await.is_err() {
                return; // manager gone
            }
        }
        let gen = capture.generation.fetch_add(1, Ordering::SeqCst) + 1;
        let (bin, mut child) = match spawn_capture() {
            Ok(value) => value,
            Err(e) => {
                log_info!("event=capture.spawn_failed gen={gen} error={e:#}");
                restarts += 1;
                if restarts > MAX_CAPTURE_RESTARTS {
                    log_info!("event=capture.give_up reason=spawn_failed attempts={restarts}");
                    restarts = 0;
                    if let Ok(mut info) = capture.info.lock() {
                        info.wanted = false;
                        capture.desired.send_replace(false);
                    }
                    continue;
                }
                tokio::time::sleep(Duration::from_millis(500 * restarts as u64)).await;
                continue;
            }
        };
        let pid = child.id().map(|p| p as i32);
        let Some(mut stdout) = child.stdout.take() else {
            log_info!("event=capture.no_stdout gen={gen}");
            let _ = child.kill().await;
            let _ = child.wait().await;
            continue;
        };
        {
            if let Ok(mut info) = capture.info.lock() {
                info.pid = pid;
                info.gen = gen;
                info.started = Some(Instant::now());
                info.wanted = true;
            }
        }
        let sink = CaptureSink {
            frames: capture.frames.clone(),
            gop: capture.gop.clone(),
            stats: capture.stats.clone(),
        };
        let cached = capture.gop.lock().map(|g| g.frames.len()).unwrap_or(0);
        log_info!("event=capture.start gen={gen} pid={pid:?} bin={bin} gop_cache_frames={cached}");

        let started = Instant::now();
        let mut parser = AnnexBParser::new();
        let mut chunk = vec![0u8; 64 * 1024];
        let mut ticker = tokio::time::interval(Duration::from_secs(5));
        ticker.tick().await; // consume the immediate first tick
        let mut last_log = Instant::now();
        let mut last_frames = capture.stats.frames.load(Ordering::Relaxed);
        let mut last_bytes = capture.stats.bytes.load(Ordering::Relaxed);
        let stop_reason: &'static str;
        loop {
            // NOTE: no `biased` here.  The stdout read is ready at ~60fps, so a
            // biased select would starve the 5s stats ticker forever.
            tokio::select! {
                changed = desired.changed() => {
                    if changed.is_err() || !*desired.borrow() {
                        stop_reason = "requested";
                        if let Some(pid) = pid {
                            procs::signal_group_tree(pid, procs::SIGTERM);
                        }
                        break;
                    }
                }
                read = stdout.read(&mut chunk) => {
                    match read {
                        Ok(0) => { stop_reason = "eof"; break; }
                        Ok(n) => parser.feed(&chunk[..n], &sink),
                        Err(e) => {
                            log_info!("event=capture.read_error gen={gen} error={e}");
                            stop_reason = "read_error";
                            break;
                        }
                    }
                }
                _ = ticker.tick() => {
                    let now = Instant::now();
                    let total_frames = capture.stats.frames.load(Ordering::Relaxed);
                    let total_bytes = capture.stats.bytes.load(Ordering::Relaxed);
                    let dt = now.duration_since(last_log).as_secs_f64().max(0.001);
                    let fps = total_frames.saturating_sub(last_frames) as f64 / dt;
                    let kbps = total_bytes.saturating_sub(last_bytes) as f64 * 8.0 / dt / 1000.0;
                    log_info!(
                        "event=capture.stats gen={gen} fps={fps:.1} kbps={kbps:.0} frames_total={total_frames} write_errors={}",
                        capture.stats.write_errors.load(Ordering::Relaxed)
                    );
                    last_log = now;
                    last_frames = total_frames;
                    last_bytes = total_bytes;
                }
            }
        }

        // Stop requested: make sure the whole group is signalled, then reap.
        if !*desired.borrow() {
            if let Some(pid) = pid {
                procs::signal_group_tree(pid, procs::SIGTERM);
            }
        }
        let status = match tokio::time::timeout(Duration::from_secs(3), child.wait()).await {
            Ok(Ok(status)) => format!("{status}"),
            Ok(Err(e)) => format!("wait_error:{e}"),
            Err(_) => {
                log_info!("event=capture.wait_timeout gen={gen} pid={pid:?} action=kill");
                if let Some(pid) = pid {
                    procs::signal_group_tree(pid, procs::SIGKILL);
                }
                let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
                "timeout".to_string()
            }
        };
        // Sweep anything that escaped the process group (e.g. a re-parented
        // grandchild) and give the kernel a moment to release the pgid.
        if let Some(pid) = pid {
            if procs::group_alive(pid) {
                // Give a SIGTERM'd pipeline a moment to unwind before escalating.
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            if procs::group_alive(pid) {
                let remaining = procs::group_tree(pid).len();
                log_info!("event=capture.sweep gen={gen} pid={pid} remaining={remaining}");
                procs::signal_group_tree(pid, procs::SIGKILL);
                tokio::time::sleep(Duration::from_millis(150)).await;
            }
        }
        {
            if let Ok(mut info) = capture.info.lock() {
                info.pid = None;
                info.started = None;
            }
        }
        log_info!(
            "event=capture.exit gen={gen} pid={pid:?} reason={stop_reason} status={status} uptime_ms={} frames={}",
            started.elapsed().as_millis(),
            capture.stats.frames.load(Ordering::Relaxed)
        );

        if !*desired.borrow() {
            restarts = 0;
            continue;
        }
        // Capture died on its own while still wanted: bounded restart.
        restarts += 1;
        if restarts > MAX_CAPTURE_RESTARTS {
            log_info!("event=capture.give_up attempts={restarts}");
            restarts = 0;
            if let Ok(mut info) = capture.info.lock() {
                info.wanted = false;
                capture.desired.send_replace(false);
            }
            continue;
        }
        let delay = Duration::from_millis(500u64 * (1 << (restarts - 1).min(3)));
        log_info!("event=capture.restart_in gen={gen} attempt={restarts} delay_ms={}", delay.as_millis());
        tokio::time::sleep(delay).await;
    }
}

// ---------------------------------------------------------------------------
// Signaling plumbing
// ---------------------------------------------------------------------------
#[derive(Clone, Default)]
struct Signal {
    inner: Arc<Mutex<Option<mpsc::UnboundedSender<String>>>>,
}

impl Signal {
    fn new() -> Self {
        Self::default()
    }
    fn set(&self, tx: Option<mpsc::UnboundedSender<String>>) {
        if let Ok(mut slot) = self.inner.lock() {
            *slot = tx;
        }
    }
    fn send(&self, value: Value) {
        if let Ok(slot) = self.inner.lock() {
            if let Some(tx) = slot.as_ref() {
                let _ = tx.send(value.to_string());
            }
        }
    }
}

#[derive(Clone, Default)]
struct Cancel {
    flag: Arc<AtomicBool>,
    notify: Arc<Notify>,
}

impl Cancel {
    fn cancel(&self) {
        self.flag.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }
    fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }
    async fn cancelled(&self) {
        while !self.is_cancelled() {
            self.notify.notified().await;
        }
    }
}

struct PeerSession {
    gen: u64,
    pc: Arc<RTCPeerConnection>,
    video: Arc<TrackLocalStaticSample>,
    cancel: Cancel,
    pump: Option<tokio::task::JoinHandle<()>>,
    drain: tokio::task::JoinHandle<()>,
}

impl PeerSession {
    async fn discard(self, reason: &str) {
        log_info!("event=peer.discard gen={} reason={reason}", self.gen);
        self.cancel.cancel();
        self.drain.abort();
        if let Some(mut pump) = self.pump {
            // Let the pump observe cancellation and log its final frame counts.
            if tokio::time::timeout(Duration::from_millis(500), &mut pump).await.is_err() {
                pump.abort();
            }
        }
        if tokio::time::timeout(Duration::from_secs(3), self.pc.close()).await.is_err() {
            log_info!("event=peer.close_timeout gen={} reason={reason}", self.gen);
        }
    }
}

// ---------------------------------------------------------------------------
// Frame pump: replays the cached GOP, then streams live frames into one peer.
// ---------------------------------------------------------------------------
fn epoch_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

async fn write_frame(gen: u64, video: &TrackLocalStaticSample, frame: &Frame, stats: &CaptureStats) -> bool {
    let sample = Sample {
        data: frame.data.clone(),
        duration: frame.duration,
        ..Default::default()
    };
    let started = std::time::Instant::now();
    let result = video.write_sample(&sample).await;
    if std::env::var("SWAYRTC_DEBUG").as_deref() == Ok("1") {
        // One line per frame: everything needed to split capture/encode from
        // host dispatch. `age_ms` is how long the encoded frame sat in our
        // pipeline before it hit the wire.
        let now = epoch_ms();
        log_info!(
            "event=frame.tx gen={gen} bytes={} idr={} read_age_ms={} send_us={} ts={now}",
            frame.data.len(),
            frame.is_idr,
            now.saturating_sub(frame.read_at_ms),
            started.elapsed().as_micros()
        );
    }
    match result {
        Ok(()) => true,
        Err(e) => {
            let count = stats.write_errors.fetch_add(1, Ordering::Relaxed) + 1;
            if count <= 5 || count % 200 == 0 {
                log_info!("event=frame.write_error gen={gen} count={count} error={e}");
            }
            false
        }
    }
}

async fn pump_frames(
    gen: u64,
    video: Arc<TrackLocalStaticSample>,
    frames: broadcast::Sender<Arc<Frame>>,
    gop: Arc<Mutex<GopCache>>,
    cancel: Cancel,
    stats: Arc<CaptureStats>,
) {
    let mut rx = frames.subscribe();
    // Subscribe before snapshotting the cache: duplicated frames are harmless,
    // a gap would make the decoder wait for the next keyframe.
    let cached = gop.lock().map(|g| g.snapshot()).unwrap_or_default();
    log_info!("event=pump.start gen={gen} cached_frames={}", cached.len());
    let mut written = 0u64;
    let mut written_bytes = 0u64;
    let mut first = true;
    let mut ticker = tokio::time::interval(Duration::from_secs(5));
    ticker.tick().await;
    let mut last_stat = Instant::now();
    let mut last_written = 0u64;
    let mut last_bytes = 0u64;
    for frame in cached {
        if cancel.is_cancelled() {
            return;
        }
        if write_frame(gen, &video, &frame, &stats).await {
            written += 1;
            written_bytes += frame.data.len() as u64;
            if first {
                first = false;
                log_info!("event=pump.first_frame gen={gen} source=gop_cache frame_bytes={}", frame.data.len());
            }
        }
    }
    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = ticker.tick() => {
                let now = Instant::now();
                let dt = now.duration_since(last_stat).as_secs_f64().max(0.001);
                let frames_delta = written.saturating_sub(last_written);
                let bytes_delta = written_bytes.saturating_sub(last_bytes);
                log_info!(
                    "event=pump.stats gen={gen} sent_frames={frames_delta} fps={:.1} kbps={:.0} total_frames={written}",
                    frames_delta as f64 / dt,
                    bytes_delta as f64 * 8.0 / dt / 1000.0
                );
                last_stat = now;
                last_written = written;
                last_bytes = written_bytes;
            }
            received = rx.recv() => match received {
                Ok(frame) => {
                    if write_frame(gen, &video, &frame, &stats).await {
                        written += 1;
                        written_bytes += frame.data.len() as u64;
                        if first {
                            first = false;
                            log_info!("event=pump.first_frame gen={gen} source=live frame_bytes={}", frame.data.len());
                        }
                    }
                }
                Err(broadcast::error::RecvError::Lagged(skipped)) => {
                    log_debug!("event=pump.lagged gen={gen} skipped={skipped}");
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    }
    log_info!("event=pump.stop gen={gen} frames_written={written} bytes={written_bytes}");
}


// ---------------------------------------------------------------------------
// Public IPv4 discovery and candidate advertisement.
//
// This host sits behind a carrier CGNAT that only exposes a private address on
// the LAN interface, but the mapping itself is endpoint independent AND port
// preserving (observed: external port == local source port, stable over time).
// webrtc-ice gathers its server reflexive candidates from throw away sockets,
// so the ports it advertises can never be reached from the internet.
//
// We therefore keep the normal host candidates and additionally advertise the
// discovered public IPv4 carrying the *real* local port. That address/port
// pair is exactly what the NAT forwards inbound packets to.
// ---------------------------------------------------------------------------
fn parse_xor_mapped_ipv4(pkt: &[u8]) -> Option<Ipv4Addr> {
    if pkt.len() < 20 || u16::from_be_bytes([pkt[0], pkt[1]]) != 0x0101 {
        return None;
    }
    let mut i = 20;
    while i + 4 <= pkt.len() {
        let atyp = u16::from_be_bytes([pkt[i], pkt[i + 1]]);
        let alen = u16::from_be_bytes([pkt[i + 2], pkt[i + 3]]) as usize;
        if let Some(val) = pkt.get(i + 4..i + 4 + alen) {
            if atyp == 0x0020 && val.len() >= 8 && val[1] == 0x01 {
                let cookie = 0x2112_A442u32.to_be_bytes();
                return Some(Ipv4Addr::new(
                    val[4] ^ cookie[0],
                    val[5] ^ cookie[1],
                    val[6] ^ cookie[2],
                    val[7] ^ cookie[3],
                ));
            }
        }
        i += 4 + alen + ((4 - alen % 4) % 4);
    }
    None
}

async fn stun_public_ipv4_once(host: &str, port: u16) -> Option<Ipv4Addr> {
    use tokio::net::{lookup_host, UdpSocket};
    let addr = match lookup_host((host, port)).await {
        Ok(mut it) => it.find(|a| a.is_ipv4())?,
        Err(_) => return None,
    };
    let sock = UdpSocket::bind("0.0.0.0:0").await.ok()?;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u32)
        .unwrap_or(0x5EED);
    let mut req = [0u8; 20];
    req[0..2].copy_from_slice(&0x0001u16.to_be_bytes());
    req[4..8].copy_from_slice(&0x2112_A442u32.to_be_bytes());
    for (k, b) in req[8..20].iter_mut().enumerate() {
        *b = nanos.to_le_bytes()[k % 4];
    }
    sock.send_to(&req, addr).await.ok()?;
    let mut buf = [0u8; 1024];
    let (n, _) = tokio::time::timeout(Duration::from_secs(2), sock.recv_from(&mut buf))
        .await
        .ok()?
        .ok()?;
    parse_xor_mapped_ipv4(&buf[..n])
}

/// Set of recently observed public IPv4 addresses, each valid until it expires.
///
/// The carrier hands out more than one egress address and switches between them
/// over time (observed alternating between two addresses over minutes), while
/// keeping the source port. Advertising every address seen recently lets the
/// viewer reach us whichever one is active right now.
pub type PublicIps = Arc<RwLock<Vec<(Ipv4Addr, Instant)>>>;

fn note_public_ip(store: &PublicIps, ip: Ipv4Addr) {
    let mut guard = store.write().unwrap();
    if let Some(entry) = guard.iter_mut().find(|(a, _)| *a == ip) {
        entry.1 = Instant::now();
    } else {
        guard.push((ip, Instant::now()));
        while guard.len() > 4 {
            guard.remove(0);
        }
    }
}

async fn discover_public_ips(store: PublicIps) {
    const SERVERS: [(&str, u16); 3] = [
        ("stun.cloudflare.com", 3478),
        ("stun.l.google.com", 19302),
        ("stun.nextcloud.com", 443),
    ];
    let mut index = 0usize;
    loop {
        for _ in 0..2 {
            let (host, port) = SERVERS[index % SERVERS.len()];
            index = index.wrapping_add(1);
            if let Some(ip) = stun_public_ipv4_once(host, port).await {
                let known = store.read().unwrap().iter().any(|(a, _)| *a == ip);
                note_public_ip(&store, ip);
                if !known {
                    log_info!("event=public_ip.stun ip={ip} known_ips={}", store.read().unwrap().len());
                }
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        {
            let mut guard = store.write().unwrap();
            guard.retain(|(_, at)| at.elapsed() < Duration::from_secs(900));
        }
        tokio::time::sleep(Duration::from_secs(15)).await;
    }
}

fn is_non_routable_v4(ip: Ipv4Addr) -> bool {
    ip.is_private() || ip.is_loopback() || ip.is_link_local() || ip.is_unspecified() || ip.is_broadcast()
}

/// Build an additional candidate line that points at the public IPv4 while
/// keeping the real local port (the NAT preserves source ports 1:1).
fn public_variant(candidate: &str, public: Ipv4Addr) -> Option<String> {
    let mut parts: Vec<String> = candidate.split(' ').map(str::to_owned).collect();
    if parts.len() < 8 {
        return None;
    }
    let foundation = parts[0].strip_prefix("candidate:")?.to_owned();
    if !candidate.contains(" typ host") {
        return None;
    }
    let addr: Ipv4Addr = parts[4].parse().ok()?;
    if !is_non_routable_v4(addr) {
        return None;
    }
    // Use a distinct foundation: receivers prune same-foundation candidates as
    // redundant, and these two differ by base address.
    parts[0] = format!("candidate:{}", foundation.parse::<u64>().ok()?.wrapping_add(1));
    parts[4] = public.to_string();
    Some(parts.join(" "))
}

// ---------------------------------------------------------------------------
// ICE server configuration.
//
// Direct UDP is always preferred; Cloudflare TURN (credentials issued by the
// local Bun server at /api/ice) is only a fallback for symmetric-NAT pairs that
// STUN cannot connect.  The config is cached for 30 minutes because the TURN
// credentials expire after six hours; failures never become fatal: the host
// keeps running with STUN-only, and the public-IPv4 candidate advertisement is
// unaffected.
// ---------------------------------------------------------------------------
const ICE_CONFIG_TTL: Duration = Duration::from_secs(30 * 60);
const ICE_CONFIG_REFRESH: Duration = Duration::from_secs(20 * 60);
const ICE_HTTP_TIMEOUT: Duration = Duration::from_secs(4);

fn default_stun_servers() -> Vec<RTCIceServer> {
    vec![RTCIceServer {
        urls: vec![
            "stun:stun.cloudflare.com:3478".into(),
            "stun:stun.l.google.com:19302".into(),
        ],
        ..Default::default()
    }]
}

/// http base for /api/ice: SWAYRTC_HTTP_BASE, else derived from the signaling
/// URL (ws://127.0.0.1:8777/ws/host -> http://127.0.0.1:8777).
fn http_base_from_env(signaling: &str) -> Option<String> {
    if let Ok(value) = std::env::var("SWAYRTC_HTTP_BASE") {
        let value = value.trim().to_string();
        if value.starts_with("https://") {
            log_info!("event=ice.config_skipped reason=https_base_unsupported");
            return None;
        }
        return Some(value.trim_end_matches('/').to_string());
    }
    let rest = signaling.strip_prefix("ws://")?;
    let authority = rest.split('/').next().unwrap_or(rest);
    if authority.is_empty() {
        return None;
    }
    Some(format!("http://{authority}"))
}

fn decode_chunked(body: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < body.len() {
        let Some(offset) = body[i..].windows(2).position(|w| w == b"\r\n") else {
            break;
        };
        let line = String::from_utf8_lossy(&body[i..i + offset]).to_string();
        let size = usize::from_str_radix(line.split(';').next().unwrap_or("").trim(), 16).unwrap_or(0);
        i += offset + 2;
        if size == 0 || i + size > body.len() {
            break;
        }
        out.extend_from_slice(&body[i..i + size]);
        i += size + 2;
    }
    out
}

fn servers_from_json(body: &[u8]) -> Result<(Vec<RTCIceServer>, bool, usize)> {
    let value: Value = serde_json::from_slice(body).context("parsing /api/ice json")?;
    let Some(list) = value.get("iceServers").and_then(Value::as_array) else {
        bail!("/api/ice response has no iceServers");
    };
    let mut servers = Vec::new();
    let mut turn = false;
    let mut dropped = 0usize;
    for entry in list {
        let urls: Vec<String> = match entry.get("urls") {
            Some(Value::String(url)) => vec![url.clone()],
            Some(Value::Array(items)) => items.iter().filter_map(|u| u.as_str().map(str::to_owned)).collect(),
            _ => Vec::new(),
        };
        let username = entry.get("username").and_then(Value::as_str).unwrap_or("").to_string();
        let credential = entry.get("credential").and_then(Value::as_str).unwrap_or("").to_string();
        let has_credentials = !username.is_empty() && !credential.is_empty();
        let mut kept = Vec::new();
        for url in urls {
            if url.starts_with("turn:") || url.starts_with("turns:") {
                // webrtc-ice 0.12 implements TURN over UDP only; TCP/TLS variants
                // would stall ICE gathering, so drop them instead of advertising.
                let udp = url.starts_with("turn:")
                    && (url.contains("transport=udp") || !url.contains("transport="));
                if !udp || !has_credentials {
                    dropped += 1;
                    continue;
                }
                turn = true;
                kept.push(url);
            } else if url.starts_with("stun:") {
                kept.push(url);
            } else {
                dropped += 1;
            }
        }
        if !kept.is_empty() {
            servers.push(RTCIceServer {
                urls: kept,
                username,
                credential,
            });
        }
    }
    if servers.is_empty() {
        bail!("/api/ice response had no usable ice servers");
    }
    Ok((servers, turn, dropped))
}

async fn fetch_ice_servers(base: &str, token: &str) -> Result<(Vec<RTCIceServer>, bool, usize)> {
    let authority = base.strip_prefix("http://").unwrap_or(base);
    let mut stream = TcpStream::connect(authority)
        .await
        .with_context(|| format!("connecting to {authority}"))?;
    let request = format!(
        "GET /api/ice HTTP/1.1\r\nHost: {authority}\r\nAuthorization: Bearer {token}\r\nAccept: application/json\r\nConnection: close\r\n\r\n"
    );
    stream.write_all(request.as_bytes()).await.context("sending /api/ice request")?;
    let mut raw = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        let read = stream.read(&mut chunk).await.context("reading /api/ice response")?;
        if read == 0 {
            break;
        }
        raw.extend_from_slice(&chunk[..read]);
        if raw.len() > 128 * 1024 {
            bail!("/api/ice response too large");
        }
    }
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .context("/api/ice response missing headers")?;
    let head = String::from_utf8_lossy(&raw[..split]).to_string();
    let body = &raw[split + 4..];
    let status = head.lines().next().unwrap_or("").to_string();
    if !status.contains(" 200") {
        bail!("/api/ice returned {status}");
    }
    let chunked = head.to_ascii_lowercase().contains("transfer-encoding: chunked");
    let body = if chunked { decode_chunked(body) } else { body.to_vec() };
    servers_from_json(&body)
}

struct IceConfigCache {
    http_base: Option<String>,
    token: String,
    inner: tokio::sync::Mutex<IceConfigState>,
    /// Set while a short retry is scheduled after a failed /api/ice fetch.
    retry_pending: AtomicBool,
}

#[derive(Default)]
struct IceConfigState {
    servers: Vec<RTCIceServer>,
    fetched_at: Option<Instant>,
    turn: bool,
}

impl IceConfigCache {
    fn new(http_base: Option<String>, token: String) -> Self {
        Self {
            http_base,
            token,
            inner: tokio::sync::Mutex::new(IceConfigState::default()),
            retry_pending: AtomicBool::new(false),
        }
    }

    /// Returns the current ICE server list.  Fresh cache: instant.  Stale but
    /// non-empty: return at once and refresh in the background.  Empty: fetch
    /// now (bounded) and fall back to STUN on failure.
    async fn servers(self: &Arc<Self>) -> Vec<RTCIceServer> {
        let cached = {
            let guard = self.inner.lock().await;
            (guard.servers.clone(), guard.fetched_at.map(|at| at.elapsed()))
        };
        match cached {
            (servers, Some(age)) if !servers.is_empty() => {
                if age >= ICE_CONFIG_TTL {
                    self.spawn_refresh("stale");
                }
                servers
            }
            _ => self.refresh_now("initial").await,
        }
    }

    fn spawn_refresh(self: &Arc<Self>, reason: &'static str) {
        let cache = self.clone();
        tokio::spawn(async move {
            let _ = cache.refresh_now(reason).await;
        });
    }

    async fn refresh_now(self: &Arc<Self>, reason: &'static str) -> Vec<RTCIceServer> {
        let Some(base) = self.http_base.clone() else {
            let mut guard = self.inner.lock().await;
            if guard.servers.is_empty() {
                guard.servers = default_stun_servers();
                guard.fetched_at = Some(Instant::now());
            }
            log_info!("event=ice.config_disabled reason=no_http_base fallback=stun");
            return guard.servers.clone();
        };
        let mut guard = self.inner.lock().await;
        match tokio::time::timeout(ICE_HTTP_TIMEOUT, fetch_ice_servers(&base, &self.token)).await {
            Ok(Ok((servers, turn, dropped))) => {
                log_info!(
                    "event=ice.config source={base}/api/ice reason={reason} turn={turn} servers={} dropped_urls={dropped}",
                    servers.len()
                );
                guard.servers = servers;
                guard.fetched_at = Some(Instant::now());
                guard.turn = turn;
            }
            Ok(Err(e)) => {
                log_info!("event=ice.config_failed reason={reason} error={e:#} fallback=stun_only retry_in_ms=60000");
                if guard.servers.is_empty() {
                    guard.servers = default_stun_servers();
                    guard.fetched_at = Some(Instant::now());
                }
                drop(guard);
                self.schedule_retry();
                return self.inner.lock().await.servers.clone();
            }
            Err(_) => {
                log_info!("event=ice.config_failed reason={reason} error=timeout fallback=stun_only retry_in_ms=60000");
                if guard.servers.is_empty() {
                    guard.servers = default_stun_servers();
                    guard.fetched_at = Some(Instant::now());
                }
                drop(guard);
                self.schedule_retry();
                return self.inner.lock().await.servers.clone();
            }
        }
        guard.servers.clone()
    }

    /// TURN credentials are worth retrying soon after a transient failure; the
    /// flag keeps a burst of peer creations from queueing several retries.
    fn schedule_retry(self: &Arc<Self>) {
        if self.retry_pending.swap(true, Ordering::SeqCst) {
            return;
        }
        let cache = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(60)).await;
            cache.retry_pending.store(false, Ordering::SeqCst);
            let _ = cache.refresh_now("retry").await;
        });
    }

    /// Warm the cache at startup (await) and refresh it periodically so the TURN
    /// credentials never expire under a long-lived peer.
    fn spawn_maintenance(self: &Arc<Self>) {
        let cache = self.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(ICE_CONFIG_REFRESH).await;
                let _ = cache.refresh_now("periodic").await;
            }
        });
    }
}

// ---------------------------------------------------------------------------
// ICE path diagnostics: which candidate pair actually carried the media.
// ---------------------------------------------------------------------------
fn candidate_type_short(debug_name: &str) -> String {
    match debug_name {
        "Host" => "host",
        "ServerReflexive" => "srflx",
        "PeerReflexive" => "prflx",
        "Relay" => "relay",
        other => other,
    }
    .to_string()
}

async fn log_ice_path(pc: Arc<RTCPeerConnection>, gen: u64) {
    // Sample after media has actually flowed: right at Connected every pair has
    // zero bytes and the winner is not yet meaningful.
    tokio::time::sleep(Duration::from_millis(800)).await;
    let report = pc.get_stats().await;
    let mut local_types: HashMap<String, String> = HashMap::new();
    let mut remote_types: HashMap<String, String> = HashMap::new();
    for (id, item) in &report.reports {
        match item {
            StatsReportType::LocalCandidate(candidate) => {
                local_types.insert(id.clone(), candidate_type_short(&format!("{:?}", candidate.candidate_type)));
            }
            StatsReportType::RemoteCandidate(candidate) => {
                remote_types.insert(id.clone(), candidate_type_short(&format!("{:?}", candidate.candidate_type)));
            }
            _ => {}
        }
    }
    let mut best: Option<(i64, bool, String, f64, u64, u64, String, String)> = None;
    for item in report.reports.values() {
        if let StatsReportType::CandidatePair(pair) = item {
            // Prefer a nominated pair, then the one actually carrying media.
            let score = (pair.nominated as i64) * 1_000_000_000_000 + pair.bytes_sent as i64;
            if best.as_ref().map(|current| score > current.0).unwrap_or(true) {
                best = Some((
                    score,
                    pair.nominated,
                    format!("{:?}", pair.state),
                    pair.current_round_trip_time,
                    pair.bytes_sent,
                    pair.bytes_received,
                    pair.local_candidate_id.clone(),
                    pair.remote_candidate_id.clone(),
                ));
            }
        }
    }
    match best {
        Some((_, nominated, state, rtt, sent, received, local_id, remote_id)) => {
            let local = local_types.get(&local_id).cloned().unwrap_or_else(|| "unknown".to_string());
            let remote = remote_types.get(&remote_id).cloned().unwrap_or_else(|| "unknown".to_string());
            log_info!(
                "event=ice.path gen={gen} local={local} remote={remote} nominated={nominated} state={state} rtt_ms={:.1} bytes_sent={sent} bytes_recv={received}",
                rtt * 1000.0
            );
            if debug_enabled() {
                for (id, item) in &report.reports {
                    match item {
                        StatsReportType::CandidatePair(pair) => log_debug!(
                            "event=ice.pair id={id} local={} remote={} nominated={} state={:?} rtt_ms={:.1} sent={} recv={}",
                            pair.local_candidate_id,
                            pair.remote_candidate_id,
                            pair.nominated,
                            pair.state,
                            pair.current_round_trip_time * 1000.0,
                            pair.bytes_sent,
                            pair.bytes_received
                        ),
                        StatsReportType::LocalCandidate(candidate) => log_debug!(
                            "event=ice.candidate side=local id={id} type={:?} ip={} port={} proto={}",
                            candidate.candidate_type,
                            candidate.ip,
                            candidate.port,
                            candidate.relay_protocol
                        ),
                        StatsReportType::RemoteCandidate(candidate) => log_debug!(
                            "event=ice.candidate side=remote id={id} type={:?} ip={} port={} proto={}",
                            candidate.candidate_type,
                            candidate.ip,
                            candidate.port,
                            candidate.relay_protocol
                        ),
                        _ => {}
                    }
                }
            }
        }
        None => log_debug!("event=ice.path gen={gen} no_candidate_pairs"),
    }
}

// ---------------------------------------------------------------------------
// Host state machine
// ---------------------------------------------------------------------------
#[derive(Clone, Copy, PartialEq, Eq)]
enum HostState {
    Idle,
    Negotiating,
    Connected,
    Degraded,
}

impl std::fmt::Display for HostState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            HostState::Idle => "idle",
            HostState::Negotiating => "negotiating",
            HostState::Connected => "connected",
            HostState::Degraded => "degraded",
        })
    }
}

enum Event {
    PeerStatus {
        gen: u64,
        state: RTCPeerConnectionState,
    },
    NegotiationTimeout {
        epoch: u64,
    },
    DegradedExpired {
        epoch: u64,
    },
}

enum SessionEnd {
    Closed(&'static str),
    Reconnect,
    Shutdown,
}

impl SessionEnd {
    fn reason(&self) -> &'static str {
        match self {
            SessionEnd::Closed(reason) => reason,
            SessionEnd::Reconnect => "reconnect_requested",
            SessionEnd::Shutdown => "shutdown",
        }
    }
}

struct Host {
    api: Arc<API>,
    /// Recently observed public IPv4 addresses used to advertise reachable
    /// host candidates through the carrier NAT (the LAN address alone is
    /// useless to a remote viewer, and the carrier alternates egress IPs).
    public_ips: PublicIps,
    /// STUN + Cloudflare TURN credential cache for RTCPeerConnection setup.
    ice: Arc<IceConfigCache>,
    signal: Signal,
    capture: CaptureManager,
    evt_tx: mpsc::UnboundedSender<Event>,
    peer: Option<PeerSession>,
    state: HostState,
    gen: u64,
    timer_epoch: u64,
    pending_candidates: Vec<RTCIceCandidateInit>,
    reconnect_wanted: bool,
    viewer_epoch: Arc<AtomicU64>,
    viewer_grace: Duration,
    disconnected_grace: Duration,
    failed_grace: Duration,
}

impl Host {
    fn new(
        capture: CaptureManager,
        evt_tx: mpsc::UnboundedSender<Event>,
        public_ips: PublicIps,
        ice: Arc<IceConfigCache>,
    ) -> Result<Self> {
        let mut media = MediaEngine::default();
        media.register_default_codecs()?;
        let registry = register_default_interceptors(Registry::new(), &mut media)?;
        let api = Arc::new(
            APIBuilder::new()
                .with_media_engine(media)
                .with_interceptor_registry(registry)
                .build(),
        );
        Ok(Self {
            api,
            public_ips,
            ice,
            signal: Signal::new(),
            capture,
            evt_tx,
            peer: None,
            state: HostState::Idle,
            gen: 0,
            timer_epoch: 0,
            pending_candidates: Vec::new(),
            reconnect_wanted: false,
            viewer_epoch: Arc::new(AtomicU64::new(0)),
            // How long the current capture may outlive its viewer while waiting
            // for the reconnect offer.  Long enough to hand a still-warm encoder
            // to a reloading browser, short enough that nothing lingers.
            viewer_grace: env_ms("SWAYRTC_VIEWER_GRACE_MS", 800),
            disconnected_grace: env_ms("SWAYRTC_DISCONNECTED_GRACE_MS", 6_000),
            failed_grace: env_ms("SWAYRTC_FAILED_GRACE_MS", 2_000),
        })
    }

    fn set_state(&mut self, next: HostState) {
        if self.state != next {
            log_info!("event=state from={} to={} gen={}", self.state, next, self.gen);
            self.state = next;
        }
    }

    fn cancel_timers(&mut self) {
        self.timer_epoch += 1;
    }

    fn schedule(&mut self, delay: Duration, make: impl FnOnce(u64) -> Event) {
        self.timer_epoch += 1;
        let epoch = self.timer_epoch;
        let event = make(epoch);
        let evt_tx = self.evt_tx.clone();
        tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            let _ = evt_tx.send(event);
        });
    }

    /// Keeps capture alive for `viewer_grace`; if the viewer has not come back
    /// with a new offer by then, the whole process group is killed.
    fn schedule_capture_stop(&mut self, reason: &'static str) {
        let epoch = self.viewer_epoch.fetch_add(1, Ordering::SeqCst) + 1;
        let viewer_epoch = self.viewer_epoch.clone();
        let capture = self.capture.clone();
        let grace = self.viewer_grace;
        log_info!("event=capture.idle_timer reason={reason} grace_ms={}", grace.as_millis());
        tokio::spawn(async move {
            tokio::time::sleep(grace).await;
            if viewer_epoch.load(Ordering::SeqCst) == epoch {
                capture.stop(reason).await;
            } else {
                log_debug!("event=capture.idle_timer_cancelled reason={reason}");
            }
        });
    }

    async fn close_peer(&mut self, reason: &str) {
        if let Some(peer) = self.peer.take() {
            peer.discard(reason).await;
        }
    }

    async fn handle_message(&mut self, raw: &str) {
        let data: Value = match serde_json::from_str(raw) {
            Ok(value) => value,
            Err(_) => {
                log_debug!("event=signaling.invalid_json bytes={}", raw.len());
                return;
            }
        };
        match data["type"].as_str().unwrap_or("") {
            "offer" => self.handle_offer(&data).await,
            "candidate" => self.handle_candidate(&data).await,
            "reset" => {
                log_info!("event=signaling.reset");
                self.reset_viewer("server_reset").await;
            }
            "ping" => {}
            "pong" => log_debug!("event=signaling.pong"),
            "status" => log_debug!(
                "event=signaling.status host={} viewer={}",
                data["host"],
                data["viewer"]
            ),
            other => log_debug!("event=signaling.unknown type={other}"),
        }
    }

    async fn handle_offer(&mut self, data: &Value) {
        let Some(sdp) = data.get("sdp") else {
            log_info!("event=offer.invalid reason=missing_sdp");
            return;
        };
        let offer: RTCSessionDescription = match serde_json::from_value(sdp.clone()) {
            Ok(value) => value,
            Err(e) => {
                log_info!("event=offer.invalid error={e}");
                return;
            }
        };
        self.cancel_timers();
        self.gen += 1;
        let gen = self.gen;
        // Any pending idle-stop belongs to the previous viewer: cancel it and
        // hand the warm capture over to this offer instead of restarting it.
        self.viewer_epoch.fetch_add(1, Ordering::SeqCst);
        log_info!("event=offer.recv gen={gen} sdp_bytes={}", offer.sdp.len());
        self.close_peer("replaced").await;
        self.capture.start();
        let peer = match self.create_peer(gen).await {
            Ok(peer) => peer,
            Err(e) => {
                log_info!("event=peer.create_failed gen={gen} error={e:#}");
                self.signal.send(json!({ "type": "error", "message": format!("peer setup failed: {e:#}") }));
                self.set_state(HostState::Idle);
                self.schedule_capture_stop("peer_create_failed");
                return;
            }
        };
        let pc = peer.pc.clone();
        if let Err(e) = pc.set_remote_description(offer).await {
            log_info!("event=offer.remote_description_failed gen={gen} error={e}");
            peer.discard("remote_description").await;
            self.set_state(HostState::Idle);
            self.schedule_capture_stop("offer_failed");
            return;
        }
        for candidate in self.pending_candidates.drain(..) {
            if let Err(e) = pc.add_ice_candidate(candidate).await {
                log_info!("event=ice.add_failed gen={gen} source=buffered error={e}");
            }
        }
        let answer = match pc.create_answer(None).await {
            Ok(answer) => answer,
            Err(e) => {
                log_info!("event=answer.create_failed gen={gen} error={e}");
                peer.discard("create_answer").await;
                self.set_state(HostState::Idle);
                self.schedule_capture_stop("answer_failed");
                return;
            }
        };
        if let Err(e) = pc.set_local_description(answer).await {
            log_info!("event=answer.local_description_failed gen={gen} error={e}");
            peer.discard("local_description").await;
            self.set_state(HostState::Idle);
            self.schedule_capture_stop("answer_failed");
            return;
        }
        let Some(local) = pc.local_description().await else {
            log_info!("event=answer.missing gen={gen}");
            peer.discard("missing_answer").await;
            self.set_state(HostState::Idle);
            self.schedule_capture_stop("answer_failed");
            return;
        };
        log_info!("event=answer.sent gen={gen} sdp_bytes={}", local.sdp.len());
        self.signal.send(json!({ "type": "answer", "sdp": local }));
        self.peer = Some(peer);
        self.set_state(HostState::Negotiating);
        self.schedule(NEGOTIATION_TIMEOUT, |epoch| Event::NegotiationTimeout { epoch });
    }

    async fn handle_candidate(&mut self, data: &Value) {
        let Some(object) = data.get("candidate") else {
            return;
        };
        let candidate = match serde_json::from_value::<RTCIceCandidateInit>(object.clone()) {
            Ok(candidate) => candidate,
            Err(_) => {
                log_debug!("event=ice.invalid");
                return;
            }
        };
        match self.peer.as_ref() {
            Some(peer) => {
                log_debug!("event=ice.remote gen={} {}", peer.gen, candidate.candidate);
                if let Err(e) = peer.pc.add_ice_candidate(candidate).await {
                    log_info!("event=ice.add_failed gen={} error={e}", peer.gen);
                }
            }
            None => {
                // Candidates can race ahead of the offer; buffer them (bounded).
                if self.pending_candidates.len() < 64 {
                    self.pending_candidates.push(candidate);
                }
            }
        }
    }

    async fn handle_event(&mut self, event: Event) {
        match event {
            Event::PeerStatus { gen, state } => self.handle_peer_state(gen, state).await,
            Event::NegotiationTimeout { epoch } => {
                if epoch != self.timer_epoch || self.state == HostState::Connected {
                    return;
                }
                log_info!("event=negotiation.timeout gen={} state={}", self.gen, self.state);
                self.close_peer("negotiation_timeout").await;
                self.set_state(HostState::Idle);
                self.schedule_capture_stop("negotiation_timeout");
                self.reconnect_wanted = true;
            }
            Event::DegradedExpired { epoch } => {
                if epoch != self.timer_epoch {
                    return;
                }
                log_info!("event=degraded.expired gen={} state={}", self.gen, self.state);
                // No recovery within the grace window: stop capture and return
                // to a clean idle state.  Forcing a signaling reconnect makes
                // the viewer issue a fresh offer (new peer + new ICE).
                self.close_peer("degraded_expired").await;
                self.set_state(HostState::Idle);
                self.schedule_capture_stop("degraded_expired");
                self.reconnect_wanted = true;
            }
        }
    }

    async fn handle_peer_state(&mut self, gen: u64, state: RTCPeerConnectionState) {
        if gen != self.gen || self.peer.as_ref().map(|p| p.gen) != Some(gen) {
            log_debug!("event=pc.state_stale gen={gen} state={state}");
            return;
        }
        match state {
            RTCPeerConnectionState::Connected => {
                self.set_state(HostState::Connected);
                self.cancel_timers();
                self.reconnect_wanted = false;
                if let Some(peer) = self.peer.as_ref() {
                    let pc = peer.pc.clone();
                    tokio::spawn(async move { log_ice_path(pc, gen).await });
                }
                let needs_pump = self.peer.as_ref().and_then(|p| p.pump.as_ref()).is_none();
                if needs_pump {
                    let (video, cancel) = match self.peer.as_ref() {
                        Some(peer) => (peer.video.clone(), peer.cancel.clone()),
                        None => return,
                    };
                    let handle = tokio::spawn(pump_frames(
                        gen,
                        video,
                        self.capture.frames.clone(),
                        self.capture.gop.clone(),
                        cancel,
                        self.capture.stats.clone(),
                    ));
                    if let Some(peer) = self.peer.as_mut() {
                        peer.pump = Some(handle);
                    }
                }
            }
            RTCPeerConnectionState::Disconnected => {
                self.set_state(HostState::Degraded);
                let grace = self.disconnected_grace;
                self.schedule(grace, |epoch| Event::DegradedExpired { epoch });
                log_info!("event=peer.disconnected gen={gen} grace_ms={}", grace.as_millis());
            }
            RTCPeerConnectionState::Failed => {
                self.set_state(HostState::Degraded);
                let grace = self.failed_grace;
                self.schedule(grace, |epoch| Event::DegradedExpired { epoch });
                log_info!("event=peer.failed gen={gen} grace_ms={}", grace.as_millis());
            }
            RTCPeerConnectionState::Closed => {
                log_info!("event=peer.closed gen={gen}");
                self.close_peer("pc_closed").await;
                self.set_state(HostState::Idle);
                self.schedule_capture_stop("pc_closed");
            }
            other => log_debug!("event=peer.state gen={gen} state={other}"),
        }
    }

    async fn reset_viewer(&mut self, reason: &'static str) {
        self.cancel_timers();
        self.close_peer(reason).await;
        self.pending_candidates.clear();
        self.set_state(HostState::Idle);
        self.schedule_capture_stop(reason);
    }

    async fn on_signaling_down(&mut self, reason: &'static str) {
        log_info!("event=signaling.down reason={reason}");
        self.cancel_timers();
        self.close_peer("signaling_down").await;
        self.pending_candidates.clear();
        self.set_state(HostState::Idle);
        self.schedule_capture_stop("signaling_down");
    }

    async fn shutdown(&mut self) {
        log_info!("event=host.shutdown capture_running={}", self.capture.is_running());
        self.cancel_timers();
        self.close_peer("shutdown").await;
        self.capture.stop("shutdown").await;
    }

    async fn create_peer(&self, gen: u64) -> Result<PeerSession> {
        // TURN credentials are fetched (and cached) per peer so the fallback
        // works after credential rotation; on any failure this degrades to the
        // cached list or STUN-only, never to a fatal error.
        let ice_servers = self.ice.servers().await;
        let turn = ice_servers
            .iter()
            .any(|server| server.urls.iter().any(|url| url.starts_with("turn:") || url.starts_with("turns:")));
        // TURN is the supported transport for cross-CGNAT viewers, so when the
        // local server hands us TURN credentials the host goes relay-only: the
        // viewer then always gets relay↔relay pairs.  Without TURN (or with
        // SWAYRTC_RELAY_ONLY=0) it advertises every candidate as before.
        let relay_only = match std::env::var("SWAYRTC_RELAY_ONLY").as_deref() {
            Ok("1") => true,
            Ok("0") => false,
            _ => turn,
        };
        log_info!(
            "event=ice.servers turn={turn} count={} relay_only={relay_only}",
            ice_servers.len()
        );
        let pc = Arc::new(
            self.api
                .new_peer_connection(RTCConfiguration {
                    ice_servers,
                    ..Default::default()
                })
                .await?,
        );
        let video = Arc::new(TrackLocalStaticSample::new(
            RTCRtpCodecCapability {
                mime_type: "video/H264".to_string(),
                clock_rate: 90000,
                ..Default::default()
            },
            "desktop".to_string(),
            "swayrtc".to_string(),
        ));
        let sender = pc
            .add_track(video.clone() as Arc<dyn TrackLocal + Send + Sync>)
            .await?;
        let drain = tokio::spawn(async move {
            let mut buf = vec![0u8; 1500];
            while sender.read(&mut buf).await.is_ok() {}
        });

        // ---- local ICE candidate advertisement. Keep this in sync with the
        // ---- NAT/public-IP work: normal host candidates plus the same socket
        // ---- under every recently observed public IPv4 (port preserving NAT).
        let signal = self.signal.clone();
        let ice_public = self.public_ips.clone();
        let advertised = Arc::new(AtomicU64::new(0));
        let advertised_first = advertised.clone();
        pc.on_ice_candidate(Box::new(move |candidate| {
            let signal = signal.clone();
            let public_ips = ice_public.clone();
            let advertised = advertised_first.clone();
            Box::pin(async move {
                if let Some(candidate) = candidate {
                    if let Ok(data) = candidate.to_json() {
                        let text = data.candidate.clone();
                        let seen: Vec<Ipv4Addr> = public_ips
                            .read()
                            .unwrap()
                            .iter()
                            .map(|(ip, _)| *ip)
                            .collect();
                        if advertised.fetch_add(1, Ordering::Relaxed) == 0 {
                            // Per-peer advertisement summary: proves a re-created
                            // peer registers its own candidate handler and that
                            // the public IPv4 set is advertised again.
                            log_info!(
                                "event=ice.advertise gen={gen} public_ips={} {:?}",
                                seen.len(),
                                seen
                            );
                        }
                        if text.contains(" typ relay") {
                            // TURN fallback is actually available: relay candidate gathered.
                            log_info!("event=ice.relay_candidate gen={gen} {text}");
                        } else if relay_only {
                            log_debug!("event=ice.local_skipped gen={gen} reason=relay_only");
                            return;
                        } else {
                            log_debug!("event=ice.local gen={gen} {text}");
                        }
                        signal.send(json!({ "type": "candidate", "candidate": data.clone() }));
                        for public in seen {
                            if let Some(extra) = public_variant(&text, public) {
                                let mut copy = data.clone();
                                copy.candidate = extra.clone();
                                log_debug!("event=ice.local_public gen={gen} {extra}");
                                signal.send(json!({ "type": "candidate", "candidate": copy }));
                            }
                        }
                    }
                }
            })
        }));

        if debug_enabled() {
            pc.on_ice_connection_state_change(Box::new(move |state| {
                log_debug!("event=ice.state gen={gen} state={state}");
                Box::pin(async move {})
            }));
            pc.on_ice_gathering_state_change(Box::new(move |state| {
                log_debug!("event=ice.gathering gen={gen} state={state}");
                Box::pin(async move {})
            }));
        }

        let evt_tx = self.evt_tx.clone();
        pc.on_peer_connection_state_change(Box::new(move |state| {
            let evt_tx = evt_tx.clone();
            Box::pin(async move {
                log_info!("event=pc.state gen={gen} state={state}");
                let _ = evt_tx.send(Event::PeerStatus { gen, state });
            })
        }));

        // One input worker per peer, shared by both DataChannels. Splitting
        // them created two independent virtual pointers/keyboards: the motion
        // device could race the button device, causing intermittent misses.
        let (input_tx, input_rx) = mpsc::channel::<Value>(128);
        // The input worker owns the virtual Wayland keyboard and pointer.
        // Never rely on DataChannel callback destruction to release them: the
        // WebRTC stack may retain callbacks after a peer has been closed.
        // Explicit peer cancellation must drop the receiver and allow the
        // resident Wayland thread to release any held buttons and devices.
        let cancel = Cancel::default();
        let input_cancel = cancel.clone();
        tokio::spawn(async move {
            tokio::select! {
                _ = input::input_worker(input_rx) => {},
                _ = input_cancel.cancelled() => {},
            }
            log_debug!("event=input.worker_stopped gen={gen}");
        });
        // Control DataChannel: allowlisted input forwarding + RTT ping/pong.
        let last_button_seq: Arc<std::sync::Mutex<std::collections::HashMap<u64, u64>>> =
            Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
        pc.on_data_channel(Box::new(move |dc| {
            let input_tx = input_tx.clone();
            let last_button_seq = last_button_seq.clone();
            Box::pin(async move {
                // Three channels: "control" is reliable/ordered (keys, wheel,
                // text - losing a keyup leaves the key held down), "motion" is
                // unreliable/unordered (newest pointer position wins) and
                // "buttons" is reliable but unordered, so a retransmitted click
                // cannot hold up the next one.
                let label = dc.label().to_string();
                if label != "control" && label != "motion" && label != "buttons" {
                    let _ = dc.close().await;
                    return;
                }
                log_info!("event=datachannel.open label={label}");
                let is_motion = label == "motion";
                // A strong Arc<RTCDataChannel> captured by its own callback
                // forms a reference cycle and leaks the channel on reconnect.
                let dc_weak = Arc::downgrade(&dc);
                dc.on_message(Box::new(move |msg| {
                    let input_tx = input_tx.clone();
                    let dc_weak = dc_weak.clone();
                    let label = label.clone();
                    let last_button_seq = last_button_seq.clone();
                    Box::pin(async move {
                        if msg.data.len() > 512 {
                            return;
                        }
                        let Ok(value) = serde_json::from_slice::<Value>(&msg.data) else {
                            return;
                        };
                        let Some(kind) = value.get("type").and_then(Value::as_str) else {
                            return;
                        };
                        if kind == "ping" {
                            // Latency probe: echo the viewer's timestamp untouched.
                            let t = value.get("t").cloned().unwrap_or(json!(0));
                            if let Some(dc_tx) = dc_weak.upgrade() {
                                let _ = dc_tx.send_text(format!(r#"{{"type":"pong","t":{t}}}"#)).await;
                            }
                            return;
                        }
                        if is_motion && kind != "move" {
                            return;
                        }
                        if label == "buttons" {
                            // Unordered delivery: a retransmitted click can land
                            // after a newer one. Keep the newest sequence per
                            // button so a stale event cannot leave it stuck.
                            if kind != "button" {
                                return;
                            }
                            let seq = value.get("seq").and_then(|v| v.as_u64()).unwrap_or(0);
                            let button = value.get("button").and_then(|v| v.as_u64()).unwrap_or(0);
                            let mut last = last_button_seq.lock().unwrap();
                            let entry = last.entry(button).or_insert(0);
                            if seq != 0 && seq <= *entry {
                                return;
                            }
                            if seq != 0 {
                                *entry = seq;
                            }
                        }
                        if matches!(kind, "move" | "button" | "wheel" | "key" | "text") {
                            // The viewer stamps input with its Date.now(); both ends
                            // share this clock, so this is the real one way input
                            // latency of the data channel plus host dispatch.
                            if std::env::var("SWAYRTC_DEBUG").as_deref() == Ok("1") {
                                let now = epoch_ms() as i64;
                                match value.get("t").and_then(Value::as_i64) {
                                    Some(t) => eprintln!(
                                        "[swayrtc-host] event=input.recv kind={kind} age_ms={} ts={now}",
                                        now - t
                                    ),
                                    None => eprintln!("[swayrtc-host] event=input.recv kind={kind} ts={now}"),
                                }
                            }
                            if is_motion {
                                // Stale motion is disposable; never stall the
                                // WebRTC callback waiting for mouse movements.
                                let _ = input_tx.try_send(value);
                            } else {
                                // Reliable control events must not be silently
                                // dropped when the input queue fills up.
                                let _ = input_tx.send(value).await;
                            }
                        }
                    })
                }));
            })
        }));

        Ok(PeerSession {
            gen,
            pc,
            video,
            cancel,
            pump: None,
            drain,
        })
    }

    async fn run_session(
        &mut self,
        ws: WebSocketStream<MaybeTlsStream<TcpStream>>,
        evt_rx: &mut mpsc::UnboundedReceiver<Event>,
        shutdown: &mut watch::Receiver<bool>,
    ) -> SessionEnd {
        let (mut sink, mut stream) = ws.split();
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
        self.signal.set(Some(tx));
        let writer = tokio::spawn(async move {
            while let Some(message) = rx.recv().await {
                if sink.send(Message::Text(message.into())).await.is_err() {
                    break;
                }
            }
        });
        // Keepalive: the Bun side pings idle WebSockets; a quiet JSON ping within
        // the idle window also keeps the socket alive through Cloudflare.
        let pinger = {
            let signal = self.signal.clone();
            tokio::spawn(async move {
                let mut ticker = tokio::time::interval(WS_PING_INTERVAL);
                ticker.tick().await;
                loop {
                    ticker.tick().await;
                    signal.send(json!({ "type": "ping" }));
                }
            })
        };
        log_info!("event=signaling.online");
        // A reconnect request is a one-shot: it exists to make the viewer issue a
        // fresh offer. Leaving it set makes every new session end immediately,
        // which turns into a permanent reconnect loop (observed overnight).
        self.reconnect_wanted = false;
        let end = loop {
            if *shutdown.borrow() {
                break SessionEnd::Shutdown;
            }
            if self.reconnect_wanted {
                break SessionEnd::Reconnect;
            }
            tokio::select! {
                biased;
                _ = shutdown.changed() => break SessionEnd::Shutdown,
                message = stream.next() => {
                    match message {
                        Some(Ok(Message::Text(raw))) => self.handle_message(raw.as_str()).await,
                        Some(Ok(Message::Close(frame))) => {
                            log_info!("event=signaling.close_frame code={:?}", frame.map(|f| f.code));
                            break SessionEnd::Closed("close_frame");
                        }
                        Some(Ok(_)) => {}
                        Some(Err(e)) => {
                            log_info!("event=signaling.read_error error={e}");
                            break SessionEnd::Closed("read_error");
                        }
                        None => break SessionEnd::Closed("stream_ended"),
                    }
                }
                event = evt_rx.recv() => {
                    if let Some(event) = event {
                        self.handle_event(event).await;
                    }
                }
            }
        };
        pinger.abort();
        writer.abort();
        self.signal.set(None);
        end
    }
}

async fn connect_signaling(url: &str, token: &str) -> Result<WebSocketStream<MaybeTlsStream<TcpStream>>> {
    let mut request = url.into_client_request().context("building signaling request")?;
    request
        .headers_mut()
        .insert("Authorization", format!("Bearer {token}").parse().context("authorization header")?);
    let (ws, _response) = connect_async(request).await.context("connecting to Bun signaling")?;
    Ok(ws)
}

async fn shutdown_signal() {
    use tokio::signal::unix::{signal, SignalKind};
    let mut terminate = match signal(SignalKind::terminate()) {
        Ok(sig) => sig,
        Err(_) => return,
    };
    let mut interrupt = match signal(SignalKind::interrupt()) {
        Ok(sig) => sig,
        Err(_) => return,
    };
    tokio::select! {
        _ = terminate.recv() => log_info!("event=signal.term"),
        _ = interrupt.recv() => log_info!("event=signal.int"),
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    // Resolve the project root that holds .secrets/. Prefer SWAYRTC_ROOT, then
    // the compile time project directory, then the working directory. This keeps
    // binaries built elsewhere (CI / isolated builds) usable in place.
    let root = if let Ok(p) = std::env::var("SWAYRTC_ROOT") {
        std::path::PathBuf::from(p)
    } else {
        let compiled = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .map(std::path::PathBuf::from);
        match compiled {
            Some(p) if p.join(".secrets/host-token").exists() => p,
            _ => std::env::current_dir().context("project root")?,
        }
    };
    let token = tokio::fs::read_to_string(root.join(".secrets/host-token"))
        .await
        .with_context(|| format!("reading {}/.secrets/host-token", root.display()))?
        .trim()
        .to_string();
    let url = std::env::var("SWAYRTC_SIGNALING_URL").unwrap_or_else(|_| "ws://127.0.0.1:8777/ws/host".into());

    let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
    tokio::spawn(async move {
        shutdown_signal().await;
        let _ = shutdown_tx.send(true);
    });

    // Public IPv4 advertisement: SWAYRTC_PUBLIC_IP=off disables it (same-host
    // tests have no NAT hairpin), an explicit address pins it, otherwise we
    // discover and refresh the set of egress addresses in the background.
    let public_ips: PublicIps = Arc::new(RwLock::new(Vec::new()));
    match std::env::var("SWAYRTC_PUBLIC_IP").as_deref() {
        Ok("off") | Ok("0") | Ok("none") => log_info!("event=public_ip.disabled"),
        Ok(value) if value.parse::<Ipv4Addr>().is_ok() => {
            let ip: Ipv4Addr = value.parse().expect("checked above");
            note_public_ip(&public_ips, ip);
            log_info!("event=public_ip.configured ip={ip}");
        }
        _ => {
            let store = public_ips.clone();
            tokio::spawn(discover_public_ips(store));
        }
    }

    // ICE servers: STUN always, Cloudflare TURN as a fallback from /api/ice.
    let ice = Arc::new(IceConfigCache::new(
        http_base_from_env(&url),
        token.clone(),
    ));
    // Warm the cache before serving so the first peer is never STUN-only by
    // accident (a failure here only logs and falls back).
    let _ = ice.servers().await;
    ice.spawn_maintenance();

    let (evt_tx, mut evt_rx) = mpsc::unbounded_channel::<Event>();
    let (frames, _frames_rx) = broadcast::channel::<Arc<Frame>>(FRAME_CHANNEL_CAPACITY);
    let gop = Arc::new(Mutex::new(GopCache::default()));
    let (capture, capture_desired) = CaptureManager::new(frames, gop);
    let mut host = Host::new(capture.clone(), evt_tx, public_ips, ice)?;
    // Must outlive every CaptureManager clone; kills a stray group on panic/exit.
    let _capture_guard = CaptureKiller { capture: capture.clone() };
    // Warm start: bring the capture pipeline up before anyone connects, so the
    // first viewer gets a frame immediately instead of paying the encoder and
    // GPU setup (measured as a ~0.5 s stall on the first frames).
    capture.start();
    let supervisor = tokio::spawn(capture_supervisor(capture_desired, capture));

    log_info!(
        "event=host.start pid={} url={url} debug={} viewer_grace_ms={} disconnected_grace_ms={} failed_grace_ms={}",
        std::process::id(),
        debug_enabled(),
        host.viewer_grace.as_millis(),
        host.disconnected_grace.as_millis(),
        host.failed_grace.as_millis()
    );

    let mut backoff = SIGNAL_BACKOFF_MIN;
    let mut attempt: u64 = 0;
    loop {
        if *shutdown_rx.borrow() {
            break;
        }
        attempt += 1;
        log_info!("event=signaling.connecting attempt={attempt} url={url}");
        let connected = tokio::select! {
            biased;
            _ = shutdown_rx.changed() => None,
            result = connect_signaling(&url, &token) => Some(result),
        };
        let Some(result) = connected else {
            log_info!("event=shutdown.during_connect");
            break;
        };
        match result {
            Ok(ws) => {
                log_info!("event=signaling.connected attempt={attempt}");
                let started = Instant::now();
                let end = host.run_session(ws, &mut evt_rx, &mut shutdown_rx).await;
                log_info!("event=signaling.session_end reason={}", end.reason());
                if matches!(end, SessionEnd::Shutdown) {
                    break;
                }
                host.on_signaling_down(end.reason()).await;
                // Only a session that actually stayed up resets the backoff, so a
                // server that accepts then instantly drops us cannot hot-loop.
                if started.elapsed() > Duration::from_secs(5) {
                    backoff = SIGNAL_BACKOFF_MIN;
                }
            }
            Err(e) => log_info!("event=signaling.connect_failed attempt={attempt} error={e:#}"),
        }
        let delay = backoff + Duration::from_millis((attempt * 37) % 250);
        log_info!("event=signaling.retry_in delay_ms={}", delay.as_millis());
        tokio::select! {
            biased;
            _ = shutdown_rx.changed() => break,
            _ = tokio::time::sleep(delay) => {}
        }
        backoff = (backoff * 2).min(SIGNAL_BACKOFF_MAX);
    }

    supervisor.abort();
    host.shutdown().await;
    log_info!("event=host.exit");
    Ok(())
}
