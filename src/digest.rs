//! SHA-256 of files and streams.

/*************************************************************************************************
 * Copyright © 2022-2026 Kristian Thy (<thy@42.dk>)                                              *
 * This file is part of PixRS: <https://github.com/kthy/pixrs>                                   *
 *************************************************************************************************/

use ring::digest::{Context, Digest, SHA256};
use std::fs::File;
use std::io::{self, BufReader, Read, Write};
use std::path::Path;

/// How much is read at once to copy and hash files: large reads keep the round trips to a
/// network share few.
pub const READ_SIZE: usize = 256 * 1024;

/// Hashes what is written to it.
pub struct Hasher(Context);

impl Hasher {
    pub fn new() -> Self {
        Self(Context::new(&SHA256))
    }

    /// The SHA-256 of everything written.
    pub fn finish(self) -> Digest {
        self.0.finish()
    }
}

impl Default for Hasher {
    fn default() -> Self {
        Self::new()
    }
}

impl Write for Hasher {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.update(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// SHA-256 of everything `reader` reads.
pub fn sha256_from(reader: impl Read) -> io::Result<Digest> {
    let mut hasher = Hasher::new();
    io::copy(
        &mut BufReader::with_capacity(READ_SIZE, reader),
        &mut hasher,
    )?;
    Ok(hasher.finish())
}

/// SHA-256 of a file.
pub fn sha256_of(path: &Path) -> io::Result<Digest> {
    sha256_from(File::open(path)?)
}

/// Whether two digests are equal.
pub fn same_digest(a: &Digest, b: &Digest) -> bool {
    a.as_ref() == b.as_ref()
}
