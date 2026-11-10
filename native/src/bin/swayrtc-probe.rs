//! swayrtc-probe: enumerate GPU VA-API capabilities and print the capture
//! plan the generalization planner would choose on this machine.
//!
//! Phase 1 of the plan in DESIGN.md; the probe/planner itself lives in
//! `caps` (shared with swayrtc-capture since phase 2). It changes nothing:
//! no device state, no config files, no services. Its output is the doctor
//! surface — run it on any machine and it states what SwayRTC would do
//! there, and why.
//!
//! Usage:
//!   swayrtc-probe [--verbose]              human-readable doctor report
//!   swayrtc-probe --format env             SWAYRTC_PLAN_* assignments (shell)
//!   swayrtc-probe --format json            the plan as one JSON object

use swayrtc_host::caps::{self, ConvertLoc, EncodeLoc, Strategy};

fn print_env(devs: &[caps::Device]) {
    if devs.is_empty() {
        println!("SWAYRTC_PLAN_STRATEGY={}", Strategy::CpuX264.slug());
        println!("SWAYRTC_PLAN_STRATEGY_NAME={}", Strategy::CpuX264.name());
        println!("SWAYRTC_PLAN_ENCODER=x264");
        println!("SWAYRTC_PLAN_FALLBACK=");
        return;
    }
    let (cap_idx, _why) = caps::pick_capture_device(devs);
    let p = caps::plan(devs, cap_idx);
    println!("SWAYRTC_PLAN_STRATEGY={}", p.strategy.slug());
    println!("SWAYRTC_PLAN_STRATEGY_NAME={}", p.strategy.name());
    println!("SWAYRTC_PLAN_CAPTURE_DEVICE={}", p.capture_node.display());
    match &p.convert {
        ConvertLoc::Vpp(node) => {
            println!("SWAYRTC_PLAN_CONVERT=vpp");
            println!("SWAYRTC_PLAN_CONVERT_DEVICE={}", node.display());
        }
        ConvertLoc::Cpu => println!("SWAYRTC_PLAN_CONVERT=cpu"),
    }
    println!("SWAYRTC_PLAN_ENCODER={}", p.strategy.encoder());
    if let EncodeLoc::Vaapi { node, .. } = &p.encode {
        println!("SWAYRTC_PLAN_ENCODE_DEVICE={}", node.display());
    }
    let fb: Vec<&str> = p.fallback.iter().map(|s| s.slug()).collect();
    println!("SWAYRTC_PLAN_FALLBACK={}", fb.join(","));
}

fn print_json(devs: &[caps::Device]) {
    let value = if devs.is_empty() {
        serde_json::json!({
            "strategy": Strategy::CpuX264.name(),
            "strategy_slug": Strategy::CpuX264.slug(),
            "capture_device": null,
            "capture_reason": "no render nodes found under /sys/class/drm",
            "convert": "cpu",
            "convert_device": null,
            "encoder": "x264",
            "encode_device": null,
            "fallback": [],
            "notes": [],
        })
    } else {
        let (cap_idx, why) = caps::pick_capture_device(devs);
        let p = caps::plan(devs, cap_idx);
        let (convert, convert_device) = match &p.convert {
            ConvertLoc::Vpp(node) => ("vpp", Some(node.display().to_string())),
            ConvertLoc::Cpu => ("cpu", None),
        };
        let encode_device = match &p.encode {
            EncodeLoc::Vaapi { node, .. } => Some(node.display().to_string()),
            EncodeLoc::X264 => None,
        };
        serde_json::json!({
            "strategy": p.strategy.name(),
            "strategy_slug": p.strategy.slug(),
            "capture_device": p.capture_node.display().to_string(),
            "capture_reason": why,
            "convert": convert,
            "convert_device": convert_device,
            "encoder": p.strategy.encoder(),
            "encode_device": encode_device,
            "fallback": p.fallback.iter().map(|s| s.slug()).collect::<Vec<_>>(),
            "notes": p.notes,
        })
    };
    println!("{}", serde_json::to_string_pretty(&value).unwrap());
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let verbose = args.iter().any(|a| a == "--verbose" || a == "-v");
    let format = args
        .iter()
        .position(|a| a == "--format")
        .and_then(|i| args.get(i + 1))
        .map(|s| s.as_str())
        .or_else(|| {
            args.iter()
                .find_map(|a| a.strip_prefix("--format="))
        })
        .unwrap_or("human");

    let devs = caps::enumerate_devices();
    match format {
        "env" => print_env(&devs),
        "json" => print_json(&devs),
        _ => print!("{}", caps::doctor_report(&devs, verbose)),
    }
}
