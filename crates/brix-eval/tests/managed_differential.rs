//! The established expression corpus through the public precise-GC API.
//! Compare stdout, error diagnostics, warnings and traces byte for byte;
//! run every expression with ordinary threshold collection and stress GC.

mod common;

use brix_eval::managed_api::{GcConfig, GcEvaluator};
use brix_eval::{Arena, Config, ErrorKind, Evaluator, SrcPos};
use brix_store::{CanonPath, FsAccessor, HashAlgo, HashFormat};
use bstr::BString;
use common::cases::DIFFERENTIAL_CASES;
use common::{Format, Outcome, TestHost};

// Keep the established corpus sample stable and check every new hot-path case.
const OPTIMIZATION_CASES: &[&str] = &[
    "let target = { type = \"derivation\"; outPath = \"match\"; }; a = { type = \"derivation\"; outPath = \"a\"; }; b = { type = \"derivation\"; outPath = \"match\"; }; delayed = builtins.deepSeq (builtins.genList (i: { value = i; }) 200) { type = \"derivation\"; outPath = \"other\"; }; in builtins.seq (target == a) (builtins.seq (target == b) (builtins.elem target [a delayed b (abort \"unused\")]))",
    "let xs = builtins.genList (i: i) 100; in builtins.deepSeq xs [ (builtins.elem 99 xs) (builtins.elem 101 xs) (builtins.elem 10.0 xs) ]",
    "let set = { a = { b = 42; }; }; key = \"a\"; in [ set.${key}.b (set.${key}.missing or 17) (set ? ${key}.b) (set ? ${key}.missing) ]",
    "let set = { a = throw \"must stay lazy\"; }; key = \"a\"; in builtins.all (x: set ? ${key}) [1 2 3]",
    "builtins.toString [ \"a\" [] [ [] \"b\" ] \"c\" ]",
];

fn managed(host: &TestHost, config: Config, expression: &str, stress: bool) -> Outcome {
    let show_trace = config.show_trace;
    let arena = Arena::new();
    let mut evaluator = GcEvaluator::new(
        Evaluator::new(&arena, config, host),
        GcConfig {
            workers: 0,
            stress,
            steady_threshold_bytes: None,
            trace_collections: false,
            threshold_bytes: 4096,
            chunk_bytes: 1024,
        },
    )
    .unwrap();
    let result = evaluator
        .eval_str(expression.as_bytes(), b"/pwd")
        .and_then(|value| {
            // Retaining an API handle across an explicit collection also checks the
            // boundary between evaluation and serialization for every expression.
            evaluator.collect()?;
            evaluator.to_json_with_options(&value, SrcPos::NONE, true)
        });
    match result {
        Ok(mut bytes) => {
            bytes.push(b'\n');
            Outcome {
                stdout: bytes.into(),
                stderr: host.take_stderr(),
                ok: true,
            }
        }
        Err(error) => {
            let mut stderr = host.take_stderr();
            stderr.extend_from_slice(&brix_eval::filter_ansi_escapes(
                &error.render(show_trace),
                true,
                usize::MAX,
            ));
            stderr.push(b'\n');
            if error.kind == ErrorKind::Usage {
                stderr.extend_from_slice(b"\nTry 'nix-instantiate --help' for more information.\n");
            }
            Outcome {
                stdout: BString::default(),
                stderr,
                ok: false,
            }
        }
    }
}

fn expressions() -> (std::path::PathBuf, Vec<String>) {
    let source = brix_testutil::nix_src();
    let lang = source.join("tests/functional/lang");
    let (hash, _) = brix_store::nar::hash(
        &FsAccessor::new(&lang),
        &CanonPath::root(),
        HashAlgo::Sha256,
    )
    .unwrap();
    let tree = format!(
        "(builtins.fetchTree {{ type = \"path\"; path = \"{}\"; narHash = \"{}\"; }})",
        lang.display(),
        hash.to_string(HashFormat::Sri, true)
    );
    let expressions = DIFFERENTIAL_CASES
        .iter()
        .chain(OPTIMIZATION_CASES.iter())
        .map(|expression| {
            expression
                .replace("{tree}", &tree)
                .replace("{src}", &source.display().to_string())
        })
        .collect();
    (source, expressions)
}

fn compare_legacy(stress: bool) {
    let (source, expressions) = expressions();
    let pwd = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    let source_text = source.to_str().unwrap();
    let host = TestHost::with_allowed(
        &[
            ("/pwd", pwd.path()),
            (source_text, &source),
            ("/nix/store", store.path()),
        ],
        &[source_text],
    );
    let mut failures = Vec::new();
    for (index, expression) in expressions.iter().enumerate() {
        let config = Config {
            eval_cores: 0,
            max_call_depth: 256,
            ..Config::default()
        };
        let expected =
            common::run_brix_expr(&host, config.clone(), expression, "/pwd", Format::Json);
        let actual = managed(&host, config, expression, stress);
        if expected != actual {
            let message = format!("case {index}, stress={stress}: {expression}\nexpected: {expected:?}\nactual: {actual:?}");
            eprintln!("{message}");
            failures.push(message);
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} expressions differed:\n{}",
        failures.len(),
        expressions.len(),
        failures.join("\n\n")
    );
}

#[test]
fn existing_expression_corpus_matches_legacy_with_threshold_collection() {
    compare_legacy(false);
}

#[test]
fn existing_expression_corpus_matches_legacy_with_stress_collection() {
    compare_legacy(true);
}

#[test]
fn representative_managed_expressions_match_nix_oracle() {
    let (source, expressions) = expressions();
    let pwd = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    let source_text = source.to_str().unwrap();
    let host = TestHost::with_allowed(
        &[
            ("/pwd", pwd.path()),
            (source_text, &source),
            ("/nix/store", store.path()),
        ],
        &[source_text],
    );
    // Spread the samples through the whole established corpus, so this covers
    // more than the arithmetic/string cases at its beginning.
    for index in (0..DIFFERENTIAL_CASES.len())
        .step_by(11)
        .chain(DIFFERENTIAL_CASES.len()..expressions.len())
    {
        let expression = &expressions[index];
        let expected = common::run_nix_expr(
            expression,
            pwd.path(),
            Format::Json,
            &["--max-call-depth", "256"],
            false,
        );
        for stress in [false, true] {
            let config = Config {
                eval_cores: 0,
                max_call_depth: 256,
                ..Config::default()
            };
            let actual = managed(&host, config, expression, stress);
            assert_eq!(
                actual, expected,
                "case {index}, stress={stress}: {expression}"
            );
        }
    }
}
