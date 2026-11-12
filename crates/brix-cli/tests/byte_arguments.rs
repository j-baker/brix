//! Command-line arguments as bytes, against the reference `nix`
//! (`BRIX_NIX_BIN`): the expressions of `--expr` and `--apply`, the paths
//! of `--file` and `-I`, installables that name a store path through a
//! file name that is not UTF-8, and a current directory whose name is not
//! UTF-8. Status, stdout and stderr are compared byte for byte (never as
//! text, where every ill-formed sequence would look alike). What brix
//! keeps as text (flake references, attribute paths, most flags' values)
//! is refused by name, which is checked against brix's own message.
//!
//! Every test needs `BRIX_NIX_BIN` (the devshell sets it). Evaluation
//! uses the dummy store.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::process::{Command, Stdio};

use assert_cmd::prelude::*;
use brix_testutil::{strip_noise, Oracle, ORACLE_NIX_CONFIG};

/// What a command did: its status, stdout, and stderr without the
/// environment's remarks and with the program in a usage error's
/// "Try '<program> --help'" hint written as `nix`.
#[derive(PartialEq, Eq)]
struct Run {
    status: Option<i32>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

/// The bytes escaped, so that a failed comparison shows them.
impl std::fmt::Debug for Run {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "status {:?}\nstdout: {}\nstderr: {}",
            self.status,
            self.stdout.escape_ascii(),
            self.stderr.escape_ascii()
        )
    }
}

impl Run {
    fn new(status: Option<i32>, stdout: Vec<u8>, stderr: &[u8]) -> Run {
        Run {
            status,
            stdout,
            stderr: try_help_as_nix(&strip_noise(stderr)),
        }
    }
}

/// `stderr` with the "Try '<program> --help' for more information." line
/// naming `nix`, whatever the program's path.
fn try_help_as_nix(stderr: &[u8]) -> Vec<u8> {
    const END: &[u8] = b" --help' for more information.\n";
    stderr
        .split_inclusive(|&b| b == b'\n')
        .flat_map(|line| {
            if line.starts_with(b"Try '") && line.ends_with(END) {
                b"Try 'nix --help' for more information.\n".as_slice()
            } else {
                line
            }
        })
        .copied()
        .collect()
}

fn os_args<'a>(args: &[&'a [u8]]) -> Vec<&'a OsStr> {
    args.iter().map(|a| OsStr::from_bytes(a)).collect()
}

/// `brix --store dummy:// <args>` in `dir`, configured as the oracle is,
/// with `stdin` on standard input.
fn brix(dir: &Path, args: &[&[u8]], stdin: &[u8]) -> Run {
    use std::io::Write;
    let mut child = Command::cargo_bin("brix")
        .expect("brix binary")
        .current_dir(dir)
        .env("NIX_CONFIG", ORACLE_NIX_CONFIG)
        .env_remove("NIX_STORE_DIR")
        .env_remove("NIX_STORE")
        .args(["--store", "dummy://"])
        .args(os_args(args))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("run brix");
    let mut pipe = child.stdin.take().expect("stdin is piped");
    match pipe.write_all(stdin) {
        Ok(()) => {}
        // It exited without reading everything; its output says why.
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => {}
        Err(e) => panic!("writing brix's stdin: {e}"),
    }
    drop(pipe);
    let out = child.wait_with_output().expect("wait for brix");
    Run::new(out.status.code(), out.stdout, &out.stderr)
}

/// The reference `nix --store dummy:// <args>` in `dir`, with `stdin`
/// on standard input.
fn nix(dir: &Path, args: &[&[u8]], stdin: &[u8]) -> Run {
    let out = Oracle::new()
        .dir(dir)
        .env_remove("NIX_STORE_DIR")
        .env_remove("NIX_STORE")
        .args(["--store", "dummy://"])
        .args(os_args(args))
        .stdin(stdin)
        .output();
    Run::new(out.status.code(), out.stdout, &out.stderr)
}

/// Runs `args` with brix and with Nix in `dir`, `stdin` on standard
/// input, and asserts that they agree; Nix's run.
fn same_with_stdin(dir: &Path, args: &[&[u8]], stdin: &[u8]) -> Run {
    let theirs = nix(dir, args, stdin);
    let ours = brix(dir, args, stdin);
    let shown: Vec<String> = args.iter().map(|a| a.escape_ascii().to_string()).collect();
    assert_eq!(ours, theirs, "{shown:?}");
    theirs
}

