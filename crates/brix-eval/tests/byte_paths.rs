//! Paths whose names are not UTF-8, against the reference `nix`, byte for
//! byte. Nix keeps a path's bytes as they are: `/a + "\xff"` (the raw
//! byte in the expression) is the path `/a\xff`, a file name read from a
//! directory is the name's bytes, and messages, traces and positions
//! quote them unchanged. brix does the same.
//!
//! The expressions are bytes, given to Nix on its command line as they
//! are. Names are chosen to pin the order (`z` is 0x7a, before `caf\xe9`,
//! `a\x80b` and `\xff`: Nix sorts names as unsigned bytes) and to cover a
//! directory (`d\xe9/`), a file in it with a relative path literal, and
//! symlinks whose targets are bytes too.

mod common;

use std::path::Path;

use brix_eval::Config;
use brix_testutil::{scratch_store, symlink_bytes, write_bytes};
use common::{
    compare_all_bytes, run_brix_expr_bytes, run_nix_expr_bytes, same_bytes, Format, TestHost,
    NIX_READS_CWD,
};

/// The reference binary is Determinate Nix, which runs the parallel
/// evaluator (`eval-cores = 0`).
fn config() -> Config {
    Config {
        eval_cores: 0,
        ..Config::default()
    }
}

/// A tree with names that are not UTF-8 under `dir`: regular files,
/// a directory with a Nix file that uses a relative path literal, Nix
/// files with an error, a position and a parse error in them, a file
/// holding a NUL byte, a dangling symlink and one to a byte-named file.
fn byte_tree(dir: &Path) {
    write_bytes(dir, b"z", "z");
    write_bytes(dir, b"caf\xe9", "cafe");
    write_bytes(dir, b"\xff", "ff");
    write_bytes(dir, b"a\x80b", "a80b");
    write_bytes(
        dir,
        b"d\xe9/default.nix",
        "{ x = ./y; s = builtins.readFile ./y; }",
    );
    write_bytes(dir, b"d\xe9/y", "Y");
    symlink_bytes(dir, b"l\xff", b"t\xe9");
    symlink_bytes(dir, b"l2", b"caf\xe9");
}

/// [`byte_tree`] plus the Nix files and the file Nix cannot read as a
/// string, which the copies leave out.
fn byte_tree_with_nix_files(dir: &Path) {
    byte_tree(dir);
    write_bytes(dir, b"caf\xe9.nix", "1 + { }");
    write_bytes(dir, b"pos\xe9.nix", " __curPos");
    write_bytes(dir, b"bad\xe9.nix", "{ a = 1;\n  b = ");
    write_bytes(dir, b"nul\xe9", b"a\0b");
}

/// `expr` in brix (pure, nothing readable but an empty store) and in
/// `nix-instantiate --eval --strict --pure-eval` on the `dummy://` store.
fn compare_pure(expr: &[u8]) -> Result<(), String> {
    let pwd = tempfile::tempdir().expect("create a working directory");
    let store = tempfile::tempdir().expect("create a store directory");
    let host = TestHost::with_allowed(&[("/pwd", pwd.path()), ("/nix/store", store.path())], &[]);
    let ours = run_brix_expr_bytes(&host, config(), expr, b"/pwd", Format::Plain);
    let theirs = run_nix_expr_bytes(expr, pwd.path(), Format::Plain, &[], false);
    same_bytes(expr, &ours, &theirs)
}

