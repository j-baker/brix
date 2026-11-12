args:
{ config, lib, ... }:
{
  imports = [ (import ./common.nix args) ];

  config = lib.mkIf config.programs.brix.enable {
    nix.package = config.programs.brix.finalPackage;
    home.packages = [ config.programs.brix.finalPackage ];
  };
}
