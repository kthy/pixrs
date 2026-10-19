//! Build script: record the versions of the crates that decode images, so the catalog can tell
//! which image hashes were made by other decoder versions and must be made again. Sets
//! `PIXRS_CRATE_<name>` to `major.minor` for each, from `Cargo.lock`.

/*************************************************************************************************
 * Copyright © 2022-2026 Kristian Thy (<thy@42.dk>)                                              *
 * This file is part of PixRS: <https://github.com/kthy/pixrs>                                   *
 *************************************************************************************************/

use std::fs;

/// Crates whose upgrades can change decoded pixels or perceptual hashes.
const DECODER_CRATES: &[&str] = &[
    "gif",
    "image",
    "image-webp",
    "image_hasher",
    "png",
    "tiff",
    "zune-jpeg",
];

fn main() {
    println!("cargo:rerun-if-changed=Cargo.lock");
    let lock = fs::read_to_string("Cargo.lock").unwrap_or_default();
    let mut name: Option<&str> = None;
    for line in lock.lines() {
        if let Some(n) = line.strip_prefix("name = ") {
            name = Some(n.trim_matches('"'));
        } else if let Some(v) = line.strip_prefix("version = ") {
            if let Some(n) = name.take().filter(|n| DECODER_CRATES.contains(n)) {
                // Major and minor only: patch releases do not change decoded pixels on purpose,
                // and every change here makes the next scan decode the whole library again.
                let v: Vec<&str> = v.trim_matches('"').split('.').take(2).collect();
                println!(
                    "cargo:rustc-env=PIXRS_CRATE_{}={}",
                    n.replace('-', "_"),
                    v.join(".")
                );
            }
        }
    }
}
