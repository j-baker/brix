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
  # The fallback's other outputs (`dev`, `doc`, `man`, …) are this
  # package's too, so that `nix.package.dev` is still Nix's headers. `out`
  # is the joined tree below, and a `bin` output is part of it: forwarding
  # that one would hand `lib.getBin` the original bin/nix.
  forwardedOutputs = lib.subtractLists [
    "out"
    "bin"
  ] (fallbackPackage.outputs or [ ]);
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
  outputs = [ "out" ] ++ forwardedOutputs;
  postBuild = ''
    ${lib.concatMapStrings (output: ''
      ln -sT ${fallbackPackage.${output}} "''$${output}"
    '') forwardedOutputs}
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
