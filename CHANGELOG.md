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
- `pixrs scan --allow-empty` removes the cataloged files of a library, or of the scanned folder
  inside one, that holds no media files any more. Without it a scan that finds no media where
  the catalog lists some is still refused, since an unmounted drive looks the same. The folder
  must exist and be readable, and the library stays in the catalog with no files.

### Changed

- `pixrs organize` names WhatsApp files by their date and WhatsApp's counter, which stands in for
  the time they lack: `IMG-20190501-WA0001.jpg` becomes `2019-05-01_WA0001.jpg` with the
  presets (`{date}_{time}.{ext}`), so they sort among the other files of the day instead of
  keeping WhatsApp's name. Templates that use `{hour}`, `{minute}` or `{second}` keep the name.
- Scanning a folder inside a scanned library scans that part of the library instead of adding
  a second library with the same files. Scanning a folder that contains scanned libraries folds
  them into it, keeping their hashes, instead of reading and hashing their files again.
- `pixrs scan` shows progress while it walks the library (media files found, current folder) and
  then how many files it found and how long that took; on a network share the walk alone can take
  hours. Each step is numbered (`[1/4] Walking` to `[4/4] Decoding`), and the progress bars show
  an ETA and the rate instead of the estimated total time. After each step a line says what it
  did and how long it took (`Hashed 286 files, 1.33 GiB (3 minutes)`).
- `pixrs scan` hashes up to four files at a time instead of one, which keeps an SSD or a network
  share busy; decoding was already parallel.
- `pixrs apply` and `pixrs undo` check each folder once a run instead of for every file moved,
  so a batch of files into the same folders asks the file system far fewer questions: 200 moves
  on a drive mounted in WSL took 1.5 s instead of 4 s, and on a network share, where every
  question is a round trip, the difference is larger.

### Fixed

- A file whose name has accents spelled another way than in the catalog (macOS may decompose
  `é` into `e` and a combining accent, where Linux and Windows compose it) no longer looks like
  a new file whose cataloged row is gone. `pixrs scan` matches names in Unicode NFC, keeps the
  row and its hashes, and gives it the name as it is on disk; `pixrs relocate` finds such files
  at the new place. `pixrs organize` and `pixrs dupes --quarantine` take names that differ only
  in normalization for the same name, as they do names that differ only in letter case.
- On an SMB share mounted in WSL, `pixrs apply` skipped the first file moved into each new
  folder ("cannot create its folder: Input/output error"), and would have stopped after a copy
  to another drive: the share answers every request to flush a folder to disk with an I/O
  error. That is now taken, like the "not supported" answers of other network file systems, as
  a file system that cannot flush folders, and the move goes on.
- Stopping `pixrs apply` or `pixrs undo` with Ctrl-C left `pixrs.lock` next to the catalog,
  so the next `apply`, `undo` or `relocate` refused to run until it was deleted by hand.
- An image file claiming absurd dimensions (a few hundred bytes can claim 100,000 x 100,000
  pixels) made `pixrs scan` and the viewer try to allocate the pixels and abort out of memory,
  and the next scan died on the same file. Images are now decoded in-process only up to 65,536
  pixels per side and 512 MiB of pixels (about 130 megapixels in color), from files up to
  512 MiB; anything beyond counts as undecodable and is left out of same-pixel and similarity
  matching.
- `ffmpeg`, which decodes HEIC, AVIF and video frames, had no limit on the frame size a file
  claims, and decoded on several threads, each holding frames of its own; a small crafted file
  could make it allocate gigabytes for each of the files decoded at once. It now decodes on one
  thread per file and refuses frames over about 134 megapixels, the same budget as in-process
  decoding; such files count as undecodable.
- `pixrs scan` progress bars no longer garble the terminal when a file path is longer than the
  line: the path is cut to fit. The "Found … media files" line no longer runs into the next bar
  when copied.
