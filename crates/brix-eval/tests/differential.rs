//! Differential tests: expressions evaluated by brix and by the reference
//! `nix-instantiate --eval --strict --pure-eval` must produce the same
//! stdout and stderr, byte for byte, in the plain, `--json` and `--xml`
//! output formats.
//!
//! The table focuses on behaviour that is easy to get subtly wrong:
//! laziness and forcing order, `with` scoping, string contexts, number
//! formatting, error messages and traces.

mod common;

use brix_eval::Config;
use brix_testutil::{nix_src, require_env, REAL_STORE};
use common::{compare_all, run_brix_expr, run_nix_expr, same_outcome, Format, TestHost};

use common::cases::DIFFERENTIAL_CASES as CASES;

/// The Nix source tree is a store path, so pure evaluation may read it;
/// `{src}` in a case stands for it. The working directory `/pwd` is
/// mounted but, as in Nix's pure mode, not readable.
fn compare(expr: &str, format: Format) -> Result<(), String> {
    compare_with(expr, format, false)
}

fn compare_with(expr: &str, format: Format, real_store: bool) -> Result<(), String> {
    let src = nix_src();
    let src_s = src.display().to_string();
    assert!(
        src_s.starts_with("/nix/store/"),
        "BRIX_NIX_SRC must be a store path for the differential tests"
    );
    let expr = expr
        .replace("{tree}", &tree_expr())
        .replace("{src}", &src_s);
    let expr = expr.as_str();
    // The working directory is an empty directory: pure mode forbids
    // reading it anyway, and it must not be a prefix of the store paths
    // that appear in the output. The store directory exists and is empty.
    let pwd = tempfile::tempdir().expect("temporary directory");
    let store = tempfile::tempdir().expect("temporary directory");
    let host = TestHost::with_allowed(
        &[
            ("/pwd", pwd.path()),
            (&src_s, &src),
            ("/nix/store", store.path()),
        ],
        &[&src_s],
    );
    // The reference binary is Determinate Nix, which runs the parallel
    // evaluator (`eval-cores = 0`).
    let config = Config {
        eval_cores: 0,
        ..Config::default()
    };
    let ours = run_brix_expr(&host, config, expr, "/pwd", format);
    let theirs = run_nix_expr(expr, pwd.path(), format, &[], real_store);
    same_outcome(&format!("{expr}  [{format:?}]"), &ours, &theirs)
}

