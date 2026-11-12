//! `brix hash` against the reference `nix` (`BRIX_NIX_BIN`): status,
//! stdout and stderr, where flags are parsed (`--mode`, `--to`, `--from`,
//! `--format`, the algorithm flags) and how their errors and warnings
//! read.

use std::process::Command;

use assert_cmd::prelude::*;
use brix_testutil::{is_noise, nix_bin};

/// Status, stdout and stderr, with the program in a usage error's
/// "Try '<program> --help'" hint written as `nix`. brix and the reference
/// `nix` get the same configuration, so this builds the command itself
/// rather than through [`brix_testutil::Oracle`], and drops the oracle's
/// noise ([`is_noise`]) by hand.
fn run(cmd: &mut Command) -> (Option<i32>, String, String) {
    // Pinned so that the host's nix.conf enables no experimental feature.
    cmd.env("NIX_CONFIG", "experimental-features = nix-command");
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

#[test]
fn hash_flags_and_their_errors_match_nix() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("f"), "hello\n").unwrap();
    let f = dir.path().join("f");
    let f = f.to_str().unwrap();
    let sri = "sha256-ungWv48Bz+pBQUDeXa4iI7ADYaOWF3qctBD/YfIAFa0=";
    let nix32 = "1b8m03r63zqhnjf7l5wnldhh7c134ap5vpj0850ymkq1iyzicy5s";
    let blake3 = "6437b3ac38465133ffb63b75273a8db548c558465d79db03fd359c6cd5bd9d85";
    let blake3_on = ["--extra-experimental-features", "blake3-hashes"];
    let sha256_prefixed = format!("sha256:{nix32}");
    let blake3_prefixed = format!("blake3:{blake3}");
    let cases: Vec<Vec<&str>> = vec![
        // `parseFileIngestionMethod` knows no `recursive`.
        vec!["hash", "path", "--mode", "recursive", f],
        vec!["hash", "path", "--mode", "bogus", f],
        vec!["hash", "path", "--mode", "nar", f],
        // `git` needs `git-hashing` (which brix does not implement).
        vec!["hash", "path", "--mode", "git", f],
        // `base32` is accepted with a warning, once per use, in order.
        vec!["hash", "path", "--format", "base32", f],
        vec!["hash", "convert", "--to", "base32", sri],
        vec![
            "hash",
            "convert",
            "--from",
            "base32",
            "--to",
            "base32",
            "--hash-algo",
            "sha256",
            nix32,
        ],
        vec!["hash", "convert", "--from", "base32", "--to", "base16", sri],
        vec!["hash", "convert", "--to", "bogus", sri],
        // The first bad flag is the one reported.
        vec![
            "hash",
            "convert",
            "--to",
            "base32",
            "--hash-algo",
            "sha3",
            nix32,
        ],
        vec![
            "hash",
            "convert",
            "--hash-algo",
            "sha3",
            "--to",
            "bogus",
            nix32,
        ],
        // An unknown algorithm is a usage error wherever it is named.
        vec!["hash", "convert", "sha3:abc"],
        vec!["hash", "to-base16", "--type", "sha3", "abc"],
        vec!["hash", "to-base16", &sha256_prefixed],
        vec!["hash", "file", "--type", "md5", "--type", "sha1", f],
        // BLAKE3 needs its experimental feature, however it is named.
        vec!["hash", "path", "--type", "blake3", f],
        vec!["hash", "convert", "--hash-algo", "blake3", blake3],
        vec!["hash", "convert", &blake3_prefixed],
        vec![
            "hash",
            "convert",
            "--hash-algo",
            "sha256",
            "blake3-ZDezrDhGUTP/tjt1JzqNtUjFWEZdedsD/TWcbNW9nYU=",
        ],
        [&blake3_on[..], &["hash", "path", "--type", "blake3", f]].concat(),
        [
            &blake3_on[..],
            &["hash", "convert", "--hash-algo", "blake3", blake3],
        ]
        .concat(),
    ];
    for case in cases {
        let ours = run(Command::cargo_bin("brix").expect("brix binary").args(&case));
        let theirs = run(Command::new(nix_bin()).args(&case));
        assert_eq!(ours, theirs, "{case:?}");
    }
}
