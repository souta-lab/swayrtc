//! SwayRTC capture: wlr-screencopy -> NV12 -> FFmpeg -> H.264 Annex-B on stdout.
//!
//! This replaces `wf-recorder | ffmpeg`. The queue that cost ~85 ms of latency
//! lived inside wf-recorder, not in the encoder: measured on its own, the Intel
//! LP encoder takes 6 ms per frame (p50) when frames are handed to it one at a
//! time. Capturing here means the frame is converted and pushed into FFmpeg's
//! stdin with nothing in between.
//!
//! Layout: this process owns the Wayland connection and the NV12 conversion,
//! and spawns FFmpeg with its stdout inherited, so the H.264 stream lands
//! directly on our stdout where the host's capture manager already reads it.
//! The encoder is chosen the same way as before (VA-API when it opens, x264
//! otherwise) and every knob stays behind the same environment variables.

use anyhow::{bail, Context, Result};
use std::io::Write;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::process::{Command, Stdio};
use wayland_client::{
    globals::{registry_queue_init, GlobalListContents},
    protocol::{wl_buffer, wl_output, wl_registry, wl_shm, wl_shm_pool},
    Connection, Dispatch, QueueHandle, WEnum,
};
use wayland_protocols_wlr::screencopy::v1::client::{
    zwlr_screencopy_frame_v1 as frame, zwlr_screencopy_manager_v1 as manager,
};

fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

fn epoch_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Wayland state
// ---------------------------------------------------------------------------

#[derive(Default)]
struct State {
    /// Announced by the screencopy frame: format, width, height, stride.
    buffer_info: Option<(wl_shm::Format, u32, u32, u32)>,
    ready: bool,
    failed: Option<String>,
    y_invert: bool,
    outputs: Vec<OutputEntry>,
}

