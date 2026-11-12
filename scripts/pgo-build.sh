#!/usr/bin/env bash
# Builds an optimised `brix` (and the `eval_nixpkgs` example used for
# in-process benchmarks) with profile-guided optimisation (PGO) and, by
# default, a BOLT post-link layout pass on top.
#
# Usage: scripts/pgo-build.sh [--mode pgo-bolt|pgo|bolt] [--out DIR]
#
#   pgo-bolt  (default) PGO build, then BOLT using an instrumented run
#   pgo       PGO build only
#   bolt      plain release build, then BOLT (reference only)
#
# The result is DIR/brix and DIR/eval_nixpkgs (default DIR: target/pgo/out).
# For a sandboxed Nix build of the CLI, use `nix build .#brix-pgo-bolt`.
#
# Requirements, all provided by `nix develop`:
#   - cargo/rustc, and llvm-profdata from the LLVM rustc was built with
#     (profile formats are version-specific; checked below);
#   - llvm-bolt and merge-fdata (for the modes using BOLT);
#   - BRIX_NIXPKGS (a nixpkgs tree) and BRIX_NIX_SRC (the Nix source, for
#     the language corpus): the training inputs.
# Set CARGO_BUILD_JOBS to limit build parallelism.
#
# How it works:
#   1. build instrumented binaries (-Cprofile-generate);
#   2. train: run them over realistic workloads, see `train` below: nixpkgs
#      package evaluations in process and through the CLI (with compiled
#      inputs, compiling them first, and from source), the whole language
#      corpus (eval-okay and eval-fail), and flake show/check/metadata on
#      the CLI test fixture, plus CLI evaluations with small nursery budgets
#      to train the garbage collector, and parallel flake checks with shared
#      nixpkgs dependencies on two and four evaluation workers;
#   3. merge the raw profiles and rebuild with -Cprofile-use;
#   4. (BOLT) instrument the optimised binaries with llvm-bolt, run the same
#      training again, and let llvm-bolt reorder blocks and functions using
#      the collected profile. Instrumentation rather than perf sampling:
#      BOLT's sampling mode wants hardware branch records (LBR), which not
#      every machine exposes.
#
# Measured 2026-11-08 on the development machine (20 threads), wall time
# against the plain fat-LTO release build, best of 5 interleaved; training
# used a newer nixpkgs revision than the benchmark, so the gains are not
# from training on the benchmark itself:
#
#   workload                    pgo-bolt   pgo     bolt
#   in-process lib.version       -9.4%   -11.6%   -7.0%
#   in-process hello.drvPath    -10.3%   -15.4%   -3.1%
#   in-process numpy.drvPath     -9.2%    -6.4%   -1.7%
#   in-process firefox.drvPath   -8.5%    -6.6%   -1.3%
#   in-process NixOS toplevel   -10.7%    -8.2%   -1.2%
#   CLI lib.version (images)    -16.1%   -15.6%   -1.7%
#   CLI hello.drvPath           -18.9%    -9.6%   -5.3%
#   CLI firefox.drvPath          -8.2%    -9.0%   -2.1%
#
# pgo-bolt is the default: it is best on the large evaluations and on the
# CLI. pgo alone measured better on the two smallest in-process cases, but
# within run-to-run noise (the baseline itself moved between 126 and 136 ms
# on hello across runs). bolt alone is a reference point. The whole pipeline takes about 18 minutes with
# CARGO_BUILD_JOBS=4 (two fat-LTO builds, two training runs, BOLT). BOLT
# drops debug info, so the pgo-bolt binary is about 21 MB against 83 MB for
# the plain release build (which carries debug = 1).
set -euo pipefail

mode=pgo-bolt
out=
while [ $# -gt 0 ]; do
  case "$1" in
    --mode) mode=${2:?--mode needs an argument}; shift 2 ;;
    --out) out=${2:?--out needs an argument}; shift 2 ;;
    -h | --help) sed -n '2,13p' "$0"; exit 0 ;;
    *) echo "error: unknown argument '$1' (see --help)" >&2; exit 1 ;;
  esac
done
case "$mode" in
  pgo-bolt | pgo | bolt) ;;
  *) echo "error: unknown mode '$mode' (expected pgo-bolt, pgo or bolt)" >&2; exit 1 ;;
esac

