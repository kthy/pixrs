# Agent Instructions for PixRS

Read `README.md` for usage and build instructions. This file covers what an agent needs beyond
that.

## Purpose

PixRS is for personal media libraries built up over years from many devices (phones, laptops,
old disks), where the same photos and videos often exist in several folders. It should:

1. Let the user view images, play audio and play video in a browser.
2. Report suspected duplicates.
3. Optionally rename and reorganize files according to a layout and naming pattern the user
   chooses.

The owner's library is about 50k files, but the tool is meant for other people too. It should
work well at tens of thousands of files and not fall over at hundreds of thousands. Do not
optimize beyond that without a measured need.

## Direction

The planned shape, tracked in GitHub issues #32 to #37 (one per step):

- `scan`: walk the library into a SQLite catalog (path, size, mtime, media type, EXIF fields),
  then hash only files whose size matches another cataloged file. Incremental: only reread files
  whose path, size or mtime changed.
- `dupes`: tiered matching. Group by size, then content hash, then hash of decoded pixels
  (same image, different metadata), then perceptual hash (resized or recompressed copies).
  Fuzzy video and audio matching comes last.
- `serve`: local viewer (axum, maud, HTMX) reading from the catalog, with a thumbnail cache.
- `organize`: compute a rename/move plan, show it (dry run is the default), and only touch files
  with `--apply`.

Decided details:

- Layouts: one folder per year (optionally with month subfolders), one folder per media type
  (`image`, `video`, `audio`), or everything in one folder. Layouts can be combined, e.g.
  `video/2023/05/`.
- One template sets both folders and file names, with `/` separating folders, e.g.
  `{type}/{year}/{month}/{date}_{time}.{ext}`. Planned tokens: `{year} {month} {day} {hour}
  {minute} {second} {date} {time} {type} {camera} {name} {ext}`, where `{name}` is the original
  file name without extension. Layout presets (`flat`, `year`, `year-month`, `type`) are
  shorthand for templates.
- Name collisions in the plan raise a warning, and the colliding file gets a configurable suffix
  (default `_{n}`).
