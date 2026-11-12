//! Path values made from strings, and the names paths are copied to the
//! store under, against the reference `nix`.

mod common;

use brix_eval::{Arena, Config, Evaluator, NullHost, SrcPos};
use brix_testutil::{require_env, Oracle, REAL_STORE};
use bstr::ByteSlice;
use common::{
    assert_same_bytes, run_brix_expr, run_brix_expr_bytes, run_nix_expr_bytes, Format, TestHost,
};

/// `nix eval --expr` (pure, against the real store, which it writes:
/// `nix-instantiate --eval` is read-only and cannot read back the files
/// these expressions add): the printed value, or the error's last line.
fn nix_eval(expr: &str, cwd: &std::path::Path) -> Result<String, String> {
    let out = Oracle::new()
        .dir(cwd)
        .args(["eval", "--expr", expr])
        .output();
    if out.success() {
        Ok(out.stdout_lossy())
    } else {
        Err(last_error_line(&out.stderr_lossy()))
    }
}

/// The line holding the message itself, without traces.
fn last_error_line(rendered: &str) -> String {
    rendered
        .lines()
        .rev()
        .find(|l| l.trim_start().starts_with("error:"))
        .unwrap_or_else(|| panic!("no error line in {rendered:?}"))
        .trim()
        .to_owned()
}

/// Evaluates `expr` with `nix eval` and with brix and compares the
/// results, or the error messages.
fn compare(expr: &str) -> Result<(), String> {
    let pwd = tempfile::tempdir().expect("create a working directory");
    let store = tempfile::tempdir().expect("create a store directory");
    let host = TestHost::with_allowed(&[("/pwd", pwd.path()), ("/nix/store", store.path())], &[]);
    let config = Config {
        eval_cores: 0,
        ..Config::default()
    };
    let ours = run_brix_expr(&host, config, expr, "/pwd", Format::Plain);
    let ours = if ours.ok {
        Ok(String::from_utf8_lossy(&ours.stdout).into_owned())
    } else {
        Err(last_error_line(&String::from_utf8_lossy(&ours.stderr)))
    };
    let theirs = nix_eval(expr, pwd.path());
    if ours != theirs {
        return Err(format!("{expr}\n--- nix\n{theirs:?}\n--- brix\n{ours:?}"));
    }
    Ok(())
}

fn compare_all(cases: &[&str]) {
    common::compare_all("paths", cases, compare);
}

/// `computeBaseName`: a path that is a store path itself is copied under
/// the name fetching it into the store again would give it,
/// `<hash>-<name>`, whatever kind of store object it is.
///
/// Nix has to write the files into the store to read them back, and
/// only the daemon's store, the one brix stands for, behaves as brix
/// does: in a store under another root (Nix's choice in the build
/// sandbox) the first case fails with "path '…-x' is not a valid store
/// path", and in a writable `dummy://` store the files cannot be read.
#[test]
#[ignore = "writes into the user's store; set BRIX_TEST_REAL_STORE=1"]
fn copying_a_store_path_again_names_it_after_its_nar() {
    require_env(REAL_STORE);
    compare_all(&[
        // A `toFile` text: its NAR path differs from its text path.
        r#""${/. + builtins.unsafeDiscardStringContext (builtins.toFile "x" "y")}""#,
        r#"builtins.filterSource (p: t: true) (/. + builtins.unsafeDiscardStringContext (builtins.toFile "x" "y"))"#,
        // A flat copy.
        r#"let f = builtins.path { name = "f"; path = /. + builtins.unsafeDiscardStringContext (builtins.toFile "x" "y"); recursive = false; };
           in "${/. + builtins.unsafeDiscardStringContext f}""#,
        // A NAR copy is its own path.
        r#"let f = builtins.path { name = "f"; path = /. + builtins.unsafeDiscardStringContext (builtins.toFile "x" "y"); };
           in "${/. + builtins.unsafeDiscardStringContext f}""#,
        // Still the store path itself once canonicalised.
        r#""${/. + builtins.unsafeDiscardStringContext (builtins.toFile "x" "y") + "/."}""#,
        // An output nobody built cannot be read.
        r#""${/. + builtins.unsafeDiscardStringContext (derivation { name = "o"; system = "x"; builder = "/b"; }).outPath}""#,
    ]);
}