/// [`same_with_stdin`] with nothing on standard input.
fn same(dir: &Path, args: &[&[u8]]) -> Run {
    same_with_stdin(dir, args, b"")
}

/// Whether `needle` occurs in `haystack`.
fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// `--expr` and `--apply` are bytes: a string and a path with a byte that
/// is not UTF-8, printed, raw, as an attribute name, in a syntax error's
/// excerpt, and from `--apply`.
#[test]
fn expressions_are_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let dir = dir.path();
    for args in [
        [&b"eval"[..], b"--expr", b"\"caf\xe9\""].as_slice(),
        &[b"eval", b"--raw", b"--expr", b"\"caf\xe9\""],
        &[b"eval", b"--expr", b"/a + \"/caf\xe9\""],
        &[b"eval", b"--expr", b"{ \"caf\xe9\" = 1; }"],
        &[b"eval", b"--expr", b"1", b"--apply", b"x: \"caf\xe9\""],
    ] {
        let theirs = same(dir, args);
        assert_eq!(theirs.status, Some(0), "{theirs:?}");
        assert!(contains(&theirs.stdout, b"caf\xe9"), "{theirs:?}");
    }
    // A syntax error quotes the expression's bytes in its excerpt.
    let theirs = same(dir, &[b"eval", b"--expr", b"\"caf\xe9\" +"]);
    assert_eq!(theirs.status, Some(1), "{theirs:?}");
    assert!(contains(&theirs.stderr, b"\"caf\xe9\" +"), "{theirs:?}");
}

/// `--file` names a file by bytes: absolute, relative, a directory's
/// `default.nix`, with a relative path literal and a file read beside it
/// (Nix's `--file` implies `--impure`; brix makes the file's directory
/// readable). `--file -` reads the expression's bytes from stdin, and
/// `-I` takes a path in bytes (`-I$'\xe9'`, as `-I` and its value).
#[test]
fn file_arguments_are_bytes() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let sub = dir.join(OsStr::from_bytes(b"d\xe9"));
    std::fs::create_dir(&sub).unwrap();
    std::fs::write(
        sub.join("default.nix"),
        b"{ s = \"caf\xe9\"; p = ./x; r = builtins.readFile ./f; }\n",
    )
    .unwrap();
    std::fs::write(sub.join("f"), b"hi\xff").unwrap();
    let absolute = [dir.as_os_str().as_bytes(), b"/d\xe9/default.nix"].concat();
    for args in [
        [&b"eval"[..], b"--file", &absolute].as_slice(),
        &[b"eval", b"--file", b"./d\xe9/default.nix"],
        &[b"eval", b"-f", b"d\xe9", b"p"],
        &[b"eval", b"--raw", b"-f", b"d\xe9", b"r"],
        &[b"eval", b"--expr", b"1", b"-I\xe9"],
    ] {
        let theirs = same(dir, args);
        assert_eq!(theirs.status, Some(0), "{theirs:?}");
    }
    let theirs = same_with_stdin(dir, &[b"eval", b"--file", b"-"], b"\"caf\xe9\"");
    assert_eq!(theirs.stdout, b"\"caf\xe9\"\n");
}

/// A current directory whose name is not UTF-8 is the base of relative
/// paths in `--expr`, `--apply` and `--file`, as bytes; pure evaluation's
/// refusal to read a file there quotes them.
#[test]
fn the_current_directory_is_bytes() {
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().join(OsStr::from_bytes(b"d\xe9"));
    std::fs::create_dir(&cwd).unwrap();
    std::fs::write(cwd.join("default.nix"), "{ here = ./.; up = ./sub/..; }\n").unwrap();
    std::fs::write(cwd.join("x.nix"), "1\n").unwrap();
    for args in [
        [&b"eval"[..], b"--expr", b"./."].as_slice(),
        &[b"eval", b"--raw", b"--expr", b"toString ./x"],
        &[b"eval", b"--expr", b"1", b"--apply", b"x: ./."],
        &[b"eval", b"--file", b"default.nix"],
    ] {
        let theirs = same(&cwd, args);
        assert_eq!(theirs.status, Some(0), "{theirs:?}");
        assert!(contains(&theirs.stdout, b"/d\xe9"), "{theirs:?}");
    }
    let theirs = same(&cwd, &[b"eval", b"--expr", b"import ./x.nix"]);
    assert_eq!(theirs.status, Some(1), "{theirs:?}");
    assert!(contains(&theirs.stderr, b"/d\xe9/x.nix"), "{theirs:?}");
}

