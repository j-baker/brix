use brix_eval::managed_api::{GcConfig, GcEvaluator};
use brix_eval::{Arena, Config, Evaluator, NullHost};
use brix_eval_parallel as brix_eval;

#[test]
fn batch_shares_values_and_survives_collection() {
    let arena = Arena::new();
    let host = NullHost;
    let evaluator = Evaluator::new(
        &arena,
        Config {
            eval_cores: 4,
            ..Config::default()
        },
        &host,
    );
    let mut evaluator = GcEvaluator::new(
        evaluator,
        GcConfig {
            threshold_bytes: 1024,
            steady_threshold_bytes: Some(1024),
            workers: 2,
            ..GcConfig::default()
        },
    )
    .unwrap();
    let list = evaluator.eval_str(b"let shared = builtins.foldl' (a: b: a + b) 0 (builtins.genList (x: x) 1000); in [ (shared + 1) (shared + 2) (shared + 3) (shared + 4) ]", b"/").unwrap();
    let values: Vec<_> = (0..4)
        .map(|i| evaluator.list_item(&list, i).unwrap().unwrap())
        .collect();
    let results = evaluator.force_many(&values);
    for result in results {
        result.unwrap();
    }
    evaluator.collect().unwrap();
    assert_eq!(
        evaluator.to_json(&list).unwrap(),
        b"[499501,499502,499503,499504]"
    );
    assert!(evaluator.gc_stats().collections > 0);
    assert!(evaluator.gc_stats().parallel_collections > 0);
}

use brix_eval::managed_api::{GcValue, GcValueView};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Barrier,
};

fn unavailable<'a>(
    _: &Evaluator<'a>,
    _: brix_eval::SrcPos,
    _: &[brix_eval::ValueRef<'a>],
) -> brix_eval::Result<brix_eval::Value<'a>> {
    panic!("legacy callback must not run")
}

fn runtime<'a>(arena: &'a Arena, host: &'a NullHost) -> GcEvaluator<'a> {
    let legacy = Evaluator::with_builtins(
        arena,
        Config {
            eval_cores: 2,
            ..Config::default()
        },
        host,
        |ev| {
            ev.add_primop("__gate", 1, &unavailable);
            ev.add_primop("__tick", 1, &unavailable);
        },
    );
    let mut ev = GcEvaluator::new(legacy, GcConfig::default()).unwrap();
    let barrier = Arc::new(Barrier::new(2));
    ev.register_callback("gate", move |_, _, args| {
        barrier.wait();
        Ok(args[0].clone())
    })
    .unwrap();
    ev
}

fn elements(ev: &mut GcEvaluator<'_>, expression: &[u8]) -> Vec<GcValue> {
    let list = ev.eval_str(expression, b"/").unwrap();
    (0..2)
        .map(|i| ev.list_item(&list, i).unwrap().unwrap())
        .collect()
}

