//! GPU VA-API capability probe + capture planner (DESIGN.md §2).
//!
//! Shared by swayrtc-probe (the doctor surface) and swayrtc-capture
//! (SWAYRTC_ENCODER=auto). The probe talks to libva directly
//! (vaGetDisplayDRM + vaQueryConfigProfiles/Entrypoints); it never parses
//! vainfo output. It changes nothing: no device state, no config files,
//! no services.

use std::collections::BTreeMap;
use std::ffi::{CStr, CString};
use std::fs;
use std::os::raw::{c_char, c_int, c_void};
use std::path::PathBuf;

// ---------------------------------------------------------------------------
// libva FFI (constants verified against /usr/include/va/va.h, libva 2.23)
// ---------------------------------------------------------------------------

pub type VaDisplay = *mut c_void;
pub type VaStatus = c_int;
pub type VaProfile = c_int;
pub type VaEntrypoint = c_int;

const VA_STATUS_SUCCESS: VaStatus = 0;
pub const VA_PROFILE_NONE: VaProfile = -1;
pub const VA_PROFILE_H264_MAIN: VaProfile = 6;
pub const VA_PROFILE_H264_HIGH: VaProfile = 7;
pub const VA_PROFILE_H264_CONSTRAINED_BASELINE: VaProfile = 13;
pub const VA_ENTRYPOINT_ENC_SLICE: VaEntrypoint = 6;
// VAEntrypointEncPicture (7) exists but is a picture/JPEG encoder entrypoint,
// not an H.264 slice encoder; the probe deliberately never counts it.
pub const VA_ENTRYPOINT_ENC_SLICE_LP: VaEntrypoint = 8;
pub const VA_ENTRYPOINT_VIDEO_PROC: VaEntrypoint = 10;

#[link(name = "va")]
extern "C" {
    fn vaInitialize(dpy: VaDisplay, major: *mut c_int, minor: *mut c_int) -> VaStatus;
    fn vaTerminate(dpy: VaDisplay) -> VaStatus;
    fn vaQueryConfigProfiles(
        dpy: VaDisplay,
        profiles: *mut VaProfile,
        num_profiles: *mut c_int,
    ) -> VaStatus;
    fn vaQueryConfigEntrypoints(
        dpy: VaDisplay,
        profile: VaProfile,
        entrypoints: *mut VaEntrypoint,
        num_entrypoints: *mut c_int,
    ) -> VaStatus;
    fn vaQueryVendorString(dpy: VaDisplay) -> *const c_char;
}

#[link(name = "va-drm")]
extern "C" {
    fn vaGetDisplayDRM(fd: c_int) -> VaDisplay;
}

pub fn profile_name(p: VaProfile) -> String {
    match p {
        -1 => "None".into(),
        0 => "MPEG2Simple".into(),
        1 => "MPEG2Main".into(),
        2 => "MPEG4Simple".into(),
        3 => "MPEG4AdvancedSimple".into(),
        4 => "MPEG4Main".into(),
        5 => "H264Baseline".into(),
        6 => "H264Main".into(),
        7 => "H264High".into(),
        8 => "VC1Simple".into(),
        9 => "VC1Main".into(),
        10 => "VC1Advanced".into(),
        11 => "H263Baseline".into(),
        12 => "JPEGBaseline".into(),
        13 => "H264ConstrainedBaseline".into(),
        14 => "VP8Version0_3".into(),
        15 => "H264MultiviewHigh".into(),
        16 => "H264StereoHigh".into(),
        17 => "HEVCMain".into(),
        18 => "HEVCMain10".into(),
        19 => "VP9Profile0".into(),
        20 => "VP9Profile1".into(),
        21 => "VP9Profile2".into(),
        22 => "VP9Profile3".into(),
        23 => "HEVCMain12".into(),
        24 => "HEVCMain422_10".into(),
        25 => "HEVCMain422_12".into(),
        26 => "HEVCMain444".into(),
        27 => "HEVCMain444_10".into(),
        28 => "HEVCMain444_12".into(),
        29 => "AV1Profile0".into(),
        30 => "AV1Profile1".into(),
        other => format!("Profile({other})"),
    }
}

pub fn entrypoint_name(e: VaEntrypoint) -> String {
    match e {
        1 => "VLD".into(),
        2 => "IZZ".into(),
        3 => "IDCT".into(),
        4 => "MoComp".into(),
        5 => "Deblocking".into(),
        6 => "EncSlice".into(),
        7 => "EncPicture".into(),
        8 => "EncSliceLP".into(),
        10 => "VideoProc".into(),
        11 => "FEI".into(),
        other => format!("Entrypoint({other})"),
    }
}

