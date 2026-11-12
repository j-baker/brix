//! How installables are read, against the reference `nix`: store paths
//! reached through symlinks or paths inside them, and attribute names in
//! "does not provide attribute" errors.
//!
//! Every test needs `BRIX_NIX_BIN` (the devshell sets it).

use std::path::Path;
use std::process::Command;

use assert_cmd::prelude::*;
use brix_testutil::{nix_bin, require_env, Oracle, OracleOutput, ORACLE_NIX_CONFIG, REAL_DAEMON};

struct Output {
    status: i32,
    stdout: String,
    stderr: String,
}

fn run(cmd: &mut Command) -> Output {
    let out = cmd.output().expect("run command");
    Output {
        status: out.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

fn from_oracle(out: OracleOutput) -> Output {
    Output {
        status: out.code(),
        stdout: out.stdout_lossy(),
        stderr: out.stderr_lossy(),
    }
}

/// The reference `nix` in `dir` (brix-testutil's [`Oracle`]).
fn nix(dir: &Path, args: &[&str]) -> Output {
    from_oracle(Oracle::new().dir(dir).args(args).output())
}

/// `brix` without a store, for evaluation-only commands.
fn brix(dir: &Path, args: &[&str]) -> Output {
    run(Command::cargo_bin("brix")
        .expect("brix binary")
        .current_dir(dir)
        .arg("--store")
        .arg("dummy://")
        .args(args))
}

/// `brix` with the default store (the daemon).
fn brix_with_store(dir: &Path, args: &[&str]) -> Output {
    run(Command::cargo_bin("brix")
        .expect("brix binary")
        .current_dir(dir)
        .args(args))
}

/// Attribute names that are not identifiers are quoted as in a string
/// literal, where `$` is escaped only before `{`.
#[test]
fn missing_attribute_names_are_quoted_like_nix() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("flake.nix"),
        "{ outputs = { self }: { x = 1; }; }\n",
    )
    .unwrap();
    for fragment in [r#""a$b""#, r#""x.y""#] {
        let installable = format!("path:{}#{fragment}", dir.path().display());
        let theirs = nix(dir.path(), &["eval", &installable]);
        let ours = brix(dir.path(), &["eval", &installable]);
        assert_eq!(theirs.status, 1, "{}", theirs.stderr);
        assert!(
            theirs.stderr.contains("does not provide attribute"),
            "{}",
            theirs.stderr
        );
        assert_eq!(ours.status, theirs.status, "{fragment}: {}", ours.stderr);
        assert_eq!(ours.stderr, theirs.stderr, "{fragment}");
    }
}

/// `nix path-info ./result`: a symlink chain into the store, or a path
/// inside a store path, names that store path (`followLinksToStorePath`).
#[test]
#[ignore = "queries the real Nix daemon; set BRIX_TEST_REAL_DAEMON=1"]
fn store_paths_through_symlinks_match_nix() {
    require_env(REAL_DAEMON);
    // The reference nix's own package: valid in the store on any machine
    // that runs these tests.
    let package = nix_bin()
        .canonicalize()
        .unwrap()
        .ancestors()
        .find(|p| p.parent() == Some(Path::new("/nix/store")))
        .expect("the reference nix lives in /nix/store")
        .to_path_buf();
    let dir = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(&package, dir.path().join("result")).unwrap();
    std::os::unix::fs::symlink("result", dir.path().join("result-again")).unwrap();
    std::os::unix::fs::symlink(package.join("bin"), dir.path().join("bin")).unwrap();
    let inner = format!("{}/bin/no-such-file", package.display());
    for installable in ["./result", "./result-again", "./bin", inner.as_str()] {
        let theirs = nix(dir.path(), &["path-info", installable]);
        assert_eq!(theirs.status, 0, "{installable}: {}", theirs.stderr);
        let ours = brix_with_store(dir.path(), &["path-info", installable]);
        assert_eq!(ours.status, 0, "{installable}: {}", ours.stderr);
        assert_eq!(ours.stdout, theirs.stdout, "{installable}");
    }
}

