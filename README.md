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

## Building

Install Rust with [rustup]. There are no other build dependencies.

```bash
cargo build
cargo test
cargo run -- -cdor tests/files target/output
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
