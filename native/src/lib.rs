//! Library targets shared by the swayrtc-host binaries.
//!
//! `caps` is the GPU capability probe + capture planner of DESIGN.md §2:
//! swayrtc-probe prints its decisions, swayrtc-capture consumes them when
//! SWAYRTC_ENCODER=auto.

pub mod caps;