fn run_all(format: Format) {
    std::thread::Builder::new()
        .stack_size(1 << 30)
        .spawn(move || {
            compare_all(&format!("differential [{format:?}]"), CASES, |expr| {
                compare(expr, format)
            })
        })
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn plain_output_matches_nix() {
    run_all(Format::Plain);
}

#[test]
fn json_output_matches_nix() {
    run_all(Format::Json);
}

#[test]
fn xml_output_matches_nix() {
    run_all(Format::Xml);
}

/// Expressions that read store paths or depend on the store knowing a
/// derivation (`drvPath` context closures). The reference `nix` can only
/// agree when it uses the real store, so these tests are ignored unless
/// asked for, and then need `BRIX_TEST_REAL_STORE=1` to confirm that
/// `nix-instantiate` may write the derivations it instantiates into the
/// user's store:
///
/// ```text
/// BRIX_TEST_REAL_STORE=1 cargo test -p brix-eval --test differential -- --ignored store_
/// ```
const STORE_CASES: &[&str] = &[
    "{tree}",
    "{tree}.outPath",
    "let t = {tree}.outPath; in t + \"/lib.nix\" + \"x\"",
    "let t = {tree}.outPath; in builtins.pathExists (t + \"/lib.nix\")",
    "let t = {tree}.outPath; in builtins.pathExists (t + \"/nope\")",
    "let t = {tree}.outPath; in builtins.readFile (t + \"/data\")",
    "let t = {tree}.outPath; in builtins.readFileType t",
    "let t = {tree}.outPath; in builtins.readDir (t + \"/readDir\")",
    "let t = {tree}.outPath; in builtins.hashFile \"sha256\" (t + \"/data\")",
    "let t = {tree}.outPath; in import (t + \"/lib.nix\")",
    "let t = {tree}.outPath; in (import (t + \"/lib.nix\")).range 1 3",
    "let t = {tree}.outPath; in import (t + \"/nope.nix\")",
    "let t = {tree}.outPath; in builtins.readFile (t + \"/nope\")",
    "let t = {tree}.outPath; in builtins.readDir (t + \"/data\")",
    "let t = {tree}.outPath; in builtins.findFile [ { prefix = \"\"; path = t; } ] \"lib.nix\"",
    "let t = {tree}.outPath; in builtins.findFile [ { prefix = \"foo\"; path = t; } ] \"foo/lib.nix\"",
    "let t = {tree}.outPath; in \"${/. + t + \"/readDir\"}\"",
    "let t = {tree}.outPath; in builtins.path { path = t + \"/readDir\"; }",
    "let t = {tree}.outPath; in builtins.path { path = t + \"/readDir\"; name = \"custom\"; }",
    "let t = {tree}.outPath; in builtins.path { path = t + \"/readDir\"; filter = p: t: t != \"symlink\"; }",
    "let t = {tree}.outPath; in builtins.filterSource (p: t: baseNameOf p != \"bar\") (t + \"/readDir\")",
    "let t = {tree}.outPath; in builtins.path { path = t + \"/data\"; recursive = false; }",
    "let t = {tree}.outPath; in builtins.path { path = t + \"/data\"; sha256 = \"0000000000000000000000000000000000000000000000000000000000000000\"; }",
    // Not compared: `builtins.getContext t`. With lazy trees Nix mounts the
    // input at a random store path that only gets rewritten when a string
    // is observed; brix uses the content-addressed path from the start.
    "let t = {tree}.outPath; in builtins.toString (/. + t)",
    "let a = derivation { name = \"a\"; builder = \"/bin/sh\"; system = \"x86_64-linux\"; }; b = derivation { name = \"b\"; builder = \"${a}/bin/sh\"; system = \"x86_64-linux\"; dep = a.drvPath; }; in [ b.drvPath b.outPath (builtins.getContext b.drvPath) (builtins.getContext \"${b}\") ]",
];

// Not compared: `builtins.readFile (builtins.toFile "foo" "bar")`. In
// `nix-instantiate --eval` the store is read-only, so Nix cannot read the
// file back ("path … is not valid"), while brix serves it from the
// virtual store, as `nix eval` would.

/// `{tree}`: `builtins.fetchTree` of the corpus directory as a locked path
/// input, which pure evaluation may then read.
fn tree_expr() -> String {
    let src = nix_src();
    let lang = src.join("tests/functional/lang");
    let accessor = brix_store::FsAccessor::new(&lang);
    let (hash, _) = brix_store::nar::hash(
        &accessor,
        &brix_store::CanonPath::root(),
        brix_store::HashAlgo::Sha256,
    )
    .expect("hash the corpus directory");
    format!(
        "(builtins.fetchTree {{ type = \"path\"; path = \"{}\"; narHash = \"{}\"; }})",
        lang.display(),
        hash.to_string(brix_store::HashFormat::Sri, true)
    )
}

fn run_store_cases(format: Format) {
    // The cases write derivations into the user's store.
    require_env(REAL_STORE);
    // The store must already know the derivations the cases make:
    // `nix-instantiate --eval` is read-only, and it hashes a dependent
    // of a derivation it only names (`dep = a.drvPath`) differently
    // while the store lacks that `.drv`. A developer's store has them
    // from earlier runs; a fresh one (a new machine, the flake check's
    // daemon) does not. The `{tree}` cases are left alone: their fetched
    // tree must NOT be in the store — read-only evaluation never adds
    // it, and once it is valid, `nix` switches to the store's accessor
    // and renders the tree's paths as `«path:…»`.
    static WARM: std::sync::Once = std::sync::Once::new();
    WARM.call_once(|| {
        for case in STORE_CASES {
            if !case.contains("{tree}") {
                brix_testutil::warm_real_store(case);
            }
        }
    });
    std::thread::Builder::new()
        .stack_size(1 << 30)
        .spawn(move || {
            compare_all(
                &format!("differential store cases [{format:?}]"),
                STORE_CASES,
                |expr| compare_with(expr, format, true),
            )
        })
        .unwrap()
        .join()
        .unwrap();
}

#[test]
#[ignore = "writes derivations into the user's store; set BRIX_TEST_REAL_STORE=1"]
fn store_plain_output_matches_nix() {
    run_store_cases(Format::Plain);
}

#[test]
#[ignore = "writes derivations into the user's store; set BRIX_TEST_REAL_STORE=1"]
fn store_json_output_matches_nix() {
    run_store_cases(Format::Json);
}

#[test]
#[ignore = "writes derivations into the user's store; set BRIX_TEST_REAL_STORE=1"]
fn store_xml_output_matches_nix() {
    run_store_cases(Format::Xml);
}
