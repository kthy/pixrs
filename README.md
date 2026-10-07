# PixRS

A command-line utility to present a bunch of media files in a reasonable manner.

[![Continuous Integration workflow status badge][badge-ci-svg]][badge-ci-href]
[![Security Audit workflow status badge][badge-audit-svg]][badge-audit-href]

## Usage

```text
Usage: pixrs [OPTIONS] [COMMAND]

Commands:
  dupes    Report exact duplicates in the catalog
  gallery  Generate a static HTML gallery and serve it
  scan     Scan a media library into the catalog
  help     Print this message or the help of the given subcommand(s)

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
never read in full. Rescanning only reads files that are new or whose size or modification time
changed. The library itself is never modified.

### Report exact duplicates

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

  -h, --help
          Print help (see a summary with '-h')
```

Duplicates are files with the same size and SHA-256, in any scanned library. In each group the
file with the earliest modification time is marked `keep`; ties go to the first path in sort
order. Naming libraries shows only groups with a file in them, but with all copies: for example,
`pixrs dupes /mnt/old-disk` lists what on the old disk also exists elsewhere.

The report comes from the catalog as of each library's last scan, so rescan libraries that have
changed. It never touches the library.

### Generate a static gallery

```text
Usage: pixrs gallery [OPTIONS] [BASE_PATH] [OUT_PATH]

Arguments:
  [BASE_PATH]  The base path to search [default: .]
  [OUT_PATH]   The path where the output is placed [default: .]

Options:
  -c, --clobber     Clobber output file if it exists
  -d, --create-dir  Create output directory if it does not exist
  -n, --no-server   Do not start local server when finished (conflicts with
                    --open)
  -o, --open        Open output in default browser when finished (conflicts with
                    --no-server)
  -q, --quiet       Disable progress bars
  -r, --recurse     Recurse into subdirs below the base path
  -h, --help        Print help
```

## Building

Install Rust with [rustup]. You also need a C compiler, which some dependencies (SQLite, `ring`)
use: the Visual Studio C++ build tools on Windows (the rustup installer offers them), the Xcode
command line tools on macOS, or `gcc`/`clang` on Linux.

```bash
cargo build
cargo test
cargo run -- gallery -cdor tests/files target/output
```

The test media files are stored with [Git LFS][git-lfs]. Without it they are checked out as small
pointer files; the tests still pass, but the generated gallery shows broken media. Install Git LFS
and run `git lfs pull` to fetch the real files.

[badge-audit-href]: https://github.com/kthy/pixrs/actions/workflows/audit.yml
[badge-audit-svg]: https://github.com/kthy/pixrs/actions/workflows/audit.yml/badge.svg
[badge-ci-href]: https://github.com/kthy/pixrs/actions/workflows/ci.yml
[badge-ci-svg]: https://github.com/kthy/pixrs/actions/workflows/ci.yml/badge.svg
[git-lfs]: https://git-lfs.com/
[rustup]: https://rustup.rs/
