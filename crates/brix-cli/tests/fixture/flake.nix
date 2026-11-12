{
  description = "brix CLI fixture";
  outputs =
    { self }:
    let
      mkDrv =
        system: name:
        (derivation {
          inherit name system;
          builder = "/bin/sh";
          args = [
            "-c"
            "echo ${name} > $out"
          ];
        })
        // {
          meta.description = "The ${name} package";
        };
      # A package with a `mainProgram`, which `flake show`, `flake check`
      # and `search` list. It is never built: the build sandbox's `/bin/sh`
      # cannot run its builder, so the `run` and `shell` tests take their
      # program from nixpkgs.
      mkScript =
        system:
        (derivation {
          name = "script-1.0";
          inherit system;
          builder = "/bin/sh";
          args = [
            "-c"
            "mkdir -p $out/bin; printf '#!/bin/sh\\necho script ran with \"$@\"\\n' > $out/bin/script; chmod +x $out/bin/script"
          ];
        })
        // {
          meta.description = "A script";
          meta.mainProgram = "script";
        };
      forSystems =
        f:
        builtins.listToAttrs (
          map
            (s: {
              name = s;
              value = f s;
            })
            [
              "x86_64-linux"
              "aarch64-linux"
            ]
        );
    in
    {
      packages = forSystems (system: {
        default = mkDrv system "hello-1.0";
        tool = mkDrv system "tool-2.3";
        script = mkScript system;
      });
      legacyPackages = forSystems (system: {
        alpha = mkDrv system "alpha-0.1";
        nested = {
          recurseForDerivations = true;
          beta = mkDrv system "beta-0.2";
        };
        notADrv = {
          x = 1;
        };
      });
      apps = forSystems (system: {
        default = {
          type = "app";
          program = "${self.packages.${system}.default}/bin/hello";
          meta.description = "say hello";
        };
      });
      checks = forSystems (system: {
        smoke = mkDrv system "smoke-check";
      });
      devShells = forSystems (system: {
        default = mkDrv system "shell-env";
      });
      formatter = forSystems (system: mkDrv system "fmt-1");
      overlays.default = final: prev: { };
      nixosModules.default = { config, ... }: { };
      templates.default = {
        path = ./tmpl;
        description = "A template";
      };
      hydraJobs.build = forSystems (system: mkDrv system "hydra-job");
      somethingElse = 42;
    };
}
