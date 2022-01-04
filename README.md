# PixRS

A command-line utility to present a bunch of media files in a reasonable manner.

[![Continuous Integration workflow status badge][badge-ci-svg]][badge-ci-href]
[![Security Audit workflow status badge][badge-audit-svg]][badge-audit-href]

## Usage

```text
USAGE:
    pixrs.exe [FLAGS] <base-path> <out-path>

FLAGS:
    -c, --clobber       Clobber output file if it exists
    -d, --create-dir    Create output directory if it does not exist
    -h, --help          Prints help information
    -n, --no-server     Do not start local server when finished (forbidden if --open is set)
    -o, --open          Open output in default browser when finished (forbidden if --no-server is set)
    -q, --quiet         Disable progress bars
    -r, --recurse       Recurse into subdirs below the base path
    -V, --version       Prints version information

ARGS:
    <base-path>    The base path to search
    <out-path>     The path where the output is placed
```

[badge-audit-href]: https://github.com/pyxy-dk/pixrs/actions/workflows/audit.yml
[badge-audit-svg]: https://github.com/pyxy-dk/pixrs/actions/workflows/audit.yml/badge.svg
[badge-ci-href]: https://github.com/pyxy-dk/pixrs/actions/workflows/ci.yml
[badge-ci-svg]: https://github.com/pyxy-dk/pixrs/actions/workflows/ci.yml/badge.svg