// ---------------------------------------------------------------------------
// Capability model (DESIGN.md §2.2)
// ---------------------------------------------------------------------------

#[derive(Default)]
pub struct VaCaps {
    pub vendor_string: String,
    pub version: (c_int, c_int),
    /// VAProfileNone entrypoints contain VideoProc.
    pub vpp: bool,
    /// H.264 profile -> encode entrypoints found (EncSlice / EncSliceLP).
    /// EncPicture is not an H.264 slice encoder and is deliberately ignored.
    pub h264_encode: BTreeMap<VaProfile, Vec<VaEntrypoint>>,
    /// Every profile -> entrypoints, for --verbose.
    pub all: Vec<(VaProfile, Vec<VaEntrypoint>)>,
}

impl VaCaps {
    pub fn has_h264_encode(&self) -> bool {
        !self.h264_encode.is_empty()
    }
    /// The entrypoint we would encode ConstrainedBaseline with, if any.
    pub fn baseline_entrypoint(&self) -> Option<VaEntrypoint> {
        self.h264_encode
            .get(&VA_PROFILE_H264_CONSTRAINED_BASELINE)
            .and_then(|eps| eps.first().copied())
    }
}

pub struct Device {
    pub name: String, // renderD128
    pub node: PathBuf,
    pub pci_vendor: u32,
    pub pci_device: u32,
    pub vendor_name: String,
    pub kernel_driver: String,
    pub card: Option<String>, // card1
    pub connectors: Vec<(String, String)>, // (HDMI-A-4, connected|disconnected|unknown)
    pub boot_vga: bool,
    pub va: Result<VaCaps, String>,
}

impl Device {
    pub fn caps(&self) -> Option<&VaCaps> {
        self.va.as_ref().ok()
    }
    pub fn vpp(&self) -> bool {
        self.caps().map(|c| c.vpp).unwrap_or(false)
    }
    pub fn has_h264_encode(&self) -> bool {
        self.caps().map(|c| c.has_h264_encode()).unwrap_or(false)
    }
    pub fn connected_connector(&self) -> Option<&str> {
        self.connectors
            .iter()
            .find(|(_, status)| status == "connected")
            .map(|(name, _)| name.as_str())
    }
    pub fn short(&self) -> String {
        format!("{} ({})", self.node.display(), self.vendor_name)
    }
}

// ---------------------------------------------------------------------------
// sysfs enumeration
// ---------------------------------------------------------------------------

fn read_trim(path: &PathBuf) -> Option<String> {
    fs::read_to_string(path).ok().map(|s| s.trim().to_string())
}

fn parse_hex(s: &str) -> u32 {
    u32::from_str_radix(s.trim_start_matches("0x"), 16).unwrap_or(0)
}

fn vendor_name(id: u32) -> String {
    match id {
        0x8086 => "Intel".into(),
        0x1002 => "AMD".into(),
        0x10de => "NVIDIA".into(),
        other => format!("vendor {other:#06x}"),
    }
}

pub fn enumerate_devices() -> Vec<Device> {
    let mut names: Vec<String> = Vec::new();
    if let Ok(rd) = fs::read_dir("/sys/class/drm") {
        for e in rd.flatten() {
            let n = e.file_name().to_string_lossy().into_owned();
            if n.starts_with("renderD") {
                names.push(n);
            }
        }
    }
    names.sort_by_key(|n| {
        n.trim_start_matches("renderD")
            .parse::<u32>()
            .unwrap_or(u32::MAX)
    });

    let mut devs = Vec::new();
    for name in names {
        let base = PathBuf::from("/sys/class/drm").join(&name).join("device");
        let pci_vendor = read_trim(&base.join("vendor")).map(|s| parse_hex(&s)).unwrap_or(0);
        let pci_device = read_trim(&base.join("device")).map(|s| parse_hex(&s)).unwrap_or(0);
        let kernel_driver = fs::read_link(base.join("driver"))
            .ok()
            .and_then(|p| p.file_name().map(|f| f.to_string_lossy().into_owned()))
            .unwrap_or_else(|| "?".into());
        let boot_vga = read_trim(&base.join("boot_vga")).as_deref() == Some("1");

        // The card this render node belongs to, and its connectors.
        let mut card = None;
        let mut connectors = Vec::new();
        if let Ok(rd) = fs::read_dir(base.join("drm")) {
            for e in rd.flatten() {
                let n = e.file_name().to_string_lossy().into_owned();
                if n.len() > 4
                    && n.starts_with("card")
                    && n[4..].chars().all(|c| c.is_ascii_digit())
                {
                    card = Some(n);
                }
            }
        }
        if let Some(card_name) = &card {
            let prefix = format!("{card_name}-");
            if let Ok(rd) = fs::read_dir("/sys/class/drm") {
                for e in rd.flatten() {
                    let n = e.file_name().to_string_lossy().into_owned();
                    if let Some(conn) = n.strip_prefix(&prefix) {
                        let status = read_trim(&e.path().join("status"))
                            .unwrap_or_else(|| "?".into());
                        connectors.push((conn.to_string(), status));
                    }
                }
            }
            connectors.sort();
        }

        let node = PathBuf::from("/dev/dri").join(&name);
        let va = probe_va(&node);
        devs.push(Device {
            name,
            node,
            pci_vendor,
            pci_device,
            vendor_name: vendor_name(pci_vendor),
            kernel_driver,
            card,
            connectors,
            boot_vga,
            va,
        });
    }
    devs
}

