//! The Nix language test corpus (`tests/functional/lang` in the Nix
//! sources), as `lang.sh` runs it for the parser:
//!
//! - `parse-okay-*.nix`: `nix-instantiate --parse` output must equal `.exp`;
//! - `parse-fail-*.nix`: must fail, with stderr equal to `.err.exp`;
//! - `eval-*.nix`: parsed the same way, compared against the live oracle.
//!
//! The corpus is read from stdin with the working directory `/pwd` (the
//! expected files have the real directory replaced by that). Undefined
//! variables are found by the compiler, not the parser: those cases are
//! only checked to parse here, and compared in full by
//! `brix-eval/tests/parse_oracle.rs`.

mod common;

use std::fs;
use std::path::{Path, PathBuf};

use brix_syntax::Origin;
use brix_testutil::nix_src;
use common::{brix_parse, nix_parse_stdin, Oracle};

fn lang_dir() -> PathBuf {
    nix_src().join("tests/functional/lang")
}

fn corpus_files(prefix: &str) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = fs::read_dir(lang_dir())
        .expect("read the corpus directory")
        .map(|e| e.expect("directory entry").path())
        .filter(|p| {
            p.extension().is_some_and(|e| e == "nix")
                && p.file_name()
                    .is_some_and(|n| n.to_string_lossy().starts_with(prefix))
        })
        .collect();
    files.sort();
    assert!(
        !files.is_empty(),
        "no {prefix}* files in {}",
        lang_dir().display()
    );
    files
}

fn stem(file: &Path) -> String {
    file.file_stem()
        .expect("file stem")
        .to_string_lossy()
        .into_owned()
}

/// Known differences from the expected output, with the reason.
fn parse_fail_allow_list(name: &str) -> Option<&'static str> {
    match name {
        // brix only evaluates purely, and Nix rejects `~/…` paths in pure
        // mode before the lexer reaches the trailing slash.
        "parse-fail-home-path-trailing-slash" => Some("pure mode rejects ~/ paths first"),
        _ => None,
    }
}

#[test]
fn parse_okay() {
    let mut failures = Vec::new();
    for file in corpus_files("parse-okay-") {
        let src = fs::read(&file).expect("read source");
        let expected = fs::read_to_string(file.with_extension("exp")).expect("read .exp");
        match brix_parse(&src, "/pwd", &Origin::Stdin) {
            Oracle::Parsed { out, warnings } if out == expected && warnings.is_empty() => {}
            other => failures.push(format!(
                "{}:\n  expected: {expected:?}\n  got:      {other:?}",
                stem(&file)
            )),
        }
    }
    assert!(
        failures.is_empty(),
        "{} parse-okay failures:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

#[test]
fn parse_fail() {
    let mut failures = Vec::new();
    let mut allowed = 0;
    let mut scoping = 0;
    for file in corpus_files("parse-fail-") {
        let name = stem(&file);
        let src = fs::read(&file).expect("read source");
        let expected = fs::read_to_string(file.with_extension("err.exp")).expect("read .err.exp");
        let got = brix_parse(&src, "/pwd", &Origin::Stdin);
        if expected.starts_with("error: undefined variable '") {
            scoping += 1;
            if !matches!(got, Oracle::Parsed { .. }) {
                failures.push(format!("{name}: should parse, got {got:?}"));
            }
            continue;
        }
        match got {
            Oracle::Failed(err) if err == expected => {}
            Oracle::Failed(err) if parse_fail_allow_list(&name).is_some() => {
                allowed += 1;
                eprintln!(
                    "{name}: allowed difference ({}):\n{err}",
                    parse_fail_allow_list(&name).unwrap()
                );
            }
            other => failures.push(format!(
                "{name}:\n  expected: {expected:?}\n  got:      {other:?}"
            )),
        }
    }
    assert!(
        failures.is_empty(),
        "{} parse-fail failures:\n{}",
        failures.len(),
        failures.join("\n")
    );
    assert_eq!(
        allowed, 1,
        "the allow list should only be used by the files that need it"
    );
    assert!(scoping > 0, "the corpus has undefined-variable cases");
}

/// Every evaluation test file, parsed by both parsers from the corpus
/// directory (so that relative paths resolve to the same place).
#[test]
fn eval_files_parse_like_nix() {
    let mut failures = Vec::new();
    let dir = lang_dir();
    let base = dir.to_str().expect("utf-8 path");
    for file in corpus_files("eval-") {
        let src = fs::read(&file).expect("read source");
        let expected = nix_parse_stdin(&src, &dir);
        let got = brix_parse(&src, base, &Origin::Stdin);
        let same = if expected.needs_scoping() {
            matches!(got, Oracle::Parsed { .. })
        } else {
            expected == got
        };
        if !same {
            failures.push(format!(
                "{}:\n  nix:  {expected:?}\n  brix: {got:?}",
                stem(&file)
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} differences:\n{}",
        failures.len(),
        failures.join("\n")
    );
}
