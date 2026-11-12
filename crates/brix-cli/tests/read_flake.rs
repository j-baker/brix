//! `readFlake`'s errors for a `flake.nix` of the wrong shape, positions
//! included, against the reference `nix`: the last error line, and for
//! the traces, all of stderr. And `path` inputs, which `readFlake` takes
//! by how they are written and the `flake` commands lock as Nix does.
//!
//! Needs `BRIX_NIX_BIN` (the devshell sets it).

use std::path::Path;
use std::process::Command;

use assert_cmd::prelude::*;
use brix_testutil::{strip_noise, Oracle};

/// The last `error:` line of `cmd`'s stderr, and whether it failed.
fn last_error(cmd: &mut Command) -> (bool, Option<String>) {
    let out = cmd.output().expect("run command");
    (out.status.success(), last_error_line(&out.stderr))
}

fn last_error_line(stderr: &[u8]) -> Option<String> {
    String::from_utf8_lossy(stderr)
        .lines()
        .rfind(|l| l.trim_start().starts_with("error:"))
        .map(str::to_owned)
}

#[test]
fn read_flake_errors_name_their_positions_like_nix() {
    same_error_as_nix(&[
        r#"description = 1; outputs = _: {};"#,
        r#"description = "x"; outputs = 1;"#,
        r#"outputs = _: {}; foo = 1;"#,
        r#"inputs.a.url = 1; outputs = _: {};"#,
        r#"outputs = _: {}; nixConfig = 1;"#,
        r#"inputs.a = 1; outputs = _: {};"#,
        r#"inputs = 1; outputs = _: {};"#,
        r#"inputs.a.flake = "x"; outputs = _: {};"#,
        r#"inputs.a.follows = 1; outputs = _: {};"#,
        r#"inputs.a = { url = "github:a/b"; follows = "c"; }; outputs = _: {};"#,
        r#"inputs.a.inputs.self = {}; outputs = _: {};"#,
        r#"inputs.a.foo = "x"; outputs = _: {};"#,
    ]);
}

/// `readFlake` forces only what is trivial to force (`forceTrivialValue`:
/// a set, list or function not evaluated yet), and input attributes
/// other than `url`, `flake`, `buildTime`, `follows` and `inputs` not at
/// all (`parseFlakeInputAttr`): anything computed is a thunk where it is
/// read. And `flake.nix` must be a set as written (`mustBeTrivial`).
#[test]
fn read_flake_forces_only_trivial_values_like_nix() {
    same_error_as_nix(&[
        r#"description = "a" + "b"; outputs = _: {};"#,
        r#"description = let d = "ab"; in d; outputs = _: {};"#,
        r#"description = with { d = "x"; }; d; outputs = _: {};"#,
        r#"outputs = (x: x) (_: {});"#,
        r#"inputs = { x.url = "path:/tmp"; } // {}; outputs = _: {};"#,
        r#"inputs.x = { type = "path"; path = "/" + "tmp"; }; outputs = _: {};"#,
        r#"inputs.x = { type = "path"; path = [ 1 ]; }; outputs = _: {};"#,
        r#"inputs.x = { type = "path"; path = "/tmp"; lastModified = 1 + 1; }; outputs = _: {};"#,
        r#"inputs.x.url = "path:" + "/tmp"; outputs = _: {};"#,
        r#"inputs.x = { url = "path:/tmp"; flake = !true; }; outputs = _: {};"#,
        r#"inputs.x = { url = "path:/tmp"; follows = "a" + "b"; }; outputs = _: {};"#,
        r#"inputs.self.submodules = !true; outputs = _: {};"#,
        r#"nixConfig.x = "a" + "b"; outputs = _: {};"#,
        r#"nixConfig.x = [ ("a" + "b") ]; outputs = _: {};"#,
        r#"nixConfig = { x = "a"; } // {}; outputs = _: {};"#,
    ]);
    for flake_nix in [
        r#"rec { d = "x"; description = d; outputs = _: {}; }"#,
        r#"rec { o = _: {}; outputs = o; }"#,
        r#"let d = "x"; in { description = d; outputs = _: {}; }"#,
        r#"({ description = 1; outputs = _: {}; })"#,
    ] {
        same_error_for(flake_nix);
    }
}

