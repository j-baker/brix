//! Import-from-derivation through a real Nix daemon. Needs a daemon that
//! can build, so it only runs on request:
//!
//! ```text
//! BRIX_TEST_REAL_DAEMON=1 cargo test -p brix --test ifd -- --ignored
//! ```

use brix::{Config, Format, Session};
use brix_testutil::{require_env, REAL_DAEMON};

#[test]
#[ignore = "builds through the real Nix daemon; set BRIX_TEST_REAL_DAEMON=1"]
fn import_from_derivation_builds_through_the_daemon() {
    require_env(REAL_DAEMON);
    let config = Config {
        daemon: Some("daemon".into()),
        ..Config::default()
    };
    let session = Session::new(config).expect("connect to the daemon");
    let system = brix::host_system();
    let expr = format!(
        r#"let
  drv = derivation {{
    name = "brix-ifd-test";
    system = "{system}";
    builder = "/bin/sh";
    args = [ "-c" "echo '{{ answer = 6 * 7; }}' > $out" ];
  }};
in (import drv).answer"#
    );
    let got = session
        .eval_expr_to_string(&expr, "/", Format::Nix)
        .expect("evaluate through import-from-derivation");
    assert_eq!(got, "42");
    // The derivation went through the virtual store on its way to the
    // daemon.
    assert!(session
        .derivations()
        .iter()
        .any(|(_, d)| d.name == "brix-ifd-test"));
}

/// The last line of `nix eval --expr expr`'s error: it must fail.
fn nix_eval_error(expr: &str) -> String {
    let out = std::process::Command::new(brix_testutil::nix_bin())
        .args(["eval", "--expr", expr])
        .env(
            "NIX_CONFIG",
            "experimental-features = nix-command flakes\neval-cache = false\nlazy-trees = true\neval-cores = 0",
        )
        .output()
        .expect("run nix");
    assert!(!out.status.success(), "nix eval succeeded on {expr}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    stderr
        .lines()
        .rev()
        .find(|l| l.trim_start().starts_with("error:"))
        .unwrap_or_else(|| panic!("no error line in nix's stderr:\n{stderr}"))
        .trim()
        .to_owned()
}

/// `realiseContext` allows the closures of the outputs an import needed
/// built (`allowClosure`), not the whole store: after the build, another
/// valid store path is as forbidden as it was before, in the same words
/// as Nix's.
#[test]
#[ignore = "builds through the real Nix daemon; set BRIX_TEST_REAL_DAEMON=1"]
fn import_from_derivation_allows_only_the_output_closure() {
    if std::env::var_os("BRIX_TEST_REAL_DAEMON").is_none() {
        panic!("BRIX_TEST_REAL_DAEMON=1 is required for this test");
    }
    // The reference Nix's own store path: valid, and no output's
    // dependency.
    let nix_bin = brix_testutil::nix_bin();
    let unrelated = nix_bin
        .parent()
        .and_then(|bin| bin.parent())
        .expect("the reference nix is <store path>/bin/nix")
        .to_str()
        .expect("store paths are UTF-8")
        .to_owned();
    let system = brix::host_system();
    let drv = format!(
        r#"derivation {{
    name = "brix-ifd-allow-test";
    system = "{system}";
    builder = "/bin/sh";
    args = [ "-c" "echo '{{ answer = 42; }}' > $out" ];
  }}"#
    );
    let session = Session::new(Config {
        daemon: Some("daemon".into()),
        ..Config::default()
    })
    .expect("connect to the daemon");
    // The output itself is readable after the import.
    let got = session
        .eval_expr_to_string(
            format!("let drv = {drv}; in builtins.seq (import drv).answer (builtins.readFile drv)"),
            "/",
            Format::Raw,
        )
        .expect("evaluate through import-from-derivation");
    assert_eq!(got, "{ answer = 42; }\n");
    let expr = format!(
        "let drv = {drv}; in builtins.seq (import drv).answer (builtins.readDir {unrelated})"
    );
    let ours = session
        .eval_expr_to_string(&expr, "/", Format::Nix)
        .expect_err("an unrelated store path is not readable")
        .to_string();
    let theirs = nix_eval_error(&expr);
    assert_eq!(
        theirs,
        format!("error: access to absolute path '{unrelated}' is forbidden in pure evaluation mode (use '--impure' to override)")
    );
    let ours_last = ours
        .lines()
        .rev()
        .find(|l| l.trim_start().starts_with("error:"))
        .unwrap_or_else(|| panic!("no error line in brix's error:\n{ours}"))
        .trim();
    assert_eq!(ours_last, theirs, "brix's error:\n{ours}");
}
