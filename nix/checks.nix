# The source checks of `nix flake check`: what a pull request must pass
# besides building and its tests (flake.nix adds those). Each runs in the
# sandbox, against the crates Cargo.lock names, vendored.
{
  pkgs,
  # The flake's formatter (treefmt, configured for nixfmt and rustfmt).
  formatter,
  # The crate2nix CLI that generates the committed Cargo.nix.
  crate2nix,
}:
let
  inherit (pkgs) lib;

  cargoSrc = lib.fileset.toSource {
    root = ../.;
    fileset = lib.fileset.unions [
      ../Cargo.toml
      ../Cargo.lock
      ../Cargo.nix
      # The POPCNT target feature brix-gc requires on x86-64.
      ../.cargo
      ../crates
      ../scripts/check-wasm.sh
      ../scripts/wasm
    ];
  };

  # Runs `command` at the root of the workspace, with cargo reading the
  # dependencies from the vendored Cargo.lock and not the network.
  cargoCheck =
    name: nativeBuildInputs: command:
    pkgs.stdenv.mkDerivation {
      name = "brix-${name}";
      src = cargoSrc;
      cargoDeps = pkgs.rustPlatform.importCargoLock { lockFile = ../Cargo.lock; };
      nativeBuildInputs = [
        pkgs.rustPlatform.cargoSetupHook
        pkgs.cargo
        pkgs.rustc
      ]
      ++ nativeBuildInputs;
      buildPhase = ''
        runHook preBuild
        export HOME=$TMPDIR
        ${command}
        runHook postBuild
      '';
      installPhase = "touch $out";
      dontFixup = true;
    };
in
{
  # `nix fmt -- --ci`: every Nix and Rust file is formatted.
  fmt =
    pkgs.runCommand "brix-fmt"
      {
        src = lib.fileset.toSource {
          root = ../.;
          fileset = lib.fileset.fileFilter (file: file.hasExt "nix" || file.hasExt "rs") ../.;
        };
        nativeBuildInputs = [ formatter ];
      }
      ''
        cp -r $src tree
        chmod -R u+w tree
        cd tree
        treefmt --ci --tree-root .
        touch $out
      '';

  clippy = cargoCheck "clippy" [ pkgs.clippy ] ''
    cargo clippy --offline --workspace --all-targets --all-features -- -D warnings
  '';

  # brix-syntax, brix-store, brix-eval and brix-flake (without its
  # fetchers) build for wasm32-unknown-unknown and call nothing that
  # panics there; nixpkgs' rustc has that target's standard library.
  wasm = cargoCheck "wasm" [ pkgs.clippy ] ''
    CARGO_NET_OFFLINE=true bash scripts/check-wasm.sh
  '';

  # The committed Cargo.nix is what crate2nix makes of Cargo.lock.
  cargoNix = cargoCheck "cargo-nix" [ crate2nix ] ''
    cp Cargo.nix committed.nix
    CARGO_NET_OFFLINE=true crate2nix generate
    diff -u committed.nix Cargo.nix || {
      echo "Cargo.nix is out of date: run 'crate2nix generate' and commit the result" >&2
      exit 1
    }
  '';
}