/// `flake-registry` from `nix.conf` (here `NIX_CONFIG`) chooses the
/// global registry: a file, none at all (empty), or a URL whose download
/// fails, which falls back to Nix's builtin registry with a warning.
#[test]
fn the_global_registry_follows_flake_registry() {
    let dir = tempfile::tempdir().unwrap();
    let flake = dir.path().join("flake");
    std::fs::create_dir(&flake).unwrap();
    std::fs::write(
        flake.join("flake.nix"),
        "{ outputs = { self }: { x = 1; }; }\n",
    )
    .unwrap();
    let registry = dir.path().join("registry.json");
    std::fs::write(
        &registry,
        format!(
            r#"{{"version": 2, "flakes": [{{"from": {{"type": "indirect", "id": "brix-registry-test"}}, "to": {{"type": "path", "path": "{}"}}}}]}}"#,
            flake.display()
        ),
    )
    .unwrap();
    let with_registry = |location: &str, brix: bool| {
        let registry = format!("flake-registry = {location}");
        let args = ["flake", "metadata", "--json", "brix-registry-test"];
        if brix {
            run(Command::cargo_bin("brix")
                .expect("brix binary")
                .args(["--store", "dummy://"])
                .current_dir(dir.path())
                .args(args)
                .env("NIX_CONFIG", format!("{ORACLE_NIX_CONFIG}\n{registry}")))
        } else {
            from_oracle(
                Oracle::new()
                    .dir(dir.path())
                    .args(args)
                    .config(&registry)
                    .output(),
            )
        }
    };
    let resolved = |out: &Output| -> String {
        assert_eq!(out.status, 0, "{}", out.stderr);
        let v: serde_json::Value = serde_json::from_str(&out.stdout).unwrap();
        v["resolvedUrl"].as_str().unwrap().to_owned()
    };
    let location = registry.to_str().unwrap();
    let theirs = with_registry(location, false);
    assert_eq!(resolved(&theirs), format!("path:{}", flake.display()));
    assert_eq!(resolved(&with_registry(location, true)), resolved(&theirs));

    let theirs = with_registry("", false);
    let ours = with_registry("", true);
    assert_eq!(theirs.status, 1, "{}", theirs.stderr);
    assert_eq!((ours.status, ours.stderr), (theirs.status, theirs.stderr));

    // The download error itself is the downloader's own text.
    let gone = "file:///nonexistent/brix-registry-test.json";
    let theirs = with_registry(gone, false);
    let ours = with_registry(gone, true);
    assert_eq!(ours.status, theirs.status, "{}", ours.stderr);
    let warning = format!(
        "warning: cannot fetch global flake registry '{gone}', will use builtin fallback registry: "
    );
    for out in [&theirs, &ours] {
        let lines: Vec<&str> = out.stderr.lines().collect();
        assert_eq!(lines.len(), 2, "{}", out.stderr);
        assert!(lines[0].starts_with(&warning), "{}", out.stderr);
        assert_eq!(
            lines[1],
            "error: cannot find flake 'flake:brix-registry-test' in the flake registries"
        );
    }
}

