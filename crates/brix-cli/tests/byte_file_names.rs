//! File names that are not UTF-8, as Nix has them: bytes. `brix` against
//! the reference `nix`, with stdout and stderr compared byte for byte
//! (never as text, which would hide a difference in the bytes).
//!
//! The trees use the names that pin the order Nix lists a directory in
//! (unsigned bytes): `z` (0x7a), `caf\xe9`, `\xff`, `a\x80b`, a directory
//! `d\xe9` holding `f\xe9`, and a symlink `l\xff` to `t\xe9`.
//!
//! Every test needs `BRIX_NIX_BIN`; the one that builds needs the Nix
//! daemon and `BRIX_TEST_REAL_DAEMON=1`.

use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::process::Command;

use assert_cmd::prelude::*;
use brix_testutil::{
    join_bytes, require_env, scratch_store, strip_noise, symlink_bytes, write_bytes, Oracle,
    REAL_DAEMON,
};

/// What a run printed, as bytes.
#[derive(Debug, PartialEq, Eq)]
struct Run {
    status: i32,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

impl Run {
    /// The run with `from` replaced by `to` in what it printed (a
    /// directory that differs between the two tools' runs).
    fn replacing(self, from: &[u8], to: &[u8]) -> Run {
        Run {
            status: self.status,
            stdout: bstr::ByteSlice::replace(self.stdout.as_slice(), from, to),
            stderr: bstr::ByteSlice::replace(self.stderr.as_slice(), from, to),
        }
    }
}

/// `brix` in `dir`, with `--store <store>` when given (`dummy://` for
/// evaluation alone). Its stderr goes through the oracle's noise filter
/// too, so that what the machine says is left out on both sides.
fn brix(dir: &Path, store: Option<&str>, args: &[&str]) -> Run {
    let mut cmd = Command::cargo_bin("brix").expect("brix binary");
    if let Some(store) = store {
        cmd.arg("--store").arg(store);
    }
    let out = cmd.current_dir(dir).args(args).output().expect("run brix");
    Run {
        status: out.status.code().unwrap_or(-1),
        stdout: out.stdout,
        stderr: strip_noise(&out.stderr),
    }
}

/// The reference `nix` in `dir`, with the store `store` when given (a
/// scratch store, for what Nix copies).
fn nix(dir: &Path, store: Option<&Path>, args: &[&str]) -> Run {
    let mut oracle = Oracle::new();
    oracle.dir(dir).args(args);
    if let Some(store) = store {
        oracle.store(store.to_str().expect("a scratch store's path is text"));
    }
    let out = oracle.output();
    Run {
        status: out.code(),
        stdout: out.stdout,
        stderr: out.stderr,
    }
}

/// Writes the tree with the names that pin the order under `dir`.
fn write_byte_tree(dir: &Path) {
    write_bytes(dir, b"z", "z");
    write_bytes(dir, b"caf\xe9", "cafe");
    write_bytes(dir, b"\xff", "ff");
    write_bytes(dir, b"a\x80b", "a");
    write_bytes(dir, b"d\xe9/f\xe9", "in");
    symlink_bytes(dir, b"l\xff", b"t\xe9");
}

/// Everything under `dir`, by path relative to it (bytes): a directory
/// as such, a symlink with its target, a file with its contents.
fn listing(dir: &Path) -> Vec<(Vec<u8>, Vec<u8>)> {
    fn walk(dir: &Path, rel: &[u8], out: &mut Vec<(Vec<u8>, Vec<u8>)>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            let name = entry.file_name();
            let sep: &[u8] = if rel.is_empty() { b"" } else { b"/" };
            let rel = [rel, sep, name.as_bytes()].concat();
            let path = entry.path();
            let file_type = entry.file_type().unwrap();
            if file_type.is_symlink() {
                let target = std::fs::read_link(&path).unwrap();
                let what = [&b"symlink to "[..], target.as_os_str().as_bytes()].concat();
                out.push((rel, what));
            } else if file_type.is_dir() {
                out.push((rel.clone(), b"directory".to_vec()));
                walk(&path, &rel, out);
            } else {
                out.push((rel, std::fs::read(&path).unwrap()));
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, b"", &mut out);
    out.sort();
    out
}

/// `nix hash path` of a tree with byte names: its NAR, the names in
/// unsigned byte order (`z` before `caf\xe9` and `\xff`), symlink target
/// as it is.
#[test]
fn hash_path_of_byte_names_matches_nix() {
    let dir = tempfile::tempdir().unwrap();
    write_byte_tree(dir.path());
    let tree = dir.path().to_str().expect("a temporary directory is text");
    let args = ["hash", "path", tree];
    let theirs = nix(dir.path(), None, &args);
    // What `nix hash path` gives for this tree (`nix hash path` of the
    // same tree made with `printf` in a shell).
    assert_eq!(
        theirs.stdout,
        b"sha256-MILeOEzCphbGtFQAKFy6eoEVgfop81HfycvhlejC16Y=\n",
        "{}",
        String::from_utf8_lossy(&theirs.stderr)
    );
    assert_eq!(brix(dir.path(), Some("dummy://"), &args), theirs);
}

/// `nix flake init` from a template with byte names: every file written
/// under its own name, in the template's order, a directory reported as
/// written after what is in it; run again, identical files are skipped
/// and a different one is refused, with the template's paths shown as
/// they are (read in place, not copied).
#[test]
fn flake_init_copies_byte_names_like_nix() {
    let tpl = tempfile::tempdir().unwrap();
    std::fs::write(
        tpl.path().join("flake.nix"),
        "{ outputs = { self }: { templates.default = { path = ./t; description = \"bytes\"; }; }; }\n",
    )
    .unwrap();
    write_byte_tree(&tpl.path().join("t"));
    let template = format!("path:{}", tpl.path().display());
    let args = ["flake", "init", "-t", template.as_str()];
    let root = tempfile::tempdir().unwrap();
    let store = scratch_store();
    let mut outcomes = Vec::new();
    for who in ["brix", "nix"] {
        let dest = root.path().join(who);
        std::fs::create_dir(&dest).unwrap();
        let run = |dir: &Path| -> Run {
            let out = if who == "brix" {
                brix(dir, Some("dummy://"), &args)
            } else {
                nix(dir, Some(store.path()), &args)
            };
            out.replacing(dest.as_os_str().as_bytes(), b"DEST")
        };
        let first = run(&dest);
        let written = listing(&dest);
        std::fs::write(join_bytes(&dest, b"caf\xe9"), "changed").unwrap();
        let second = run(&dest);
        outcomes.push((first, written, second));
    }
    let (theirs, ours) = (outcomes.pop().unwrap(), outcomes.pop().unwrap());
    // What Nix does, so that the comparison below compares something.
    assert_eq!(
        theirs.0,
        Run {
            status: 0,
            stdout: Vec::new(),
            stderr: b"wrote: \"DEST/a\x80b\"\nwrote: \"DEST/caf\xe9\"\nwrote: \"DEST/d\xe9/f\xe9\"\nwrote: \"DEST/d\xe9\"\nwrote: \"DEST/l\xff\"\nwrote: \"DEST/z\"\nwrote: \"DEST/\xff\"\n".to_vec(),
        }
    );
    assert_eq!(theirs.2.status, 1);
    let shown = [tpl.path().as_os_str().as_bytes(), b"/t/caf\xe9"].concat();
    assert!(
        bstr::ByteSlice::find(theirs.2.stderr.as_slice(), &shown).is_some(),
        "{:?}",
        bstr::BStr::new(&theirs.2.stderr)
    );
    assert_eq!(ours.0, theirs.0, "the first run");
    assert_eq!(ours.1, theirs.1, "the files written");
    assert_eq!(ours.2, theirs.2, "the second run");
}

/// A `path:` flake that reads its own files, whose names are bytes: the
/// directory listing (names in byte order, with their types), files read
/// through a directory and a name that are not UTF-8, a symlink's type, a
/// path value shown where the flake is, and the store's refusal of such a
/// name for a store path. Nix copies nothing here but gets a scratch
/// store all the same; brix reads in place.
#[test]
fn a_path_flake_reads_byte_named_files_like_nix() {
    let dir = tempfile::tempdir().unwrap();
    write_byte_tree(dir.path());
    std::fs::write(
        dir.path().join("flake.nix"),
        b"{ outputs = { self }: {\n  names = builtins.attrNames (builtins.readDir ./.);\n  types = builtins.readDir ./.;\n  cafe = builtins.readFile (./. + \"/caf\xe9\");\n  inner = builtins.readFile (./. + \"/d\xe9/f\xe9\");\n  link = builtins.readFileType (./. + \"/l\xff\");\n  p = ./. + \"/caf\xe9\";\n  s = \"${./. + \"/d\xe9\"}\";\n  };\n}\n",
    )
    .unwrap();
    let store = scratch_store();
    let flake = format!("path:{}", dir.path().display());
    for attr in ["names", "types", "cafe", "inner", "link", "p", "s"] {
        let installable = format!("{flake}#{attr}");
        let args = ["eval", installable.as_str()];
        let theirs = nix(dir.path(), Some(store.path()), &args);
        let ours = brix(dir.path(), Some("dummy://"), &args);
        assert_eq!(ours, theirs, "{attr}");
        if attr == "names" {
            assert_eq!(
                theirs.stdout,
                b"[ \"a\x80b\" \"caf\xe9\" \"d\xe9\" \"flake.nix\" \"l\xff\" \"z\" \"\xff\" ]\n"
            );
        }
    }
}

/// A build whose builder prints bytes that are not UTF-8 and fails: with
/// `-L` its log lines are written as the builder printed them, prefixed
/// with the derivation's name, and the failure's message (the daemon's
/// `BuildResult::errorMsg`, which ends with the last log lines) as the
/// daemon sent it. A failed build is not remembered, so both tools build
/// it and fail alike.
#[test]
#[ignore = "builds through the real Nix daemon; set BRIX_TEST_REAL_DAEMON=1"]
fn build_logs_and_failures_keep_their_bytes() {
    require_env(REAL_DAEMON);
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("flake.nix"),
        format!(
            r#"{{
  outputs = {{ self }}: {{
    packages.{system}.broken = derivation {{
      name = "brix-bytes-{nonce}-{pid}";
      system = "{system}";
      builder = "/bin/sh";
      args = [ "-c" "printf 'first \\377 line\\n'; printf 'second \\351\\n' >&2; exit 3" ];
    }};
  }};
}}
"#,
            system = brix::host_system(),
            pid = std::process::id(),
        ),
    )
    .unwrap();
    let args = ["build", "-L", "--no-link", ".#broken"];
    let ours = brix(dir.path(), None, &args);
    let theirs = nix(dir.path(), None, &args);
    assert_eq!(
        theirs.status,
        1,
        "{}",
        String::from_utf8_lossy(&theirs.stderr)
    );
    for expected in [
        &b"brix-bytes> first \xff line\n"[..],
        b"brix-bytes> second \xe9\n",
        b"> first \xff line\n       > second \xe9\n",
    ] {
        assert!(
            bstr::ByteSlice::find(theirs.stderr.as_slice(), expected).is_some(),
            "{:?}",
            bstr::BStr::new(&theirs.stderr)
        );
    }
    assert_eq!(ours, theirs);
}