// ---------------------------------------------------------------------------
// libva probing
// ---------------------------------------------------------------------------

fn probe_va(node: &PathBuf) -> Result<VaCaps, String> {
    let cpath = CString::new(node.to_string_lossy().as_bytes())
        .map_err(|_| "node path contains NUL".to_string())?;
    let fd = unsafe { libc::open(cpath.as_ptr(), libc::O_RDWR) };
    if fd < 0 {
        let err = std::io::Error::last_os_error();
        return Err(format!("open {} failed: {err}", node.display()));
    }
    // libva logs driver-open chatter ("Trying to open ..._drv_video.so") to
    // stderr at its default messaging level. A doctor tool's output is its
    // interface, so stderr is parked at /dev/null for the probing section;
    // everything the probe itself has to say travels via the Result.
    let saved_stderr = unsafe { libc::dup(libc::STDERR_FILENO) };
    let devnull = unsafe {
        libc::open(
            CString::new("/dev/null").unwrap().as_ptr(),
            libc::O_WRONLY,
        )
    };
    if saved_stderr >= 0 && devnull >= 0 {
        unsafe { libc::dup2(devnull, libc::STDERR_FILENO) };
    }
    let result = unsafe { probe_va_inner(fd) };
    if saved_stderr >= 0 {
        unsafe {
            libc::dup2(saved_stderr, libc::STDERR_FILENO);
            libc::close(saved_stderr);
        }
    }
    if devnull >= 0 {
        unsafe { libc::close(devnull) };
    }
    unsafe {
        libc::close(fd);
    }
    result
}

unsafe fn probe_va_inner(fd: c_int) -> Result<VaCaps, String> {
    let dpy = vaGetDisplayDRM(fd);
    if dpy.is_null() {
        return Err("vaGetDisplayDRM returned null".into());
    }
    let mut major: c_int = 0;
    let mut minor: c_int = 0;
    let st = vaInitialize(dpy, &mut major, &mut minor);
    if st != VA_STATUS_SUCCESS {
        vaTerminate(dpy);
        return Err(format!("vaInitialize failed (VAStatus {st})"));
    }

    let vendor_string = {
        let p = vaQueryVendorString(dpy);
        if p.is_null() {
            String::new()
        } else {
            CStr::from_ptr(p).to_string_lossy().into_owned()
        }
    };

    const MAX_PROFILES: usize = 128;
    const MAX_ENTRYPOINTS: usize = 32;
    let mut profiles = [0 as VaProfile; MAX_PROFILES];
    let mut num_profiles: c_int = MAX_PROFILES as c_int;
    let st = vaQueryConfigProfiles(dpy, profiles.as_mut_ptr(), &mut num_profiles);
    if st != VA_STATUS_SUCCESS {
        vaTerminate(dpy);
        return Err(format!("vaQueryConfigProfiles failed (VAStatus {st})"));
    }

    let mut caps = VaCaps {
        vendor_string,
        version: (major, minor),
        ..Default::default()
    };
    for &profile in &profiles[..num_profiles.max(0) as usize] {
        let mut eps = [0 as VaEntrypoint; MAX_ENTRYPOINTS];
        let mut num_eps: c_int = MAX_ENTRYPOINTS as c_int;
        let st = vaQueryConfigEntrypoints(dpy, profile, eps.as_mut_ptr(), &mut num_eps);
        if st != VA_STATUS_SUCCESS {
            continue; // profile listed but not queryable; skip it
        }
        let eps: Vec<VaEntrypoint> = eps[..num_eps.max(0) as usize].to_vec();
        if profile == VA_PROFILE_NONE && eps.contains(&VA_ENTRYPOINT_VIDEO_PROC) {
            caps.vpp = true;
        }
        if matches!(
            profile,
            VA_PROFILE_H264_MAIN | VA_PROFILE_H264_HIGH | VA_PROFILE_H264_CONSTRAINED_BASELINE
        ) {
            let enc: Vec<VaEntrypoint> = eps
                .iter()
                .copied()
                .filter(|e| *e == VA_ENTRYPOINT_ENC_SLICE || *e == VA_ENTRYPOINT_ENC_SLICE_LP)
                .collect();
            if !enc.is_empty() {
                caps.h264_encode.insert(profile, enc);
            }
        }
        caps.all.push((profile, eps));
    }

    vaTerminate(dpy);
    Ok(caps)
}

