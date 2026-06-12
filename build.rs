// SPDX-License-Identifier: GPL-3.0-or-later

//! Build script for the `gander` crate.
//!
//! Fails the build loudly if `crates/gander-chat/dist/index.html` does not
//! exist (i.e. the Leptos WASM bundle has not been built yet).  Run
//! `cargo xtask build-chat` to produce it.

use std::path::Path;

fn main() {
    // Re-run this script whenever the gander-chat dist directory changes so
    // that new asset builds are picked up immediately.
    println!("cargo:rerun-if-changed=crates/gander-chat/dist");

    let dist_index = Path::new("crates/gander-chat/dist/index.html");
    if !dist_index.exists() {
        eprintln!(
            "error[gander build.rs]: gander-chat dist not found.\n\
             Run `cargo xtask build-chat` to build the Leptos WASM bundle first."
        );
        std::process::exit(1);
    }
}
