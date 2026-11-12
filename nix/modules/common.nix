{ defaultPackage }:
{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.programs.brix;
in
{
  options.programs.brix = {
    enable = lib.mkEnableOption "brix as the nix command";

    package = lib.mkOption {
      type = lib.types.package;
      default = defaultPackage pkgs;
      defaultText = lib.literalExpression "brix-pgo-bolt from this flake on supported native Linux systems, otherwise brix";
      description = "The brix package providing bin/brix. Defaults to the PGO/BOLT build where supported.";
    };

    fallbackPackage = lib.mkOption {
      type = lib.types.package;
      default = pkgs.nix;
      defaultText = lib.literalExpression "pkgs.nix";
      example = lib.literalExpression "pkgs.lix";
      description = ''
        The original Nix package. Its nix binary handles commands brix
        delegates, and its other executables, libraries and data are kept
        in the installed package. Configure the Nix implementation here
        instead of setting nix.package.
      '';
    };

    finalPackage = lib.mkOption {
      type = lib.types.package;
      readOnly = true;
      description = "The fallback Nix package with bin/nix replaced by brix.";
    };
  };

  config = lib.mkIf cfg.enable {
    programs.brix.finalPackage = pkgs.callPackage ../with-brix.nix { } {
      brix = cfg.package;
      inherit (cfg) fallbackPackage;
    };
  };
}
