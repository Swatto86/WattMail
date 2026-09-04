#!/usr/bin/env bash
# Fast iteration gate — types/fmt only when scoped; clippy when full workspace.
# Never --release, never tauri packaging, never cargo clean.
set -euo pipefail
cd "$(dirname "$0")/.."
export PATH="$HOME/.cargo/bin:$PATH"

pkg=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    -p|--package)
      pkg="${2:?missing package name after $1}"
      shift 2
      ;;
    -h|--help)
      echo "Usage: $0 [-p <crate>]"
      echo "  (no args)  fmt check + clippy workspace"
      echo "  -p <crate> cargo check --locked -p <crate> --all-targets only"
      exit 0
      ;;
    *)
      echo "unknown arg: $1" >&2
      exit 2
      ;;
  esac
done

if [[ -n "$pkg" ]]; then
  cargo check --locked -p "$pkg" --all-targets
  exit 0
fi

cargo fmt --all -- --check
# Frontend typecheck only — not a production vite build (tauri dev uses HMR).
npx tsc --noEmit
cargo clippy --locked --workspace --all-targets -- -D warnings
