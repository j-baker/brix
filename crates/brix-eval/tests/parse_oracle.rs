//! `nix-instantiate --parse` against brix's parser *and compiler*: Nix
//! resolves variables right after parsing, so `--parse` reports undefined
//! variables, which brix finds when it compiles the AST
//! (`Evaluator::load`). brix-syntax's own oracle tests compare everything
//! else; these run the same corpora through the real front end, so that
//! the scoping rules checked are the evaluator's.
//!
//! - the language corpus: `parse-okay-*` against `.exp`, `parse-fail-*`
//!   against `.err.exp`, `eval-*` against the live oracle;
//! - expressions that exercise every kind of binding;
//! - a sample of nixpkgs (all of it with `--ignored`).

mod common;

use std::fs;
use std::path::{Path, PathBuf};

use brix_eval::{Arena, Config, Evaluator, Origin};
use brix_testutil::{nix_src, nixpkgs, Oracle};
use bstr::ByteSlice;
use common::TestHost;

/// What a parse printed: the expression and the warnings, or everything
/// on stderr when it failed.
#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    Parsed { out: String, warnings: String },
    Failed(String),
}

/// `nix-instantiate --parse -` on `src`, run from `cwd`.
fn nix_parse(src: &[u8], cwd: &Path) -> Outcome {
    let out = Oracle::nix_instantiate()
        .args(["--parse", "--pure-eval", "-"])
        .dir(cwd)
        .dummy_store()
        .config("warn-dirty = false")
        .stdin(src)
        .output();
    let stderr = out.stderr_lossy();
    if out.success() {
        Outcome::Parsed {
            out: out.stdout_lossy(),
            warnings: stderr,
        }
    } else {
        Outcome::Failed(stderr)
    }
}

/// brix's front end, as `nix-instantiate --parse -` uses Nix's: parse
/// stdin with relative paths against `base_dir`, then bind variables.
struct Brix<'a> {
    host: &'a TestHost,
    ev: Evaluator<'a>,
}

impl<'a> Brix<'a> {
    fn new(arena: &'a Arena, host: &'a TestHost) -> Brix<'a> {
        let config = Config {
            show_trace: false,
            ..Config::default()
        };
        Brix {
            host,
            ev: Evaluator::new(arena, config, host),
        }
    }

    fn parse(&self, src: &[u8], base_dir: &str) -> Outcome {
        let result = self
            .ev
            .parse_source(src, base_dir.as_bytes(), &Origin::Stdin)
            .and_then(|expr| {
                self.ev
                    .load(&expr, Origin::Stdin, src.to_vec(), None, false)
                    .map(|_| expr)
            });
        let logged = self.host.take_stderr();
        match result {
            Ok(expr) => {
                let mut out = brix_syntax::show_bytes(&expr);
                out.push(b'\n');
                Outcome::Parsed {
                    out: String::from_utf8_lossy(&out).into_owned(),
                    warnings: logged.to_str_lossy().into_owned(),
                }
            }
            // Decoded as the oracle's output is.
            Err(e) => Outcome::Failed(format!(
                "{}{}\n",
                logged.to_str_lossy(),
                brix_eval::filter_ansi_escapes(&e.render(false), true, usize::MAX).to_str_lossy()
            )),
        }
    }
}

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
    assert!(!files.is_empty(), "no {prefix}* files in the corpus");
    files
}

fn name(file: &Path) -> String {
    file.file_stem()
        .expect("file stem")
        .to_string_lossy()
        .into_owned()
}

