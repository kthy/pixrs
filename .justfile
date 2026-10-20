set dotenv-load
set fallback

set windows-shell := ["pwsh.exe", "-NoLogo", "-Command"]

# Print this list
@default:
  just --list

# Run a full, clean build
build:
  cargo clean && cargo build --frozen

# Lint
lint:
  cargo fmt --check
  cargo check --frozen
  cargo clippy --no-deps

# Regenerate THIRD_PARTY_NOTICES.txt (needs `cargo install --locked cargo-about --features cli`)
notices:
  cargo about generate --frozen --fail -o THIRD_PARTY_NOTICES.txt about.hbs

# Run pre-commit checks
pre-commit: lint build test

# Run tests
test:
  cargo test

# Run tests and collect coverage info
test-coverage:
  cargo llvm-cov
