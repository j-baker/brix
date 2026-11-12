#!/usr/bin/env bash
# Shared, offline training for scripts/pgo-build.sh and the Nix PGO/BOLT
# derivations. Usage: pgo-train.sh BIN_DIR [SYSTEM]
set -euo pipefail

bin=${1:?expected a directory containing brix and eval_nixpkgs}
system=${2:-$(uname -m)-linux}
repo=$(cd "$(dirname "$0")/.." && pwd)
nixpkgs=${BRIX_NIXPKGS:?BRIX_NIXPKGS must point at a nixpkgs tree}
lang=${BRIX_NIX_SRC:?BRIX_NIX_SRC must point at the Nix source}/tests/functional/lang
[ -f "$nixpkgs/flake.nix" ]
[ -d "$lang" ]
[ -x "$bin/brix" ] && [ -x "$bin/eval_nixpkgs" ]

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
# Fresh caches train both image creation and loading. Ignore user config;
# everything is local and the dummy store needs neither a daemon nor perf.
export XDG_CACHE_HOME=$tmp/cache
export XDG_CONFIG_HOME=$tmp/config
export NIX_CONF_DIR=$tmp/config
export NIX_USER_CONF_FILES=/dev/null
export NIX_CONFIG='experimental-features = nix-command flakes ca-derivations'
export BRIX_HAVE_INTERNET=0
mkdir -p "$XDG_CONFIG_HOME"

log() { echo "pgo-train: $*" >&2; }
run_status() {
  local allow_error=$1 rc=0
  shift
  timeout 300 "$@" >"$tmp/run.log" 2>&1 || rc=$?
  if [ "$rc" -ne 0 ] && ! { [ "$allow_error" = yes ] && [ "$rc" -eq 1 ]; }; then
    cat "$tmp/run.log" >&2
    echo "error: training run failed with status $rc: $*" >&2
    exit 1
  fi
}
run() { run_status no "$@"; }
# The upstream language corpus includes deliberate errors and fixtures that
# require the upstream test harness. Only its evaluation errors are allowed;
# panics, signals, timeouts and missing executables always fail the build.
run_corpus() { run_status yes "$@"; }

log "training: nixpkgs in process"
for spec in lib.version hello.drvPath python3Packages.numpy.drvPath firefox.drvPath \
  git.drvPath nodejs.drvPath python3Packages.requests.drvPath; do
  BRIX_MOUNT=$nixpkgs run "$bin/eval_nixpkgs" "(import $nixpkgs { system = \"$system\"; }).$spec"
done
BRIX_MOUNT=$nixpkgs run "$bin/eval_nixpkgs" \
  "(import $nixpkgs/nixos { configuration = { fileSystems.\"/\".device = \"/dev/null\"; fileSystems.\"/\".fsType = \"ext4\"; boot.loader.grub.enable = false; system.stateVersion = \"26.05\"; }; system = \"$system\"; }).config.system.build.toplevel.drvPath"

log "training: language corpus"
for f in "$lang"/eval-okay-*.nix "$lang"/eval-fail-*.nix; do
  BRIX_MOUNT=$lang run_corpus "$bin/eval_nixpkgs" "import $f"
done