// ---------------------------------------------------------------------------
// Capture-source determination (DESIGN.md §2.3)
// ---------------------------------------------------------------------------

pub fn pick_capture_device(devs: &[Device]) -> (usize, String) {
    // 1. Explicit override.
    if let Ok(want) = std::env::var("SWAYRTC_CAPTURE_DEVICE") {
        if let Some((i, _)) = devs
            .iter()
            .enumerate()
            .find(|(_, d)| d.name == want || d.node.to_string_lossy() == want)
        {
            return (i, format!("SWAYRTC_CAPTURE_DEVICE={want} (operator override)"));
        }
    }
    // 2. Device driving a connected connector.
    for (i, d) in devs.iter().enumerate() {
        if let Some(conn) = d.connected_connector() {
            return (i, format!("drives connected output {conn}"));
        }
    }
    // 3. boot_vga device.
    for (i, d) in devs.iter().enumerate() {
        if d.boot_vga {
            return (
                i,
                "boot_vga GPU (no connected connector found; headless session renders on the primary GPU)".into(),
            );
        }
    }
    // 4. First render node.
    (0, "first render node (fallback)".into())
}

// ---------------------------------------------------------------------------
// Planner (DESIGN.md §2.4)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Strategy {
    ZeroCopySingle,
    ZeroCopyCross,
    CpuVaapi,
    CpuX264,
}

impl Strategy {
    /// Human-readable plan name (the phase 1 doctor wording).
    pub fn name(&self) -> &'static str {
        match self {
            Strategy::ZeroCopySingle => "zero-copy, single-device",
            Strategy::ZeroCopyCross => "zero-copy, cross-device",
            Strategy::CpuVaapi => "cpu-convert + vaapi-encode",
            Strategy::CpuX264 => "cpu-convert + x264",
        }
    }
    /// Machine-readable slug for --format env / JSON.
    pub fn slug(&self) -> &'static str {
        match self {
            Strategy::ZeroCopySingle => "zerocopy-single-device",
            Strategy::ZeroCopyCross => "zerocopy-cross-device",
            Strategy::CpuVaapi => "cpu-vaapi",
            Strategy::CpuX264 => "cpu-x264",
        }
    }
    /// The pipeline family that executes this plan at the capture level:
    /// GPU zero-copy, CPU-convert + VA-API encode, or software x264.
    pub fn encoder(&self) -> &'static str {
        match self {
            Strategy::ZeroCopySingle | Strategy::ZeroCopyCross => "zerocopy",
            Strategy::CpuVaapi => "vaapi",
            Strategy::CpuX264 => "x264",
        }
    }
}

#[derive(Clone, Debug)]
pub enum ConvertLoc {
    /// Colour conversion on this device's VPP.
    Vpp(PathBuf),
    Cpu,
}

#[derive(Clone, Debug)]
pub enum EncodeLoc {
    Vaapi { node: PathBuf, entrypoint: String },
    X264,
}

pub struct Plan {
    pub strategy: Strategy,
    pub capture_node: PathBuf,
    pub capture_desc: String,
    pub convert: ConvertLoc,
    pub encode: EncodeLoc,
    pub lines: Vec<String>,
    pub notes: Vec<String>,
    pub fallback: Vec<Strategy>,
}

