#!/usr/bin/env bash
# Checks the crates that are meant to run anywhere, wasm included:
# brix-syntax, brix-store (without its `std-fs` accessor), brix-eval, and
# brix-flake without its `native` fetchers. They must build for
# wasm32-unknown-unknown and must not call what panics there: Clippy runs
# with scripts/wasm/clippy.toml, which disallows the clock, threads and
# environment reads (brix-eval's `platform` module is the one place that
# deals with the first two).
#
# Run it inside `nix develop`: nixpkgs' rustc ships the standard library
# for wasm32-unknown-unknown.
set -euo pipefail
repo=$(cd "$(dirname "$0")/.." && pwd)
cd "$repo"
CLIPPY_CONF_DIR="$repo/scripts/wasm" exec cargo clippy \
  -p brix-syntax -p brix-store -p brix-eval -p brix-flake \
  --target wasm32-unknown-unknown --no-default-features \
  -- -D warnings
