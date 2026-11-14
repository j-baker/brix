#!/usr/bin/env bash
# Runs every test that can run unattended: the default suite (`cargo test
# --workspace --no-fail-fast`), then each `#[ignore]`d test gated on an
# opt-in variable, with that variable set to 1. Gated tests say so in
# their ignore reason (`#[ignore = "…; set BRIX_TEST_REAL_DAEMON=1"]`) and
# call brix_testutil::require_env:
#
#   BRIX_TEST_REAL_DAEMON  talks to the running Nix daemon (builds, store
#                          reads and writes)
#   BRIX_TEST_REAL_STORE   has the reference nix write into the user's store
#   BRIX_TEST_NETWORK      downloads from GitHub (only with --network)
#
# Usage: scripts/test-all.sh [--network] [--release] [--gated-only]
#
# Run it inside `nix develop` (the tests need BRIX_NIX_SRC, BRIX_NIX_BIN
# and BRIX_NIXPKGS) on a machine with a Nix daemon and a writable store.
# With --gated-only the default suite is skipped and only the gated tests
# run: the flake check `gated-tests` (nix/tests/gated.nix) uses it, with
# a daemon of its own, next to the `tests` check that runs the default
# suite. The other ignored tests are slow ones (whole-nixpkgs runs, the
# fuzz long run), run by hand with `cargo test -p <crate> --test <file>
# -- --ignored <name>`.
set -euo pipefail

network=0
gated_only=0
profile=()
while [ $# -gt 0 ]; do
  case "$1" in
    --network) network=1; shift ;;
    --release) profile=(--release); shift ;;
    --gated-only) gated_only=1; shift ;;
    -h | --help) sed -n '2,22p' "$0"; exit 0 ;;
    *) echo "error: unknown argument '$1' (see --help)" >&2; exit 1 ;;
  esac
done

repo=$(cd "$(dirname "$0")/.." && pwd)
cd "$repo"
for var in BRIX_NIX_SRC BRIX_NIX_BIN BRIX_NIXPKGS; do
  if [ -z "${!var:-}" ]; then
    echo "error: $var is not set; run inside 'nix develop'" >&2
    exit 1
  fi
done

variables=(BRIX_TEST_REAL_DAEMON BRIX_TEST_REAL_STORE)
[ "$network" = 1 ] && variables+=(BRIX_TEST_NETWORK)

log() { echo "test-all: $*" >&2; }
failed=()

if [ "$gated_only" = 0 ]; then
  log "cargo test --workspace --no-fail-fast ${profile[*]}"
  cargo test --workspace --no-fail-fast "${profile[@]}" || failed+=("cargo test --workspace")
fi

# The gated tests, one line per test: `<crate> <test file> <variable> <name>`.
# An ignore reason that names a BRIX_TEST_ variable in any other form than
# `set BRIX_TEST_…=1` is an error, so that no gated test is skipped here
# by accident.
gated=$(
  for file in crates/*/tests/*.rs; do
    crate=$(basename "$(dirname "$(dirname "$file")")")
    awk -v crate="$crate" -v test="$(basename "$file" .rs)" -v file="$file" '
      /#\[ignore = ".*BRIX_TEST_/ {
        if (match($0, /set BRIX_TEST_[A-Z_]+=1/)) {
          var = substr($0, RSTART + 4, RLENGTH - 6)
        } else {
          printf "%s:%d: an ignore reason must name its variable as `set BRIX_TEST_…=1`\n", file, NR > "/dev/stderr"
          bad = 1
        }
        next
      }
      var != "" && /^ *(pub )?fn [a-z0-9_]+/ {
        name = $0
        sub(/^ *(pub )?fn /, "", name)
        sub(/[^a-z0-9_].*/, "", name)
        print crate, test, var, name
        var = ""
      }
      END { exit bad }
    ' "$file"
  done
)
if [ -z "$gated" ]; then
  echo "error: found no gated tests; has the ignore-reason convention changed?" >&2
  exit 1
fi

for var in "${variables[@]}"; do
  # One cargo invocation per test file, with every test of it that this
  # variable gates.
  for target in $(awk -v var="$var" '$3 == var { print $1 ":" $2 }' <<<"$gated" | sort -u); do
    crate=${target%%:*}
    test=${target#*:}
    mapfile -t names < <(awk -v var="$var" -v crate="$crate" -v test="$test" \
      '$1 == crate && $2 == test && $3 == var { print $4 }' <<<"$gated")
    log "$var=1 cargo test -p $crate --test $test -- --ignored --exact ${names[*]}"
    env "$var=1" cargo test -p "$crate" --test "$test" "${profile[@]}" --no-fail-fast \
      -- --ignored --exact "${names[@]}" || failed+=("$var: $crate --test $test")
  done
done

if [ ${#failed[@]} -gt 0 ]; then
  log "failed:"
  printf '  %s\n' "${failed[@]}" >&2
  exit 1
fi
log "all passed"