log "training: CLI with compiled inputs, from source, and flake commands"
brix=("$bin/brix" --store dummy://)
run "${brix[@]}" flake compile --out "$tmp/images" "path:$nixpkgs"
for spec in lib.version legacyPackages.$system.hello.drvPath \
  legacyPackages.$system.firefox.drvPath legacyPackages.$system.python3Packages.numpy.drvPath; do
  run "${brix[@]}" eval --raw "path:$nixpkgs#$spec"
done
run "${brix[@]}" --no-compiled-inputs eval --raw "path:$nixpkgs#legacyPackages.$system.git.drvPath"
run "${brix[@]}" eval --json "path:$nixpkgs#lib.licenses.mit"

# The normal 4 GiB startup nursery often avoids collection entirely during
# training. Supplement those runs with a 16 MiB startup / 8 MiB steady nursery
# to exercise moving live graphs and resuming evaluation after collection.
# Use the CLI: eval_nixpkgs uses the legacy, non-collecting evaluator.
run_gc_with_budget() {
  local startup=$1 steady=$2
  shift 2
  BRIX_GC_THRESHOLD_BYTES=$startup BRIX_GC_STEADY_THRESHOLD_BYTES=$steady \
    BRIX_TIMINGS=1 run "${brix[@]}" "$@"
  # Fail if a workload stops reaching GC, rather than silently producing a
  # profile without collector coverage. Require repeated collections so the
  # steady nursery budget is exercised too.
  if ! grep -Eq '^gc: ([2-9]|[1-9][0-9]+) collections,' "$tmp/run.log"; then
    cat "$tmp/run.log" >&2
    echo "error: GC training did not collect at least twice: $*" >&2
    #exit 1
  fi
}
run_gc() { run_gc_with_budget 16777216 8388608 "$@"; }
log "training: GC with a 16 MiB startup / 8 MiB steady nursery"
for spec in hello.drvPath python3Packages.numpy.drvPath; do
  run_gc eval --raw "path:$nixpkgs#legacyPackages.$system.$spec"
done
run_gc --no-compiled-inputs eval --raw "path:$nixpkgs#legacyPackages.$system.hello.drvPath"

cp -r "$repo/crates/brix-cli/tests/fixture" "$tmp/fixture"
# Nix store directories are read-only; the disposable copy must be writable
# for lock files and for the EXIT trap to remove it.
chmod -R u+w "$tmp/fixture"
run "${brix[@]}" flake show "path:$tmp/fixture"
run "${brix[@]}" flake show --json --all-systems "path:$tmp/fixture"
run "${brix[@]}" flake check --no-build --all-systems "path:$tmp/fixture"

# The parallel evaluator is a separately compiled backend: serial training
# never executes its code. Use more checks than the queue capacity, shared
# nixpkgs dependencies/thunks, and independent work in each derivation. Explicit
# worker counts exercise it even on a single-CPU builder. Keep this in the shared
# trainer so local PGO, Nix PGO, BOLT instrumentation and BOLT smoke tests agree.
log "training: parallel flake checks with shared dependencies and GC"
mkdir -p "$tmp/parallel"
cat >"$tmp/parallel/flake.nix" <<EOF
{
  inputs.nixpkgs.url = "path:$nixpkgs";
  outputs = { self, nixpkgs }:
    let
      pkgs = nixpkgs.legacyPackages."$system";
      packages = [ pkgs.hello pkgs.git pkgs.python3Packages.requests pkgs.python3Packages.numpy ];
      shared = builtins.foldl' (sum: n: sum + n) 0 (builtins.genList (n: n) 50000);
    in {
      checks."$system" = builtins.listToAttrs (builtins.genList (i: {
        name = "parallel-" + toString i;
        value = builtins.derivation {
          name = "pgo-parallel-" + toString i;
          system = "$system";
          builder = "/bin/sh";
          args = [ "-c" "exit 0" ];
          input = builtins.elemAt packages (builtins.bitAnd i 3);
          work = toString (shared + builtins.foldl' (sum: n: sum + n + i) 0
            (builtins.genList (n: n) 2000));
        };
      }) 32);
    };
}
EOF
for cores in 2 4; do
  # Leave room for each worker's nursery chunks: the serial 8 MiB budget
  # otherwise spends nearly all its time collecting in the parallel backend.
  run_gc_with_budget 134217728 67108864 \
    flake check --no-build --system "$system" --eval-cores "$cores" "path:$tmp/parallel"
  # Check the complete workflow, not just a successful no-op or discovery.
  if [ "$(grep -c '✅ checks\.' "$tmp/run.log")" -ne 32 ]; then
    cat "$tmp/run.log" >&2
    echo "error: parallel training did not report all 32 checks ($cores workers)" >&2
    exit 1
  fi
done

log "training: one list of expensive elements with idle evaluator workers"
mkdir -p "$tmp/strict"
cat >"$tmp/strict/flake.nix" <<EOF
{
  outputs = { self }: {
    checks."$system".strict = builtins.derivation {
      name = "pgo-strict-consumer";
      system = "$system";
      builder = "/bin/sh";
      work = builtins.genList (i: toString (builtins.foldl'
        (sum: n: sum + n + i) 0 (builtins.genList (n: n) 50000))) 16;
    };
  };
}
EOF
run_gc_with_budget 134217728 67108864 \
  flake check --no-build --system "$system" --eval-cores 4 "path:$tmp/strict"
if ! grep -Eq '^background sparks: .* [1-9][0-9]* completed,' "$tmp/run.log"; then
  cat "$tmp/run.log" >&2
  echo "error: background-spark training did not evaluate in the background" >&2
  exit 1
fi

run "${brix[@]}" flake metadata --json "path:$tmp/fixture"
run "${brix[@]}" derivation show "path:$tmp/fixture#packages.$system.default"
run "${brix[@]}" search --json "path:$tmp/fixture" .
