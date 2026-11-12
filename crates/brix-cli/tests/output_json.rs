//! The JSON documents of the CLI's output module against what the
//! reference `nix` prints: `nix derivation show` compact and `--pretty`
//! (nlohmann's `dump()` and `dump(2)`), and `nix flake metadata --json`.
//!
//! Every test needs `BRIX_NIX_BIN` (the devshell sets it).

use std::path::PathBuf;
use std::process::Command;

use assert_cmd::prelude::*;

fn nix_bin() -> PathBuf {
    PathBuf::from(
        std::env::var("BRIX_NIX_BIN").expect(
            "BRIX_NIX_BIN must point at the reference nix binary (run inside `nix develop`)",
        ),
    )
}

const ORACLE_NIX_CONFIG: &str =
    "experimental-features = nix-command flakes\neval-cache = false\nlazy-trees = true\neval-cores = 0";

/// The successful command's stdout.
fn stdout(mut cmd: Command) -> String {
    let out = cmd.output().expect("run command");
    assert!(
        out.status.success(),
        "{cmd:?} failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).expect("UTF-8 output")
}

fn nix(args: &[&str]) -> String {
    let mut cmd = Command::new(nix_bin());
    cmd.args(args).env("NIX_CONFIG", ORACLE_NIX_CONFIG);
    stdout(cmd)
}

fn brix(args: &[&str]) -> String {
    let mut cmd = Command::cargo_bin("brix").expect("brix binary");
    cmd.args(["--store", "dummy://"]).args(args);
    stdout(cmd)
}

/// Structured attributes give nested objects and arrays, empty ones,
/// escapes, non-ASCII text and a float; a fixed output gives `hash` and
/// `method`.
const DRV: &str = r#"derivation {
  name = "x"; system = "x86_64-linux"; builder = "/bin/sh";
  __structuredAttrs = true;
  e = { }; l = [ ]; n = [ [ 1 ] { a = "é\t\"\\"; b = null; } ]; f = 1.5; t = true;
  outputHashMode = "flat"; outputHashAlgo = "sha256";
  outputHash = "sha256-47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU=";
}"#;

#[test]
fn derivation_show_json_is_nix_s_compact_and_pretty() {
    assert_eq!(
        brix(&["derivation", "show", "--expr", DRV]),
        nix(&["derivation", "show", "--impure", "--expr", DRV])
    );
    assert_eq!(
        brix(&["derivation", "show", "--pretty", "--expr", DRV]),
        nix(&["derivation", "show", "--pretty", "--impure", "--expr", DRV])
    );
}

/// A local flake with an input: `fingerprint` aside (brix keeps no
/// evaluation cache), the same document.
#[test]
fn flake_metadata_json_is_nix_s() {
    let tmp = tempfile::tempdir().unwrap();
    let dep = tmp.path().join("dep");
    std::fs::create_dir(&dep).unwrap();
    std::fs::write(dep.join("flake.nix"), "{ outputs = { self }: { }; }\n").unwrap();
    let root = tmp.path().join("root");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(
        root.join("flake.nix"),
        format!(
            "{{ description = \"a \\\"test\\\" flake\"; inputs.dep.url = \"path:{}\"; outputs = {{ self, dep }}: {{ }}; }}\n",
            dep.display()
        ),
    )
    .unwrap();
    let flake_url = format!("path:{}", root.display());
    // Nix writes the lock file; both then read it.
    nix(&["flake", "lock", &flake_url]);
    let json = |out: String| -> serde_json::Value {
        let mut doc: serde_json::Value = serde_json::from_str(&out).expect("JSON");
        doc.as_object_mut()
            .expect("an object")
            .remove("fingerprint");
        doc
    };
    assert_eq!(
        json(brix(&["flake", "metadata", "--json", &flake_url])),
        json(nix(&["flake", "metadata", "--json", &flake_url]))
    );
}

/// What a command printed on stderr, without the reference nix's
/// remarks about its environment ([`brix_testutil::is_noise`]), and
/// whether it failed.
fn stderr_of(mut cmd: Command) -> (bool, Vec<u8>) {
    let out = cmd.output().expect("run command");
    (
        !out.status.success(),
        brix_testutil::strip_noise(&out.stderr),
    )
}

/// Bytes as text with every byte that is not printable ASCII escaped:
/// readable when an assertion fails, and as exact as the bytes.
fn escaped(bytes: &[u8]) -> String {
    bytes.escape_ascii().to_string()
}

/// `l` is the byte 0xc3, which is not UTF-8 on its own.
const LATIN1: &str = r#"let l = builtins.substring 0 1 "é"; in "#;

