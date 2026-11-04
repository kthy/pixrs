//! Build script: record the versions of the crates that decode images, so the catalog can tell
//! which image hashes were made by other decoder versions and must be made again. Sets
//! `PIXRS_CRATE_<name>` to `major.minor` for each, from `Cargo.lock`.

/*************************************************************************************************
 * Copyright © 2022-2026 Kristian Thy (<thy@42.dk>)                                              *
 * This file is part of PixRS: <https://github.com/kthy/pixrs>                                   *
 *************************************************************************************************/

use std::collections::BTreeMap;
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

/// The numeric parts of a version like `0.25.10`, for comparing versions.
fn numbers(version: &str) -> Vec<u64> {
    version
        .split('.')
        .map(|part| {
            let digits: String = part.chars().take_while(char::is_ascii_digit).collect();
            digits.parse().unwrap_or(0)
        })
        .collect()
}

fn main() {
    println!("cargo:rerun-if-changed=Cargo.lock");
    let lock = fs::read_to_string("Cargo.lock")
        .unwrap_or_else(|e| panic!("Cargo.lock lists the decoder crates' versions: {e}"));
    // A crate can be in the lock in several versions, when other crates need other ones. The
    // highest is taken, so the result does not depend on the order of the lock.
    let mut versions: BTreeMap<&str, &str> = BTreeMap::new();
    let mut name: Option<&str> = None;
    for line in lock.lines() {
        if let Some(n) = line.strip_prefix("name = ") {
            name = Some(n.trim_matches('"'));
        } else if let Some(v) = line.strip_prefix("version = ") {
            if let Some(n) = name.take().filter(|n| DECODER_CRATES.contains(n)) {
                let v = v.trim_matches('"');
                let highest = versions.entry(n).or_insert(v);
                if numbers(v) > numbers(highest) {
                    *highest = v;
                }
            }
        }
    }
    for crate_name in DECODER_CRATES {
        let version = versions
            .get(crate_name)
            .unwrap_or_else(|| panic!("Cargo.lock lists no version of {crate_name}"));
        // Major and minor only: patch releases do not change decoded pixels on purpose, and every
        // change here makes the next scan decode the whole library again.
        let major_minor: Vec<&str> = version.split('.').take(2).collect();
        println!(
            "cargo:rustc-env=PIXRS_CRATE_{}={}",
            crate_name.replace('-', "_"),
            major_minor.join(".")
        );
    }
}