- File names were printed as they are, so a name holding terminal escape sequences (legal on
  every system pixrs runs on) could change the terminal's title or colors, clear the screen or
  print lines of its own during `pixrs scan`, in `pixrs dupes` and `pixrs organize` output and
  in `pixrs apply` and `pixrs undo` warnings. Control characters in names and in `ffmpeg`'s
  messages are now shown escaped, as in `\u{1b}`.
- `pixrs undo` removed or renamed any file a journal's temporary-file records named, so a
  crafted or damaged journal could delete files; it also created a catalog, or pixrs tables in
  any SQLite file, at the path the journal named, as `pixrs apply` did for a plan. Undo now only
  touches temporary files named and placed as pixrs makes them, only recreates pruned folders
  a moved file came from, only removes a copy at a target that also has the moved file's
  mtime, and both commands refuse a catalog path that does not hold a pixrs catalog.
- A move to another drive wrote its copy by the name of the temporary file it had just created,
  so someone else who can write to the target folder (a shared quarantine folder, say) could
  swap that name for a link and have `pixrs apply` overwrite one of your files with the moved
  photo; the thumbnail cache was written the same way. Copies and cached thumbnails are now
  written through the file pixrs created, and a temporary file whose name no longer leads to it
  is never put in place.
- `pixrs scan` no longer prints `nom_exif` errors in the middle of the progress bars (files it
  cannot read are already reported); set `RUST_LOG` to see dependencies' log output.
- A file that made the metadata parser panic ended `pixrs scan`, and every later scan of the
  library ended at the same file. The file is now cataloged without metadata, with a warning,
  and the scan goes on, as it already did when a decoder panicked.
- `pixrs apply` carried out whatever moves a plan file listed, so a crafted or edited plan
  could move any file of known size and mtime anywhere, and a sidecar entry pointing at a
  move that did not exist crashed it. A plan is now checked as a whole before anything
  happens: every move must start in one of the plan's libraries and end below the plan's
  target folder (the library, the `--to` folder or the quarantine folder), and must refer to
  earlier media moves and the plan's libraries only. The plan file format is now version 2
  (it records the target folder); plans written by 0.2.0 must be made again.
- `pixrs scan` stops when the library disappears (an unplugged drive, a dropped network share)
  while reading metadata or hashing, as it already did while decoding, instead of failing on
  every remaining file. What was scanned so far is kept.
- A symbolic link inside a library named like a target folder (`2023`, say, as an extracted
  archive can contain) made `pixrs organize` plan moves into it and `pixrs apply` carry them
  out, so the files landed wherever the link pointed, while the catalog said they were in the
  library. Files are no longer moved through links: `organize` and `dupes --quarantine` leave
  files whose target folder is a link where they are, with a warning, and `apply` and `undo`
  skip a move whose source or target folder is reached through a link.
- A file cataloged in two overlapping libraries (one scanned inside the other) was reported as
  a duplicate of itself, and `dupes --quarantine` planned to move it, leaving no copy in the
  library. `dupes` and the viewer now count each file on disk once.
- `pixrs dupes --format csv` wrote file names as they are, so a file named like a formula
  (`=HYPERLINK(...).jpg`) ran as one when the report was opened in a spreadsheet. A library or
  path that starts with `=`, `+`, `-`, `@`, a tab or a carriage return now gets a leading `'`,
  which spreadsheets read as text.
- On Linux, a library on a case-insensitive file system (a Windows drive under WSL, an SMB
  share, exFAT) scanned under another letter case (`/mnt/c/photos` after `/mnt/c/Photos`)
  became a second library with the same files, every file was reported as a duplicate of
  itself, and `dupes --quarantine` followed by `apply` moved the whole library into quarantine.
  A folder is now recognized as a cataloged library, or part of one, whatever its letter case,
  and a scan folds a library cataloged under another spelling into it; `relocate` and the
  commands that take library arguments accept any spelling too. Quarantine never moves a copy
  that is the same file as the kept one (one folder under two names, or a hard link).
- Log lines enabled with `RUST_LOG` were written into the progress bars of `pixrs scan` and
  `pixrs relocate`. The bar is now cleared for each line and drawn again below it. File names in
  the viewer's log lines are escaped like everywhere else.

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