#[test]
fn simultaneous_forces_publish_once_and_waiters_survive_gc() {
    let arena = Arena::new();
    let host = NullHost;
    let mut ev = runtime(&arena, &host);
    let calls = Arc::new(AtomicUsize::new(0));
    let count = calls.clone();
    ev.register_callback("tick", move |cx, pos, args| {
        count.fetch_add(1, Ordering::SeqCst);
        let value = cx.force_int(&args[0], pos, "tick")?;
        cx.collect()?;
        cx.int(value)
    })
    .unwrap();
    let values = elements(&mut ev, b"let shared = builtins.tick 42; in [ (builtins.seq (builtins.gate 0) (shared + 1)) (builtins.seq (builtins.gate 1) (shared + 2)) ]");
    for result in ev.force_many(&values) {
        result.unwrap();
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(matches!(ev.view(&values[0]).unwrap(), GcValueView::Int(43)));
    assert!(matches!(ev.view(&values[1]).unwrap(), GcValueView::Int(44)));
    // Another batch reuses worker slots and pool threads after a collection.
    let barrier = Barrier::new(2);
    let first = ev.evaluate_many(2, |cx, _| {
        barrier.wait();
        cx.collect()?;
        Ok(std::thread::current().id())
    });
    let first: std::collections::HashSet<_> = first.into_iter().map(Result::unwrap).collect();
    let second = ev.evaluate_many(2, |_, _| {
        barrier.wait();
        Ok(std::thread::current().id())
    });
    assert_eq!(
        first,
        second
            .into_iter()
            .map(Result::unwrap)
            .collect::<std::collections::HashSet<_>>()
    );
}

#[test]
fn cross_worker_recursion_is_an_error_and_pool_remains_usable() {
    let arena = Arena::new();
    let host = NullHost;
    let mut ev = runtime(&arena, &host);
    let values = elements(&mut ev, b"let a = builtins.seq (builtins.gate 0) b; b = builtins.seq (builtins.gate 1) a; in [ a b ]");
    for error in ev.force_many(&values) {
        assert_eq!(
            error.unwrap_err().kind,
            brix_eval::ErrorKind::InfiniteRecursion
        );
    }
    ev.collect().unwrap();
    assert_eq!(
        ev.evaluate_many(4, |cx, index| {
            let value = cx.int(index as i64)?;
            cx.to_json(&value)
        })
        .into_iter()
        .map(Result::unwrap)
        .collect::<Vec<_>>(),
        [b"0", b"1", b"2", b"3"]
    );
}

#[test]
fn memoized_errors_have_independent_propagation_traces() {
    let arena = Arena::new();
    let host = NullHost;
    let mut ev = runtime(&arena, &host);
    let values = elements(&mut ev, b"let shared = throw \"shared failure\"; in [ (builtins.addErrorContext \"left context\" shared) (builtins.addErrorContext \"right context\" shared) ]");
    let errors: Vec<_> = ev
        .force_many(&values)
        .into_iter()
        .map(|r| String::from_utf8_lossy(&r.unwrap_err().render(true)).into_owned())
        .collect();
    assert!(
        errors[0].contains("… left context") && !errors[0].contains("… right context"),
        "{}",
        errors[0]
    );
    assert!(
        errors[1].contains("… right context") && !errors[1].contains("… left context"),
        "{}",
        errors[1]
    );
}

#[test]
fn panicking_owner_restores_suspension_and_wakes_waiters() {
    let arena = Arena::new();
    let host = NullHost;
    let mut ev = runtime(&arena, &host);
    let calls = Arc::new(AtomicUsize::new(0));
    let count = calls.clone();
    ev.register_callback("tick", move |cx, _, args| {
        if count.fetch_add(1, Ordering::SeqCst) == 0 {
            panic!("host panic");
        }
        cx.force(&args[0])?;
        Ok(args[0].clone())
    })
    .unwrap();
    let values = elements(
        &mut ev,
        b"let shared = builtins.tick 42; in [ shared shared ]",
    );
    let barrier = Barrier::new(2);
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| ev.evaluate_many(
            2,
            |cx, index| {
                barrier.wait();
                cx.force(&values[index])
            }
        )))
        .is_err()
    );
    // The gate arguments are already memoized; retry cannot wait on the old
    // barrier or an abandoned owner, and the shared computation ran twice.
    for result in ev.force_many(&values) {
        result.unwrap();
    }
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[test]
fn imports_share_one_memoized_graph_across_workers_and_batches() {
    use brix_store::{CanonPath, MemoryAccessor};
    let arena = Arena::new();
    let host = NullHost;
    let mut ev = runtime(&arena, &host);
    let calls = Arc::new(AtomicUsize::new(0));
    let count = calls.clone();
    ev.register_callback("tick", move |cx, _, args| {
        count.fetch_add(1, Ordering::SeqCst);
        cx.force(&args[0])?;
        cx.collect()?;
        Ok(args[0].clone())
    })
    .unwrap();
    let mut files = MemoryAccessor::new();
    files
        .add_file(
            &CanonPath::new("/shared.nix").unwrap(),
            b"builtins.tick { answer = 42; }",
            false,
        )
        .unwrap();
    files
        .add_file(
            &CanonPath::new("/recursive.nix").unwrap(),
            b"import ./recursive.nix",
            false,
        )
        .unwrap();
    ev.services()
        .mount(CanonPath::new("/fixtures").unwrap(), Arc::new(files));
    ev.services()
        .allow_path(CanonPath::new("/fixtures").unwrap());
    for _ in 0..3 {
        let values = elements(&mut ev, b"[ (builtins.seq (builtins.gate 0) (import /fixtures/shared.nix).answer) (builtins.seq (builtins.gate 1) (import /fixtures/shared.nix).answer) ]");
        for result in ev.force_many(&values) {
            result.unwrap();
        }
        for value in values {
            assert_eq!(ev.to_json(&value).unwrap(), b"42");
        }
        ev.collect().unwrap();
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let values = elements(
        &mut ev,
        b"[ (import /fixtures/recursive.nix) (import /fixtures/recursive.nix) ]",
    );
    for result in ev.force_many(&values) {
        assert_eq!(
            result.unwrap_err().kind,
            brix_eval::ErrorKind::InfiniteRecursion
        );
    }
}

#[test]
fn cancellation_is_not_caught_or_memoized_and_can_be_cleared() {
    let arena = Arena::new();
    let host = NullHost;
    let mut ev = runtime(&arena, &host);
    let cancellation = ev.cancellation();
    let calls = AtomicUsize::new(0);
    ev.register_callback("tick", move |cx, _, args| {
        if calls.fetch_add(1, Ordering::SeqCst) == 0 {
            cancellation.cancel();
        }
        cx.force(&args[0])?;
        Ok(args[0].clone())
    })
    .unwrap();
    let values = elements(&mut ev, b"let shared = builtins.tick 42; in [ (builtins.tryEval shared).success (builtins.tryEval shared).success ]");
    for result in ev.force_many(&values) {
        assert_eq!(result.unwrap_err().kind, brix_eval::ErrorKind::Interrupted);
    }
    ev.clear_cancellation();
    for result in ev.force_many(&values) {
        result.unwrap();
    }
    for value in values {
        assert_eq!(ev.to_json(&value).unwrap(), b"true");
    }
}

#[test]
fn blocked_host_work_allows_collection_and_reloads_rooted_values() {
    use std::sync::{mpsc, Mutex};
    use std::time::Duration;

    let arena = Arena::new();
    let host = NullHost;
    let mut ev = runtime(&arena, &host);
    let (entered_tx, entered_rx) = mpsc::channel();
    let (collected_tx, collected_rx) = mpsc::channel();
    let entered_rx = Mutex::new(entered_rx);
    let collected_rx = Mutex::new(collected_rx);
    let results = ev.evaluate_many(2, |cx, index| {
        if index == 0 {
            let value = cx.string(b"survives collection during blocking host work")?;
            cx.blocking(|| {
                entered_tx.send(()).unwrap();
                collected_rx
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(10))
                    .expect("collection must finish while the first worker is blocked");
            });
            assert_eq!(
                cx.string_bytes(&value)?,
                b"survives collection during blocking host work"
            );
        } else {
            cx.blocking(|| {
                entered_rx
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(10))
                    .unwrap();
            });
            cx.collect()?;
            collected_tx.send(()).unwrap();
        }
        Ok(())
    });
    for result in results {
        result.unwrap();
    }
}