/// An installable that names a store path through a file name that is
/// not UTF-8 (a symlink to it, a path inside it) is that store path:
/// `nix eval` then says the store path has no value.
#[test]
fn installables_name_store_paths_through_bytes() {
    const HELLO: &str = "/nix/store/5v2nn7qarcn9l81hvbphg27gvqy4zjma-hello-2.12.2";
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    std::os::unix::fs::symlink(HELLO, dir.join(OsStr::from_bytes(b"res\xe9"))).unwrap();
    let inside = [HELLO.as_bytes(), b"/caf\xe9"].concat();
    for args in [[&b"eval"[..], b"./res\xe9"].as_slice(), &[b"eval", &inside]] {
        let theirs = same(dir, args);
        assert_eq!(theirs.status, Some(1), "{theirs:?}");
        assert!(
            contains(&theirs.stderr, format!("installable '{HELLO}'").as_bytes()),
            "{theirs:?}"
        );
    }
}

/// Usage errors quote the argument's bytes, and an unknown command's
/// suggestions compare bytes, as Nix's do.
#[test]
fn usage_errors_quote_bytes() {
    let tmp = tempfile::tempdir().unwrap();
    for args in [
        [&b"ev\xffal"[..]].as_slice(),
        &[b"\xe9val"],
        &[b"eval", b"--ex\xffpr", b"1"],
        &[b"eval", b"--expr", b"1", b"a", b"b\xff"],
    ] {
        let theirs = same(tmp.path(), args);
        assert_eq!(theirs.status, Some(1), "{theirs:?}");
    }
}

/// What brix keeps as text is refused by name when it is not UTF-8,
/// where Nix takes bytes (KNOWN_DEVIATIONS.md, "Command-line arguments
/// that are not UTF-8"): a flake reference, an attribute path, a value of
/// a flag brix reads as text, and a flake reference in a current
/// directory whose name is not UTF-8.
#[test]
fn text_arguments_that_are_not_utf8_are_refused_by_name() {
    let tmp = tempfile::tempdir().unwrap();
    let refused = |dir: &Path, args: &[&[u8]], message: &[u8]| {
        let ours = brix(dir, args, b"");
        assert_eq!(ours.status, Some(1), "{ours:?}");
        assert_eq!(
            ours.stderr.escape_ascii().to_string(),
            [b"error: ".as_slice(), message, b"\n"]
                .concat()
                .escape_ascii()
                .to_string()
        );
    };
    refused(
        tmp.path(),
        &[b"eval", b"./a\xffb#x"],
        b"brix does not support flake references that are not valid UTF-8, such as './a\xffb#x'",
    );
    refused(
        tmp.path(),
        &[b"eval", b"--expr", b"{ }", b"a\xffb"],
        b"brix does not support attribute paths that are not valid UTF-8, such as 'a\xffb'",
    );
    refused(
        tmp.path(),
        &[b"eval", b"--expr", b"x: x", b"--argstr", b"x", b"\xff"],
        b"brix does not support command-line arguments that are not valid UTF-8 for '--argstr', such as '\xff'",
    );
    let cwd = tmp.path().join(OsStr::from_bytes(b"d\xe9"));
    std::fs::create_dir(&cwd).unwrap();
    // As `getcwd` returns it.
    let shown = std::fs::canonicalize(&cwd).unwrap();
    refused(
        &cwd,
        &[b"eval", b"nixpkgs#hello"],
        &[
            b"brix does not support flake references or file arguments in current directories that are not valid UTF-8, such as '".as_slice(),
            shown.as_os_str().as_bytes(),
            b"'",
        ]
        .concat(),
    );
}
