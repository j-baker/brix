# NixOS, nix-darwin and System Manager share these options. Installing the
# package explicitly also supports hosts whose Nix daemon is managed elsewhere.
args:
{ config, lib, ... }:
{
  imports = [ (import ./common.nix args) ];

  config = lib.mkIf config.programs.brix.enable {
    nix.package = config.programs.brix.finalPackage;
    environment.systemPackages = [ config.programs.brix.finalPackage ];
  };
}
