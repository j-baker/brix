//! The store directory from the environment (`NIX_STORE_DIR`, then
//! `NIX_STORE`), against the reference `nix` (`BRIX_NIX_BIN`): how it is
//! canonicalised, which variable wins, the error for a relative one, and
//! the paths computed under it.

use std::ffi::OsStr;
use std::process::Command;

use assert_cmd::prelude::*;
use brix_testutil::{is_noise, nix_bin};

/// Status, stdout and stderr of `cmd` evaluating `expr` with the dummy
/// store and `env` set (the variables are cleared first), with the
/// program in a usage error's "Try '<program> --help'" hint written as
/// `nix`. brix and the reference `nix` get the same configuration, so
/// this builds the command itself rather than through
/// [`brix_testutil::Oracle`], and drops the oracle's noise
/// ([`is_noise`]) by hand.
fn eval(mut cmd: Command, env: &[(&str, &OsStr)], args: &[&str]) -> (Option<i32>, String, String) {
    cmd.env_remove("NIX_STORE_DIR")
        .env_remove("NIX_STORE")
        .env("NIX_CONFIG", "experimental-features = nix-command")
        .args(["eval", "--store", "dummy://"])
        .args(args);
    for (k, v) in env {
        cmd.env(k, v);
    }
    let out = cmd.output().expect("run command");
    let stderr = String::from_utf8_lossy(&out.stderr)
        .lines()
        .filter(|line| !is_noise(line.as_bytes()))
        .map(|line| match line.strip_prefix("Try '") {
            Some(rest) if rest.ends_with(" --help' for more information.") => {
                "Try 'nix --help' for more information.".to_owned()
            }
            _ => line.to_owned(),
        })
        .collect::<Vec<_>>()
        .join("\n");
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr,
    )
}

fn brix() -> Command {
    Command::cargo_bin("brix").expect("brix binary")
}

#[test]
fn store_dir_from_the_environment_matches_nix() {
    let os = |s: &'static str| OsStr::new(s);
    let envs: Vec<Vec<(&str, &OsStr)>> = vec![
        vec![("NIX_STORE_DIR", os("/nix/store/"))],
        vec![("NIX_STORE_DIR", os("/nix//store/./"))],
        vec![("NIX_STORE_DIR", os("/nix/store/.."))],
        vec![("NIX_STORE_DIR", os("/foo/store"))],
        vec![("NIX_STORE_DIR", os("foo"))],
        vec![("NIX_STORE", os("/foo//store/"))],
        // The first non-empty one wins.
        vec![("NIX_STORE_DIR", os("")), ("NIX_STORE", os("/bar/store"))],
        vec![
            ("NIX_STORE_DIR", os("/foo/store")),
            ("NIX_STORE", os("/bar/store")),
        ],
    ];
    let exprs = [
        "builtins.storeDir",
        r#"[ (builtins.toFile "x" "y") (derivation { name = "x"; builder = "/bin/sh"; system = "x86_64-linux"; }).drvPath ]"#,
    ];
    for env in &envs {
        for expr in exprs {
            let args = ["--json", "--expr", expr];
            assert_eq!(
                eval(brix(), env, &args),
                eval(Command::new(nix_bin()), env, &args),
                "{env:?} {expr}"
            );
        }
    }
}

#[test]
fn a_store_dir_that_is_not_utf8_is_an_error() {
    use std::os::unix::ffi::OsStrExt;
    let (status, stdout, stderr) = eval(
        brix(),
        &[("NIX_STORE_DIR", OsStr::from_bytes(b"/nix/st\xffre"))],
        &["--expr", "1"],
    );
    assert_eq!(status, Some(1), "{stdout}");
    assert_eq!(
        stderr,
        "error: the store directory in NIX_STORE_DIR is not valid UTF-8"
    );
}

