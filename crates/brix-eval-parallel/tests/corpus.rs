//! The existing expression corpus evaluated as independent jobs over one heap.
#[path = "../../brix-eval/tests/common/cases.rs"]
mod cases;

use brix_eval::managed_api::{GcConfig, GcEvaluator};
use brix_eval::{Arena, Config, Evaluator, NullHost};
use brix_eval_parallel as brix_eval;

#[test]
fn concurrent_expression_corpus_matches_serial_values_and_errors() {
    let serial_arena = brix_eval_serial::Arena::new();
    let serial_host = brix_eval_serial::NullHost;
    let mut serial = brix_eval_serial::managed_api::GcEvaluator::new(
        brix_eval_serial::Evaluator::new(
            &serial_arena,
            brix_eval_serial::Config {
                eval_cores: 4,
                max_call_depth: 128,
                ..Default::default()
            },
            &serial_host,
        ),
        Default::default(),
    )
    .unwrap();
    let arena = Arena::new();
    let host = NullHost;
    let mut parallel = GcEvaluator::new(
        Evaluator::new(
            &arena,
            Config {
                eval_cores: 4,
                max_call_depth: 128,
                ..Default::default()
            },
            &host,
        ),
        GcConfig {
            threshold_bytes: 262144,
            steady_threshold_bytes: Some(262144),
            chunk_bytes: 16384,
            ..Default::default()
        },
    )
    .unwrap();
    let expressions: Vec<_> = cases::DIFFERENTIAL_CASES
        .iter()
        .filter(|e| !e.contains("{src}") && !e.contains("{tree}"))
        .collect();
    for expression in expressions {
        let source = format!("[ ({expression}) ({expression}) ({expression}) ({expression}) ]");
        let serial_list = serial.eval_str(source.as_bytes(), b"/");
        let parallel_list = parallel.eval_str(source.as_bytes(), b"/");
        let (serial_list, parallel_list) = match (serial_list, parallel_list) {
            (Ok(a), Ok(b)) => (a, b),
            (Err(a), Err(b)) => {
                assert_eq!(
                    (format!("{:?}", a.kind), &a.msg),
                    (format!("{:?}", b.kind), &b.msg),
                    "{expression}"
                );
                continue;
            }
            (a, b) => panic!("{expression}: serial={a:?} parallel={b:?}"),
        };
        let expected: Vec<_> = (0..4)
            .map(|index| {
                let value = serial.list_item(&serial_list, index).unwrap().unwrap();
                serial
                    .to_json(&value)
                    .map_err(|e| (format!("{:?}", e.kind), e.msg.clone()))
            })
            .collect();
        let values: Vec<_> = (0..4)
            .map(|index| parallel.list_item(&parallel_list, index).unwrap().unwrap())
            .collect();
        for (result, expected) in parallel
            .evaluate_many(values.len(), |cx, index| cx.to_json(&values[index]))
            .into_iter()
            .zip(expected)
        {
            let actual = result.map_err(|e| (format!("{:?}", e.kind), e.msg.clone()));
            assert_eq!(actual, expected, "{expression}");
        }
        parallel.collect().unwrap();
    }
}