fn report(failures: Vec<String>) {
    assert!(
        failures.is_empty(),
        "{} differences:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

#[test]
fn corpus_parse_okay() {
    let (arena, host) = (Arena::new(), TestHost::new(&[]));
    let brix = Brix::new(&arena, &host);
    let mut failures = Vec::new();
    for file in corpus_files("parse-okay-") {
        let src = fs::read(&file).expect("read source");
        let out = fs::read_to_string(file.with_extension("exp")).expect("read .exp");
        let expected = Outcome::Parsed {
            out,
            warnings: String::new(),
        };
        let got = brix.parse(&src, "/pwd");
        if got != expected {
            failures.push(format!(
                "{}:\n  nix:  {expected:?}\n  brix: {got:?}",
                name(&file)
            ));
        }
    }
    report(failures);
}

#[test]
fn corpus_parse_fail() {
    let (arena, host) = (Arena::new(), TestHost::new(&[]));
    let brix = Brix::new(&arena, &host);
    let mut failures = Vec::new();
    let mut undefined = 0;
    for file in corpus_files("parse-fail-") {
        // brix only evaluates purely, and Nix rejects `~/…` paths in pure
        // mode before the lexer reaches the trailing slash (see
        // brix-syntax/tests/corpus.rs).
        if name(&file) == "parse-fail-home-path-trailing-slash" {
            continue;
        }
        let src = fs::read(&file).expect("read source");
        let err = fs::read_to_string(file.with_extension("err.exp")).expect("read .err.exp");
        if err.starts_with("error: undefined variable") {
            undefined += 1;
        }
        let expected = Outcome::Failed(err);
        let got = brix.parse(&src, "/pwd");
        if got != expected {
            failures.push(format!(
                "{}:\n  nix:  {expected:?}\n  brix: {got:?}",
                name(&file)
            ));
        }
    }
    report(failures);
    assert!(undefined > 0, "the corpus has undefined-variable cases");
}

/// Known differences in the `eval-*` comparison, with the reason. Each
/// must still differ, so the list stays honest.
const EVAL_FILES_ALLOWLIST: &[(&str, &str)] = &[];

#[test]
fn corpus_eval_files() {
    let (arena, host) = (Arena::new(), TestHost::new(&[]));
    let brix = Brix::new(&arena, &host);
    let dir = lang_dir();
    let base = dir.to_str().expect("utf-8 path");
    let mut failures = Vec::new();
    for file in corpus_files("eval-") {
        let src = fs::read(&file).expect("read source");
        let expected = nix_parse(&src, &dir);
        let got = brix.parse(&src, base);
        let allowed = EVAL_FILES_ALLOWLIST.iter().any(|(n, _)| *n == name(&file));
        match (got == expected, allowed) {
            (true, false) | (false, true) => {}
            (true, true) => failures.push(format!("{}: allowlisted but now the same", name(&file))),
            (false, false) => failures.push(format!(
                "{}:\n  nix:  {expected:?}\n  brix: {got:?}",
                name(&file)
            )),
        }
    }
    report(failures);
}

/// Every construct that binds variables, used correctly and with a
/// variable it does not bind.
#[test]
fn scoping() {
    let (arena, host) = (Arena::new(), TestHost::new(&[]));
    let brix = Brix::new(&arena, &host);
    let cases = [
        "x",
        "x: x",
        "x: y",
        "{ a, b ? a }: b",
        "{ a ? b }: a",
        "args@{ a }: args",
        "{ a }@args: args.b",
        "{ ... }@args: a",
        "let a = 1; in a",
        "let a = b; b = 1; in a",
        "let a = 1; in b",
        "let { a = 1; body = a; }",
        "let { a = 1; body = b; }",
        "rec { a = 1; b = a; }",
        "rec { a = c; }",
        "{ a = 1; b = a; }",
        "let a = 1; in { inherit a; }",
        "{ inherit a; }",
        "let a = { b = 1; }; in { inherit (a) b; }",
        "{ inherit (a) b; }",
        "let a = 1; in rec { inherit a; b = a; }",
        "rec { inherit (b) c; b = { c = 1; }; }",
        "with {}; x",
        "with x; y",
        "x: with x; y",
        "let x = 1; in with {}; x",
        "{ ${a} = 1; }",
        "x: { ${x} = 1; }",
        "\"${x}\"",
        "./a/${x}",
        "x: x.${y}",
        "x: x ? ${y}",
        "x: x.a or y",
        "assert x; 1",
        "if x then 1 else 2",
        "[ x ]",
        "x: [ x y ]",
        "__curPos",
        "builtins",
        "true && false || null",
        "map toString [ ]",
        "__sub 1 2",
        "__unknownBuiltin",
        "derivation",
        "<nixpkgs>",
        "__nixPath",
        "__findFile",
        "1 - 2",
        "let __sub = 1; in 1 - 2",
        "import",
        "scopedImport",
        "let or = 1; in [ (x: x) or ]",
    ];
    let cwd = Path::new("/");
    let mut failures = Vec::new();
    for src in cases {
        let expected = nix_parse(src.as_bytes(), cwd);
        let got = brix.parse(src.as_bytes(), "/");
        if got != expected {
            failures.push(format!("{src:?}:\n  nix:  {expected:?}\n  brix: {got:?}"));
        }
    }
    report(failures);
}

fn nixpkgs_files() -> Vec<PathBuf> {
    let root = PathBuf::from(nixpkgs());
    let mut files: Vec<PathBuf> = walk(&root);
    files.sort();
    assert!(
        files.len() > 10_000,
        "expected a full nixpkgs checkout at {}",
        root.display()
    );
    files
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for entry in fs::read_dir(dir).expect("read nixpkgs directory") {
        let entry = entry.expect("directory entry");
        let path = entry.path();
        let ty = entry.file_type().expect("file type");
        if ty.is_dir() {
            out.extend(walk(&path));
        } else if ty.is_file() && path.extension().is_some_and(|e| e == "nix") {
            out.push(path);
        }
    }
    out
}

fn compare_nixpkgs(files: &[PathBuf]) {
    let threads = std::thread::available_parallelism()
        .map_or(4, |n| n.get())
        .min(16);
    let chunk = files.len().div_ceil(threads).max(1);
    let failures: Vec<String> = std::thread::scope(|scope| {
        let handles: Vec<_> = files
            .chunks(chunk)
            .map(|chunk| {
                scope.spawn(move || {
                    let (arena, host) = (Arena::new(), TestHost::new(&[]));
                    let brix = Brix::new(&arena, &host);
                    let mut failures = Vec::new();
                    for file in chunk {
                        let src = fs::read(file).expect("read file");
                        let dir = file.parent().expect("parent");
                        let expected = nix_parse(&src, dir);
                        let got = brix.parse(&src, dir.to_str().expect("utf-8 path"));
                        if got != expected {
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
    report(failures);
}

#[test]
fn nixpkgs_sample() {
    let files = nixpkgs_files();
    let sample: Vec<PathBuf> = files.iter().step_by(50).cloned().collect();
    compare_nixpkgs(&sample);
}

#[test]
#[ignore = "runs nix-instantiate on every file in nixpkgs; slow"]
fn nixpkgs_full() {
    compare_nixpkgs(&nixpkgs_files());
}
