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
  then hash only files whose size matches another cataloged file, on up to 4 threads
  (`MAX_HASHERS`: a few reads at once hide a network share's round trips without making a
  spinning disk seek much), with one thread writing the catalog, as the decoding pass does
  (`in_parallel` in `src/scan.rs`). Incremental: only reread files
  whose path, size or mtime changed. A metadata parser that panics on a file is caught, as the
  decoders are: the file is cataloged without metadata, with a warning, and the scan goes on.
  Libraries never overlap: scanning a folder inside a
  library scans that part of the library, and scanning a folder that contains libraries folds
  them into it (rows move with their hashes; where both list a file with the same size and
  mtime, the outer row takes the hashes it lacks). `dupes` and the viewer keep one row per file
  on disk, for catalogs from 0.2.0 that already have overlapping libraries. A folder is matched
  to the stored roots by spelling and then by file identity (`same_file`), since on Linux
  `canonicalize` keeps the letter case the user typed on a case-insensitive file system (WSL's
  `/mnt/c`, SMB, exFAT): `/mnt/c/photos` is scanned as the library `/mnt/c/Photos`, and a
  library cataloged under another spelling is folded in like a nested one (`path_below`,
  `cataloged_spelling` in `src/catalog.rs`). Quarantine never moves a copy that is the same
  file as its keeper (one folder under two names, or a hard link). A moved library
  is pointed at its new place with `pixrs relocate OLD NEW` (#50): OLD is matched against the
  stored roots (it need not exist), up to 100 files spread over the library must be at NEW with
  the same size and mtime (at least 90%, unless `--force`), and NEW follows the overlap rules
  above (merged into a library containing it, folding libraries inside it). It takes the
  `apply`/`undo` lock. A scan whose walk finds no media where the catalog lists some (for the
  library, or the scanned part of it) is refused, since an unmounted drive often leaves an empty
  mount point; `--allow-empty` (#49) removes those rows instead. The folder must still exist and
  be listable. The library's entry stays, with no files, as for a library first scanned empty;
  entries only go when folded into another library. Paths are stored as the file system spells
  them, but file names are matched to rows in Unicode NFC (#51; `src/names.rs`): macOS may
  decompose accents (NFD) where Linux and Windows compose them, so a library copied or read
  across systems would otherwise look new. A scan looks a walked name up as spelled, then in NFC (in memory, no schema change:
  the rows not spelled in NFC are collected once per scan, usually none); a row found so keeps
  its hashes and takes the name on disk, unless its own spelling is another file on disk (Linux
  keeps both forms apart) or another file already matched it. `relocate` compares a sampled file
  it does not find as spelled under a name in its folder that is the same in NFC, and matches
  OLD to the roots in NFC after spelling. Library roots are matched by identity as before, with
  `on_disk_name` ignoring normalization too. Rows that `fold_nested` merges are still compared
  as spelled; the next scan respells them.
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
  fills the date folders, and the file keeps its original name when the template needs a time,
  except a WhatsApp name: its counter stands in for `{time}` (`IMG-20190501-WA0001.jpg` becomes
  `2019-05-01_WA0001.jpg` with the presets), so the day's WhatsApp files sort among the other
  files of that day; a name pixrs gave this way keeps its counter on the next run. A counter
  cannot fill `{hour}`, `{minute}` or `{second}`: there the name stays. Dates before 1980 or
  more than a day in the future count as missing (broken metadata, e.g. QuickTime's 1904
  epoch); `--trust-dates` turns that check off, for scans of old photos named with their real
  date. The viewer shows such dates with a `?` and a tooltip. A video or audio file's QuickTime
  header time is read as `ffmpeg` reads it (`dates::quicktime_creation`): some encoders write
  Unix time there, which counted from 1904 comes out before 1970, so a value below the 1904 to
  1970 offset is taken as Unix time, and 0 as no date. `nom-exif` cannot tell these from
  QuickTime's `creationdate` key, so the rule applies to track dates before 1970 in UTC (phones
  write the key with their local offset).
- Duplicates: report only by default. A flag enables quarantine, which keeps the oldest file and
  moves the others to a quarantine folder. "Oldest" means earliest mtime, with path order as the
  tie-breaker. Exception: in `similar` groups the largest file is kept (most pixels, then most
  bytes, then oldest), since similar copies are often scaled down; then the oldest of its exact and
  same-pixel copies replaces it (a copy with rewritten metadata can be larger). Quarantine only
  moves files that match their keeper `exact` or `same-pixels`, never `similar` ones (similar
  images are mostly burst shots, see "Image hashes"); exact copies in a `similar` group still move.
- Organize commands: `pixrs organize` and `pixrs dupes --quarantine <DIR>` only write a plan
  file (and print a summary); `pixrs apply <PLAN>` carries out either kind of plan and
  `pixrs undo <JOURNAL>` reverses it. An interrupted `apply` is not resumed: undo it and plan
  again. Plans and journals live next to the catalog (`plans/`, `journals/`). A plan file is
  input like any other file: `Plan::check` (run by `read_plan` and `apply`, before the lock is
  taken) refuses a plan unless every source is inside one of the plan's libraries, every target
  is below the plan's `destination` (the library root, the `--to` folder or the quarantine
  folder, which must not overlap a library), sidecar moves point at earlier media moves, and
  the library ids and kept copies are the plan's. Plan format version 2 added `destination`.
- Organize details: moves never overwrite (collisions get the suffix; compared case-insensitively
  and in Unicode NFC on every OS (`names::folded`), so plans are the same everywhere; a rename
  that only changes letter case or normalization is refused if another file has exactly the new
  name, and goes through a temporary name like a case-only one). Target folders that differ from
  an existing folder only in letter case or normalization are that folder, so folder case is
  never changed. Token values are sanitized
  to names valid on Windows. Sidecar files (`.xmp`, `.aae`, `.thm`, `.srt`, Google Takeout `.json`)
  with the same base name in the same folder move and are renamed with their media file, unless
  another media file with that base name stays in the folder. Files are never moved through
  symbolic links, which could lead anywhere (the scan does not follow links, so a link inside a
  library is not cataloged and nothing else sees it): a plan puts no file into a folder that is a
  link, in any letter case, and warns; `apply` and `undo` skip a move whose source or target folder
  is reached through a link (`through_symlink`: `apply` checks that the plan's library root and
  `destination` are canonical and that the parts below them are not links; `undo` has no plan, so
  there the target's nearest existing folder must be canonical, as resolved roots are). Each folder
  is checked once a run (`KnownFolders`), since on a network share every check is a round trip;
  a folder swapped for a link during the run goes unnoticed, the same race as #44. Folders left
  empty stay unless
  `--prune-empty` is given. Each move checks that the file still matches the plan (size, mtime) and
  that the target is free, else skips it; a move that fails before anything changed is skipped too,
  and a folder that cannot be read counts as taken, and only a half-done move stops `apply`. `undo`
  ignores skipped moves, only removes a file at a target if it is a copy pixrs journaled and
  verified, and only moves catalog rows that still point at the target. Temporary files are
  journaled with their kind: a stop on the way of a case-only rename is the file itself and is only
  ever put back (next to its old place if that is taken); a copy is only removed while its source
  still holds every byte of it, otherwise it is kept as `name.pixrs-recovered.ext`. A journal is
  input like any other file: `undo` only acts on temporary files named (`.pixrs-<pid>-<ns>-<n>.tmp`)
  and placed (the target's or source's folder, copied from one of the two) as pixrs makes them,
  only recreates pruned folders a moved file came from, and like `apply` never creates a catalog
  or touches a SQLite file without the pixrs schema. A source must
  still match the plan right before it is removed after a cross-filesystem copy. Cross-filesystem
  moves copy to a temporary name, verify the SHA-256, keep the mtime, rename, then remove the
  source. Temporary files (here and in the thumbnail cache) are created exclusively by
  `reserve_temp` and written, read back and given their permissions through the open file, never
  by name; one is only renamed into place while its name still leads to that file
  (`still_ours`), since anyone who can write to the folder can swap the name for a link. The
  catalog is updated as files move. The quarantine folder must be outside every
  cataloged library and contain none; files go to `<DIR>/<library name>/<path in library>`. With
  library arguments, only copies in those libraries are quarantined, and each move requires the
  group's kept copy to still be there unchanged at `apply`. Accepted risk: each move checks that
  its target is free and then renames, and OS renames replace, so a file another program creates at
  exactly that name in the microseconds between would be overwritten. Closing it needs OS-specific
  no-replace renames; the owner decided not to (#44).
- Media probing: call the `ffmpeg`/`ffprobe`/`fpcalc` binaries if they are on `PATH`, and degrade
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
  and AVIF the pixel hash is of the upright image. In-process decoding is capped at 65,536
  pixels per side, 512 MiB of decoded pixels and 512 MiB of file (`src/decode.rs`); a file
  beyond that is undecodable. A file can claim any size in a few bytes, and `image`'s
  `into_decoder` does not apply its memory limit to the pixel buffer, so pixrs reserves it.
  Every `ffmpeg` run starts with `ffmpeg_input_options` (`src/decode.rs`): one decoding thread
  (each thread holds frames of its own) and `-max_pixels` at the same budget, 512 MiB at 4 bytes
  a pixel, about 134 megapixels per frame. Phones store larger HEIC and AVIF photos as grids of
  small tiles, which stay below it.
- Video and audio similarity: a video's signature is 9 frames at 5%, 15%, ... 85% of its duration
  (`ffmpeg -ss`, duration from `ffprobe`), each with the image perceptual hash; frames shift with
  re-encoding when keyframes are used, fixed fractions do not. The fractions are of the video
  stream's own duration and start, not the container's (phones record a little more audio than
  video, and a copy without audio would otherwise be sampled at other moments). Matroska keeps the
  stream duration only in a `DURATION` tag. `-ss` counts from the container's start, so the
  stream's start is taken relative to it (MTS files start far from 0). Videos are similar when
  durations are within 1 s or 1%, aspect ratios within 3%, and at least 7 of 9 frame pairs have
  hashes within `--threshold` (frames without detail do not count; at least 5 must have a hash).
  Frames are compared by hash alone, not also by fingerprint: re-encoders convert variable frame
  rates to constant ones, which moves frames by milliseconds. Calibrated on 38 of the owner's
  videos (each re-encoded three ways): 35 to 36 of 38 copies matched, and none of the 703 pairs of
  different videos. Audio uses Chromaprint via the `fpcalc` binary (never linked), first 120 s:
  similar when durations are within 2 s and the bit error rate at the best offset within 2 s is at
  most 0.15 (re-encodes to MP3, AAC, Opus and MP2 measured 0.002 to 0.042; unrelated audio is
  around 0.5). Clips under about 5 s give too short a fingerprint and never match (under about 3 s
  `fpcalc` gives none; it is stored empty, not as undecodable). Fingerprints with fewer than 10%
  distinct values (silence, hum, steady tones all look alike) are never compared. Both reuse the
  `similar` tier (the media type tells them apart) and are always on, since calibration showed no
  false matches; similar images stay behind `--similar` (burst shots), and so do videos under 4 s
  (Live Photo clips shot moments apart were not in the calibration sample). `--threshold` applies
  to images and video frames. Both are skipped with a note when `ffmpeg`/`ffprobe` or `fpcalc` are
  not on `PATH`.
- Hash versions: each file stores the version it was hashed with. Images: the hash
  definitions, the `image` and `image_hasher` crates and its format's decoder crate (major.minor,
  from `Cargo.lock` via `build.rs`), plus `ffmpeg -version` for HEIC/AVIF. Videos: the signature
  and image hash definitions, the crates that hash frames and `ffmpeg`'s version. Audio: the
  signature definitions and `fpcalc`'s version. A scan rehashes files whose version differs,
  since another decoder can produce slightly different pixels. Read errors and timeouts are not
  recorded, so those files are tried again; only files the decoder rejects are recorded as
  undecodable. A video frame `ffmpeg` cannot decode counts as one without detail.

## Invariants

- **The library is read-only** except during `pixrs apply` and `pixrs undo`. Nothing else may
  write, rename, delete or touch timestamps in the library. (Access times updated by the OS
  when files are read are unavoidable and accepted.) The catalog, thumbnails, plans, journals
  and reports live outside it.
- **Never delete user files.** Quarantine moves files; it does not delete them.
- **Every move is journaled before it happens**, so `pixrs apply` can be undone.
- **Cross-platform**: Windows, Linux and macOS. Use `Path`/`PathBuf` operations, not string
  slicing on `/`. Unicode file names must work; non-UTF-8 names must not crash the scan.
- No runtime dependencies beyond the binary itself, except optional `ffmpeg`/`ffprobe`/`fpcalc`.

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
- Print file names, paths from the catalog and messages of external tools through
  `text::shown` or `text::printable` (`printable_lines` for errors of several lines), in
  warnings, reports and progress messages alike: a file name can hold control characters, which
  a terminal acts on.
- `Cargo.toml` dependencies: major-version spec where possible, with the crate's license as a
  trailing comment aligned with the others. New dependencies must have AGPL-3.0-compatible
  licenses (listed in `about.toml`). After changing dependencies, run `just notices` to
  regenerate `THIRD_PARTY_NOTICES.txt`; CI fails if it is out of date.
- Catalog schema changes bump `SCHEMA_VERSION` in `src/catalog.rs` and must migrate existing
  catalogs (`MIGRATIONS`, run when any `Catalog::open*` finds an older version); users'
  catalogs hold hours of hashing. Schema v1 shipped in 0.2.0. Likewise, a
  change to the plan or journal format bumps `PLAN_VERSION` in `src/plan.rs`, and `pixrs undo`
  must still read journals written by released versions.
- Conventional Commits: `feat`, `fix`, `chore`, `chore(deps)`, `docs`, `ci`, `test`.
- Add user-visible changes to `[Unreleased]` in `CHANGELOG.md` (Keep a Changelog format).
- Keep the usage block in `README.md` identical to `pixrs --help` output.
- GitHub Actions are pinned to commit SHAs with the version as a trailing comment. Workflow
  changes must pass `zizmor --persona=auditor --strict-collection .github/` (run locally with
  `uvx zizmor ...`); CI enforces it.
- `CLAUDE.md` is a symlink to this file.