#[test]
fn stream_reports_during_discovery_and_refills_while_an_earlier_job_blocks() {
    use std::sync::{mpsc, Mutex};
    use std::time::Duration;
    let arena = Arena::new();
    let host = NullHost;
    let mut ev = runtime(&arena, &host);
    let (resume_discovery, discovered) = mpsc::channel();
    let (resume_first, first) = mpsc::channel();
    let first = Mutex::new(first);
    let mut next = 0;
    let mut completed = Vec::new();
    ev.evaluate_stream(
        |cx| {
            if next == 2 {
                // Reporting must progress independently of blocked discovery.
                cx.blocking(|| discovered.recv_timeout(Duration::from_secs(10)).unwrap());
            }
            if next == 13 {
                return Ok(None);
            }
            cx.collect()?;
            let index = next;
            next += 1;
            Ok(Some((index, cx.string(format!("job-{index}").as_bytes())?)))
        },
        |cx, (index, value)| {
            if index == 0 {
                cx.blocking(|| {
                    first
                        .lock()
                        .unwrap()
                        .recv_timeout(Duration::from_secs(10))
                        .unwrap()
                });
            }
            cx.collect()?;
            assert_eq!(cx.string_bytes(&value)?, format!("job-{index}").as_bytes());
            Ok(index)
        },
        |result| {
            let index = result.unwrap();
            completed.push(index);
            if index == 1 {
                resume_discovery.send(()).unwrap();
            }
            // Beyond the old eight-job batch: this cannot complete if the
            // first slow job holds up refill or ordered reporting.
            if index == 12 {
                resume_first.send(()).unwrap();
            }
            true
        },
    )
    .unwrap();
    assert_eq!(completed, (1..13).chain([0]).collect::<Vec<_>>());
}

#[test]
fn stream_stops_admission_and_survives_panicking_callbacks() {
    let arena = Arena::new();
    let host = NullHost;
    let mut ev = runtime(&arena, &host);
    let mut next = 0;
    let mut reports = 0;
    ev.evaluate_stream(
        |_| {
            next += 1;
            Ok(Some(next))
        },
        |cx, index| {
            cx.collect()?;
            Ok(index)
        },
        |result| {
            result.unwrap();
            reports += 1;
            false
        },
    )
    .unwrap();
    assert_eq!(reports, 1);
    assert!(next <= 12, "discovery must stay bounded: {next}");
    for panic_in in ["discovery", "job", "report"] {
        let mut next = 0;
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            ev.evaluate_stream(
                |_| {
                    next += 1;
                    if panic_in == "discovery" && next == 3 {
                        panic!("discovery");
                    }
                    Ok((next <= 20).then_some(next))
                },
                |cx, index| {
                    if panic_in == "job" {
                        panic!("job");
                    }
                    cx.collect()?;
                    Ok(index)
                },
                |result| {
                    result.unwrap();
                    if panic_in == "report" {
                        panic!("report");
                    }
                    true
                },
            )
        }))
        .is_err());
        // No abandoned queue, active mutator, or pool attachment remains.
        ev.collect().unwrap();
        for result in ev.evaluate_many(2, |cx, _| cx.collect()) {
            result.unwrap();
        }
    }
}