/// Path values made of bytes that are not UTF-8, which need no file:
/// printing, `toString`, `baseNameOf`, `dirOf`, byte-level
/// canonicalisation, equality and (unsigned) order, JSON's refusal, and
/// the messages pure mode and lookups give with the bytes in them.
#[test]
fn path_values_keep_their_bytes_as_in_nix() {
    compare_all_bytes(
        "byte path values",
        &[
            &b"/a + \"\xff\""[..],
            b"[ (/a + \"\xff\") ]",
            b"/. + \"a\xff\"",
            b"/nix/store + \"/\xff\"",
            b"toString (/a + \"\xff/b\")",
            b"builtins.stringLength (toString (/a + \"\xff\"))",
            b"builtins.hashString \"sha256\" (toString (/a + \"\xff\"))",
            b"baseNameOf (/a + \"\xff/b\")",
            b"baseNameOf (/a + \"/b\xff\")",
            b"dirOf (/a + \"\xff/b\")",
            b"dirOf \"/a\xff/b\"",
            b"/a + \"/\xff/..\"",
            b"builtins.toPath \"/a/\xff/../b\xe9\"",
            b"(/a + \"\xff\") == (/a + \"\xff\")",
            b"(/a + \"\xff\") == (/a + \"\xfe\")",
            b"(/a + \"\xff\") < (/a + \"\xfe\")",
            b"builtins.sort (x: y: x < y) [ (/a + \"\xff\") /ab /a/b (/a + \"\xe9z\") ]",
            b"builtins.toXML (/a + \"\xff\")",
            b"builtins.toJSON (toString (/a + \"\xff\"))",
            b"builtins.readFile (/a + \"\xff\")",
            b"builtins.readDir \"/a\xffb\"",
            b"builtins.pathExists (/a + \"\xff\")",
            b"import (/a + \"\xff\")",
            b"builtins.path { path = /a + \"\xff\"; }",
            b"builtins.filterSource (p: t: true) (/a + \"\xff\")",
            b"builtins.filterSource (p: t: false) (/a + \"\xff\")",
            b"builtins.path { path = /a + \"\xff\"; filter = p: t: true; }",
            b"__findFile [ { path = builtins.appendContext \"/a\xff\" { \"/nix/store/00000000000000000000000000000000-missing\" = { path = true; }; }; } ] \"x\"",
            b"\"${/a + \"\xff\"}\"",
            b"__findFile [ ] \"nix/a/../b\xe9\"",
            b"__findFile [ ] \"q\xe9\"",
            b"__findFile [ { prefix = \"p\xe9\"; path = \"/nope\xe9\"; } ] \"p\xe9/caf\xe9\"",
            b"builtins.readFile (__findFile [ ] \"nix/fetchurl.nix\xe9\")",
        ],
        compare_pure,
    );
}

/// `findFile` only rewrites `InvalidPathError`; an unreachable store
/// remains the original failure instead of being reported as invalid.
#[test]
fn find_file_preserves_store_failures() {
    struct UnavailableStore;
    impl brix_eval::Host for UnavailableStore {
        fn root_accessor(&self) -> std::sync::Arc<dyn brix_store::SourceAccessor> {
            std::sync::Arc::new(brix_store::MemoryAccessor::new())
        }

        fn is_valid_path(&self, _: &brix_store::StorePath) -> Result<bool, String> {
            Err("store is unavailable".into())
        }
    }

    let arena = brix_eval::Arena::new();
    let ev = brix_eval::Evaluator::new(&arena, config(), &UnavailableStore);
    let error = ev
        .eval_str(
            br#"__findFile [ { path = builtins.appendContext "/entry" {
                "/nix/store/00000000000000000000000000000000-missing" = { path = true; };
            }; } ] "x""#,
            "/",
        )
        .and_then(|value| ev.force(ev.alloc_cell(value), brix_eval::SrcPos::NONE))
        .expect_err("the store cannot validate the path");
    assert_eq!(error.msg, "store is unavailable");
}

/// `expr` in brix (pure, with `/pwd` readable) and in Nix reading the
/// real directory `dir` (impurely: pure mode forbids it), shown as
/// `/pwd`, on the `dummy://` store.
fn compare_reading(dir: &Path, expr: &[u8]) -> Result<(), String> {
    let host = TestHost::with_allowed(&[("/pwd", dir)], &["/pwd"]);
    let ours = run_brix_expr_bytes(&host, config(), expr, b"/pwd", Format::Plain);
    let theirs = run_nix_expr_bytes(expr, dir, Format::Plain, &NIX_READS_CWD, false);
    same_bytes(expr, &ours, &theirs)
}

