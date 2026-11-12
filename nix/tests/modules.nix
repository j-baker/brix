# Small executable fixtures exercise packaging and delegation without rebuilding
# the Rust workspace. The CLI's own delegation tests live in brix-cli/tests.
{ pkgs }:
let
  inherit (pkgs) lib;
  fallback =
    pkgs.runCommand "fixture-nix-2.34.0"
      {
        pname = "nix";
        version = "2.34.0";
        outputs = [
          "out"
          "man"
        ];
        meta.outputsToInstall = [
          "out"
          "man"
        ];
        passthru.fixtureMarker = true;
      }
      ''
        mkdir -p "$out/bin" "$out/share/nix" "$out/libexec"
        cat > "$out/bin/nix" <<'EOF'
        #!${pkgs.runtimeShell}
        if [ "$1" = fail ]; then
          echo fallback-error >&2
          exit 23
        fi
        printf 'fallback:<%s>\n' "$@"
        EOF
        chmod +x "$out/bin/nix"
        # Nix implementations can use relative symlinks for legacy commands.
        ln -s nix "$out/bin/nix-store"
        ln -s nix "$out/bin/nix-daemon"
        echo data > "$out/share/nix/data"
        echo helper > "$out/libexec/helper"
        mkdir -p "$man/share/man/man1"
        echo manual > "$man/share/man/man1/nix.1"
      '';
  brix = pkgs.writeShellScriptBin "brix" ''
    if [ "$1" = handled ]; then
      echo brix
    else
      exec "$BRIX_DELEGATE_NIX" "$@"
    fi
  '';
  moduleArgs.defaultPackage = _: brix;
  systemModule = import ../modules/system.nix moduleArgs;
  homeModule = import ../modules/home-manager.nix moduleArgs;

  # Check both module interfaces, including when upstream Nix management is off.
  eval =
    module: home: enable: nixEnable:
    lib.evalModules {
      specialArgs = { inherit pkgs; };
      modules = [
        module
        ({ lib, ... }: {
          options = {
            nix.enable = lib.mkOption {
              type = lib.types.bool;
              default = nixEnable;
            };
            nix.package = lib.mkOption {
              type = lib.types.package;
              default = fallback;
            };
          }
          //
            lib.setAttrByPath
              (
                if home then
                  [
                    "home"
                    "packages"
                  ]
                else
                  [
                    "environment"
                    "systemPackages"
                  ]
              )
              (
                lib.mkOption {
                  type = lib.types.listOf lib.types.package;
                  default = [ ];
                }
              );
          config.programs.brix = {
            inherit enable;
            fallbackPackage = fallback;
          };
        })
      ];
    };
  system = (eval systemModule false true true).config;
  home = (eval homeModule true true true).config;
  combined = system.programs.brix.finalPackage;
  realNix = pkgs.callPackage ../with-brix.nix { } {
    inherit brix;
    fallbackPackage = pkgs.nix;
  };
  assertions =
    lib.all
      (
        home:
        lib.all
          (
            nixEnable:
            let
              module = if home then homeModule else systemModule;
              enabled = (eval module home true nixEnable).config;
              disabled = (eval module home false nixEnable).config;
              packages = c: if home then c.home.packages else c.environment.systemPackages;
            in
            enabled.nix.enable == nixEnable
            && enabled.nix.package == enabled.programs.brix.finalPackage
            && packages enabled == [ enabled.programs.brix.finalPackage ]
            && disabled.nix.package == fallback
            && packages disabled == [ ]
          )
          [
            true
            false
          ]
      )
      [
        true
        false
      ];

  # Evaluate against the actual NixOS options as well as the small interfaces.
  nixos = import (pkgs.path + "/nixos/lib/eval-config.nix") {
    inherit pkgs;
    system = pkgs.stdenv.hostPlatform.system;
    modules = [
      systemModule
      {
        system.stateVersion = "26.05";
        programs.brix = {
          enable = true;
          fallbackPackage = pkgs.nix;
        };
      }
    ];
  };
  profile = pkgs.buildEnv {
    name = "brix-module-profile";
    paths = system.environment.systemPackages;
  };
in
assert assertions;
assert home.nix.package == combined;
assert combined.version == fallback.version;
assert combined.out.version == fallback.version;
assert combined.pname == fallback.pname;
assert combined.fixtureMarker;
assert nixos.config.nix.package == nixos.config.programs.brix.finalPackage;
assert lib.elem nixos.config.nix.package nixos.config.environment.systemPackages;
pkgs.runCommand "brix-module-tests" { } ''
  export PATH=${profile}/bin:$PATH
  test "$(nix handled)" = brix
  test "$(nix 'argument with spaces' '*')" = $'fallback:<argument with spaces>\nfallback:<*>'
  # The configured delegate wins even over an inherited, incorrect setting.
  test "$(BRIX_DELEGATE_NIX=/missing nix handled)" = brix
  test "$(BRIX_DELEGATE_NIX=/missing nix delegated)" = 'fallback:<delegated>'
  # Bypass even commands brix implements, without consulting its delegate.
  test "$(BRIX_USE_NIX=1 BRIX_DELEGATE_NIX=/missing nix handled)" = 'fallback:<handled>'
  test "$(BRIX_USE_NIX=0 nix handled)" = brix
  test "$(BRIX_USE_NIX= nix handled)" = brix
  test "$(BRIX_USE_NIX=1 nix 'argument with spaces' '*')" = $'fallback:<argument with spaces>\nfallback:<*>'
  status=0
  BRIX_USE_NIX=1 nix fail >stdout 2>stderr || status=$?
  test "$status" = 23
  test ! -s stdout
  test "$(cat stderr)" = fallback-error
  status=0
  nix fail >stdout 2>stderr || status=$?
  test "$status" = 23
  test ! -s stdout
  test "$(cat stderr)" = fallback-error
  # Legacy multicall links must still resolve to Nix, never back to brix.
  test "$(nix-store handled)" = 'fallback:<handled>'
  test "$(nix-daemon handled)" = 'fallback:<handled>'
  test "$(cat ${combined}/share/nix/data)" = data
  test "$(cat ${combined}/libexec/helper)" = helper
  test "$(cat ${combined}/share/man/man1/nix.1)" = manual
  test "$(readlink -f ${combined}/bin/nix-store)" = ${fallback}/bin/nix
  # A real Nix binary checks argv[0], unlike the shell fixtures above.
  expectedVersion="$(${lib.getExe' pkgs.nix "nix"} --version)"
  test "$(BRIX_USE_NIX=1 ${realNix}/bin/nix --version)" = "$expectedVersion"
  touch "$out"
''
