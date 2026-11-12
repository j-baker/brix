//! The experimental features the store primitives check, against the
//! reference `nix-instantiate --eval --strict`: BLAKE3 needs
//! `blake3-hashes` however a hash names it, and `builtin:substitute`
//! derivations get the checks of `processDerivationOutputPaths` and
//! `type()`, the latter behind `baked-derivations`.

mod common;

use brix_eval::Config;
use common::{run_brix_expr, run_nix_expr, Format, TestHost};

/// `abc` hashed with BLAKE3, in the forms a hash can take.
const BLAKE3_SRI: &str = "blake3-ZDezrDhGUTP/tjt1JzqNtUjFWEZdedsD/TWcbNW9nYU=";
const BLAKE3_BASE16: &str = "6437b3ac38465133ffb63b75273a8db548c558465d79db03fd359c6cd5bd9d85";

fn blake3_cases() -> Vec<String> {
    let drv = |attrs: &str| {
        format!(
            r#"let d = derivation {{ name = "b"; builder = "/bin/sh"; system = "x86_64-linux"; {attrs} }}; in [ d.drvPath d.outPath ]"#
        )
    };
    vec![
        r#"builtins.hashString "blake3" "abc""#.to_owned(),
        format!(r#"builtins.convertHash {{ hash = "{BLAKE3_SRI}"; toHashFormat = "base16"; }}"#),
        format!(
            r#"builtins.convertHash {{ hash = "blake3:{BLAKE3_BASE16}"; toHashFormat = "sri"; }}"#
        ),
        format!(
            r#"builtins.convertHash {{ hash = "{BLAKE3_BASE16}"; hashAlgo = "blake3"; toHashFormat = "nix32"; }}"#
        ),
        // A BLAKE3 prefix is refused before the algorithms are compared.
        format!(
            r#"builtins.convertHash {{ hash = "{BLAKE3_SRI}"; hashAlgo = "sha256"; toHashFormat = "base16"; }}"#
        ),
        drv(&format!(r#"outputHash = "{BLAKE3_SRI}";"#)),
        drv(&format!(
            r#"outputHashMode = "recursive"; outputHashAlgo = "blake3"; outputHash = "{BLAKE3_BASE16}";"#
        )),
        drv(&format!(
            r#"__structuredAttrs = true; outputHashAlgo = "blake3"; outputHash = "{BLAKE3_BASE16}";"#
        )),
    ]
}

fn substitute_cases() -> Vec<String> {
    let drv = |attrs: &str| {
        format!(r#"(derivation {{ name = "s"; builder = "builtin:substitute"; {attrs} }}).drvPath"#)
    };
    vec![
        drv(r#"system = "x86_64-linux";"#),
        drv(r#"system = "builtin";"#),
        drv(r#"system = "builtin"; args = [ "x" ];"#),
        drv(
            r#"system = "builtin"; outputHash = "sha256-ungWv48Bz+pBQUDeXa4iI7ADYaOWF3qctBD/YfIAFa0=";"#,
        ),
    ]
}

/// Evaluates `expr` with brix and with Nix, both with `features` enabled,
/// and describes any difference in success, stdout or stderr.
fn compare(expr: &str, features: &[&str]) -> Result<(), String> {
    let pwd = tempfile::tempdir().expect("create a working directory");
    let host = TestHost::new(&[("/pwd", pwd.path())]);
    let mut config = Config {
        // The reference binary is Determinate Nix, which runs the
        // parallel evaluator (`eval-cores = 0`).
        eval_cores: 0,
        ..Config::default()
    };
    let mut extra = Vec::new();
    for feature in features {
        config.experimental_features.insert((*feature).to_owned());
        extra.extend(["--extra-experimental-features", feature]);
    }
    let ours = run_brix_expr(&host, config, expr, "/pwd", Format::Plain);
    let theirs = run_nix_expr(expr, pwd.path(), Format::Plain, &extra, false);
    if ours.ok != theirs.ok || ours.stdout != theirs.stdout || ours.stderr != theirs.stderr {
        return Err(format!(
            "{expr}  [features: {features:?}]\n--- nix (ok={})\n{}{}--- brix (ok={})\n{}{}",
            theirs.ok, theirs.stdout, theirs.stderr, ours.ok, ours.stdout, ours.stderr
        ));
    }
    Ok(())
}

fn compare_all(cases: &[String], feature_sets: &[&[&str]]) {
    let mut failures = Vec::new();
    for features in feature_sets {
        for expr in cases {
            if let Err(msg) = compare(expr, features) {
                failures.push(msg);
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{} evaluations differ from nix:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

#[test]
fn blake3_needs_its_experimental_feature_like_nix() {
    compare_all(&blake3_cases(), &[&[], &["blake3-hashes"]]);
}

#[test]
fn substitute_derivations_are_checked_like_nix() {
    compare_all(&substitute_cases(), &[&[], &["baked-derivations"]]);
}

/// Derivation output names that are not UTF-8 (`o` is `a` and the byte
/// 0xc3), which are bytes in brix as in Nix: an input-addressed output's
/// path is computed from them and refused, with the path in the message;
/// floating content-addressed and impure outputs keep them, in their
/// placeholders, in string contexts (`getContext`, `appendContext`, the
/// inputs of a derivation using one, the messages that show a context)
/// and in `builtins.placeholder`.
fn output_name_cases() -> Vec<String> {
    let prelude = r#"let l = builtins.substring 0 1 "é"; o = "a" + l; d = a: derivation ({ name = "x"; builder = "/bin/sh"; system = "x86_64-linux"; outputs = [ "out" o ]; } // a); c = d { __contentAddressed = true; }; in "#;
    [
        "(d { }).drvPath",
        "[ c.drvPath c.outPath c.${o}.outPath (builtins.getContext c.${o}.outPath) c.outputs c.${o}.outputName ]",
        r#"let e = derivation { name = "e"; builder = "/bin/sh"; system = "x86_64-linux"; p = c.${o}; }; in [ e.drvPath e.outPath ]"#,
        "builtins.getContext (builtins.appendContext \"x\" { ${builtins.unsafeDiscardStringContext c.drvPath} = { outputs = [ o ]; }; })",
        "builtins.addDrvOutputDependencies c.${o}.outPath",
        r#"builtins.toFile "f" c.${o}.outPath"#,
        r#"builtins.hashString c.${o}.outPath "x""#,
        "builtins.unsafeDiscardOutputDependency c.${o}.outPath",
        "let i = d { __impure = true; }; in [ i.drvPath i.${o}.outPath ]",
        "builtins.placeholder o",
    ]
    .iter()
    .map(|case| format!("{prelude}{case}"))
    .collect()
}

#[test]
fn output_names_that_are_not_utf8_are_bytes_like_nix() {
    compare_all(
        &output_name_cases(),
        &[&["ca-derivations", "impure-derivations"]],
    );
}
