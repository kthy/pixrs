# Changelog

<!-- markdownlint-disable-file MD024 -->

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
