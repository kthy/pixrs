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

### Changed

- The gallery generator moved to the `pixrs gallery` subcommand.
- Replace the `rouille` web server with `axum`.
- CI runs tests on Linux, Windows and macOS, and uses maintained actions instead of `actions-rs`.
- Build instructions no longer require LLVM or FFmpeg.

### Fixed

- `--license` prints the license instead of panicking.
- Local media URLs were truncated when the base path was `.`.

### Removed

- Unused `polars` dependency.
