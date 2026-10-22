# Changelog

<!-- markdownlint-disable-file MD024 -->

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- `pixrs relocate OLD NEW` points a library at the folder it was moved to (another mount point,
  drive letter or parent folder), keeping its hashes, instead of scanning it again as a new
  library. It checks a sample of the files at the new place first, with a progress bar;
  `--force` skips that check.

### Changed

- Scanning a folder inside a scanned library scans that part of the library instead of adding
  a second library with the same files. Scanning a folder that contains scanned libraries folds
  them into it, keeping their hashes, instead of reading and hashing their files again.
- `pixrs scan` shows progress while it walks the library (media files found, current folder) and
  then how many files it found and how long that took; on a network share the walk alone can take
  hours. Each step is numbered (`[1/4] Walking` to `[4/4] Decoding`), and the progress bars show
  an ETA and the rate instead of the estimated total time.

### Fixed

- Stopping `pixrs apply` or `pixrs undo` with Ctrl-C left `pixrs.lock` next to the catalog,
  so the next `apply`, `undo` or `relocate` refused to run until it was deleted by hand.
- An image file claiming absurd dimensions (a few hundred bytes can claim 100,000 x 100,000
  pixels) made `pixrs scan` and the viewer try to allocate the pixels and abort out of memory,
  and the next scan died on the same file. Images are now decoded in-process only up to 65,536
  pixels per side and 512 MiB of pixels (about 130 megapixels in color), from files up to
  512 MiB; anything beyond counts as undecodable and is left out of same-pixel and similarity
  matching.
- `pixrs scan` progress bars no longer garble the terminal when a file path is longer than the
  line: the path is cut to fit. The "Found … media files" line no longer runs into the next bar
  when copied.
- `pixrs scan` no longer prints `nom_exif` errors in the middle of the progress bars (files it
  cannot read are already reported); set `RUST_LOG` to see dependencies' log output.
- `pixrs scan` stops when the library disappears (an unplugged drive, a dropped network share)
  while reading metadata or hashing, as it already did while decoding, instead of failing on
  every remaining file. What was scanned so far is kept.
- A file cataloged in two overlapping libraries (one scanned inside the other) was reported as
  a duplicate of itself, and `dupes --quarantine` planned to move it, leaving no copy in the
  library. `dupes` and the viewer now count each file on disk once.
- On Linux, a library on a case-insensitive file system (a Windows drive under WSL, an SMB
  share, exFAT) scanned under another letter case (`/mnt/c/photos` after `/mnt/c/Photos`)
  became a second library with the same files, every file was reported as a duplicate of
  itself, and `dupes --quarantine` followed by `apply` moved the whole library into quarantine.
  A folder is now recognized as a cataloged library, or part of one, whatever its letter case,
  and a scan folds a library cataloged under another spelling into it; `relocate` and the
  commands that take library arguments accept any spelling too. Quarantine never moves a copy
  that is the same file as the kept one (one folder under two names, or a hard link).

## [0.2.0] - 2026-10-20

### Added

- `pixrs scan` catalogs a media library in SQLite: path, size, mtime and EXIF/track metadata
  (date taken, camera, dimensions). Files that share their size with another cataloged file are
  hashed (SHA-256) as duplicate candidates. Rescans only read new or changed files.
- `pixrs dupes` reports exact duplicates from the catalog across all scanned libraries, or those
  involving given libraries, with the copy to keep marked (earliest mtime, then path order).
  Text, JSON and CSV output.
- `pixrs scan` also decodes images, in parallel, for a pixel hash, a perceptual hash and a
  small fingerprint. `pixrs dupes` and the viewer use them to report images with the same
  pixels (e.g. rewritten metadata) and, with `--similar`, similar images (e.g. resized or
  recompressed), each group labeled with its tier. `--threshold` sets how similar is similar.
- `pixrs organize` plans renaming and moving a library's files by a template or layout presets,
  with dates from metadata or file names, an `undated` folder, collision suffixes and sidecar
  files. `pixrs dupes --quarantine <DIR>` plans moving duplicate copies out of the library.
  `pixrs apply` carries out a plan with a journal of every step, and `pixrs undo` reverses it.
- `pixrs dupes` and the viewer find re-encoded and resized videos (frames at 9 points, with
  `ffmpeg` and `ffprobe`) and re-encoded audio (Chromaprint fingerprints, with `fpcalc`) as
  `similar`, without `--similar`; videos under 4 seconds need `--similar`, like images.
  `--threshold` applies to video frames too and no longer needs `--similar`. Quarantine also
  moves exact and same-pixel copies in `similar` groups.
- `pixrs serve` starts a local web viewer on the catalog: browse by folder or date, view images,
  play audio and video, and compare duplicate groups. Thumbnails are cached outside the
  libraries; HEIC, AVIF and video thumbnails use `ffmpeg` if it is on `PATH`. Works offline.

- `--third-party-notices` prints the licenses of the third-party software included in PixRS,
  generated with `cargo-about` into `THIRD_PARTY_NOTICES.txt`.

### Changed

- Replace the `rouille` web server with `axum`.
- CI runs tests on Linux, Windows and macOS, and uses maintained actions instead of `actions-rs`.
- Build instructions no longer require LLVM or FFmpeg.

### Fixed

- `--license` prints the license instead of panicking.

### Removed

- The static HTML gallery generator. `pixrs serve` replaces it.
- Unused `polars` dependency.

[Unreleased]: https://github.com/kthy/pixrs/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/kthy/pixrs/releases/tag/v0.2.0
