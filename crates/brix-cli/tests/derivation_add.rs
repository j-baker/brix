//! `brix derivation add` against `nix derivation add --dry-run`: the same
//! store path for what Nix accepts, filled in and checked as
//! `Derivation::parseJsonAndValidate` does, and Nix's error for what it
//! rejects.
//!
//! Every test needs `BRIX_NIX_BIN` (the devshell sets it).

use std::io::Write;
use std::process::{Command, Stdio};

use assert_cmd::prelude::*;
use brix_testutil::{require_env, Oracle, REAL_DAEMON};

#[derive(Debug, PartialEq, Eq)]
struct Output {
    status: i32,
    stdout: String,
    stderr: String,
}

fn run(mut cmd: Command, stdin: &str) -> Output {
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("run command");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    Output {
        status: out.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

fn nix(args: &[&str], stdin: &str) -> Output {
    let out = Oracle::new().args(args).stdin(stdin).output();
    Output {
        status: out.code(),
        stdout: out.stdout_lossy(),
        stderr: out.stderr_lossy(),
    }
}

/// `derivation add --dry-run` by both, with `store` as brix's store.
fn add_both(json: &str, store: Option<&str>) -> (Output, Output) {
    let theirs = nix(&["derivation", "add", "--dry-run"], json);
    let mut cmd = Command::cargo_bin("brix").expect("brix binary");
    if let Some(store) = store {
        cmd.args(["--store", store]);
    }
    cmd.args(["derivation", "add", "--dry-run"]);
    (run(cmd, json), theirs)
}

/// A derivation in `nix derivation show`'s format, without inputs.
fn drv(outputs: &str, env: &str, extra: &str) -> String {
    format!(
        r#"{{"args": ["-c", "echo"], "builder": "/bin/sh", "env": {env}, "inputs": {{"drvs": {{}}, "srcs": []}}, "name": "x", "outputs": {outputs}, "system": "x86_64-linux"{extra}, "version": 4}}"#
    )
}

#[test]
fn derivations_are_filled_in_and_checked_like_nix() {
    let deferred = r#"{"out": {}}"#;
    let cases = [
        // Filled in: the output path and its environment variable.
        drv(deferred, r#"{"out": ""}"#, ""),
        // Structured attributes are stored with their keys sorted.
        drv(
            deferred,
            r#"{"out": ""}"#,
            r#", "structuredAttrs": {"z": 1, "a": [2, {"y": 1.5, "b": null}]}"#,
        ),
        // A fixed output.
        drv(
            r#"{"out": {"hash": "sha256-47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU=", "method": "flat"}}"#,
            "{}",
            "",
        ),
        // The check after the fill-in fails: filling in `env.out` changed
        // the hash it was computed from.
        drv(deferred, "{}", ""),
        drv(deferred, "{}", r#", "structuredAttrs": {"z": 1}"#),
        // A wrong input-addressed path.
        drv(
            r#"{"out": {"path": "yrfn7smf6i23mcqbqnbicans1wlzh5aj-x"}}"#,
            "{}",
            "",
        ),
        // Malformed: keys of no output kind, a disabled kind, a store
        // path that is not a base name, a hash that is not SRI, values of
        // the wrong type, missing keys, the wrong version.
        drv(r#"{"out": {"path": "x", "y": 1}}"#, "{}", ""),
        drv(
            r#"{"out": {"hashAlgo": "sha256", "method": "nar"}}"#,
            "{}",
            "",
        ),
        drv(
            r#"{"out": {"hash": "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855", "method": "flat"}}"#,
            "{}",
            "",
        ),
        drv(deferred, r#"{"a": 1}"#, ""),
        drv(deferred, "{}", r#", "structuredAttrs": []"#)
            .replace(r#""version": 4"#, r#""version": 4.0"#),
        drv(deferred, "{}", "").replace(r#""drvs": {}, "#, ""),
        drv(deferred, "{}", "").replace(
            r#""srcs": []"#,
            r#""srcs": ["/nix/store/5v2nn7qarcn9l81hvbphg27gvqy4zjma-hello"]"#,
        ),
        drv(deferred, "{}", "").replace(r#", "version": 4"#, ""),
        drv(deferred, "{}", "").replace(r#""version": 4"#, r#""version": 3"#),
        drv(deferred, "{}", "").replace(r#""version": 4"#, r#""version": -4"#),
        "[1]".to_owned(),
    ];
    for json in &cases {
        let (ours, theirs) = add_both(json, Some("dummy://"));
        assert_eq!(ours, theirs, "{json}");
    }
}

/// `nix derivation show` and back: a derivation with an input derivation,
/// whose hash brix reads from the daemon's store.
#[test]
#[ignore = "writes derivations through the real Nix daemon; set BRIX_TEST_REAL_DAEMON=1"]
fn derivation_show_round_trips() {
    require_env(REAL_DAEMON);
    let expr = r#"derivation { name = "brix-add-test"; system = "x86_64-linux"; builder = "/bin/sh"; __structuredAttrs = true; b = 1; a = [ "x" ]; dep = derivation { name = "brix-add-dep"; system = "x86_64-linux"; builder = "/bin/sh"; }; }"#;
    let shown = nix(&["derivation", "show", "--impure", "--expr", expr], "");
    assert_eq!(shown.status, 0, "{}", shown.stderr);
    let doc: serde_json::Value = serde_json::from_str(&shown.stdout).unwrap();
    let (path, drv) = doc["derivations"]
        .as_object()
        .unwrap()
        .iter()
        .next()
        .unwrap();
    let json = drv.to_string();
    let (ours, theirs) = add_both(&json, None);
    assert_eq!(theirs.status, 0, "{}", theirs.stderr);
    assert_eq!(theirs.stdout, format!("/nix/store/{path}\n"));
    assert_eq!(ours, theirs);
}
