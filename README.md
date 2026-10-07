# PixRS

A command-line utility to present a bunch of media files in a reasonable manner.

[![Continuous Integration workflow status badge][badge-ci-svg]][badge-ci-href]
[![Security Audit workflow status badge][badge-audit-svg]][badge-audit-href]

## Usage

```text
Usage: pixrs [OPTIONS] [BASE_PATH] [OUT_PATH]

Arguments:
  [BASE_PATH]  The base path to search [default: .]
  [OUT_PATH]   The path where the output is placed [default: .]

Options:
  -c, --clobber     Clobber output file if it exists
  -d, --create-dir  Create output directory if it does not exist
  -l, --license     Display full license notice
  -n, --no-server   Do not start local server when finished (conflicts with
                    --open)
  -o, --open        Open output in default browser when finished (conflicts with
                    --no-server)
  -q, --quiet       Disable progress bars
  -r, --recurse     Recurse into subdirs below the base path
  -h, --help        Print help
  -V, --version     Print version
```

## Building on Windows

### Prerequisites

#### Rust

Download and run the [`rustup-init.exe`][rustup] installer.

#### LLVM

Download and run the [`LLVM-x.y.z-win64.exe`][llvm] installer. Add LLVM to the system PATH for all
users during installation.

#### FFmpeg

Download the newest [`ffmpeg-release-full-shared.7z`][ffmpeg-win] from gyan.dev, unpack it and
set the `FFMPEG_DIR` environment variable to the directory containing `include` and `lib`.

### Build, Test and Run

```bash
cargo build
cargo test
cargo run -- -cdor .\tests\files\ target/output
```

...

[badge-audit-href]: https://github.com/kthy/pixrs/actions/workflows/audit.yml
[badge-audit-svg]: https://github.com/kthy/pixrs/actions/workflows/audit.yml/badge.svg
[badge-ci-href]: https://github.com/kthy/pixrs/actions/workflows/ci.yml
[badge-ci-svg]: https://github.com/kthy/pixrs/actions/workflows/ci.yml/badge.svg
[ffmpeg-win]: https://www.gyan.dev/ffmpeg/builds/ffmpeg-release-full-shared.7z
[llvm]: https://github.com/llvm/llvm-project/releases
[rustup]: https://static.rust-lang.org/rustup/dist/x86_64-pc-windows-msvc/rustup-init.exe
