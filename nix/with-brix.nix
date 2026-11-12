# Keep Nix's tools, libraries and data, replacing only the modern CLI.
{
  lib,
  symlinkJoin,
  writeShellScript,
}:
{ brix, fallbackPackage }:
let
  originalNix = lib.getExe' fallbackPackage "nix";
  dispatcher = writeShellScript "nix-with-brix" ''
    if [ "''${BRIX_USE_NIX:-}" = 1 ]; then
      exec ${originalNix} "$@"
    fi
    export BRIX_DELEGATE_NIX=${originalNix}
    exec ${lib.getExe' brix "brix"} "$@"
  '';
in
symlinkJoin {
  name = "${fallbackPackage.name}-with-brix";
  inherit (fallbackPackage) version;
  pname = fallbackPackage.pname or "nix";
  paths = lib.unique (
    [
      (lib.getBin fallbackPackage)
      fallbackPackage
    ]
    ++ map (output: lib.getOutput output fallbackPackage) (
      fallbackPackage.meta.outputsToInstall or [ "out" ]
    )
  );
  postBuild = ''
    # lndir preserves relative symlinks. Anchor every original bin entry to
    # its source so aliases such as nix-store -> nix keep running real Nix.
    for executable in ${lib.getBin fallbackPackage}/bin/*; do
      ln -sfn "$executable" "$out/bin/$(basename "$executable")"
    done
    rm "$out/bin/nix"
    ln -s ${dispatcher} "$out/bin/nix"
  '';
  passthru = (fallbackPackage.passthru or { }) // {
    inherit brix fallbackPackage;
  };
  meta = (fallbackPackage.meta or { }) // {
    description = "Nix with brix as its nix command";
    mainProgram = "nix";
    outputsToInstall = [ "out" ];
  };
}
