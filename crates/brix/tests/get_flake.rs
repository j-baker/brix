//! `builtins.getFlake` reports a failure inside the flake as the error
//! itself, traces included, as Nix does, whether it comes from locking
//! (an input's `flake.nix`) or from calling the flake (its `outputs`).

mod common;

use std::path::Path;

use brix::{Config, Format, Session};
use brix_testutil::Oracle;
use common::nix;

/// `nix eval --expr expr`'s stderr; it must fail.
fn nix_eval_error(expr: &str) -> String {
    let out = Oracle::new().args(["eval", "--expr", expr]).output();
    assert!(!out.success(), "nix evaluated {expr}");
    String::from_utf8(out.stderr).expect("nix printed UTF-8")
}

fn brix_eval_error(expr: &str) -> String {
    let session = Session::new(Config::default()).unwrap();
    session
        .eval_expr_to_string(expr, "/", Format::Nix)
        .expect_err(expr)
        .render(false)
}

/// `(builtins.getFlake "path:<dir>?narHash=…").x`, locked as pure
/// evaluation requires.
fn get_flake_x(dir: &Path) -> String {
    let hash = nix(&["hash", "path", "--sri", dir.to_str().unwrap()]);
    format!(
        "(builtins.getFlake \"path:{}?narHash={}\").x",
        dir.display(),
        hash.trim()
    )
}

/// An input's `flake.nix` fails to parse while the lock is computed: the
/// parse error, under the lock algorithm's traces.
#[test]
fn a_failure_while_locking_is_rethrown() {
    let tmp = tempfile::tempdir().unwrap();
    let dep = tmp.path().join("dep");
    std::fs::create_dir(&dep).unwrap();
    std::fs::write(dep.join("flake.nix"), "{ outputs = { self }: { } \n").unwrap();
    let dep_hash = nix(&["hash", "path", "--sri", dep.to_str().unwrap()]);
    let root = tmp.path().join("root");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(
        root.join("flake.nix"),
        format!(
            "{{ inputs.dep.url = \"path:{}?narHash={}\"; outputs = {{ self, dep }}: {{ x = dep; }}; }}\n",
            dep.display(),
            dep_hash.trim()
        ),
    )
    .unwrap();
    let expr = get_flake_x(&root);
    let ours = brix_eval_error(&expr);
    assert!(ours.contains("syntax error"), "{ours}");
    assert_eq!(format!("{ours}\n"), nix_eval_error(&expr));
}

/// Calling the flake fails (`outputs` throws): that error, not a bare
/// "flake evaluation failed".
#[test]
fn a_failure_while_calling_the_flake_is_rethrown() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(
        tmp.path().join("flake.nix"),
        "{ outputs = { self }: throw \"boom in outputs\"; }\n",
    )
    .unwrap();
    let expr = get_flake_x(tmp.path());
    let ours = brix_eval_error(&expr);
    let theirs = nix_eval_error(&expr);
    // brix's `call-flake.nix` is laid out differently from Nix's, so the
    // positions of its traces differ; the rest is the same.
    let shape = |e: &str| -> Vec<String> {
        e.lines()
            .filter(|l| l.trim_start().starts_with('…') || l.trim_start().starts_with("error:"))
            .map(str::to_owned)
            .collect()
    };
    assert_eq!(shape(&ours), shape(&theirs), "{ours}\n---\n{theirs}");
    assert!(ours.ends_with("error: boom in outputs"), "{ours}");
}
