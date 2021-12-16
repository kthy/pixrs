# PixRS

A command-line utility to display a bunch of photographs in a reasonable manner.

## Usage

```text
pixrs.exe [FLAGS] [OPTIONS] [base-path]

FLAGS:
    -c, --clobber    Clobber output file if it exists
    -d, --debug      Activate debug mode
    -h, --help       Prints help information
    -r, --recurse    Recurse into subdirs below the base path
    -V, --version    Prints version information

OPTIONS:
    -o, --output <out-path>    The path where the output file is placed [default: .]

ARGS:
    <base-path>    The base path to search [default: .]
```
