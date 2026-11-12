//! nixpkgs as a parser corpus.
//!
//! Every `.nix` file must parse. A deterministic sample is also compared
//! against `nix-instantiate --parse`; the whole tree is compared when run
//! with `--ignored`. Undefined variables are the compiler's to find, and
//! compared by `brix-eval/tests/parse_oracle.rs`.

mod common;

use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;

use brix_syntax::{parse, Origin, ParseOptions};
use brix_testutil::nixpkgs;
use common::{brix_parse, nix_parse_file, Oracle};

fn nix_files() -> Vec<PathBuf> {
    let root = nixpkgs();
    let mut files: Vec<PathBuf> = walkdir::WalkDir::new(root)
        .sort_by_file_name()
        .into_iter()
        .map(|e| e.expect("walk nixpkgs"))
        .filter(|e| e.file_type().is_file() && e.path().extension().is_some_and(|x| x == "nix"))
        .map(|e| e.into_path())
        .collect();
    files.sort();
    assert!(
        files.len() > 10_000,
        "expected a full nixpkgs checkout at {}",
        root
    );
    files
}

#[test]
fn every_file_parses() {
    let files = nix_files();
    let options = ParseOptions::default();
    let mut failures = Vec::new();
    let mut home_paths = 0;
    let mut warned = Vec::new();
    for file in &files {
        let src = fs::read(file).expect("read file");
        let dir = file.parent().expect("parent").as_os_str().as_bytes();
        let origin = Origin::Path(file.as_os_str().as_bytes().into());
        let mut warnings = Vec::new();
        let result = parse(&src, dir, &options, &mut warnings);
        if !warnings.is_empty() {
            warned.push(file.display().to_string());
        }
        match result {
            Ok(_) => {}
            // A few files use `~/…` paths, which pure evaluation rejects.
            Err(err) if err.msg.ends_with(b"can not be resolved in pure mode") => home_paths += 1,
            Err(err) => failures.push(format!(
                "{}: {}",
                file.display(),
                String::from_utf8_lossy(&err.render(&origin, &src))
            )),
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} files failed to parse:\n{}",
        failures.len(),
        files.len(),
        failures.join("\n")
    );
    // nixpkgs keeps clear of the `or` deprecation warning; the comparisons
    // with Nix below check the warnings' text.
    assert!(warned.is_empty(), "files with parse warnings: {warned:?}");
    assert!(
        home_paths < 10,
        "{home_paths} files rejected for home paths; expected only a handful"
    );
}

fn compare_with_nix(files: &[PathBuf]) {
    let threads = std::thread::available_parallelism()
        .map_or(4, |n| n.get())
        .min(16);
    let chunk = files.len().div_ceil(threads).max(1);
    let failures: Vec<String> = std::thread::scope(|scope| {
        let handles: Vec<_> = files
            .chunks(chunk)
            .map(|chunk| {
                scope.spawn(move || {
                    let mut failures = Vec::new();
                    for file in chunk {
                        let src = fs::read(file).expect("read file");
                        let dir = file.parent().expect("parent").as_os_str().as_bytes();
                        let origin = Origin::Stdin;
                        let expected = nix_parse_file(file);
                        let got = brix_parse(&src, dir, &origin);
                        let same = if expected.needs_scoping() {
                            matches!(got, Oracle::Parsed { .. })
                        } else {
                            expected == got
                        };
                        if !same {
                            failures.push(format!(
                                "{}:\n  nix:  {expected:?}\n  brix: {got:?}",
                                file.display()
                            ));
                        }
                    }
                    failures
                })
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|h| h.join().expect("thread"))
            .collect()
    });
    assert!(
        failures.is_empty(),
        "{} of {} files differ from nix:\n{}",
        failures.len(),
        files.len(),
        failures.join("\n")
    );
}

#[test]
fn sample_matches_nix() {
    let files = nix_files();
    let sample: Vec<PathBuf> = files.iter().step_by(50).cloned().collect();
    compare_with_nix(&sample);
}

#[test]
#[ignore = "runs nix-instantiate on every file in nixpkgs; slow"]
fn nixpkgs_full() {
    compare_with_nix(&nix_files());
}