/// Paths: an input may not name an absolute path, inside the flake's
/// directory or not ("must be in the same source tree"), and a path
/// setting must be one pure evaluation may read and copy to the store.
#[test]
fn read_flake_checks_paths_like_nix() {
    same_error_as_nix(&[
        r#"inputs.x.url = /tmp; outputs = _: {};"#,
        r#"inputs.x = { url = /tmp; flake = false; }; outputs = _: {};"#,
        r#"inputs.x.foo = ./.; outputs = _: {};"#,
        r#"nixConfig.x = /tmp; outputs = _: {};"#,
        r#"nixConfig.x = ./missing; outputs = _: {};"#,
        // `publicKeys` of another type than a string needs a feature.
        r#"inputs.x = { type = "git"; url = "file:///x"; publicKeys = [ ]; }; outputs = _: {};"#,
    ]);
    // The flake's own directory, named by its absolute path.
    let dir = tempfile::tempdir().unwrap();
    same_error_for_in(
        &format!(
            "{{ inputs.x.url = {}/sub; outputs = _: {{}}; }}\n",
            dir.path().display()
        ),
        dir,
    );
}

/// An input's `url` path is of the flake's own tree when it is written
/// relative to `flake.nix`, wherever it leads, and of another when it is
/// written absolute, wherever it lies (Nix tells them by their accessor):
/// a relative one that leaves the tree fails when its `flake.nix` is
/// read, as Nix's does, and an absolute one into the store directory is
/// refused. Through a variable too.
#[test]
fn read_flake_tells_path_literals_apart_like_nix() {
    for body in [
        r#"inputs.a.url = ../../x; outputs = _: {};"#,
        r#"inputs.a.url = ../../../../../../../../tmp; outputs = _: {};"#,
        r#"inputs.a.url = /nix/store/x; outputs = _: {};"#,
        r#"inputs.a.url = /nix/store; outputs = _: {};"#,
    ] {
        same_metadata_failure(&format!("{{\n  {body}\n}}\n"), &[]);
    }
    for flake_nix in [
        "rec { p = /tmp; inputs.a = { url = p; flake = false; }; outputs = _: {}; }\n",
        // Accepted, so the unsupported attribute is what fails.
        "rec { p = ../../x; inputs.a = { url = p; flake = false; }; outputs = _: {}; }\n",
    ] {
        same_metadata_failure(flake_nix, &[]);
    }
    // A relative one that leaves the tree, of an input that is no flake,
    // is locked as written.
    let (tmp, dir) =
        flake_dir("{\n  inputs.a = { url = ../../x; flake = false; };\n  outputs = _: {};\n}\n");
    let flake = format!("path:{}", dir.display());
    same_outcome_as_nix(tmp.path(), &dir, &["flake", "metadata", &flake]);
}

/// A relative input that names the store directory itself (`path:..` at
/// the root of a tree) is locked, and `flake metadata`, `lock`, `update`
/// and `prefetch-inputs`, which only lock the flake, succeed as Nix's do;
/// calling the flake (`eval`) fails in both, but not with the lock file
/// written, which leaves the input out of `callFlake`'s overrides.
#[test]
fn an_input_naming_the_store_directory_is_locked_like_nix() {
    let (tmp, dir) = flake_dir(
        "{\n  inputs.a = { url = \"path:..\"; flake = false; };\n  outputs = _: { x = 1; };\n}\n",
    );
    let flake = format!("path:{}", dir.display());
    let x = format!("{flake}#x");
    let commands: [&[&str]; 6] = [
        &["flake", "metadata", &flake],
        &["flake", "metadata", "--json", &flake],
        &["flake", "lock", &flake],
        &["flake", "update", "--flake", &flake],
        &["flake", "prefetch-inputs", &flake],
        &["eval", &x],
    ];
    for args in commands {
        same_outcome_as_nix(tmp.path(), &dir, args);
    }
    // With the lock file in place.
    let theirs = Oracle::new().args(["flake", "lock", &flake]).output();
    assert!(theirs.success(), "{}", theirs.stderr_lossy());
    let ours = Command::cargo_bin("brix")
        .expect("brix binary")
        .args(["--store", "dummy://", "eval", &x])
        .env("XDG_CACHE_HOME", tmp.path().join("cache"))
        .output()
        .expect("run brix");
    let theirs = Oracle::new().args(["eval", &x]).output();
    assert_eq!(
        (
            ours.status.success(),
            String::from_utf8_lossy(&ours.stdout).into_owned(),
            String::from_utf8_lossy(&strip_noise(&ours.stderr)).into_owned()
        ),
        (true, theirs.stdout_lossy(), theirs.stderr_lossy())
    );
}

