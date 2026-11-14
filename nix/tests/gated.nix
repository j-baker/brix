# The tests `scripts/test-all.sh` runs beyond the default suite: the
# `#[ignore]`d tests gated on BRIX_TEST_REAL_DAEMON and
# BRIX_TEST_REAL_STORE, inside the sandbox (the `tests` check runs the
# default suite). They need what a developer's machine has and the
# sandbox does not: a running `nix-daemon` with a writable store at
# /nix/store, whose outputs the tests then read and run at their real
# paths. A user namespace provides one: a writable directory is mounted
# over /nix/store with the build's inputs bound back in, /nix/var is
# created for the daemon's state and its socket at the default path
# (tests that clear the environment look there), the closures whose
# paths the tests name are registered, and the reference `nix-daemon`
# builds in sandboxes of its own, through nested user namespaces. The
# tests gated on BRIX_TEST_NETWORK stay out: the sandbox has no network
# (test-all.sh runs those only with --network).
{
  pkgs,
  # BRIX_NIX_SRC, BRIX_NIXPKGS, BRIX_NIX_BIN and NIX_CONFIG, as the
  # devshell and the `tests` check set them.
  testEnv,
  # The reference Nix package: its nix-daemon serves the store, its
  # nix-store loads the seed database, and its own closure is seeded so
  # the tests that name its paths (`env shell`) find them valid.
  referenceNix,
  # What the brix CLI hands unimplemented commands to (BRIX_DELEGATE_NIX).
  delegateNix,
}:
let
  inherit (pkgs) lib;

  # The paths the tests expect the daemon's store to have, as a
  # developer's machine does (built or substituted long ago): the
  # reference Nix (`env shell` enters it, and its sandbox shell builds
  # in it), the Nix sources and the nixpkgs tree (flake inputs the tests
  # evaluate), the nixpkgs packages the tests run (hello, coreutils,
  # `nix develop`'s bashInteractive), and the build closures of the
  # derivations the tests' flakes make — each `inputDerivation` is a
  # cheap derivation whose output names every input of its original, so
  # its closure is exactly what building that original needs
  # (writeShellScriptBin a shell, mkShell and mkDerivation their
  # stdenvs' tools).
  registration = pkgs.closureInfo {
    rootPaths = [
      referenceNix
      testEnv.BRIX_NIX_SRC
      testEnv.BRIX_NIXPKGS
      (pkgs.mkShell { packages = [ pkgs.hello ]; }).inputDerivation
      (pkgs.writeShellScriptBin "brix-gated-seed" "").inputDerivation
      (pkgs.stdenv.mkDerivation {
        name = "brix-gated-seed";
        dontUnpack = true;
        buildPhase = "echo done > $out";
      }).inputDerivation
    ]
    # Every output, not just `out`: `nix develop` realises outputs of
    # nixpkgs#bashInteractive beyond the one it runs.
    ++ pkgs.bashInteractive.all
    ++ pkgs.coreutils.all
    ++ pkgs.hello.all;
  };

  # Runs inside `unshare --map-root-user --mount`, with the test
  # environment already exported: sets up the store and daemon, then
  # hands over to test-all.sh for the gated tests themselves.
  run = pkgs.writeShellScript "brix-gated-tests-run" ''
    set -euo pipefail

    # A writable /nix at its real location: a writable /nix/store with
    # every input bound back in (directories) or copied (.drv files and
    # other loose entries) and a /nix/var for the daemon's state,
    # assembled first and then mounted over the sandbox's read-only /nix
    # in one step — the tools doing the assembling live in its store.
    # Bind mounts rather than symlinks, so a store path looks the same
    # here as on a developer's machine.
    mkdir -p "$TMPDIR/nix/store" "$TMPDIR/nix/var/nix" "$TMPDIR/nix/var/log/nix"
    for p in /nix/store/*; do
      name=''${p##*/}
      if [ -d "$p" ] && ! [ -L "$p" ]; then
        mkdir "$TMPDIR/nix/store/$name"
        mount --rbind "$p" "$TMPDIR/nix/store/$name"
      else
        # Not ownership: the namespace cannot represent the real owner.
        cp -a --no-preserve=ownership "$p" "$TMPDIR/nix/store/$name"
      fi
    done
    mount --rbind "$TMPDIR/nix" /nix

    # The daemon's state, where everything expects it, with the seed
    # closures registered as valid. No build users: Nix is root in the
    # namespace, but the nixbld group's id is not mapped into it.
    ${referenceNix}/bin/nix-store --store local \
      --option build-users-group "" \
      --load-db < ${registration}/registration

    # No substituters: the sandbox has no network. Supplementary groups
    # cannot be dropped in the namespace (its setgroups is denied), so
    # builds must not insist. The socket appears at its default path.
    ${referenceNix}/bin/nix-daemon \
      --option substituters "" \
      --option build-users-group "" \
      --option require-drop-supplementary-groups false &

    socket=/nix/var/nix/daemon-socket/socket
    for _ in $(seq 600); do
      [ -S "$socket" ] && break
      sleep 0.1
    done
    if ! [ -S "$socket" ]; then
      echo "nix-daemon did not create $socket" >&2
      exit 1
    fi

    # The default store, as on a developer's machine: the daemon.
    export NIX_REMOTE=daemon
    # The tests run as an ordinary user, as on a developer's machine: a
    # nested namespace maps root back to an unprivileged id, so nothing
    # they run takes the root-only paths (build users, store ownership).
    # Through bash: the sandbox has no /usr/bin/env for the shebang.
    exec unshare --map-user=1000 --map-group=1000 \
      bash scripts/test-all.sh --gated-only
  '';
in
pkgs.stdenv.mkDerivation {
  name = "brix-gated-tests";
  src = lib.fileset.toSource {
    root = ../..;
    fileset = lib.fileset.unions [
      ../../Cargo.toml
      ../../Cargo.lock
      # The POPCNT target feature brix-gc requires on x86-64.
      ../../.cargo
      ../../crates
      ../../scripts/test-all.sh
    ];
  };
  cargoDeps = pkgs.rustPlatform.importCargoLock { lockFile = ../../Cargo.lock; };
  nativeBuildInputs = [
    pkgs.rustPlatform.cargoSetupHook
    pkgs.cargo
    pkgs.rustc
    # `unshare`, for the namespace here and for tests/offline.rs.
    pkgs.util-linux
    # What the gated tests run, as in the `tests` check (nix/package.nix).
    pkgs.git
    pkgs.openssh
    pkgs.mercurial
    pkgs.gnutar
    pkgs.gzip
  ];
  env = testEnv // {
    # The sandbox has no network; the tests expect what brix prints on a
    # connected machine.
    BRIX_HAVE_INTERNET = "1";
    BRIX_DELEGATE_NIX = delegateNix;
  };
  buildPhase = ''
    runHook preBuild
    export HOME=$TMPDIR
    unshare --map-root-user --mount ${run}
    runHook postBuild
  '';
  installPhase = "touch $out";
  dontFixup = true;
}