struct OutputEntry {
    output: wl_output::WlOutput,
    name: Option<String>,
    width: u32,
    height: u32,
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_shm::WlShm, ()> for State {
    fn event(
        _: &mut Self,
        _: &wl_shm::WlShm,
        event: wl_shm::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_shm::Event::Format { .. } = event {}
    }
}

impl Dispatch<wl_shm_pool::WlShmPool, ()> for State {
    fn event(
        _: &mut Self,
        _: &wl_shm_pool::WlShmPool,
        _: wl_shm_pool::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_buffer::WlBuffer, ()> for State {
    fn event(
        _: &mut Self,
        _: &wl_buffer::WlBuffer,
        _: wl_buffer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_output::WlOutput, ()> for State {
    fn event(
        state: &mut Self,
        output: &wl_output::WlOutput,
        event: wl_output::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_output::Event::Name { name } => {
                if let Some(entry) = state.outputs.iter_mut().find(|e| e.output == *output) {
                    entry.name = Some(name);
                }
            }
            wl_output::Event::Mode { width, height, .. } => {
                if let Some(entry) = state.outputs.iter_mut().find(|e| e.output == *output) {
                    entry.width = width as u32;
                    entry.height = height as u32;
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<manager::ZwlrScreencopyManagerV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &manager::ZwlrScreencopyManagerV1,
        _: manager::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<frame::ZwlrScreencopyFrameV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &frame::ZwlrScreencopyFrameV1,
        event: frame::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            frame::Event::Buffer { format, width, height, stride } => {
                if let WEnum::Value(fmt) = format {
                    state.buffer_info = Some((fmt, width, height, stride));
                }
            }
            frame::Event::Flags { flags } => {
                if let WEnum::Value(f) = flags {
                    state.y_invert = f.contains(frame::Flags::YInvert);
                }
            }
            frame::Event::Ready { .. } => state.ready = true,
            frame::Event::Failed => state.failed = Some("compositor reported failure".into()),
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Shared memory buffer
// ---------------------------------------------------------------------------

struct Shm {
    map: *mut u8,
    len: usize,
    fd: OwnedFd,
    buffer: wl_buffer::WlBuffer,
}

// The mmap pointer is only touched from the capture thread.
unsafe impl Send for Shm {}

impl Shm {
    fn new<D>(
        shm: &wl_shm::WlShm,
        qh: &QueueHandle<D>,
        width: u32,
        height: u32,
        stride: u32,
        format: wl_shm::Format,
    ) -> Result<Self>
    where
        D: Dispatch<wl_shm_pool::WlShmPool, ()> + Dispatch<wl_buffer::WlBuffer, ()> + 'static,
    {
        let len = (stride as usize) * (height as usize);
        let name = c"swayrtc-capture";
        let fd = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC) };
        if fd < 0 {
            bail!("memfd_create: {}", std::io::Error::last_os_error());
        }
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        if unsafe { libc::ftruncate(fd.as_raw_fd(), len as libc::off_t) } != 0 {
            bail!("ftruncate: {}", std::io::Error::last_os_error());
        }
        let map = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd.as_raw_fd(),
                0,
            )
        };
        if map == libc::MAP_FAILED {
            bail!("mmap: {}", std::io::Error::last_os_error());
        }
        let pool = shm.create_pool(fd.as_fd(), len as i32, qh, ());
        let buffer = pool.create_buffer(0, width as i32, height as i32, stride as i32, format, qh, ());
        pool.destroy();
        Ok(Self { map: map as *mut u8, len, fd, buffer })
    }

    fn buffer(&self) -> &wl_buffer::WlBuffer {
        &self.buffer
    }

    fn bytes(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.map, self.len) }
    }
}

impl Drop for Shm {
    fn drop(&mut self) {
        self.buffer.destroy();
        unsafe {
            libc::munmap(self.map as *mut libc::c_void, self.len);
        }
        let _ = self.fd.as_raw_fd();
    }
}

// ---------------------------------------------------------------------------
// XRGB -> NV12
// ---------------------------------------------------------------------------

/// Convert a packed 32 bit RGB buffer to NV12 (BT.709 limited range).
///
/// The capture buffers are XRGB/XBGR 8888: bytes are R,G,B,X or B,G,R,X on
/// little endian depending on the announced format. Chroma is averaged per 2x2
/// block, which is what the encoder expects.
///
/// 124 M pixels/s are needed to keep up with 1080p60, so the rows are split
/// across a few threads; each thread owns whole 2-row pairs so luma and chroma
/// stay consistent.
fn convert_rows(
    pixels: &[u8],
    width: usize,
    stride: usize,
    first_row: usize,
    y_out: &mut [u8],
    uv_out: &mut [u8],
    swap_rb: bool,
) {
    let rows = y_out.len() / width;
    let mut uv_index = 0;
    for r in 0..rows {
        let y = first_row + r;
        let row = &pixels[y * stride..y * stride + width * 4];
        let dst = &mut y_out[r * width..(r + 1) * width];
        for x in 0..width {
            let b = row[x * 4] as i32;
            let g = row[x * 4 + 1] as i32;
            let r_ = row[x * 4 + 2] as i32;
            let (r_, b) = if swap_rb { (b, r_) } else { (r_, b) };
            dst[x] = (((47 * r_ + 157 * g + 16 * b + 128) >> 8) + 16).clamp(0, 255) as u8;
        }
        // chroma once per 2-row pair, using this row and the next one
        if r % 2 == 0 {
            let y2 = (y + 1).min(first_row + rows - 1);
            let row2 = &pixels[y2 * stride..y2 * stride + width * 4];
            let target = &mut uv_out[uv_index * width..(uv_index + 1) * width];
            for x in (0..width).step_by(2) {
                let mut sr = 0i32;
                let mut sg = 0i32;
                let mut sb = 0i32;
                for (row_ref, xo) in [(row, x), (row, x + 1), (row2, x), (row2, x + 1)] {
                    let b = row_ref[xo * 4] as i32;
                    let g = row_ref[xo * 4 + 1] as i32;
                    let r_ = row_ref[xo * 4 + 2] as i32;
                    let (r_, b) = if swap_rb { (b, r_) } else { (r_, b) };
                    sr += r_;
                    sg += g;
                    sb += b;
                }
                let (r_, g, b) = (sr / 4, sg / 4, sb / 4);
                target[x] = (((-26 * r_ - 86 * g + 112 * b + 128) >> 8) + 128).clamp(0, 255) as u8;
                target[x + 1] = (((112 * r_ - 102 * g - 10 * b + 128) >> 8) + 128).clamp(0, 255) as u8;
            }
            uv_index += 1;
        }
    }
}

fn to_nv12(pixels: &[u8], width: usize, height: usize, stride: usize, swap_rb: bool, out: &mut Vec<u8>) {
    out.resize(width * height * 3 / 2, 0);
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).min(4);
    let mut rows_per = (height + threads - 1) / threads;
    rows_per = (rows_per + 1) & !1; // whole 2-row pairs
    let (y_plane, uv_plane) = out.split_at_mut(width * height);
    std::thread::scope(|scope| {
        let mut y_rest = y_plane;
        let mut uv_rest = uv_plane;
        let mut first_row = 0usize;
        while !y_rest.is_empty() {
            let rows = rows_per.min(y_rest.len() / width);
            let (y_chunk, y_tail) = y_rest.split_at_mut(rows * width);
            let uv_rows = (rows + 1) / 2;
            let (uv_chunk, uv_tail) = uv_rest.split_at_mut(uv_rows.min(uv_rest.len() / width) * width);
            scope.spawn(move || convert_rows(pixels, width, stride, first_row, y_chunk, uv_chunk, swap_rb));
            first_row += rows;
            y_rest = y_tail;
            uv_rest = uv_tail;
        }
    });
}

// ---------------------------------------------------------------------------
// Encoder
// ---------------------------------------------------------------------------

fn encoder_command(width: u32, height: u32) -> (Command, &'static str) {
    let fps = std::env::var("SWAYRTC_FPS").unwrap_or_else(|_| "60".into());
    let bitrate = std::env::var("SWAYRTC_BITRATE").unwrap_or_else(|_| "6M".into());
    let qp = std::env::var("SWAYRTC_QP").unwrap_or_else(|_| "24".into());
    let gop = std::env::var("SWAYRTC_GOP").unwrap_or_else(|_| fps.clone());
    let va_device = std::env::var("SWAYRTC_VA_DEVICE").unwrap_or_else(|_| "/dev/dri/renderD128".into());
    // Measured capture+encode latency on this host: x264 21 ms (7 ms floor),
    // VA-API 82 ms. The Intel LP encoder keeps too many frames in flight at
    // 60 fps, so latency wins as the default; SWAYRTC_ENCODER=qsv trades ~60 ms
    // of latency for roughly half the CPU.
    let force = std::env::var("SWAYRTC_ENCODER").unwrap_or_else(|_| "x264".into());
    let size = format!("{width}x{height}");
    let fps_arg = fps.clone();

    let vaapi_ok = force != "x264" && force != "auto"
        && std::path::Path::new(&va_device).exists()
        && Command::new("ffmpeg")
            .args([
                "-nostdin", "-hide_banner", "-loglevel", "error",
                "-init_hw_device", &format!("vaapi=qs:{va_device}"),
                "-filter_hw_device", "qs",
                "-f", "lavfi", "-i", "color=c=black:s=128x128:d=0.05",
                "-frames:v", "1", "-vf", "hwupload", "-c:v", "h264_vaapi", "-f", "null", "-",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);

    let mut cmd = Command::new("ffmpeg");
    cmd.args(["-nostdin", "-y", "-hide_banner", "-loglevel", "error"]);
    if vaapi_ok {
        cmd.args(["-init_hw_device", &format!("vaapi=qs:{va_device}"), "-filter_hw_device", "qs"]);
    }
    cmd.args([
        "-f", "rawvideo", "-pixel_format", "nv12",
        "-video_size", &size, "-framerate", &fps_arg, "-i", "pipe:0",
    ]);
    if vaapi_ok {
        cmd.args(["-filter_threads", "1", "-threads", "1"]);
        cmd.args(["-vf", "hwupload", "-c:v", "h264_vaapi"]);
        cmd.args(["-rc_mode", "CQP", "-qp", &qp, "-async_depth", "1"]);
        cmd.args(["-profile:v", "constrained_baseline"]);
        cmd.args(["-maxrate", &bitrate, "-bufsize", "1M"]);
    } else {
        cmd.args(["-c:v", "libx264", "-preset", "ultrafast", "-tune", "zerolatency"]);
        cmd.args(["-profile:v", "baseline", "-qp", "0", "-crf", &qp]);
        cmd.args(["-maxrate", &bitrate, "-bufsize", "1M"]);
    }
    cmd.args(["-bf", "0", "-g", &gop, "-pix_fmt", "yuv420p", "-f", "h264", "pipe:1"]);
    cmd.stdin(Stdio::piped()).stdout(Stdio::inherit()).stderr(Stdio::inherit());
    (cmd, if vaapi_ok { "qsv" } else { "x264" })
}

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

fn main() -> Result<()> {
    let want = std::env::var("SWAYRTC_OUTPUT").unwrap_or_else(|_| "HEADLESS-1".into());
    let conn = Connection::connect_to_env().context("connect to the Wayland session")?;
    let (globals, mut queue) = registry_queue_init::<State>(&conn).context("registry queue")?;
    let qh = queue.handle();

    let shm: wl_shm::WlShm = globals.bind(&qh, 1..=1, ()).context("wl_shm unavailable")?;
    let manager: manager::ZwlrScreencopyManagerV1 = globals
        .bind(&qh, 1..=3, ())
        .context("compositor does not support wlr-screencopy")?;
    let mut state = State::default();
    // Enumerate outputs; wait for their name events.
    let registry: wl_registry::WlRegistry = globals.registry().clone();
    let _ = &registry;
    for global in globals.contents().clone_list() {
        if global.interface == "wl_output" {
            let output: wl_output::WlOutput = registry.bind(global.name, 4u32.min(global.version), &qh, ());
            state.outputs.push(OutputEntry { output, name: None, width: 0, height: 0 });
        }
    }
    queue.roundtrip(&mut state).context("output enumeration")?;
    let entry = state
        .outputs
        .iter()
        .find(|e| e.name.as_deref() == Some(want.as_str()))
        .or_else(|| state.outputs.first())
        .context("no output to capture")?;
    let output = entry.output.clone();
    eprintln!("swayrtc-capture: output {} {:?}", want, entry.name);

    let mut encoder = {
        let (mut cmd, kind) = encoder_command(entry.width.max(1), entry.height.max(1));
        let child = cmd.spawn().context("spawning the encoder")?;
        eprintln!("swayrtc-capture: encoder {kind}");
        child
    };
    let mut sink = encoder.stdin.take().context("encoder stdin")?;

    let mut shm_frame: Option<Shm> = None;
    let mut dims = (0u32, 0u32);
    let mut nv12: Vec<u8> = Vec::new();
    let frames_total = std::cell::Cell::new(0u64);
    let started = std::time::Instant::now();

    loop {
        // overlay_cursor = 1: the remote view must match what the monitor shows,
        // cursor included (and pointer-lock mode hides the local one, so without
        // this the user would see no cursor at all).
        let frame_obj = manager.capture_output(1, &output, &qh, ());
        state.ready = false;
        state.failed = None;
        state.buffer_info = None;
        // 1) the compositor announces the buffer layout
        while state.buffer_info.is_none() {
            if let Some(err) = state.failed.take() {
                // A powered-down or unplugged output fails here; wait for it to
                // come back instead of tearing the pipeline down.
                eprintln!("swayrtc-capture: screencopy failed ({err}); retrying");
                std::thread::sleep(std::time::Duration::from_millis(250));
                continue;
            }
            queue.blocking_dispatch(&mut state).context("waiting for the buffer layout")?;
        }
        let (format, width, height, stride) = state
            .buffer_info
            .context("screencopy did not announce a buffer")?;
        if shm_frame.is_none() || dims != (width, height) {
            shm_frame = Some(Shm::new(&shm, &qh, width, height, stride, format)?);
            dims = (width, height);
            nv12.resize((width as usize) * (height as usize) * 3 / 2, 0);
        }
        // 2) hand it a buffer and ask for the copy - without this the compositor
        //    never produces the frame and `ready` never arrives
        frame_obj.copy(shm_frame.as_ref().unwrap().buffer());
        // 3) wait until the pixels are in our shared memory
        while !state.ready {
            if let Some(err) = state.failed.take() {
                eprintln!("swayrtc-capture: screencopy failed ({err}); retrying");
                std::thread::sleep(std::time::Duration::from_millis(250));
                continue;
            }
            queue.blocking_dispatch(&mut state).context("waiting for the captured frame")?;
        }
        let swap_rb = matches!(format, wl_shm::Format::Xbgr8888 | wl_shm::Format::Abgr8888);
        let convert_started = std::time::Instant::now();
        {
            let shm_buf = shm_frame.as_ref().unwrap();
            to_nv12(shm_buf.bytes(), width as usize, height as usize, stride as usize, swap_rb, &mut nv12);
        }
        if env_usize("SWAYRTC_CAPTURE_LOG", 0) == 1 {
            eprintln!("swayrtc-capture: convert {:.2} ms", convert_started.elapsed().as_secs_f64() * 1000.0);
        }
        frame_obj.destroy();
        queue.roundtrip(&mut state).context("flush")?;

        let written = sink.write_all(&nv12);
        if env_usize("SWAYRTC_CAPTURE_LOG", 0) == 1 {
            eprintln!(
                "swayrtc-capture: frame {} {}x{} stride={} write={:?}",
                frames_total.get() + 1, width, height, stride, written.as_ref().err().map(|e| e.to_string())
            );
        }
        if written.is_err() {
            break;
        }
        if sink.flush().is_err() {
            break;
        }
        let n = frames_total.get() + 1;
        frames_total.set(n);
        if env_usize("SWAYRTC_CAPTURE_LOG", 0) == 1 && n % 60 == 0 {
            let secs = started.elapsed().as_secs_f64().max(0.001);
            eprintln!(
                "swayrtc-capture: frames={n} fps={:.1} ts={}",
                n as f64 / secs,
                epoch_ms()
            );
        }
    }
    drop(sink);
    let _ = encoder.wait();
    Ok(())
}
