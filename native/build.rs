//! Build glue for the Wayland input backend.
//!
//! The protocol bindings themselves are generated at compile time by the
//! `wayland-scanner` proc-macros used from `src/input.rs`; cargo does not know
//! that those macros read files from `protocols/`, so this build script keeps
//! the crate in sync when the protocol XML changes.  Keeping the XML in-tree
//! means the crate builds offline with no dependency on a system
//! wayland-protocols / wlr-protocols checkout.

use std::path::Path;

fn main() {
    // Rebuild whenever a protocol description changes (the scanner macros in
    // src/input.rs read these files while the crate is being compiled).
    println!("cargo:rerun-if-changed=protocols");
    println!("cargo:rerun-if-changed=protocols/virtual-keyboard-unstable-v1.xml");
    println!("cargo:rerun-if-changed=protocols/wlr-virtual-pointer-unstable-v1.xml");

    // Fail early with a readable message when the checkout is incomplete,
    // instead of a proc-macro panic deep inside the scanner.
    for file in [
        "protocols/virtual-keyboard-unstable-v1.xml",
        "protocols/wlr-virtual-pointer-unstable-v1.xml",
    ] {
        let path = Path::new(file);
        if !path.exists() {
            println!(
                "cargo:warning=missing protocol description: {}",
                path.display()
            );
            panic!("missing protocol description: {}", path.display());
        }
    }

    println!("cargo:rerun-if-changed=build.rs");
}
