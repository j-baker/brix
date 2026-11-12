#!/usr/bin/env bash
# Regenerates crates/brix-cli/data/nix-cli-tree.json from the reference
# `nix` binary: its `__dump-cli` output with every description and
# document removed (brix writes its own help text).
set -euo pipefail
cd "$(dirname "$0")/.."
: "${BRIX_NIX_BIN:?set BRIX_NIX_BIN to the reference nix binary (the devshell does)}"
"$BRIX_NIX_BIN" __dump-cli | jq -S '
  def strip:
    del(.description, .doc)
    | (if .flags then .flags |= with_entries(.value |= del(.description)) else . end)
    | (if .category then .category |= del(.description) else . end)
    | (if .commands then .commands |= with_entries(.value |= strip) else . end);
  .args | strip' > crates/brix-cli/data/nix-cli-tree.json
echo "wrote crates/brix-cli/data/nix-cli-tree.json"
