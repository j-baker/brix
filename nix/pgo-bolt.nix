# Cargo builds preserve the workspace's fat LTO profile and build the
# in-process training driver alongside the CLI. Each expensive stage is a
# separate derivation, so profiles and intermediate binaries can be cached.
{
  pkgs,
  trainingEnv,
  delegateNix,
}:
let
  inherit (pkgs) lib;
  system = pkgs.stdenv.hostPlatform.system;
  llvm = pkgs.rustc.llvm;
  bolt = pkgs."llvmPackages_${lib.versions.major llvm.version}".bolt;
  src = lib.fileset.toSource {
    root = ../.;
    fileset = lib.fileset.unions [
      ../Cargo.toml
      ../Cargo.lock
      ../crates
    ];
  };
  trainingSrc = lib.fileset.toSource {
    root = ../.;
    fileset = lib.fileset.unions [
      ../scripts/pgo-train.sh
      ../crates/brix-cli/tests/fixture
    ];
  };
  version = (builtins.fromTOML (builtins.readFile ../Cargo.toml)).workspace.package.version;
  meta = {
    description = "Nix-compatible evaluator optimized with PGO and BOLT";
    mainProgram = "brix";
    license = lib.licenses.lgpl21Plus;
    platforms = [
      "x86_64-linux"
      "aarch64-linux"
    ];
  };
  build =
    name: flags:
    pkgs.rustPlatform.buildRustPackage {
      pname = "brix-${name}";
      inherit version src;
      cargoLock.lockFile = ../Cargo.lock;
      # Keep both passes identical apart from the profile flag. In particular,
      # retain symbols and relocations until BOLT has rewritten the executable.
      # brix-gc requires POPCNT on x86-64; RUSTFLAGS replaces the flags in
      # .cargo/config.toml.
      RUSTFLAGS =
        "-Clink-arg=-Wl,--emit-relocs "
        + lib.optionalString pkgs.stdenv.hostPlatform.isx86_64 "-Ctarget-feature=+popcnt "
        + flags;
      cargoBuildFlags = [
        "--locked"
        "-p"
        "brix-cli"
        "--bin"
        "brix"
        "-p"
        "brix-eval"
        "--example"
        "eval_nixpkgs"
      ];
      auditable = false;
      dontStrip = true;
      dontFixup = true;
      doCheck = false;
      installPhase = ''
        runHook preInstall
        mkdir -p $out/bin
        cp target/${pkgs.stdenv.hostPlatform.rust.rustcTarget}/release/brix $out/bin/
        cp target/${pkgs.stdenv.hostPlatform.rust.rustcTarget}/release/examples/eval_nixpkgs $out/bin/
        runHook postInstall
      '';
    };
  instrumented = build "pgo-instrumented" "-Cprofile-generate";
  train = bins: ''
    bash ${trainingSrc}/scripts/pgo-train.sh ${bins} ${lib.escapeShellArg system}
  '';
  profiles =
    pkgs.runCommand "brix-pgo-profile-${version}"
      (
        trainingEnv
        // {
          nativeBuildInputs = [ llvm ];
        }
      )
      ''
        mkdir -p $out profiles
        export LLVM_PROFILE_FILE="$PWD/profiles/%m-%p.profraw"
        ${train "${instrumented}/bin"}
        llvm-profdata merge profiles/*.profraw -o $out/merged.profdata
        test -s $out/merged.profdata
      '';
  pgo = build "pgo" "-Cprofile-use=${profiles}/merged.profdata";
  unwrapped =
    pkgs.runCommand "brix-pgo-bolt-unwrapped-${version}"
      (
        trainingEnv
        // {
          nativeBuildInputs = [
            bolt
            pkgs.patchelf
          ];
          # BOLT's rewritten ELF must not be subsequently patched or stripped.
          dontFixup = true;
          disallowedReferences = [ pgo ];
          passthru = { inherit instrumented profiles pgo; };
        }
      )
      ''
        mkdir -p stage instrumented $out/bin $out/share/brix-optimization
        cp ${pgo}/bin/brix stage/brix
        chmod u+w stage/brix
        # The Rust linker adds $out/lib to RPATH, even though this package
        # has no libraries. Drop that reference before BOLT sees the ELF.
        # A local input also keeps BOLT's embedded command line from retaining
        # the intermediate package in the final runtime closure.
        patchelf --shrink-rpath stage/brix
        llvm-bolt stage/brix -instrument -o instrumented/brix \
          --instrumentation-file="$PWD/brix.fdata" --instrumentation-file-append-pid
        # Only ship/optimize the CLI. The driver supplements LLVM's PGO data;
        # BOLT's profile must come from the exact CLI that it will rewrite.
        ln -s ${pgo}/bin/eval_nixpkgs instrumented/eval_nixpkgs
        ${train "$PWD/instrumented"}
        merge-fdata brix.fdata.* > brix.merged.fdata
        test -s brix.merged.fdata
        llvm-bolt stage/brix -o $out/bin/brix -data=brix.merged.fdata \
          -reorder-blocks=ext-tsp -reorder-functions=cdsort -split-functions \
          -split-all-cold -split-eh -icf=all -use-gnu-stack -dyno-stats \
          2>&1 | tee $out/share/brix-optimization/bolt.log
        # Exercise the rewritten binary, including exceptions and flake paths.
        mkdir -p smoke
        ln -s $out/bin/brix smoke/brix
        ln -s ${pgo}/bin/eval_nixpkgs smoke/eval_nixpkgs
        ${train "$PWD/smoke"}
        test "$($out/bin/brix --store dummy:// eval --json --expr '1 + 2')" = 3
      '';
in
assert lib.assertMsg (
  pkgs.stdenv.buildPlatform == pkgs.stdenv.hostPlatform
) "brix-pgo-bolt requires a native build: training executes the target binary";
pkgs.runCommand "brix-pgo-bolt-${version}"
  {
    nativeBuildInputs = [ pkgs.makeBinaryWrapper ];
    inherit meta;
    passthru = {
      inherit
        unwrapped
        instrumented
        profiles
        pgo
        ;
    };
  }
  ''
    makeBinaryWrapper ${unwrapped}/bin/brix $out/bin/brix \
      --set-default BRIX_DELEGATE_NIX ${delegateNix}
    ln -s ${unwrapped}/share $out/share
  ''
