# PixRS

A command-line utility to present a bunch of media files in a reasonable manner.

[![Continuous Integration workflow status badge][badge-ci-svg]][badge-ci-href]
[![Security Audit workflow status badge][badge-audit-svg]][badge-audit-href]

## Usage

```text
Usage: pixrs [OPTIONS] [COMMAND]

Commands:
  apply     Carry out a plan from `organize` or `dupes --quarantine`
  dupes     Report duplicate and similar files in the catalog
  organize  Plan how to rename and move a library's files by a template
  scan      Scan a media library into the catalog
  serve     Browse the catalog in a local web viewer
  undo      Reverse an applied plan, from its journal
  help      Print this message or the help of the given subcommand(s)

Options:
  -l, --license  Display full license notice
  -h, --help     Print help
  -V, --version  Print version
```

### Scan a library into the catalog

```text
Usage: pixrs scan [OPTIONS] <LIBRARY>

Arguments:
  <LIBRARY>  The media library to scan

Options:
      --catalog <CATALOG>  The catalog file [default: catalog.sqlite in the OS
                           data directory]
  -q, --quiet              Disable progress bars
  -h, --help               Print help
```

The catalog keeps one row per media file: path, size, modification time and, where the file has
them, date taken, camera and dimensions. Only files that share their size with another cataloged
file can be exact duplicates, so only those are hashed (SHA-256); files with a unique size are
never read in full. Images are also decoded, in parallel, for a hash of their pixels and a
perceptual hash, which find copies that are not byte-identical. JPEG, PNG, GIF, WebP, BMP and TIFF
are decoded in-process; HEIC and AVIF need [FFmpeg][ffmpeg] 7.1 or later on `PATH`; RAW files are
skipped.
Rescanning only reads files that are new or whose size or modification time changed, and
decodes images again only if an update changed the decoders. The library itself is never
modified.

### Report duplicates

```text
Usage: pixrs dupes [OPTIONS] [LIBRARIES]...

Arguments:
  [LIBRARIES]...
          Only report duplicates of files in these libraries [default: all
          scanned libraries]

Options:
      --catalog <CATALOG>
          The catalog file [default: catalog.sqlite in the OS data directory]

      --format <FORMAT>
          Output format

          Possible values:
          - text: For people to read
          - json: One object with all groups and a summary
          - csv:  One row per file

          [default: text]

      --similar
          Also find similar images (resized or recompressed copies). Expect
          burst shots among them

      --threshold <THRESHOLD>
          How many of the 64 perceptual hash bits two images may differ in to
          count as similar (at most 16)

          [default: 6]

      --quarantine <DIR>
          Plan moving all but the kept copy of each exact and same-pixels group
          to this folder (similar groups are never quarantined). Nothing moves
          until `pixrs apply`

  -h, --help
          Print help (see a summary with '-h')
```

Duplicates are found in any scanned library, in three tiers:

- `exact`: the same size and SHA-256.
- `same-pixels`: images whose decoded pixels are identical, for example after a photo manager
  rewrote their metadata.
- `similar`, only with `--similar`: images with the same shape (aspect ratio) whose perceptual
  hashes differ in at most `--threshold` of 64 bits and whose small grayscale fingerprints
  match, for example resized or recompressed copies. Expect burst shots among them: photos
  taken a moment apart can look alike even to the fingerprint check, so look at similar groups
  before acting on them. Images with almost no detail, like black frames or blank scans, are
  never similar.

Files linked through other files form one group, which gets the weakest tier among them, and
each file shows how it matches the kept one. In each group the file with the earliest
modification time is marked `keep`; ties go to the first path in sort order. In `similar`
groups the largest image is kept instead (most pixels, then most bytes), since similar copies
are often scaled down. Naming libraries shows only groups with a file in them, but with all
copies: for example, `pixrs dupes /mnt/old-disk` lists what on the old disk also exists
elsewhere.

The report comes from the catalog as of each library's last scan, so rescan libraries that have
changed. It never touches the library.

### Browse the catalog

```text
Usage: pixrs serve [OPTIONS] [LIBRARIES]...

Arguments:
  [LIBRARIES]...  Only show files in these libraries [default: all scanned
                  libraries]

Options:
      --catalog <CATALOG>      The catalog file [default: catalog.sqlite in the
                               OS data directory]
      --cache <CACHE>          The thumbnail cache folder [default: thumbnails
                               in the OS cache directory]
      --similar                Also find similar images (resized or recompressed
                               copies). Expect burst shots among them
      --threshold <THRESHOLD>  How many of the 64 perceptual hash bits two
                               images may differ in to count as similar (at most
                               16) [default: 6]
      --port <PORT>            The port to listen on, on localhost only
                               [default: 2635]
  -o, --open                   Open the viewer in the default browser
  -h, --help                   Print help
```

Starts a viewer at `http://127.0.0.1:2635/`: browse by folder or by date (date taken, or the
modification time if that is unknown), view images, play audio and video, and compare duplicate
groups side by side with their tier and the copy to keep marked. It only reads from the
libraries and the catalog, so it can run while a scan is going on.

