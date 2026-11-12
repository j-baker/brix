//! Nix's language test corpus (`tests/functional/lang` in the Nix source
//! tree), run the way `lang.sh` runs it: `eval-okay-*` must print exactly
//! their `.exp` (and `.err.exp` on stderr), `eval-fail-*` must fail with
//! exactly their `.err.exp`, byte for byte.
//!
//! The tests run in pure mode; tests that need `--impure`, `$NIX_PATH`
//! or `--arg` are listed in `ALLOWLIST` with a reason, as are the few whose
//! output brix does not match yet, and `EXPECTED_FAILURES` pins how each
//! of them fails. Anything on the list that starts passing, or fails
//! differently, fails this test, so the list stays honest.
//!
//! The corpus also runs through a compiled image of its directory, which
//! must change nothing.

mod common;

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::Arc;

use brix_eval::image::writer::CompileOptions;
use brix_eval::image::{compile_tree, Image, ImageAccessor, ImageCompileContext};
use brix_eval::{Arena, Config, Evaluator};
use brix_store::{CanonPath, FsAccessor};
use brix_testutil::nix_src;
use bstr::{BString, ByteSlice};
use common::{run_brix_file, run_brix_file_with_image, Format, Outcome, TestHost};

/// Tests whose outcome is known to differ, with the reason.
const ALLOWLIST: &[(&str, &str)] = &[
    (
        "eval-okay-builtins",
        "builtins.currentSystem exists only in impure mode",
    ),
    (
        "eval-fail-storePath-1",
        "builtins.storePath fails earlier in pure mode",
    ),
    (
        "eval-okay-path-string-interpolation",
        "needs --impure (~ paths)",
    ),
    (
        "eval-fail-readDir-nonexistent-1",
        "absolute paths outside the store are forbidden in pure mode",
    ),
    (
        "eval-fail-fetchTree-relative-path",
        "fetcher error messages come from the host",
    ),
    (
        "eval-okay-flake-ref-to-string",
        "flake built-ins are registered by the flake layer",
    ),
    (
        "eval-okay-parse-flake-ref",
        "flake built-ins are registered by the flake layer",
    ),
    (
        "eval-fail-flake-ref-to-string-negative-integer",
        "flake built-ins are registered by the flake layer",
    ),
    ("eval-fail-fromTOML-overflow", "toml11-specific error text"),
    ("eval-fail-fromTOML-underflow", "toml11-specific error text"),
    ("eval-okay-search-path", "needs -I search paths (impure)"),
    ("eval-okay-autoargs", "needs --arg/--argstr"),
    ("eval-okay-getenv", "needs an environment (impure)"),
    (
        "eval-okay-home-path-canonicalization",
        "needs --impure (~ paths)",
    ),
    ("eval-okay-home-path-warn", "needs --impure (~ paths)"),
    ("eval-fail-home-path-fatal", "needs --impure (~ paths)"),
];

/// Why each allowlisted test fails today: a substring of the failure
/// that pins the reason, so a case cannot keep failing for another one.
/// Every entry of `ALLOWLIST` needs one, and only those.
const EXPECTED_FAILURES: &[(&str, &str)] = &[
    (
        "eval-okay-builtins",
        "assertion '((builtins) ? currentSystem)' failed",
    ),
    (
        "eval-fail-storePath-1",
        "'builtins.storePath' is not allowed in pure evaluation mode",
    ),
    (
        "eval-okay-path-string-interpolation",
        "the path '~/' can not be resolved in pure mode",
    ),
    (
        "eval-fail-readDir-nonexistent-1",
        "access to absolute path '/this/path/really/should/not/exist' is forbidden in pure \
         evaluation mode",
    ),
    (
        "eval-fail-fetchTree-relative-path",
        "'fetchTree' is not supported by this host (no fetcher configured)",
    ),
    (
        "eval-okay-flake-ref-to-string",
        "attribute 'flakeRefToString' missing",
    ),
    (
        "eval-okay-parse-flake-ref",
        "attribute 'parseFlakeRef' missing",
    ),
    (
        "eval-fail-flake-ref-to-string-negative-integer",
        "attribute 'flakeRefToString' missing",
    ),
    (
        "eval-fail-fromTOML-overflow",
        "while parsing TOML: number too large to fit in target type",
    ),
    (
        "eval-fail-fromTOML-underflow",
        "while parsing TOML: number too small to fit in target type",
    ),
    ("eval-okay-search-path", "unsupported flag -I"),
    ("eval-okay-autoargs", "unsupported flag --arg"),
    (
        "eval-okay-getenv",
        "--- expected\n\"foobar\"\n--- got\n\"bar\"\n",
    ),
    (
        "eval-okay-home-path-canonicalization",
        "unsupported flag --impure",
    ),
    ("eval-okay-home-path-warn", "unsupported flag --impure"),
    ("eval-fail-home-path-fatal", "unsupported flag --impure"),
];