pub fn plan(devs: &[Device], capture_idx: usize) -> Plan {
    let cap = &devs[capture_idx];
    let mut notes: Vec<String> = Vec::new();

    let encoders: Vec<&Device> = devs.iter().filter(|d| d.has_h264_encode()).collect();
    let other_encoder = encoders
        .iter()
        .copied()
        .find(|d| d.name != cap.name)
        // Prefer the lowest render node among the other encoders.
        .or_else(|| encoders.first().copied());

    // Strategy 1: zero-copy, single-device.
    if cap.vpp() && cap.has_h264_encode() {
        let ep = cap
            .caps()
            .and_then(|c| c.baseline_entrypoint())
            .map(entrypoint_name)
            .unwrap_or_else(|| "?".into());
        return Plan {
            strategy: Strategy::ZeroCopySingle,
            capture_node: cap.node.clone(),
            capture_desc: cap.short(),
            convert: ConvertLoc::Vpp(cap.node.clone()),
            encode: EncodeLoc::Vaapi {
                node: cap.node.clone(),
                entrypoint: ep.clone(),
            },
            lines: vec![
                format!("capture  {} — compositor dma-buf", cap.short()),
                format!("convert  {} — VPP", cap.short()),
                format!(
                    "encode   {} — h264_vaapi, ConstrainedBaseline via {ep}",
                    cap.short()
                ),
            ],
            notes,
            fallback: fallback_chain(devs),
        };
    }
    if cap.vpp() && !cap.has_h264_encode() {
        notes.push(format!(
            "strategy 1 (zero-copy single-device) rejected: capture device {} has VPP but no H.264 encode entrypoint",
            cap.node.display()
        ));
    } else if !cap.vpp() {
        notes.push(format!(
            "strategy 1 (zero-copy single-device) rejected: capture device {} has no VideoProc entrypoint",
            cap.node.display()
        ));
    }

    // Strategy 2: zero-copy, cross-device.
    if cap.vpp() {
        if let Some(enc) = other_encoder {
            if enc.name != cap.name {
                let ep = enc
                    .caps()
                    .and_then(|c| c.baseline_entrypoint())
                    .map(entrypoint_name)
                    .unwrap_or_else(|| "?".into());
                return Plan {
                    strategy: Strategy::ZeroCopyCross,
                    capture_node: cap.node.clone(),
                    capture_desc: cap.short(),
                    convert: ConvertLoc::Vpp(cap.node.clone()),
                    encode: EncodeLoc::Vaapi {
                        node: enc.node.clone(),
                        entrypoint: ep.clone(),
                    },
                    lines: vec![
                        format!("capture  {} — compositor dma-buf", cap.short()),
                        format!("convert  {} — VPP", cap.short()),
                        format!(
                            "encode   {} — h264_vaapi, ConstrainedBaseline via {ep}",
                            enc.short()
                        ),
                    ],
                    notes,
                    fallback: fallback_chain(devs),
                };
            }
        }
        notes.push(
            "strategy 2 (zero-copy cross-device) rejected: no other device has an H.264 encode entrypoint"
                .into(),
        );
    }

    // Strategy 3: CPU convert + VA-API encode (prefer encoder on capture device).
    let enc_dev = if cap.has_h264_encode() {
        Some(cap)
    } else {
        other_encoder
    };
    if let Some(enc) = enc_dev {
        let ep = enc
            .caps()
            .and_then(|c| c.baseline_entrypoint())
            .map(entrypoint_name)
            .unwrap_or_else(|| "?".into());
        notes.push(format!(
            "strategy 3 (CPU convert + VA-API encode) selected: encode on {}; capture device {} {}",
            enc.node.display(),
            cap.node.display(),
            if cap.vpp() {
                "conversion stays on the CPU in this phase (zero-copy needs an encoder the plan can pair)"
            } else {
                "has no VPP, so conversion is on the CPU"
            }
        ));
        return Plan {
            strategy: Strategy::CpuVaapi,
            capture_node: cap.node.clone(),
            capture_desc: cap.short(),
            convert: ConvertLoc::Cpu,
            encode: EncodeLoc::Vaapi {
                node: enc.node.clone(),
                entrypoint: ep,
            },
            lines: vec![
                format!("capture  {} — compositor dma-buf", cap.short()),
                "convert  CPU — XRGB -> NV12".into(),
                format!(
                    "encode   {} — h264_vaapi, ConstrainedBaseline via {}",
                    enc.short(),
                    enc.caps()
                        .and_then(|c| c.baseline_entrypoint())
                        .map(entrypoint_name)
                        .unwrap_or_else(|| "?".into())
                ),
            ],
            notes,
            fallback: vec![Strategy::CpuX264],
        };
    }

    // Strategy 4: software.
    notes.push(
        "strategy 3 rejected: no device exposes an H.264 encode entrypoint; strategy 4 (software) selected".into(),
    );
    Plan {
        strategy: Strategy::CpuX264,
        capture_node: cap.node.clone(),
        capture_desc: cap.short(),
        convert: ConvertLoc::Cpu,
        encode: EncodeLoc::X264,
        lines: vec![
            format!("capture  {} — compositor dma-buf", cap.short()),
            "convert  CPU — XRGB -> NV12".into(),
            "encode   CPU — libx264".into(),
        ],
        notes,
        fallback: Vec::new(),
    }
}