Thumbnails are cached outside the libraries. JPEG, PNG, GIF, WebP, BMP and TIFF are decoded
in-process. HEIC, AVIF and video thumbnails need [FFmpeg][ffmpeg] (HEIC tested with 7.1) on
`PATH`; without it, and for RAW files, the viewer shows placeholders. Images that browsers cannot
show (HEIC, TIFF) are shown as a large JPEG preview. Videos are not transcoded: if the browser
cannot play one, the viewer says so and links to the original.

The viewer only listens on localhost and only answers requests addressed to `localhost` or
`127.0.0.1`.

### Organize a library

```text
Usage: pixrs organize [OPTIONS] <LIBRARY>

Arguments:
  <LIBRARY>  The scanned library to organize

Options:
      --template <TEMPLATE>  Where files go, e.g.
                             "{type}/{year}/{month}/{date}_{time}.{ext}".
                             Tokens: {year} {month} {day} {hour} {minute}
                             {second} {date} {time} {type} {camera} {name} {ext}
      --layout <LAYOUT>      Preset folders, combined in order: flat, year,
                             year-month, type (e.g. "type,year-month"). Files
                             are named "{date}_{time}.{ext}"
      --to <DIR>             Put the files below this folder instead of the
                             library root
      --undated <NAME>       Folder name for files without a usable date, in
                             place of the date folders [default: undated]
      --suffix <SUFFIX>      Added to a file name that is taken. The number (2,
                             3 and so on) goes where the default has it
                             [default: _{n}]
      --trust-dates          Use dates before 1980 or in the future as they are
                             (e.g. scans of old photos), instead of treating
                             them as missing
      --prune-empty          Remove folders the moves leave empty
      --catalog <CATALOG>    The catalog file [default: catalog.sqlite in the OS
                             data directory]
  -h, --help                 Print help
```

Plans where each file of a scanned library goes, from one template for folders and file names,
for example `{type}/{year}/{month}/{date}_{time}.{ext}` puts a video taken on 1 May 2023 at
14:32:05 in `video/2023/05/2023-05-01_143205.mp4`. `{date}` is `2023-05-01`, `{time}` is
`143205`, `{camera}` is the camera make and model and `{name}` the original name without
extension.

- The date is the date taken from the file's metadata, or else a date in its name, as phones
  and apps write them (`IMG_20190501_143201`, `2017-10-08 18.15.36`,
  `IMG-20190501-WA0001`). Dates before 1980 or in the future are taken as broken metadata
  unless `--trust-dates` is given.
- A file without a date keeps its name and goes in an `undated` folder in place of the date
  folders, e.g. `video/undated/DSC0047.mp4`. A file with a date but no time gets the date
  folders and keeps its name.
- Nothing is ever overwritten: a file whose new name is taken gets `_2`, `_3` and so on, with a
  warning. Names are compared without regard to letter case on every system.
- Sidecar files with the same name (`.xmp`, `.aae`, `.thm`, `.srt`, Google Takeout `.json`)
  move with their file.

`organize` moves nothing. It prints a summary and writes the plan to a file next to the
catalog; `pixrs apply` carries it out.

### Quarantine duplicates

`pixrs dupes --quarantine <DIR>` plans moving every copy but the kept one of each `exact` and
`same-pixels` group to `<DIR>/<library name>/<path in the library>`, keeping the folder
structure. Similar groups are never quarantined. The folder must be outside every scanned
library. Like `organize`, it only writes a plan.

### Apply and undo a plan

```text
Usage: pixrs apply <PLAN>

Arguments:
  <PLAN>  The plan file `organize` or `dupes --quarantine` wrote

Options:
  -h, --help  Print help
```

```text
Usage: pixrs undo <JOURNAL>

Arguments:
  <JOURNAL>  The journal file `apply` wrote

Options:
  -h, --help  Print help
```

`apply` checks each file before moving it: a file that changed since the plan was made, or
whose new place is taken, is skipped. Every step is written to a journal next to the catalog
before it is taken, and the catalog is updated as files move. Moves to another drive copy the
file, check the copy, and only then remove the original.

`undo` reverses an applied plan, including one that was interrupted, and restores the
catalog. It looks at where each file actually is, so it is safe to run again.

## Building

Install Rust with [rustup]. You also need a C compiler, which some dependencies (SQLite, `ring`)
use: the Visual Studio C++ build tools on Windows (the rustup installer offers them), the Xcode
command line tools on macOS, or `gcc`/`clang` on Linux.

```bash
cargo build
cargo test
cargo run -- scan tests/files --catalog target/catalog.sqlite
cargo run -- serve --catalog target/catalog.sqlite --open
```

The test media files are stored with [Git LFS][git-lfs]. Without it they are checked out as small
pointer files; the tests still pass, but the viewer shows placeholders for them. Install Git LFS
and run `git lfs pull` to fetch the real files.

[badge-audit-href]: https://github.com/kthy/pixrs/actions/workflows/audit.yml
[badge-audit-svg]: https://github.com/kthy/pixrs/actions/workflows/audit.yml/badge.svg
[badge-ci-href]: https://github.com/kthy/pixrs/actions/workflows/ci.yml
[badge-ci-svg]: https://github.com/kthy/pixrs/actions/workflows/ci.yml/badge.svg
[ffmpeg]: https://ffmpeg.org/
[git-lfs]: https://git-lfs.com/
[rustup]: https://rustup.rs/