struct Case {
    name: String,
    flags: Vec<String>,
    exp: Option<BString>,
    err_exp: Option<BString>,
    exp_xml: Option<BString>,
    disabled: bool,
    postprocess: bool,
}

/// The file's contents, or `None` if it does not exist.
fn read_opt(p: &Path) -> Option<BString> {
    match std::fs::read(p) {
        Ok(bytes) => Some(bytes.into()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => panic!("reading {}: {e}", p.display()),
    }
}

fn cases(lang: &Path) -> Vec<Case> {
    let mut out = Vec::new();
    let mut names: Vec<String> = std::fs::read_dir(lang)
        .expect("list the corpus")
        .map(|e| {
            let name = e.expect("list the corpus").file_name();
            name.into_string().expect("corpus file names are UTF-8")
        })
        .collect();
    names.sort();
    for fname in names {
        let Some(name) = fname.strip_suffix(".nix") else {
            continue;
        };
        if !(name.starts_with("eval-okay-") || name.starts_with("eval-fail-")) {
            continue;
        }
        let base = lang.join(name);
        out.push(Case {
            name: name.to_owned(),
            flags: read_opt(&base.with_extension("flags"))
                .map(|s| {
                    s.to_str()
                        .expect("flags are UTF-8")
                        .split_whitespace()
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default(),
            exp: read_opt(&base.with_extension("exp")),
            err_exp: read_opt(&base.with_extension("err.exp")),
            exp_xml: read_opt(&base.with_extension("exp.xml")),
            disabled: base.with_extension("exp-disabled").exists(),
            postprocess: base.with_extension("postprocess").exists(),
        });
    }
    out
}

/// `eval-fail-derivation-name.postprocess`: line numbers in the wrapper
/// change, so digits are normalised.
fn postprocess(s: &[u8]) -> BString {
    // First the 8-wide line-number field (digits and spaces) before a
    // non-digit, then any remaining digits, exactly as the sed script does.
    let field = regex::bytes::Regex::new(r"[0-9 ]{7}[0-9]([^0-9])").unwrap();
    let s = field.replace_all(s, &b"<number>$1"[..]);
    let digits = regex::bytes::Regex::new(r"[0-9]+").unwrap();
    digits.replace_all(&s, &b"<number>"[..]).into_owned().into()
}

/// How `nix-instantiate` would run `case`: the configuration and output
/// format its flags ask for.
fn case_config(case: &Case) -> Result<(Config, Format), String> {
    let mut config = Config::default();
    let mut it = case.flags.iter();
    while let Some(f) = it.next() {
        match f.as_str() {
            "--eval" | "--strict" | "--show-trace" => {}
            "--no-show-trace" => config.show_trace = false,
            "--max-call-depth" => {
                config.max_call_depth = it
                    .next()
                    .expect("--max-call-depth takes a value")
                    .parse()
                    .expect("--max-call-depth takes a number");
            }
            "--extra-experimental-features" => {
                config.experimental_features.insert(
                    it.next()
                        .expect("--extra-experimental-features takes a value")
                        .clone(),
                );
            }
            "--lint-url-literals" => config.lints.url_literals = lint_level(it.next()),
            "--lint-short-path-literals" => {
                config.lints.short_path_literals = lint_level(it.next());
            }
            "--lint-absolute-path-literals" => {
                config.lints.absolute_path_literals = lint_level(it.next());
            }
            other => return Err(format!("unsupported flag {other}")),
        }
    }
    let format = if case.exp_xml.is_some() {
        Format::Xml
    } else {
        Format::Plain
    };
    Ok((config, format))
}

/// The corpus file `case` evaluates, on the test host.
fn case_path(case: &Case) -> String {
    format!("/pwd/lang/{}.nix", case.name)
}

/// The host the corpus runs on: the functional test directory at `/pwd`
/// and an empty store directory (as in Nix's pure mode, it exists; the
/// corpus checks `pathExists builtins.storeDir`).
fn corpus_host(functional: &Path, store: &Path) -> TestHost {
    TestHost::new(&[("/pwd", functional), ("/nix/store", store)])
}

fn run_case(host: &TestHost, case: &Case) -> Result<(), String> {
    let (config, format) = case_config(case)?;
    let Outcome { stdout, stderr, ok } = run_brix_file(host, config, case_path(case), format);
    let (stdout, stderr) = if case.postprocess {
        (postprocess(&stdout), postprocess(&stderr))
    } else {
        (stdout, stderr)
    };
    let empty = BString::default();
    if case.name.starts_with("eval-fail-") {
        if ok {
            return Err(format!("should fail but evaluated to:\n{stdout}"));
        }
        return same("stderr", case.err_exp.as_ref().unwrap_or(&empty), &stderr);
    }
    if !ok {
        return Err(format!("should evaluate but failed:\n{stderr}"));
    }
    let expected = case.exp_xml.as_ref().or(case.exp.as_ref());
    same("stdout", expected.unwrap_or(&empty), &stdout)?;
    if case.exp_xml.is_none() {
        same("stderr", case.err_exp.as_ref().unwrap_or(&empty), &stderr)?;
    }
    Ok(())
}

/// Whether `got` is byte for byte what was expected; the error shows
/// both, escaped where the difference is in bytes that are not UTF-8.
fn same(what: &str, expected: &BString, got: &BString) -> Result<(), String> {
    if expected == got {
        Ok(())
    } else if expected.to_str_lossy() == got.to_str_lossy() {
        Err(format!(
            "{what} differs only in bytes that are not UTF-8\n--- expected\n{expected:?}\n--- \
             got\n{got:?}"
        ))
    } else {
        Err(format!(
            "{what} differs\n--- expected\n{expected}--- got\n{got}"
        ))
    }
}

/// Runs `f` on a thread with a deep stack: deeply nested expressions
/// need one, as in Nix (which asks for 64 MB).
fn with_deep_stack(f: fn()) {
    std::thread::Builder::new()
        .stack_size(1 << 30)
        .spawn(f)
        .expect("spawn the corpus thread")
        .join()
        .expect("the corpus thread panicked");
}

#[test]
fn language_corpus() {
    with_deep_stack(language_corpus_inner);
}

fn language_corpus_inner() {
    let functional = nix_src().join("tests/functional");
    let lang = functional.join("lang");
    let store = tempfile::tempdir().expect("temporary directory");
    let host = corpus_host(&functional, store.path());
    let allow: BTreeMap<&str, &str> = ALLOWLIST.iter().copied().collect();
    let expected_failures: BTreeMap<&str, &str> = EXPECTED_FAILURES.iter().copied().collect();
    let unexplained: Vec<&str> = allow
        .keys()
        .filter(|name| !expected_failures.contains_key(*name))
        .copied()
        .collect();
    let stale: Vec<&str> = expected_failures
        .keys()
        .filter(|name| !allow.contains_key(*name))
        .copied()
        .collect();
    assert!(
        unexplained.is_empty() && stale.is_empty(),
        "ALLOWLIST and EXPECTED_FAILURES disagree: no expected failure for {unexplained:?}, \
         not allowlisted: {stale:?}"
    );
    let mut failures: Vec<String> = Vec::new();
    let mut unexpected_passes: Vec<String> = Vec::new();
    let (mut passed, mut allowlisted, mut disabled) = (0usize, 0usize, 0usize);
    for case in cases(&lang) {
        if case.disabled {
            disabled += 1;
            continue;
        }
        let result = run_case(&host, &case);
        match (result, expected_failures.get(case.name.as_str())) {
            (Ok(()), None) => passed += 1,
            (Ok(()), Some(_)) => unexpected_passes.push(case.name.clone()),
            (Err(msg), Some(why)) if msg.contains(why) => allowlisted += 1,
            (Err(msg), Some(why)) => failures.push(format!(
                "{}: allowlisted for failing with {why:?}, but failed otherwise: {msg}",
                case.name
            )),
            (Err(msg), None) => failures.push(format!("{}: {msg}", case.name)),
        }
    }
    eprintln!(
        "language corpus: {passed} passed, {} failed, {allowlisted} allowlisted, {disabled} disabled",
        failures.len()
    );
    for f in &failures {
        eprintln!("FAIL {f}\n");
    }
    assert!(
        unexpected_passes.is_empty(),
        "these allowlisted tests now pass; remove them from ALLOWLIST and EXPECTED_FAILURES: \
         {unexpected_passes:?}"
    );
    assert!(
        failures.is_empty(),
        "{} corpus tests failed",
        failures.len()
    );
}

/// The cases that link no module from the corpus image, and why: the
/// image holds only the error importing their file raises, or cannot
/// hold their code. Every other case links its file.
const NOT_LINKED: &[(&str, &str)] = &[
    (
        "eval-fail-attrset-merge-drops-later-rec",
        "undefined variable",
    ),
    ("eval-fail-dynamic-attrs-inherit", "parse error"),
    ("eval-fail-dynamic-attrs-inherit-2", "parse error"),
    ("eval-fail-dynamic-attrs-let", "parse error"),
    ("eval-fail-dynamic-attrs-let-2", "parse error"),
    ("eval-fail-dynamic-attrs-let-3", "parse error"),
    ("eval-fail-eol-1", "parse error"),
    ("eval-fail-eol-2", "parse error"),
    ("eval-fail-eol-3", "parse error"),
    ("eval-fail-path-slash", "parse error"),
    (
        "eval-fail-pipe-operators",
        "parse error (the feature is off)",
    ),
    ("eval-fail-set", "undefined variable"),
    ("eval-fail-string-nul-1", "parse error"),
    ("eval-fail-string-nul-2", "parse error"),
    (
        "eval-okay-path-string-interpolation",
        "a `~/` path literal, which is not relative to the tree",
    ),
    (
        "eval-okay-pathexists",
        "path literals that climb out of the tree",
    ),
    // Files whose parse logs warnings are served from source, where
    // Nix logs them on every parse.
    ("eval-fail-abs-path-fatal", "parse warnings (lint)"),
    ("eval-fail-short-path-literal", "parse warnings (lint)"),
    ("eval-fail-url-literal", "parse warnings (lint)"),
    ("eval-okay-abs-path-warn", "parse warnings (lint)"),
    (
        "eval-okay-deprecate-cursed-or",
        "parse warnings (cursed or)",
    ),
    ("eval-okay-dotdotslash-abs-fatal", "parse warnings (lint)"),
    ("eval-okay-dotdotslash-path-fatal", "parse warnings (lint)"),
    ("eval-okay-short-path-literal-warn", "parse warnings (lint)"),
    ("eval-okay-short-path-variation", "parse warnings (lint)"),
    ("eval-okay-url-literal-warn", "parse warnings (lint)"),
];

/// The corpus again, with the `lang` directory compiled into an image
/// that serves it (files and code), as a compiled flake input would be:
/// every case must come out exactly as it does from source, and every
/// case but those in [`NOT_LINKED`] must link its file from the image.
/// (Cases with flags this harness does not support are skipped, as they
/// fail from source too.)
#[test]
fn language_corpus_through_image() {
    with_deep_stack(language_corpus_through_image_inner);
}

fn language_corpus_through_image_inner() {
    let functional = nix_src().join("tests/functional");
    let lang = functional.join("lang");
    let store = tempfile::tempdir().expect("temporary directory");
    let source_host = corpus_host(&functional, store.path());
    let tree = FsAccessor::new(&lang);
    // One image per evaluator fingerprint (experimental features add
    // builtins); the file tree is the same in all of them.
    let mut images: HashMap<[u8; 32], Arc<Image>> = HashMap::new();
    let mut image_for = |ctx: &ImageCompileContext| -> Arc<Image> {
        images
            .entry(ctx.fingerprint)
            .or_insert_with(|| {
                let bytes =
                    compile_tree(&tree, &CanonPath::root(), ctx, &CompileOptions::default())
                        .expect("the corpus compiles to an image");
                Arc::new(Image::open(bytes, Some(&ctx.fingerprint)).expect("the image opens"))
            })
            .clone()
    };
    let mount_at = "/pwd/lang";
    let mount = CanonPath::new(mount_at).expect("absolute");
    let default_image = {
        let arena = Arena::new();
        let ev = Evaluator::new(&arena, Config::default(), &source_host);
        image_for(ev.image_compile_context())
    };
    // Files the image does not store (all but the `.nix` files) come from
    // the corpus itself, as the facade arranges for a compiled input; the
    // image is shown at `/pwd/lang` as the source host shows the corpus.
    let served = ImageAccessor::new(default_image).with_fallback(Arc::new(FsAccessor::new(&lang)));
    let image_host = TestHost::with_accessors(
        &[
            ("/pwd", common::shown_at("/pwd", &functional)),
            (mount_at, common::shown_as(mount_at, Arc::new(served))),
            ("/nix/store", common::shown_at("/nix/store", store.path())),
        ],
        &["/pwd", "/nix/store"],
    );
    let mut differences = Vec::new();
    let (mut compared, mut linked_total) = (0usize, 0u64);
    let mut unlinked: Vec<String> = Vec::new();
    for case in cases(&lang) {
        let Ok((config, format)) = case_config(&case) else {
            continue;
        };
        if case.disabled {
            continue;
        }
        let path = case_path(&case);
        let from_source = run_brix_file(&source_host, config.clone(), &path, format);
        let (from_image, linked) =
            run_brix_file_with_image(&image_host, config, &path, format, &mount, &mut image_for);
        compared += 1;
        linked_total += linked;
        if linked == 0 {
            unlinked.push(case.name.clone());
        }
        if from_source != from_image {
            differences.push(format!(
                "{}\n--- from source (ok={})\n{}{}--- through the image (ok={})\n{}{}",
                case.name,
                from_source.ok,
                from_source.stdout,
                from_source.stderr,
                from_image.ok,
                from_image.stdout,
                from_image.stderr
            ));
        }
    }
    eprintln!(
        "language corpus through an image: {compared} cases, {} differ, {linked_total} modules \
         linked",
        differences.len(),
    );
    for d in &differences {
        eprintln!("DIFFERS {d}\n");
    }
    assert!(
        differences.is_empty(),
        "{} corpus cases evaluate differently through the image",
        differences.len()
    );
    let mut expected: Vec<&str> = NOT_LINKED.iter().map(|(name, _)| *name).collect();
    expected.sort();
    unlinked.sort();
    assert_eq!(
        unlinked, expected,
        "the cases that link nothing from the image are not the ones in NOT_LINKED"
    );
}

/// The value after a `--lint-…` flag.
fn lint_level(value: Option<&String>) -> brix_syntax::Diagnose {
    let value = value.expect("a level after the --lint-… flag");
    brix_syntax::Diagnose::parse(value).unwrap_or_else(|| panic!("not a lint level: {value}"))
}
