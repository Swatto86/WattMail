#!/usr/bin/env bash
# Project verify gate — run before declaring work done. The global stop-gate
# hook prefers this over its generic cargo fallback.
#
# PATH note: this machine carries a standalone GNU-target Rust install in
# Program Files ahead of rustup's shims, and its mingw linker cannot link the
# app's cdylib ("export ordinal too large" — >64k exports overflow PE's 16-bit
# export ordinals). Prepend rustup's shims so the pinned MSVC toolchain
# (rust-toolchain.toml) is used, matching CI.
set -euo pipefail
cd "$(dirname "$0")/.."
export PATH="$HOME/.cargo/bin:$PATH"

npm run build
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --workspace