/// No Nix string can hold a NUL byte, so `BadCanonPath` for one is out of
/// reach from Nix code: every way of making such a string fails first,
/// in brix as in Nix.
#[test]
fn nul_bytes_never_reach_a_path() {
    compare_all(&[
        r#"/a + (builtins.fromJSON "\"x\\u0000y\"")"#,
        r#"builtins.readDir (/. + builtins.fromJSON "\"x\\u0000y\"")"#,
    ]);
}

/// A host can hand Nix code a string with a NUL byte, though. Making a
/// path of it fails as `CanonPath` does in Nix, instead of silently
/// becoming `/`.
#[test]
fn a_nul_byte_from_the_host_is_a_bad_path() {
    let arena = Arena::new();
    let host = NullHost;
    let ev = Evaluator::new(&arena, Config::default(), &host);
    let nul = ev.alloc_cell(ev.mk_string("b\0c"));
    let expected = "path segment '/ab␀c' must not contain null (\\0) bytes";
    for src in ["s: /a + s", "s: builtins.readDir s"] {
        let f = ev
            .eval_str(src.as_bytes(), "/")
            .expect("the function evaluates");
        let s = if src.contains("readDir") {
            ev.alloc_cell(ev.mk_string("/ab\0c"))
        } else {
            nul
        };
        let err = ev
            .call(f, &[s], SrcPos::NONE)
            .expect_err("a NUL byte in a path is an error");
        let rendered = err.render(false);
        assert!(rendered.contains_str(expected), "{src}: {rendered}");
    }
}

/// The name `builtins.path` copies a path under is bytes, as in Nix: one
/// that is not a store path name (here `a` and the byte 0xc3) is refused
/// with the path computed from those bytes, whether the copy is hashed or
/// its path is computed from `sha256` first.
#[test]
#[ignore = "writes into the user's store; set BRIX_TEST_REAL_STORE=1"]
fn path_names_that_are_not_utf8_are_bytes() {
    require_env(REAL_STORE);
    compare_all(&[
        r#"let l = builtins.substring 0 1 "é"; in builtins.path { name = "a" + l; path = /. + builtins.unsafeDiscardStringContext (builtins.toFile "x" "y"); }"#,
        r#"let l = builtins.substring 0 1 "é"; in builtins.path { name = "a" + l; path = /. + builtins.unsafeDiscardStringContext (builtins.toFile "x" "y"); recursive = false; sha256 = "0000000000000000000000000000000000000000000000000000000000000000"; }"#,
    ]);
}

/// Nix keeps the bytes of a path that are not UTF-8: `/a + "\xffb"` (the
/// raw byte in the expression) is the path `/a\xffb`, printed as those
/// bytes, and pure mode refuses to read it naming those bytes. brix does
/// the same, byte for byte (`tests/byte_paths.rs` has the rest).
#[test]
fn path_names_that_are_not_utf8_keep_their_bytes() {
    for src in [&b"/a + \"\xffb\""[..], b"builtins.readDir \"/a\xffb\""] {
        let pwd = tempfile::tempdir().expect("create a working directory");
        let host = TestHost::with_allowed(&[("/pwd", pwd.path())], &[]);
        let config = Config {
            eval_cores: 0,
            ..Config::default()
        };
        let ours = run_brix_expr_bytes(&host, config, src, b"/pwd", Format::Plain);
        let theirs = run_nix_expr_bytes(src, pwd.path(), Format::Plain, &[], false);
        assert_same_bytes(src, &ours, &theirs);
    }
}