/// A temporary directory with a flake (`flake.nix` as given) in its
/// `flake` subdirectory, whose path is returned too.
fn flake_dir(flake_nix: &str) -> (tempfile::TempDir, std::path::PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("flake");
    std::fs::create_dir(&dir).unwrap();
    std::fs::write(dir.join("flake.nix"), flake_nix).unwrap();
    (tmp, dir)
}

/// What a command did: whether it succeeded, what it printed (stderr
/// without what the oracle's environment adds) and the lock file it left
/// in the flake's directory.
#[derive(Debug, PartialEq)]
struct Outcome {
    success: bool,
    stdout: String,
    stderr: String,
    lock: Option<String>,
}

/// brix and Nix run with `args` on the flake in `dir`, each starting
/// without a lock file: they do the same. brix keeps its caches in `tmp`.
fn same_outcome_as_nix(tmp: &Path, dir: &Path, args: &[&str]) {
    let lock_path = dir.join("flake.lock");
    let run = |mut cmd: Command| {
        assert!(!lock_path.exists(), "{args:?}: a lock file is left over");
        let out = cmd.output().expect("run command");
        let lock = match std::fs::read_to_string(&lock_path) {
            Ok(text) => {
                std::fs::remove_file(&lock_path).unwrap();
                Some(text)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => panic!("reading {}: {e}", lock_path.display()),
        };
        Outcome {
            success: out.status.success(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&strip_noise(&out.stderr)).into_owned(),
            lock,
        }
    };
    let mut ours = Command::cargo_bin("brix").expect("brix binary");
    ours.args(["--store", "dummy://"])
        .args(args)
        .env("XDG_CACHE_HOME", tmp.join("cache"));
    let ours = run(ours);
    let theirs = run(Oracle::new().args(args).command());
    assert_eq!(
        ours, theirs,
        "brix (left) and nix (right) differ for {args:?}"
    );
}

/// `readFlake`'s traces (`while evaluating flake attribute '…'`, `while
/// evaluating flake input`) show the code at the attribute or input they
/// name, as Nix's do, a nested input's outermost first; the errors Nix
/// adds no trace to have none.
#[test]
fn read_flake_traces_show_their_code_like_nix() {
    for body in [
        r#"inputs.a.url = 1; outputs = _: {};"#,
        r#"inputs.a.url = "a" + "b"; outputs = _: {};"#,
        r#"inputs.a.flake = "x"; outputs = _: {};"#,
        r#"inputs.a.follows = 1; outputs = _: {};"#,
        r#"inputs.a.buildTime = true; outputs = _: {};"#,
        r#"inputs.a.x = [ ]; outputs = _: {};"#,
        r#"inputs.a = { type = "path"; }; outputs = _: {};"#,
        r#"inputs.a.inputs.b.url = 1; outputs = _: {};"#,
        r#"inputs.a.inputs = 1; outputs = _: {};"#,
        r#"inputs.a.inputs.self = {}; outputs = _: {};"#,
        // Untraced.
        r#"inputs.a.foo = "x"; outputs = _: {};"#,
        r#"inputs.a = { url = "github:a/b"; follows = "c"; }; outputs = _: {};"#,
        r#"inputs = 1; outputs = _: {};"#,
        r#"outputs = _: {}; nixConfig.x = { };"#,
    ] {
        same_metadata_failure(&format!("{{\n  {body}\n}}\n"), &[]);
    }
    // The excerpt is of the lines around the attribute.
    same_metadata_failure(
        "{\n  description = \"d\";\n  inputs = {\n    a.url = 1;\n  };\n  outputs = _: {};\n}\n",
        &[],
    );
}

/// What evaluating an input attribute raises (`publicKeys` as JSON, with
/// `verified-fetches`) is traced at the attribute and at the attributes
/// it is nested in, but not for `self`, whose attributes Nix reads
/// without a trace.
#[test]
fn read_flake_traces_what_input_attributes_raise_like_nix() {
    for body in [
        r#"inputs.a = { type = "git"; url = "file:///x"; publicKeys = throw "boom"; }; outputs = _: {};"#,
        r#"inputs.a.inputs.b = { type = "git"; url = "file:///x"; publicKeys = throw "boom"; }; outputs = _: {};"#,
        r#"inputs.self.publicKeys = throw "boom"; outputs = _: {};"#,
        // Taken as JSON, then refused by `applySelfAttrs`.
        r#"inputs.self.publicKeys = [ ]; outputs = _: {};"#,
    ] {
        same_metadata_failure(
            &format!("{{\n  {body}\n}}\n"),
            &["--extra-experimental-features", "verified-fetches"],
        );
    }
}

/// A `flake.nix` read while locking: the lock algorithm's traces come
/// first, and the whole is cut short without `--show-trace` where Nix
/// cuts it; `builtins.getFlake` (of a reference locked by its NAR hash,
/// as pure evaluation needs) adds its own trace on top.
#[test]
fn read_flake_traces_of_inputs_like_nix() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("flake");
    let write = |rel: &str, text: &str| {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    };
    write(
        "flake.nix",
        "{\n  inputs.a.url = \"path:./a\";\n  outputs = _: {};\n}\n",
    );
    write(
        "a/flake.nix",
        "{\n  inputs.b.url = \"path:./b\";\n  outputs = _: {};\n}\n",
    );
    write(
        "a/b/flake.nix",
        "{\n  inputs.c.inputs.d.flake = 1;\n  outputs = _: {};\n}\n",
    );
    let flake = format!("path:{}", root.display());
    same_failure_as_nix(tmp.path(), &["flake", "metadata", &flake]);
    same_failure_as_nix(tmp.path(), &["flake", "metadata", "--show-trace", &flake]);
    for dir in [root.clone(), root.join("a/b")] {
        let nar_hash = Oracle::new().args(["hash", "path"]).arg(&dir).stdout();
        let expr = format!(
            "(builtins.getFlake \"path:{}?narHash={}\").outputs",
            dir.display(),
            nar_hash.trim_end()
        );
        same_failure_as_nix(tmp.path(), &["eval", "--expr", &expr]);
    }
}

