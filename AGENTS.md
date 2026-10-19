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
  folder in place of the date folders, e.g. `video/undated/DSC0047.mp4`.
- Duplicates: report only by default. A flag enables quarantine, which keeps the oldest file and
  moves the others to a quarantine folder. "Oldest" means earliest mtime, with path order as
  the tie-breaker.
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

## Invariants

- **The library is read-only** except during `organize --apply`. Nothing else may write,
  rename, delete or touch timestamps in the library. (Access times updated by the OS when files
  are read are unavoidable and accepted.) The catalog, thumbnails and reports live outside it.
- **Never delete user files.** Quarantine moves files; it does not delete them.
- **Every move is journaled before it happens**, so `--apply` can be undone.
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
  catalogs; users' catalogs hold hours of hashing.
- Conventional Commits: `feat`, `fix`, `chore`, `chore(deps)`, `docs`, `ci`, `test`.
- Add user-visible changes to `[Unreleased]` in `CHANGELOG.md` (Keep a Changelog format).
- Keep the usage block in `README.md` identical to `pixrs --help` output.
- GitHub Actions are pinned to commit SHAs with the version as a trailing comment. Workflow
  changes must pass `zizmor --persona=auditor --strict-collection .github/` (run locally with
  `uvx zizmor ...`); CI enforces it.
- `CLAUDE.md` is a symlink to this file.
