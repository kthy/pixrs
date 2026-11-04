set dotenv-load
set fallback

set windows-shell := ["pwsh.exe", "-NoLogo", "-Command"]

# Print this list
@default:
  just --list

# Run a full, clean build
build:
  cargo clean && cargo build --locked

# Lint
lint:
  cargo fmt --check
  cargo check --locked
  cargo clippy --no-deps

# Regenerate THIRD_PARTY_NOTICES.txt (needs `cargo install --locked cargo-about --features cli`)
notices:
  cargo about generate --locked --fail -o THIRD_PARTY_NOTICES.txt about.hbs

# Print the section of CHANGELOG.md for VERSION (e.g. 0.3.0), the notes of its release; fails if
# there is none
release-notes version:
  #!/usr/bin/env bash
  set -euo pipefail
  notes=$(awk -v heading="## [{{ version }}]" '
    /^## \[/ { if (found) exit; if (index($0, heading) == 1) { found = 1; next } }
    /^\[[^]]+\]: / { if (found) exit }
    found { print }
  ' CHANGELOG.md | sed -e '/./,$!d')
  if [ -z "$notes" ]; then
    echo "CHANGELOG.md has no section for {{ version }}" >&2
    exit 1
  fi
  printf '%s\n' "$notes"

# Run pre-commit checks
pre-commit: lint build test

# Run tests
test:
  cargo test

# Run tests and collect coverage info
test-coverage:
  cargo llvm-cov