/// `dummy://` with its settings: `store` sets the store directory (made
/// canonical), the others are checked, unknown ones warned about, and an
/// authority is an error, as in Nix.
#[test]
fn dummy_store_urls_match_nix() {
    for store in [
        "dummy://?store=/foo//store/",
        "dummy://?store=relative",
        "dummy://?bogus=1&read-only=false&store=/y",
        "dummy://?store=/a&store=/b",
        "dummy://?priority=x",
        "dummy://?read-only=maybe",
        "dummy://?trusted=1&system-features=kvm&path-info-cache-size=1K",
        "dummy://x?store=/z",
        "dummy:///",
        "dummy:",
        "dummy:/x",
    ] {
        // The last `--store` wins over `eval`'s own.
        let args = ["--store", store, "--expr", "builtins.storeDir"];
        assert_eq!(
            eval(brix(), &[], &args),
            eval(Command::new(nix_bin()), &[], &args),
            "{store}"
        );
    }
}

/// A daemon's store URI sets the store directory evaluation uses with its
/// `store` setting, as `dummy://`'s does; a URI Nix cannot parse, or a
/// setting value it refuses, is a usage error. Nix opens the directory of
/// a `unix://` store (brix does not), so it has to exist.
#[test]
fn daemon_store_urls_set_the_store_dir_as_nix_does() {
    let dir = tempfile::tempdir().expect("temporary directory");
    let dir = dir.path().to_str().expect("UTF-8 temporary directory");
    let stores = [
        format!("unix:///nonexistent/socket?store={dir}//"),
        format!("daemon?store={dir}"),
        format!("daemon?store={dir}&bogus=1"),
        "foo".to_owned(),
        "dummy".to_owned(),
        "daemon?store=relative".to_owned(),
        "unix://?store=".to_owned(),
        "daemon?priority=x".to_owned(),
        "zzz://x".to_owned(),
    ];
    for store in &stores {
        let expr = r#"[ builtins.storeDir (builtins.toFile "x" "y") ]"#;
        let args = ["--store", store, "--json", "--expr", expr];
        assert_eq!(
            eval(brix(), &[], &args),
            eval(Command::new(nix_bin()), &[], &args),
            "{store}"
        );
    }
}

/// `NIX_REMOTE` is the `store` setting's default: the store, and so the
/// store directory, unless a configuration file, `NIX_CONFIG` or `--store`
/// says otherwise; empty, it is `auto`.
#[test]
fn nix_remote_is_the_default_store_as_in_nix() {
    let conf_dir = tempfile::tempdir().expect("temporary directory");
    let run = |mut cmd: Command, remote: &str, config: &str, args: &[&str]| {
        cmd.env_remove("NIX_STORE_DIR")
            .env_remove("NIX_STORE")
            .env("NIX_CONF_DIR", conf_dir.path())
            .env("NIX_USER_CONF_FILES", "")
            .env("NIX_REMOTE", remote)
            .env(
                "NIX_CONFIG",
                format!("experimental-features = nix-command\n{config}"),
            )
            .args(args)
            .args(["eval", "--expr", "builtins.storeDir"]);
        let out = cmd.output().expect("run command");
        let stderr: Vec<String> = String::from_utf8_lossy(&out.stderr)
            .lines()
            .filter(|line| !is_noise(line.as_bytes()))
            .map(str::to_owned)
            .collect();
        (
            out.status.code(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr,
        )
    };
    let remote = "dummy://?store=/foo";
    for (remote, config, args) in [
        (remote, "", &[][..]),
        (remote, "store = dummy://?store=/bar", &[]),
        (remote, "", &["--store", "dummy://?store=/baz"]),
        ("", "", &[]),
    ] {
        let ours = run(brix(), remote, config, args);
        assert_eq!(
            ours.0,
            Some(0),
            "{remote:?} {config:?} {args:?}: {:?}",
            ours.2
        );
        assert_eq!(
            ours,
            run(Command::new(nix_bin()), remote, config, args),
            "{remote:?} {config:?} {args:?}"
        );
    }
}
