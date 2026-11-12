#!/usr/bin/env bash
# Regenerates the derivation fixtures with the reference Nix
# ($BRIX_NIX_BIN, set by the devshell). Needs a writable local store
# (nix-instantiate writes the .drv files), which is why the fixtures are
# committed rather than generated during `cargo test`.
#
#   ./generate.sh   (from this directory)
#
# For every attribute of cases.nix this writes:
#   <attr>.drvpath        the derivation's store path (base name)
#   <attr>.json           `nix derivation show -r`: the derivation and all
#                         of its input derivations, keyed by base name
# and for every derivation reachable from a case (inputs included):
#   drvs/<base name>      the ATerm text, copied from the store
#
# The content-addressed cases (CA_CASES) are the only ones allowed to
# fail to instantiate: a daemon without ca-derivations refuses to write
# them (or, for one depending on floating outputs, reports success without
# writing it). Their path is still known from evaluation; they get no
# .json, and no drvs/ entry unless the .drv was written. Any other failure
# stops the script.
set -euo pipefail
cd "$(dirname "$0")"
NIX=${BRIX_NIX_BIN:?BRIX_NIX_BIN must point at the reference nix binary}
NIX_INSTANTIATE=$(dirname "$NIX")/nix-instantiate
CA_CASES=" contentAddressed caFlat usesCa "
XP=(--extra-experimental-features "nix-command ca-derivations")
rm -rf drvs && mkdir -p drvs
rm -f ./*.drvpath ./*.json
copy_closure() {
  local d="$1"
  [ -e "drvs/$d" ] && return
  cp "/nix/store/$d" "drvs/$d"
  # Input derivations are the quoted .drv paths in the second list.
  for i in $(grep -o '"/nix/store/[^"]*\.drv"' "drvs/$d" | tr -d '"'); do
    copy_closure "$(basename "$i")"
  done
}
attrs=$("$NIX" "${XP[@]}" eval --impure --raw --expr \
  'builtins.concatStringsSep " " (builtins.attrNames (import ./cases.nix))')
for attr in $attrs; do
  # Evaluation alone gives the path; instantiation also writes the .drv.
  drv=$("$NIX" "${XP[@]}" --store dummy:// eval --raw --file cases.nix "$attr.drvPath")
  basename "$drv" > "$attr.drvpath"
  if [[ $CA_CASES == *" $attr "* ]]; then
    # The daemon may refuse, or report success without writing the file.
    "$NIX_INSTANTIATE" "${XP[@]}" --attr "$attr" cases.nix >/dev/null 2>&1 || true
    if [ ! -e "$drv" ]; then
      echo "note: the daemon did not write $attr; it has a .drvpath only" >&2
      continue
    fi
  else
    "$NIX_INSTANTIATE" "${XP[@]}" --attr "$attr" cases.nix >/dev/null
    "$NIX" "${XP[@]}" derivation show -r --file cases.nix "$attr" > "$attr.json"
  fi
  copy_closure "$(basename "$drv")"
done
chmod 644 drvs/*
echo "generated $(ls drvs | wc -l) derivations, $(ls ./*.json | wc -l) json files"