/// A string installable denotes the one store path or derivation output
/// its context names (`coerceToSingleDerivedPath`), with Nix's errors for
/// anything else, under the attribute's position.
#[test]
fn string_installables_follow_their_context_like_nix() {
    let dir = tempfile::tempdir().unwrap();
    let drv = r#"(derivation { name = "x"; system = "x"; builder = "/bin/sh"; })"#;
    for value in [
        format!(r#""${{{drv}}}""#),
        format!(r#""${{{drv}}}/bin""#),
        format!(r#""a${{{drv}}}""#),
        format!("{drv}.drvPath"),
        format!(r#""${{{drv}}}${{{drv}.drvPath}}""#),
        r#""no context""#.to_owned(),
        format!(r#"builtins.unsafeDiscardOutputDependency {drv}.drvPath"#),
    ] {
        let expr = format!("{{ x = {value}; }}");
        let theirs = nix(
            dir.path(),
            &["derivation", "show", "--impure", "--expr", &expr, "x"],
        );
        let ours = brix(dir.path(), &["derivation", "show", "--expr", &expr, "x"]);
        assert_eq!(ours.status, theirs.status, "{value}: {}", ours.stderr);
        assert_eq!(ours.stdout, theirs.stdout, "{value}");
        assert_eq!(ours.stderr, theirs.stderr, "{value}");
    }
    // A nested attribute, and a list element, which has no position.
    for (expr, attr) in [
        (r#"{ a.x = "no context"; }"#, "a.x"),
        (r#"{ x = [ "no context" ]; }"#, "x.0"),
    ] {
        let theirs = nix(
            dir.path(),
            &["derivation", "show", "--impure", "--expr", expr, attr],
        );
        let ours = brix(dir.path(), &["derivation", "show", "--expr", expr, attr]);
        assert_eq!(
            (ours.status, &ours.stderr),
            (theirs.status, &theirs.stderr),
            "{attr}"
        );
    }
}

/// `eval --drv-link` links the derivations in the result's context.
#[test]
#[ignore = "writes a derivation through the real Nix daemon; set BRIX_TEST_REAL_DAEMON=1"]
fn drv_links_follow_the_context_like_nix() {
    require_env(REAL_DAEMON);
    let dir = tempfile::tempdir().unwrap();
    let expr =
        r#""${derivation { name = "brix-drv-link-test"; system = "x"; builder = "/bin/sh"; }}""#;
    let theirs = nix(
        dir.path(),
        &[
            "eval",
            "--impure",
            "--raw",
            "--drv-link",
            "theirs",
            "--expr",
            expr,
        ],
    );
    assert_eq!(theirs.status, 0, "{}", theirs.stderr);
    let ours = brix_with_store(
        dir.path(),
        &["eval", "--raw", "--drv-link", "ours", "--expr", expr],
    );
    assert_eq!(ours.status, 0, "{}", ours.stderr);
    assert_eq!(ours.stdout, theirs.stdout);
    let link = |name: &str| std::fs::read_link(dir.path().join(name)).unwrap();
    assert_eq!(link("ours"), link("theirs"));

    // A `toFile` text in the context is linked too, and brix writes it to
    // the store first, as Nix's evaluation does: the link never dangles.
    // New contents, so that the file is not in the store yet.
    let nonce = format!(
        "{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let expr = format!(r#"builtins.toFile "brix-drv-link-text" "{nonce}""#);
    let ours = brix_with_store(
        dir.path(),
        &["eval", "--raw", "--drv-link", "ours-text", "--expr", &expr],
    );
    assert_eq!(ours.status, 0, "{}", ours.stderr);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("ours-text")).unwrap(),
        nonce
    );
    let theirs = nix(
        dir.path(),
        &[
            "eval",
            "--impure",
            "--raw",
            "--drv-link",
            "theirs-text",
            "--expr",
            &expr,
        ],
    );
    assert_eq!(theirs.status, 0, "{}", theirs.stderr);
    assert_eq!(ours.stdout, theirs.stdout);
    assert_eq!(link("ours-text"), link("theirs-text"));
}

/// A flake output that is neither a derivation nor a path is shown in
/// the error as Nix's `ValuePrinter` shows it: forced at the top only,
/// the rest `«thunk»` (which also keeps a failing attribute from hiding
/// the value).
#[test]
fn non_derivation_outputs_are_shown_like_nix() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("flake.nix"),
        r#"{ outputs = { self }: { a = 1.5; b = { x = 1; y = [ 1 "s" ]; z = throw "no"; }; c = [ ]; d = null; e = x: x; f = true; g = { }; }; }"#,
    )
    .unwrap();
    for attr in ["a", "b", "c", "d", "e", "f", "g"] {
        let installable = format!("path:{}#{attr}", dir.path().display());
        let theirs = nix(dir.path(), &["build", "--dry-run", &installable]);
        let ours = brix(dir.path(), &["build", "--dry-run", &installable]);
        assert_eq!(theirs.status, 1, "{}", theirs.stderr);
        assert_eq!(
            (ours.status, ours.stderr),
            (theirs.status, theirs.stderr),
            "{attr}"
        );
    }
}

/// Errors that quote a value or a name show its bytes as Nix does, UTF-8
/// or not (the flake is a Latin-1 file): the strings are compared as
/// bytes, not as text, where ill-formed sequences would all look alike.
#[test]
fn errors_quote_values_that_are_not_utf8_like_nix() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("flake.nix"),
        b"{ outputs = { self }: { h = builtins.substring 0 1 \"\xc3\xa9\"; i = { a = \"\xc3\"; \"\xff\" = 1; }; }; }\n",
    )
    .unwrap();
    let stderr = |cmd: &mut Command| -> Vec<u8> {
        let out = cmd.output().expect("run command");
        assert_eq!(out.status.code(), Some(1));
        // Without the oracle's remarks about its environment, kept as
        // bytes.
        brix_testutil::strip_noise(&out.stderr)
    };
    for attr in ["h", "i"] {
        let installable = format!("path:{}#{attr}", dir.path().display());
        let args = ["build", "--dry-run", installable.as_str()];
        let theirs = stderr(
            Oracle::new()
                .dir(dir.path())
                .args(args)
                .command()
                .stdin(std::process::Stdio::null()),
        );
        let ours = stderr(
            Command::cargo_bin("brix")
                .expect("brix binary")
                .current_dir(dir.path())
                .args(["--store", "dummy://"])
                .args(args),
        );
        assert_eq!(
            ours.escape_ascii().to_string(),
            theirs.escape_ascii().to_string(),
            "{attr}"
        );
    }
}