- Date source: EXIF date taken. Fallback: a date parsed from the file name. If there is none,
  keep the original file name (e.g. `DSC0047.jpg`) and put it in a configurable `undated`
  folder in place of the date folders, e.g. `video/undated/DSC0047.mp4`. A date without a time
  (e.g. WhatsApp `IMG-20190501-WA0001`) fills the date folders, and the file keeps its original
  name when the template needs a time. Dates before 1980 or more than a day in the future
  count as missing (broken metadata, e.g. QuickTime's 1904 epoch); `--trust-dates` turns that
  check off, for scans of old photos named with their real date.
- Duplicates: report only by default. A flag enables quarantine, which keeps the oldest file and
  moves the others to a quarantine folder. "Oldest" means earliest mtime, with path order as
  the tie-breaker. Exception: in groups of similar images the largest image is kept (most
  pixels, then most bytes, then oldest), since similar copies are often scaled down.
  Quarantine only acts on `exact` and `same-pixels` groups, never on `similar` ones (mostly
  burst shots, see "Image hashes").
- Organize commands: `pixrs organize` and `pixrs dupes --quarantine <DIR>` only write a plan
  file (and print a summary); `pixrs apply <PLAN>` carries out either kind of plan and
  `pixrs undo <JOURNAL>` reverses it. An interrupted `apply` is not resumed: undo it and plan
  again. Plans and journals live next to the catalog (`plans/`, `journals/`).
- Organize details: moves never overwrite (collisions get the suffix; compared case-insensitively
  on every OS, so plans are the same everywhere; a rename that only changes letter case is refused
  if another file has exactly the new name). Target folders that differ from an existing folder
  only in letter case are that folder, so folder case is never changed. Token values are sanitized
  to names valid on Windows. Sidecar files (`.xmp`, `.aae`, `.thm`, `.srt`, Google Takeout `.json`)
  with the same base name in the same folder move and are renamed with their media file, unless
  another media file with that base name stays in the folder. Folders left empty stay unless
  `--prune-empty` is given. Each move checks that the file still matches the plan (size, mtime) and
  that the target is free, else skips it; a move that fails before anything changed is skipped too,
  and a folder that cannot be read counts as taken, and only a half-done move stops `apply`. `undo`
  ignores skipped moves, only removes a file at a target if it is a copy pixrs journaled and
  verified, and only moves catalog rows that still point at the target. Temporary files are
  journaled with their kind: a stop on the way of a case-only rename is the file itself and is only
  ever put back (next to its old place if that is taken); a copy is only removed while its source
  still holds every byte of it, otherwise it is kept as `name.pixrs-recovered.ext`. A source must
  still match the plan right before it is removed after a cross-filesystem copy. Cross-filesystem
  moves copy to a temporary name, verify the SHA-256, keep the mtime, rename, then remove the
  source. The catalog is updated as files move. The quarantine folder must be outside every
  cataloged library and contain none; files go to `<DIR>/<library name>/<path in library>`. With
  library arguments, only copies in those libraries are quarantined, and each move requires the
  group's kept copy to still be there unchanged at `apply`. Accepted risk: each move checks that
  its target is free and then renames, and OS renames replace, so a file another program creates at
  exactly that name in the microseconds between would be overwritten. Closing it needs OS-specific
  no-replace renames; the owner decided not to (#44).
- Media probing: call the `ffmpeg`/`ffprobe` binaries if they are on `PATH`, and degrade
  gracefully if not. Do not link against libav: it complicates the build on Windows and its GPL
  licensing does not sit well with linking into this AGPL program.
- Metadata: `nom-exif` (covers JPEG, HEIC and MP4/MOV).
- Viewer: binds to `127.0.0.1` and rejects requests whose `Host` header is not
  `localhost:PORT` or `127.0.0.1:PORT` (DNS rebinding). Files are served only if they are a row
  in the catalog. HTMX 4 and all other assets are compiled into the binary.
- Viewer listings: HTMX infinite scroll, 200 items per page, with a plain "Next" link when
  JavaScript is off.
- Viewer images: thumbnails (320 px) and, for formats browsers cannot show (HEIC, TIFF), a
  2048 px JPEG preview, both cached outside the library. JPEG, PNG, GIF, WebP, BMP and TIFF
  are decoded in-process with the `image` crate; HEIC, AVIF and video poster frames via
  `ffmpeg`; RAW formats and everything without a decoder get a placeholder.
- Viewer video: no transcoding. Videos the browser cannot play show their codec (from
  `ffprobe`) and an "Open original" link.
- Image hashes: every image is decoded, in a separate scan pass after content hashing, in
  parallel. Pixel hash = hash of the decoded pixels before EXIF orientation, so a copy that only
  changed its rotation tag counts as "same pixels". Perceptual hash = `image_hasher` gradient,
  64 bits, of the upright image shrunk to 256 px; images with almost no detail (blank, black,
  one color) get none, since their hashes are all alike. Each image also stores a 32 x 32
  grayscale fingerprint. "Similar" = Hamming distance at most `--threshold`, aspect ratios
  within 3%, and matching fingerprints, found by comparing all pairs on all cores. The
  fingerprint check exists because the 64-bit hash alone cannot tell burst shots apart: on the
  owner's library, 12 of 15 pairs at distance 0 were different pictures. Calibrated
  (`examples/calibrate.rs`, 2,600 of the owner's photos plus synthetic copies): threshold 6
  bits (all copies within 4), fingerprint limit 6 at the 99th percentile pixel (keeps 99.6% of
  copies, rejects about 95% of burst pairs). Larger fingerprints (64 to 256 px) separated
  worse. Since burst pairs still outnumber real copies in such libraries, `similar` is opt-in
  (`--similar`) and documented as "expect burst shots". HEIC is decoded via
  `ffmpeg` 7.1 or later when available (older versions may decode one tile of a phone's HEIC
  grid); RAW is skipped with a note. `ffmpeg` applies the file's rotation itself, so for HEIC
  and AVIF the pixel hash is of the upright image.
- Hash versions: each image stores the version it was hashed with: the hash definitions, the
  `image` and `image_hasher` crates and its format's decoder crate (major.minor, from
  `Cargo.lock` via `build.rs`), plus `ffmpeg -version` for HEIC/AVIF. A scan rehashes images
  whose version differs, since another decoder can produce slightly different pixels. Read
  errors and timeouts are not recorded, so those images are tried again; only files the
  decoder rejects are recorded as undecodable.

## Invariants

- **The library is read-only** except during `pixrs apply` and `pixrs undo`. Nothing else may
  write, rename, delete or touch timestamps in the library. (Access times updated by the OS
  when files are read are unavoidable and accepted.) The catalog, thumbnails, plans, journals
  and reports live outside it.
- **Never delete user files.** Quarantine moves files; it does not delete them.
- **Every move is journaled before it happens**, so `pixrs apply` can be undone.
- **Cross-platform**: Windows, Linux and macOS. Use `Path`/`PathBuf` operations, not string
  slicing on `/`. Unicode file names must work; non-UTF-8 names must not crash the scan.
- No runtime dependencies beyond the binary itself, except optional `ffmpeg`/`ffprobe`.

## Workflow

```bash
cargo fmt --all
cargo clippy --all-targets -- -D warnings   # CI enforces this
cargo test
cargo run -- scan tests/files --catalog target/catalog.sqlite
cargo run -- serve --catalog target/catalog.sqlite  # serves at http://127.0.0.1:2635
```

`just pre-commit` runs lint, a clean build and the tests.

Test media under `tests/files` is stored in Git LFS. CI does not fetch LFS objects, and local
checkouts without `git-lfs` only have pointer files. Tests must pass either way. Tests that need
real decodable media should generate small fixtures in a temp dir at test time instead of relying
on the LFS files.

## Conventions

- Rust 2021 edition, `max_width = 99` (see `.rustfmt.toml`).
- New source files start with a module doc comment and the short license header from
  `src/structs.rs`. `src/lib.rs` and `src/main.rs` carry the full AGPL notice.
- `Cargo.toml` dependencies: major-version spec where possible, with the crate's license as a
  trailing comment aligned with the others. New dependencies must have AGPL-3.0-compatible
  licenses.
- Catalog schema changes bump `SCHEMA_VERSION` in `src/catalog.rs` and must migrate existing
  catalogs; users' catalogs hold hours of hashing. Until the first release that includes the
  catalog, schema v1 is edited in place: nobody has a catalog to migrate yet.
- Conventional Commits: `feat`, `fix`, `chore`, `chore(deps)`, `docs`, `ci`, `test`.
- Add user-visible changes to `[Unreleased]` in `CHANGELOG.md` (Keep a Changelog format).
- Keep the usage block in `README.md` identical to `pixrs --help` output.
- GitHub Actions are pinned to commit SHAs with the version as a trailing comment. Workflow
  changes must pass `zizmor --persona=auditor --strict-collection .github/` (run locally with
  `uvx zizmor ...`); CI enforces it.
- `CLAUDE.md` is a symlink to this file.