/// Output names are bytes, and a floating content-addressed output keeps
/// one that is not UTF-8 (`a`, 0xc3, `b`): nlohmann then refuses the
/// derivation's JSON, at the environment variable named after the output
/// (which comes first), and a derivation using that output fails at its
/// input's output name (its environment holds only a placeholder). The
/// installable `d.${o}` names the output by its `outputName`, bytes too.
/// Nix writes into a scratch store.
#[test]
fn derivation_show_fails_like_nix_on_output_names_that_are_not_utf8() {
    let ca = r#"let l = builtins.substring 0 1 "é"; o = "a" + l + "b"; d = derivation { name = "x"; system = "x86_64-linux"; builder = "/bin/sh"; outputs = [ "out" o ]; __contentAddressed = true; }; in "#;
    let xp = ["--extra-experimental-features", "ca-derivations"];
    for drv in [
        "d",
        r#"derivation { name = "e"; system = "x86_64-linux"; builder = "/bin/sh"; p = d.${o}; }"#,
        "d.${o}",
    ] {
        let expr = format!("{ca}{drv}");
        let mut ours = Command::cargo_bin("brix").expect("brix binary");
        ours.args(["--store", "dummy://"])
            .args(xp)
            .args(["derivation", "show", "--expr", &expr]);
        let store = brix_testutil::scratch_store();
        let mut theirs = Command::new(nix_bin());
        theirs
            .arg("--store")
            .arg(store.path())
            .args(xp)
            .args(["derivation", "show", "--impure", "--expr", &expr])
            .env("NIX_CONFIG", ORACLE_NIX_CONFIG);
        let (ours, theirs) = (stderr_of(ours), stderr_of(theirs));
        assert!(theirs.0, "{expr}");
        assert_eq!(
            (ours.0, escaped(&ours.1)),
            (theirs.0, escaped(&theirs.1)),
            "{expr}"
        );
    }
}

/// A derivation's strings are bytes, which nlohmann refuses to write as
/// JSON when they are not UTF-8: `nix derivation show` fails with its
/// error, for the first such string in the document.
#[test]
fn derivation_show_fails_like_nix_on_strings_that_are_not_utf8() {
    for drv in [
        r#"derivation { name = "x"; system = "x86_64-linux"; builder = "/bin/sh"; args = [ "a" ("b" + l + "c") ]; }"#,
        r#"derivation { name = "x"; system = "x86_64-linux"; builder = "/bin/sh"; ${l} = "a"; }"#,
        // `env` comes before `system` in the document.
        r#"derivation { name = "x"; system = "x86_64-linux" + l; builder = "/bin/sh"; v = "ab" + l + "x"; }"#,
        r#"derivation { name = "x"; system = "x86_64-linux" + l; builder = "/bin/sh"; }"#,
    ] {
        let expr = format!("{LATIN1}{drv}");
        let mut ours = Command::cargo_bin("brix").expect("brix binary");
        ours.args(["--store", "dummy://", "derivation", "show", "--expr", &expr]);
        let mut theirs = Command::new(nix_bin());
        theirs
            .args(["derivation", "show", "--impure", "--expr", &expr])
            .env("NIX_CONFIG", ORACLE_NIX_CONFIG);
        let (ours, theirs) = (stderr_of(ours), stderr_of(theirs));
        assert!(theirs.0, "{expr}");
        assert_eq!(
            (ours.0, escaped(&ours.1)),
            (theirs.0, escaped(&theirs.1)),
            "{expr}"
        );
    }
}

/// A derivation whose strings are not UTF-8: the `.drv` file Nix writes
/// for it is the one brix computes, it parses back to the same
/// derivation, and `import` of it gives what Nix's does.
#[test]
fn derivations_that_are_not_utf8_read_back_as_in_nix() {
    let drv = format!(
        "{LATIN1}{}",
        r#"derivation { name = "x"; builder = "/bin/sh" + l; system = "x86_64-linux" + l; args = [ l "-c" ]; ${l} = l + "\n"; a = l; }"#
    );
    let mut config = brix::Config::default();
    config.compiled.enabled = false;
    let session = brix::Session::new(config).unwrap();
    let store_dir = session.config().store_dir().clone();
    let drv_path = session
        .eval_expr_to_string(format!("({drv}).drvPath"), "/", brix::Format::Raw)
        .unwrap();
    // `nix eval` writes the derivation to the store.
    assert_eq!(
        nix(&["eval", "--raw", "--expr", &format!("({drv}).drvPath")]),
        drv_path
    );
    let mut cat = Command::new(nix_bin());
    cat.args(["store", "cat", &drv_path])
        .env("NIX_CONFIG", ORACLE_NIX_CONFIG);
    let theirs = cat.output().expect("run nix store cat");
    assert!(theirs.status.success());
    let path = store_dir.parse(&drv_path).unwrap();
    let ours = session.derivation(&path).expect("the derivation evaluated");
    assert_eq!(
        escaped(&ours.to_aterm(&store_dir).unwrap()),
        escaped(&theirs.stdout)
    );
    assert_eq!(
        brix_store::Derivation::parse(
            &store_dir,
            &theirs.stdout,
            "x",
            brix_store::XpSettings::NONE
        )
        .unwrap(),
        ours
    );

    let import = format!("let i = import ({drv}).drvPath; in [ i.drvPath i.outPath i.name ]");
    let mut ours = session
        .eval_expr_cursor(&import, "/")
        .unwrap()
        .render_bytes(brix::Format::Nix)
        .unwrap();
    ours.push(b'\n');
    let mut eval = Command::new(nix_bin());
    eval.args(["eval", "--expr", &import])
        .env("NIX_CONFIG", ORACLE_NIX_CONFIG);
    let theirs = eval.output().expect("run nix eval");
    assert!(theirs.status.success());
    assert_eq!(escaped(&ours), escaped(&theirs.stdout));
}
