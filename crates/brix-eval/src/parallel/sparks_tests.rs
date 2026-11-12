//! Tests for background sparks (see `parallel::sparks`).

use super::*;
use crate::{Arena, Config, Host};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Records what the user would see, in order.
#[derive(Default)]
struct RecordingHost(Arc<Mutex<Vec<Vec<u8>>>>);

impl Host for RecordingHost {
    fn root_accessor(&self) -> Arc<dyn brix_store::SourceAccessor> {
        Arc::new(brix_store::MemoryAccessor::new())
    }
    fn log(&self, message: &[u8]) {
        self.0.lock().unwrap().push(message.to_vec());
    }
}

/// `builtins.pause x` holds the ordered consumer back until `released` says
/// the background has done its part (always, for a serial evaluator).
fn runtime<'a>(
    arena: &'a Arena,
    host: &'a RecordingHost,
    cores: u32,
    released: impl Fn(&Evaluator<'_>) -> bool + Send + Sync + 'static,
) -> GcEvaluator<'a> {
    let evaluator = Evaluator::with_builtins(
        arena,
        Config {
            eval_cores: cores,
            max_call_depth: 256,
            ..Default::default()
        },
        host,
        |ev| {
            ev.add_primop("__pause", 1, &|_, _, _| unreachable!());
        },
    );
    let mut evaluator = GcEvaluator::new(
        evaluator,
        GcConfig {
            chunk_bytes: 4096,
            threshold_bytes: 128 * 1024,
            steady_threshold_bytes: Some(128 * 1024),
            ..Default::default()
        },
    )
    .unwrap();
    evaluator
        .register_callback("pause", move |cx, _, args| {
            if cores > 1 {
                let language = cx.language;
                cx.blocking(|| {
                    let until = Instant::now() + Duration::from_secs(10);
                    while !released(language.legacy()) {
                        assert!(
                            Instant::now() < until,
                            "idle workers must evaluate in the background while the consumer is blocked: {}",
                            language.legacy().stats.render()
                        );
                        std::thread::sleep(Duration::from_millis(1));
                    }
                });
            }
            cx.collect()?;
            cx.force(&args[0])?;
            Ok(args[0].clone())
        })
        .unwrap();
    evaluator
}

/// Evaluate `source` to a string as the one job of a batch, so the other
/// workers are idle and take sparks.
fn job(ev: &mut GcEvaluator<'_>, source: &[u8]) -> Result<Vec<u8>> {
    // A list element stays unevaluated until a worker of the batch takes it.
    let list = ev.eval_str(&[b"[ (".as_slice(), source, b") ]"].concat(), b"/")?;
    let value = ev.list_item(&list, 0)?.expect("one element");
    let mut next = Some(value);
    let mut result = None;
    ev.evaluate_stream(
        |_| Ok(next.take()),
        |cx, value| {
            let string = cx.coerce(&value, crate::Coercion::MORE)?;
            cx.string_bytes(&string)
        },
        |value| {
            result = Some(value);
            true
        },
    )?;
    result.unwrap()
}

/// A derivation whose `args` are `elements`, a Nix list expression.
fn derivation(elements: &str) -> Vec<u8> {
    format!(
        r#"(builtins.derivationStrict {{
            name = "top"; system = "test"; builder = "/bin/sh";
            args = {elements};
        }}).out"#
    )
    .into_bytes()
}

const DEPENDENCY: &str = r#"(name: {
    outPath = (builtins.derivationStrict {
        inherit name; system = "test"; builder = "/bin/sh";
    }).out;
})"#;

#[test]
fn idle_workers_instantiate_dependencies_before_the_consumer_reaches_them() {
    let source = derivation(&format!(
        r#"let dep = {DEPENDENCY}; in [ (builtins.pause "first") (dep "a") (dep "b") (dep "c") ]"#
    ));
    let run = |cores| {
        let arena = Arena::new();
        let host = RecordingHost::default();
        // The consumer registers `top` last: three derivations in the store
        // while it is paused at the first element were put there by sparks.
        let mut ev = runtime(&arena, &host, cores, |ev| ev.virtual_store().len() >= 3);
        let result = job(&mut ev, &source).unwrap();
        let stats = &ev.legacy().stats;
        if cores > 1 {
            assert!(
                stats.sparks_started.get() >= 4,
                "a walk and three dependencies"
            );
            assert!(stats.sparks_completed.get() >= 4);
            assert!(ev.gc_stats().collections > 0);
        }
        let derivations = ev.legacy().virtual_store().len();
        (result, derivations)
    };
    assert_eq!(run(4), run(1));
}