fn fallback_chain(devs: &[Device]) -> Vec<Strategy> {
    let mut chain = Vec::new();
    if devs.iter().any(|d| d.has_h264_encode()) {
        chain.push(Strategy::CpuVaapi);
    }
    chain.push(Strategy::CpuX264);
    chain
}

// ---------------------------------------------------------------------------
// Doctor report (human-readable; the phase 1 swayrtc-probe output)
// ---------------------------------------------------------------------------

pub fn doctor_report(devs: &[Device], verbose: bool) -> String {
    let mut out = String::new();
    out.push_str("swayrtc-probe — GPU / VA-API capability scan\n\n");
    if devs.is_empty() {
        out.push_str("no render nodes found under /sys/class/drm\n");
        return out;
    }

    for d in devs {
        out.push_str(&format!(
            "{} — {} (PCI {:04x}:{:04x}), kernel driver {}\n",
            d.node.display(),
            d.vendor_name,
            d.pci_vendor,
            d.pci_device,
            d.kernel_driver
        ));
        match &d.va {
            Ok(caps) => {
                out.push_str(&format!(
                    "  VA driver : {} [VA-API {}.{}]\n",
                    caps.vendor_string, caps.version.0, caps.version.1
                ));
                out.push_str(&format!(
                    "  VPP       : {}\n",
                    if caps.vpp {
                        "yes (VideoProc on VAProfileNone)"
                    } else {
                        "no"
                    }
                ));
                if caps.has_h264_encode() {
                    let parts: Vec<String> = caps
                        .h264_encode
                        .iter()
                        .map(|(p, eps)| {
                            let names: Vec<String> = eps.iter().map(|e| entrypoint_name(*e)).collect();
                            format!("{} [{}]", profile_name(*p), names.join(", "))
                        })
                        .collect();
                    out.push_str(&format!("  H.264 enc : {}\n", parts.join("; ")));
                } else {
                    out.push_str("  H.264 enc : none (H.264 profiles expose no encode entrypoint)\n");
                }
                if verbose {
                    for (p, eps) in &caps.all {
                        let names: Vec<String> = eps.iter().map(|e| entrypoint_name(*e)).collect();
                        out.push_str(&format!("    {:28} {}\n", profile_name(*p), names.join(", ")));
                    }
                }
            }
            Err(e) => out.push_str(&format!("  VA probe  : FAILED — {e}\n")),
        }
        let conns: Vec<String> = d
            .connectors
            .iter()
            .map(|(n, s)| format!("{n}={s}"))
            .collect();
        out.push_str(&format!(
            "  outputs   : {} {} (boot_vga: {})\n",
            d.card.as_deref().unwrap_or("card?"),
            if conns.is_empty() {
                "none".into()
            } else {
                conns.join(", ")
            },
            if d.boot_vga { "yes" } else { "no" }
        ));
        out.push('\n');
    }

    let (cap_idx, why) = pick_capture_device(devs);
    let cap = &devs[cap_idx];
    out.push_str(&format!("Capture source: {} — {why}\n", cap.short()));
    let p = plan(devs, cap_idx);
    out.push_str(&format!("Plan: {}\n", p.strategy.name()));
    for line in &p.lines {
        out.push_str(&format!("  {line}\n"));
    }
    for note in &p.notes {
        out.push_str(&format!("  note: {note}\n"));
    }
    if !p.fallback.is_empty() {
        let names: Vec<&str> = p.fallback.iter().map(|s| s.name()).collect();
        out.push_str(&format!("  fallback chain: {}\n", names.join(" -> ")));
    }
    out
}