/// Reading a tree whose names are not UTF-8: listings (in unsigned byte
/// order, the names as attribute names), contents, types and hashes of
/// files reached by such names, symlinks with such targets, importing
/// such files (their errors, traces, positions and `__curPos`), a
/// relative path literal in such a directory, lookups and the messages
/// that name such paths.
#[test]
fn files_with_names_that_are_not_utf8_read_as_in_nix() {
    let dir = tempfile::tempdir().expect("create a working directory");
    byte_tree_with_nix_files(dir.path());
    compare_all_bytes(
        "byte file names",
        &[
            &b"builtins.readDir ./."[..],
            b"builtins.attrNames (builtins.readDir ./.)",
            b"builtins.readDir (./. + \"/d\xe9\")",
            b"builtins.toJSON (builtins.readDir ./.)",
            b"builtins.readFile (./. + \"/caf\xe9\")",
            b"builtins.readFile ./l2",
            b"builtins.readFile (./. + \"/l\xff\")",
            b"builtins.readFile (./. + \"/nope\xe9\")",
            b"builtins.readFile (./. + \"/nul\xe9\")",
            b"builtins.readDir (./. + \"/caf\xe9\")",
            b"builtins.pathExists (./. + \"/caf\xe9\")",
            b"builtins.pathExists (./. + \"/nope\xe9\")",
            b"builtins.pathExists (toString ./. + \"/d\xe9/\")",
            b"builtins.pathExists (toString ./. + \"/caf\xe9/\")",
            b"builtins.readFileType (./. + \"/l\xff\")",
            b"builtins.readFileType (./. + \"/d\xe9\")",
            b"builtins.hashFile \"sha256\" (./. + \"/caf\xe9\")",
            b"toString (./. + \"/d\xe9\")",
            b"baseNameOf (./. + \"/d\xe9\")",
            b"dirOf (./. + \"/d\xe9/y\")",
            b"builtins.toXML (./. + \"/caf\xe9\")",
            b"import (./. + \"/caf\xe9.nix\")",
            b"builtins.scopedImport { } (./. + \"/caf\xe9.nix\")",
            b"import (./. + \"/pos\xe9.nix\")",
            b"builtins.toXML (import (./. + \"/pos\xe9.nix\"))",
            b"import (./. + \"/bad\xe9.nix\")",
            b"import (./. + \"/l\xff\")",
            b"import (./. + \"/d\xe9\")",
            b"(import (./. + \"/d\xe9\")).x == ./. + \"/d\xe9/y\"",
            b"builtins.unsafeGetAttrPos \"x\" (import (./. + \"/d\xe9\"))",
            b"__findFile [ { prefix = \"p\xe9\"; path = toString ./.; } ] \"p\xe9/caf\xe9\"",
            b"__findFile [ { prefix = \"q\"; path = toString ./.; } ] \"q/../caf\xe9\"",
        ],
        |expr| compare_reading(dir.path(), expr),
    );
}

/// `expr` in brix (pure, `/pwd` readable, its store virtual) and in Nix
/// reading the real directory `dir`, shown as `/pwd`, with a scratch
/// store for the copies.
fn compare_copying(dir: &Path, expr: &[u8]) -> Result<(), String> {
    let empty_store = tempfile::tempdir().expect("create a store directory");
    let host = TestHost::with_allowed(
        &[("/pwd", dir), ("/nix/store", empty_store.path())],
        &["/pwd"],
    );
    let ours = run_brix_expr_bytes(&host, config(), expr, b"/pwd", Format::Plain);
    let scratch = scratch_store();
    let store = scratch
        .path()
        .to_str()
        .expect("the scratch store's directory is UTF-8");
    let extra = [&NIX_READS_CWD[..], &["--store", store]].concat();
    let theirs = run_nix_expr_bytes(expr, dir, Format::Plain, &extra, true);
    same_bytes(expr, &ours, &theirs)
}

/// Copying a tree whose names are not UTF-8 to the store: its NAR (names
/// in unsigned byte order, symlink targets as bytes) and so its store
/// path, a filter seeing each path's bytes (in that order), symlinks
/// followed by `builtins.path`, and Nix's store path error for a name
/// that cannot be a store path name, under the "while adding path"
/// trace that names the source's bytes.
#[test]
fn copies_of_trees_with_names_that_are_not_utf8_match_nix() {
    let dir = tempfile::tempdir().expect("create a working directory");
    byte_tree(&dir.path().join("src"));
    compare_all_bytes(
        "byte file names copied",
        &[
            &b"\"${./src}\""[..],
            b"builtins.toJSON ./src",
            b"builtins.path { path = ./src; name = \"x\"; filter = p: t: builtins.match \".*\xe9\" p == null; }",
            b"builtins.filterSource (p: t: builtins.match \".*\xff\" p == null) ./src",
            b"builtins.path { path = ./src; name = \"y\"; filter = p: t: builtins.trace p true; }",
            b"builtins.path { path = ./src + \"/d\xe9\"; name = \"x\"; }",
            b"builtins.path { path = ./src + \"/caf\xe9\"; name = \"c\"; recursive = false; }",
            b"builtins.path { path = ./src/l2; name = \"l\"; }",
            b"builtins.filterSource (p: t: true) ./src/l2",
            b"\"${./src/l2}\"",
            b"builtins.path { path = ./src + \"/l\xff\"; name = \"l\"; }",
            b"builtins.path { path = ./src + \"/d\xe9\"; }",
            b"builtins.filterSource (p: t: true) (./src + \"/d\xe9\")",
            b"\"${./src + \"/caf\xe9\"}\"",
            b"\"${./src + \"/d\xe9\"}\"",
            b"builtins.toJSON (./src + \"/d\xe9\")",
        ],
        |expr| compare_copying(dir.path(), expr),
    );
}