/// With `flake_nix` as its `flake.nix`, `flake metadata` (with `args`
/// before it) fails in brix with what it fails with in Nix.
fn same_metadata_failure(flake_nix: &str, args: &[&str]) {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("flake");
    std::fs::create_dir(&dir).unwrap();
    std::fs::write(dir.join("flake.nix"), flake_nix).unwrap();
    let flake = format!("path:{}", dir.display());
    let mut all: Vec<&str> = args.to_vec();
    all.extend(["flake", "metadata", flake.as_str()]);
    same_failure_as_nix(tmp.path(), &all);
}

/// brix and Nix run with `args` both fail, with the same stderr byte for
/// byte (but for what the oracle's environment adds to Nix's). brix keeps
/// its caches in `tmp`.
fn same_failure_as_nix(tmp: &Path, args: &[&str]) {
    let ours = Command::cargo_bin("brix")
        .expect("brix binary")
        .args(["--store", "dummy://"])
        .args(args)
        .env("XDG_CACHE_HOME", tmp.join("cache"))
        .output()
        .expect("run brix");
    let theirs = Oracle::new().args(args).output();
    assert!(!theirs.success(), "nix {args:?} succeeded");
    assert_eq!(
        (
            ours.status.success(),
            String::from_utf8_lossy(&strip_noise(&ours.stderr)).into_owned()
        ),
        (false, theirs.stderr_lossy()),
        "brix (left) and nix (right) differ for {args:?}"
    );
}

/// For each body, `flake.nix` is `{ <body> }`: brix fails with the last
/// error line Nix fails with.
fn same_error_as_nix(bodies: &[&str]) {
    for body in bodies {
        same_error_for(&format!("{{\n  {body}\n}}\n"));
    }
}

/// With `flake_nix` as its `flake.nix`, `flake metadata` fails in brix
/// with the last error line it fails with in Nix.
fn same_error_for(flake_nix: &str) {
    same_error_for_in(flake_nix, tempfile::tempdir().unwrap());
}

/// [`same_error_for`], with the flake in `dir`.
fn same_error_for_in(flake_nix: &str, dir: tempfile::TempDir) {
    std::fs::write(dir.path().join("flake.nix"), flake_nix).unwrap();
    let flake = format!("path:{}", dir.path().display());
    let ours = last_error(
        Command::cargo_bin("brix")
            .expect("brix binary")
            .args(["--store", "dummy://", "flake", "metadata", &flake])
            .env("XDG_CACHE_HOME", dir.path().join("cache")),
    );
    let out = Oracle::new().args(["flake", "metadata", &flake]).output();
    let theirs = (out.success(), last_error_line(&out.stderr));
    assert!(
        !theirs.0 && theirs.1.is_some(),
        "{flake_nix}: nix accepted it"
    );
    assert_eq!(ours, theirs, "{flake_nix}");
}
