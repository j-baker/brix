# Derivation fixtures for brix-store's oracle tests. Each attribute is
# instantiated by generate.sh with the real `nix`; the resulting .drv files
# (and those of their input derivations) are committed next to this file.
# Only builtins are used so the fixtures do not depend on nixpkgs.
let
  system = "x86_64-linux";
  plain = derivation {
    name = "plain";
    inherit system;
    builder = "/bin/sh";
    args = [
      "-c"
      "echo hi > $out"
    ];
    FOO = "bar";
    someInt = 42;
    someBool = true;
    someList = [
      "a"
      "b"
      3
    ];
  };
  escaping = derivation {
    name = "escaping";
    inherit system;
    builder = "/bin/sh";
    args = [
      "quote\"d"
      "back\\slash"
      "new\nline"
      "tab\tchar"
      "cr\rhere"
      "ünïcödé ☃"
      "$dollar"
    ];
    WEIRD = "a\"b\\c\nd\te\rf ☃ \${x}";
  };
  multiOutput = derivation {
    name = "multi-output";
    inherit system;
    builder = "/bin/sh";
    outputs = [
      "out"
      "dev"
      "lib"
      "doc"
    ];
  };
  src = builtins.toFile "src-file" "some source text\n";
  withInputs = derivation {
    name = "with-inputs";
    inherit system;
    builder = "/bin/sh";
    args = [
      plain
      "${multiOutput.dev}"
      "${multiOutput.lib}"
      src
    ];
    dep = plain;
    multi = multiOutput;
    passAsFile = [ "script" ];
    script = "echo ${escaping}";
  };
  fixedFlat = derivation {
    name = "fixed-flat";
    inherit system;
    builder = "/bin/sh";
    outputHash = "sha256-ungWv48Bz+pBQUDeXa4iI7ADYaOWF3qctBD/YfIAFa0=";
    outputHashMode = "flat";
  };
  fixedRecursive = derivation {
    name = "fixed-recursive";
    inherit system;
    builder = "/bin/sh";
    outputHashAlgo = "sha256";
    outputHash = "1b8m03r63zqhnjf7l5wnldhh7c134ap5vpj0850ymkq1iyzicy5s";
    outputHashMode = "recursive";
  };
  fixedSha1 = derivation {
    name = "fixed-sha1";
    inherit system;
    builder = "/bin/sh";
    outputHashAlgo = "sha1";
    outputHash = "a9993e364706816aba3e25717850c26c9cd0d89d";
  };
  fixedMd5Recursive = derivation {
    name = "fixed-md5-recursive";
    inherit system;
    builder = "/bin/sh";
    outputHashAlgo = "md5";
    outputHash = "d41d8cd98f00b204e9800998ecf8427e";
    outputHashMode = "recursive";
  };
  fixedSha512 = derivation {
    name = "fixed-sha512";
    inherit system;
    builder = "/bin/sh";
    outputHashAlgo = "sha512";
    outputHash = "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f";
  };
  usesFixed = derivation {
    name = "uses-fixed";
    inherit system;
    builder = "/bin/sh";
    inputs = [
      fixedFlat
      fixedRecursive
      fixedSha1
      fixedMd5Recursive
      fixedSha512
    ];
    also = plain;
  };
  structured = derivation {
    name = "structured";
    inherit system;
    builder = "/bin/sh";
    __structuredAttrs = true;
    outputs = [
      "out"
      "bin"
    ];
    nested = {
      a = 1;
      b = [
        true
        null
        "x"
      ];
      c.d = plain;
    };
    list = [
      1
      2.5
      "three"
    ];
    exportReferencesGraph.graph = [ plain ];
  };
  contentAddressed = derivation {
    name = "content-addressed";
    inherit system;
    builder = "/bin/sh";
    __contentAddressed = true;
    outputs = [
      "out"
      "dev"
    ];
  };
  caFlat = derivation {
    name = "ca-flat";
    inherit system;
    builder = "/bin/sh";
    __contentAddressed = true;
    outputHashMode = "flat";
    outputHashAlgo = "sha512";
  };
  usesCa = derivation {
    name = "uses-ca";
    inherit system;
    builder = "/bin/sh";
    dep = contentAddressed;
    dev = contentAddressed.dev;
    flat = caFlat;
    plainDep = plain;
  };
  drvName = derivation {
    name = "ends-with-drv.drv";
    inherit system;
    builder = "/bin/sh";
    outputHashAlgo = "sha256";
    outputHash = "ungWv48Bz+pBQUDeXa4iI7ADYaOWF3qctBD/YfIAFa0=";
    outputHashMode = "text";
  };
in
{
  inherit
    plain
    escaping
    multiOutput
    withInputs
    fixedFlat
    fixedRecursive
    fixedSha1
    fixedMd5Recursive
    fixedSha512
    usesFixed
    structured
    contentAddressed
    caFlat
    usesCa
    ;
}