#[test]
fn expensive_lists_are_offered_one_element_at_a_time() {
    // Every element is its own computation, as in `symlinkJoin.paths` over a
    // workspace: the walk evaluates one, finds it expensive, and offers the
    // rest. The consumer is held at the first until some other worker has
    // evaluated a dependency.
    let element = |name: &str| {
        format!(
            r#"(let n = builtins.foldl' (a: b: a + b) 0 (builtins.genList (x: x) 20000);
                in "${{(dep "{name}-${{toString n}}").outPath}}")"#
        )
    };
    let source = derivation(&format!(
        r#"let dep = {DEPENDENCY}; in [ (builtins.pause "first") {} {} {} {} ]"#,
        element("a"),
        element("b"),
        element("c"),
        element("d"),
    ));
    let run = |cores| {
        let arena = Arena::new();
        let host = RecordingHost::default();
        let mut ev = runtime(&arena, &host, cores, |ev| ev.virtual_store().len() >= 4);
        let result = job(&mut ev, &source).unwrap();
        let derivations = ev.legacy().virtual_store().len();
        (result, derivations)
    };
    assert_eq!(run(4), run(1));
}

#[test]
fn sparks_leave_ordered_effects_and_errors_to_the_consumer() {
    // Each case pauses the consumer at the first element until the sparks
    // have looked at the rest, then compares everything the user sees with
    // serial evaluation.
    let cases = [
        // Output stays in evaluation order.
        r#"[ (builtins.pause (builtins.trace "first" "a")) (builtins.trace "second" "b") (builtins.warn "third" "c") ]"#,
        // An earlier error wins, and a later effect never happens.
        r#"[ (builtins.pause (throw "first")) (builtins.trace "must not run" "later") ]"#,
        // A later error is reported with the consumer's trace, once.
        r#"[ (builtins.pause "a") (builtins.addErrorContext (builtins.trace "context" "detail") (throw "failure")) (builtins.trace "unreachable" "b") ]"#,
        // A failure a spark met is not memoized: `tryEval` sees it in order.
        r#"let bad = throw "caught"; in [ (builtins.pause "a") (if (builtins.tryEval bad).success then "bad" else "good") ]"#,
        // Thunks shared between the consumer and the sparks.
        r#"let a = builtins.pause "x"; in [ a (a + "y") ]"#,
        r#"let later = toString (20 + 22); in [ (builtins.pause later) later ]"#,
        // A cycle through the paused element is the program's own.
        r#"let a = builtins.pause b; b = a; in [ a b ]"#,
        // `__toString` is an arbitrary function: the walk leaves it alone.
        r#"[ (builtins.pause "a") { __toString = self: builtins.trace "coercion" "b"; } ]"#,
        // Dependencies next to effects.
        r#"let dep = DEPENDENCY; in [ (builtins.pause "a") (dep "x") (builtins.trace "between" "b") (dep "y") ]"#,
    ];
    for case in cases {
        let source = derivation(&case.replace("DEPENDENCY", DEPENDENCY));
        let run = |cores| {
            let arena = Arena::new();
            let host = RecordingHost::default();
            let mut ev = runtime(&arena, &host, cores, |ev| {
                // The walk of `top` has been taken and has run.
                let stats = &ev.stats;
                stats.sparks_completed.get() + stats.sparks_deferred.get() >= 1
            });
            let result = job(&mut ev, &source).map_err(|error| error.render(true));
            let logs = host.0.lock().unwrap().clone();
            (result, logs)
        };
        // Sparks are timing-dependent: give each case several chances to
        // interleave differently.
        let serial = run(1);
        for _ in 0..8 {
            assert_eq!(run(4), serial, "{case}");
        }
    }
}

#[test]
fn a_diverging_spark_does_not_hold_up_an_error_or_outlive_the_batch() {
    let arena = Arena::new();
    let host = RecordingHost::default();
    let mut ev = runtime(&arena, &host, 4, |_| true);
    // Work without end in a later element must not hold up the first error,
    // nor the end of the batch.
    let result = job(
        &mut ev,
        &derivation(r#"[ (throw "first") (let loop = n: loop (n + 1); in loop 0) ]"#),
    );
    assert_eq!(result.unwrap_err().msg, b"first");
    ev.collect().unwrap();
    // The pool is reusable afterwards.
    assert_eq!(
        job(&mut ev, br#""${toString 1} ${toString 2}""#).unwrap(),
        b"1 2"
    );
}