repo=$(cd "$(dirname "$0")/.." && pwd)
work=${PGO_WORK:-$repo/target/pgo}
out=${out:-$work/out}
nixpkgs=${BRIX_NIXPKGS:?BRIX_NIXPKGS must point at a nixpkgs tree (training input)}
lang=${BRIX_NIX_SRC:?BRIX_NIX_SRC must point at the Nix source (training input)}/tests/functional/lang
[ -f "$nixpkgs/flake.nix" ] || { echo "error: $nixpkgs is not a nixpkgs tree" >&2; exit 1; }
[ -d "$lang" ] || { echo "error: $lang does not exist" >&2; exit 1; }

need() { command -v "$1" >/dev/null || { echo "error: '$1' is not on PATH (run inside 'nix develop')" >&2; exit 1; }; }
need cargo
need rustc
if [ "$mode" != bolt ]; then
  need llvm-profdata
  # A profile written by one LLVM major cannot be read by another.
  rustc_llvm=$(rustc -vV | sed -n 's/^LLVM version: \([0-9]*\).*/\1/p')
  profdata_llvm=$(llvm-profdata --version | sed -n 's/.*LLVM version \([0-9]*\).*/\1/p')
  if [ "$rustc_llvm" != "$profdata_llvm" ]; then
    echo "error: rustc uses LLVM $rustc_llvm but llvm-profdata is LLVM $profdata_llvm" >&2
    exit 1
  fi
fi
if [ "$mode" != pgo ]; then
  need llvm-bolt
  need merge-fdata
fi

log() { echo "pgo-build: $*" >&2; }

# Builds the brix binary and the eval_nixpkgs example into target dir $1
# with RUSTFLAGS $2, and copies both into directory $3.
build() {
  local target=$1 flags=$2 dest=$3
  # RUSTFLAGS replaces .cargo/config.toml, which is where brix-gc gets the
  # POPCNT it requires on x86-64.
  if [ "$(uname -m)" = x86_64 ]; then
    flags="-Ctarget-feature=+popcnt $flags"
  fi
  log "building with RUSTFLAGS='$flags' into $target"
  (cd "$repo" && CARGO_TARGET_DIR=$target RUSTFLAGS=$flags \
    cargo build --release --locked -p brix-cli --bin brix -p brix-eval --example eval_nixpkgs)
  mkdir -p "$dest"
  cp "$target/release/brix" "$target/release/examples/eval_nixpkgs" "$dest/"
}

# Keep local and Nix builds on the same training workloads.
train() {
  bash "$repo/scripts/pgo-train.sh" "$1"
}

mkdir -p "$work"
# emit-relocs keeps the relocations BOLT needs to move code; it does not
# change the generated code.
relocs=
[ "$mode" != pgo ] && relocs="-Clink-arg=-Wl,--emit-relocs"

if [ "$mode" = bolt ]; then
  build "$work/plain-target" "$relocs" "$work/stage"
else
  rm -rf "$work/profiles"
  build "$work/gen-target" "-Cprofile-generate=$work/profiles" "$work/gen"
  train "$work/gen"
  log "merging $(find "$work/profiles" -name '*.profraw' | wc -l) raw profiles"
  llvm-profdata merge -o "$work/merged.profdata" "$work/profiles"
  build "$work/use-target" "-Cprofile-use=$work/merged.profdata $relocs" "$work/stage"
fi

if [ "$mode" = pgo ]; then
  mkdir -p "$out"
  cp "$work/stage/brix" "$work/stage/eval_nixpkgs" "$out/"
else
  rm -rf "$work/bolt"
  mkdir -p "$work/bolt/inst" "$out"
  for name in brix eval_nixpkgs; do
    log "BOLT: instrumenting $name"
    llvm-bolt "$work/stage/$name" -instrument -o "$work/bolt/inst/$name" \
      --instrumentation-file="$work/bolt/$name.fdata" --instrumentation-file-append-pid \
      >"$work/bolt/$name.instrument.log" 2>&1
  done
  train "$work/bolt/inst"
  for name in brix eval_nixpkgs; do
    log "BOLT: optimising $name"
    merge-fdata "$work/bolt/$name".fdata.* >"$work/bolt/$name.merged.fdata"
    llvm-bolt "$work/stage/$name" -o "$out/$name" -data="$work/bolt/$name.merged.fdata" \
      -reorder-blocks=ext-tsp -reorder-functions=cdsort -split-functions -split-all-cold \
      -split-eh -icf=all -use-gnu-stack -dyno-stats >"$work/bolt/$name.optimise.log" 2>&1
  done
fi

log "done ($mode): $out/brix $out/eval_nixpkgs"
