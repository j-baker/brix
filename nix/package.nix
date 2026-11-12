# Builds the workspace with crate2nix. `brix` is the CLI; `tests` runs every
# workspace crate's test suite in the sandbox with the same environment the
# devshell gives `cargo test`.
{
  pkgs,
  cargoNix,
  testEnv,
  # The `nix` binary that runs the commands brix does not implement
  # (`repl`, `profile`, `store gc`, …): the Nix whose command line brix
  # mirrors.
  delegateNix,
}:

let
  inherit (pkgs) lib;

  # brix-alloc's tune.c includes the mimalloc.h of the mimalloc that
  # libmimalloc-sys builds. Cargo gives a direct dependent's build script
  # that header's directory as DEP_MIMALLOC_INCLUDE_DIR; buildRustCrate
  # does not (it names the variable after the crate rather than its
  # `links` key, and points it into libmimalloc-sys's build directory,
  # which is gone by then), so the header is taken from the crate's
  # source: the same tarball the libmimalloc-sys build fetched (an
  # identical fixed-output derivation). libmimalloc-sys builds mimalloc v3
  # unless its `v2` feature is on, and brix does not turn it on.
  mimallocSys = cargoNix.internal.crates."libmimalloc-sys";
  mimallocInclude =
    pkgs.runCommand "mimalloc-include-${mimallocSys.version}"
      {
        src = pkgs.fetchurl {
          name = "libmimalloc-sys-${mimallocSys.version}.tar.gz";
          url = "https://static.crates.io/crates/libmimalloc-sys/libmimalloc-sys-${mimallocSys.version}.crate";
          inherit (mimallocSys) sha256;
        };
      }
      ''
        mkdir -p $out
        tar -xzf $src -C $out --strip-components=5 \
          libmimalloc-sys-${mimallocSys.version}/c_src/mimalloc/v3/include
        test -f $out/mimalloc.h
      '';

  # The test environment (testEnv) is only exported where the tests run
  # (testPreRun below), not into the crates' compile derivations: the
  # release package does not depend on the Nix sources or nixpkgs, and
  # bumping them does not rebuild it.
  # These packages compile their sibling's sources with brix_parallel. Keep
  # both directories together, including the serial corpus used by their tests.
  parallelSource = parallel: serial: libName: entry: {
    src = lib.fileset.toSource {
      root = ../crates;
      fileset = lib.fileset.unions [
        (../crates + "/${parallel}")
        (../crates + "/${serial}")
      ];
    };
    inherit libName;
    libPath = "../${serial}/src/${entry}";
    workspace_member = parallel;
  };

  crateOverrides = pkgs.defaultCrateOverrides // {
    brix-alloc = _attrs: { DEP_MIMALLOC_INCLUDE_DIR = "${mimallocInclude}"; };
    brix-eval-parallel = _attrs: parallelSource "brix-eval-parallel" "brix-eval" "brix_eval_parallel" "lib.rs";
    brix-parallel = _attrs: parallelSource "brix-parallel" "brix" "brix_parallel" "lib.rs";
    brix-cli-parallel =
      _attrs: parallelSource "brix-cli-parallel" "brix-cli" "brix_cli_parallel" "main.rs";
  };

  # Extra cargo features for a member's test build. brix-eval's image tests
  # compare the work counters (thunks, environments, calls) of source and
  # image evaluation; release builds compile those counters out.
  testFeatures = {
    "brix-eval" = [ "work-counters" ];
  };

  # crate2nix runs a crate's tests in a separate derivation, and the test
  # environment is exported there, in testPreRun. The oracle nix needs a
  # writable home for its state, and its store, which in the sandbox is a
  # local store under that home, is created before the tests run: nix
  # processes started by tests running at the same time would race to
  # create its database ("SQLite database '…' is busy").
  memberBuild =
    name: runTests:
    cargoNix.workspaceMembers.${name}.build.override {
      inherit crateOverrides runTests;
      features = [ "default" ] ++ lib.optionals runTests (testFeatures.${name} or [ ]);
      # Oracle tests create git repositories and tarballs to compare against.
      testInputs = [
        pkgs.git
        # `ssh-keygen`, which git runs to sign commits and to check their
        # signatures (brix-flake's native/git_verify.rs, brix-cli's
        # tests/git_verify.rs).
        pkgs.openssh
        # `hg`, which brix and the oracle run to fetch Mercurial inputs
        # (brix-flake's native/mercurial.rs, brix-cli's tests/mercurial.rs).
        pkgs.mercurial
        pkgs.gnutar
        pkgs.gzip
        # `unshare`: brix-cli's tests/offline.rs takes the network away.
        pkgs.util-linux
      ];
      testPreRun = lib.concatStringsSep "\n" (
        lib.mapAttrsToList (k: v: "export ${k}=${lib.escapeShellArg v}") testEnv
        ++ [
          "export HOME=\$TMPDIR"
          # The sandbox has no network, so brix, like Nix, would warn that
          # there is no Internet access before every command; the tests
          # expect what it prints on a connected machine (the oracle's
          # warning is noise to them), and tests/offline.rs decides for
          # itself.
          "export BRIX_HAVE_INTERNET=1"
          "\"$BRIX_NIX_BIN\" --extra-experimental-features nix-command store info > /dev/null"
        ]
      );
    };

  members = builtins.attrNames cargoNix.workspaceMembers;

  unwrapped = memberBuild "brix-cli" false;
in
{
  # The CLI, wrapped to name the Nix it hands commands to. A
  # BRIX_DELEGATE_NIX already in the environment wins.
  brix =
    pkgs.runCommand "brix-${unwrapped.version}"
      {
        nativeBuildInputs = [ pkgs.makeBinaryWrapper ];
        passthru = { inherit unwrapped; };
        meta = {
          description = "A bug-for-bug compatible Nix evaluator in Rust";
          mainProgram = "brix";
          license = lib.licenses.lgpl21Plus;
          platforms = lib.platforms.unix;
        };
      }
      ''
        makeBinaryWrapper ${lib.getExe' unwrapped "brix"} $out/bin/brix \
          --set-default BRIX_DELEGATE_NIX ${delegateNix}
      '';

  tests = pkgs.linkFarm "brix-tests" (
    map (name: {
      inherit name;
      path = memberBuild name true;
    }) members
  );
}
