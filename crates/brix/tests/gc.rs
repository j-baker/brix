//! The public Session/ Cursor path owns the precise collector, including
//! automatic collection and reclamation when the last external handle drops.

use brix::{Config, Format, Session, Teardown};

fn config() -> Config {
    let mut config = Config {
        daemon: Some("unix:///nonexistent/brix-gc-test".into()),
        teardown: Teardown::Free,
        ..Config::default()
    };
    config.fetch.enabled = false;
    config.compiled.enabled = false;
    config
}

#[test]
fn session_collects_automatically_and_preserves_retained_closures() {
    let mut config = config();
    // Exercise ordinary threshold collection without requiring the production
    // 4 GiB of allocation in a regression test.
    config.gc.threshold_bytes = 16 * 1024 * 1024;
    let session = Session::new(config).unwrap();
    let function = session
        .eval_expr_cursor(
            "let captured = [ 40 2 ]; in ignored: builtins.foldl' builtins.add 0 captured",
            "/",
        )
        .unwrap();
    let collections = session.gc_stats().collections;
    // Stress mode stays off: ordinary allocation must trigger collection.
    for _ in 0..32 {
        let result = session
            .eval_expr_cursor(
                "builtins.foldl' (sum: value: sum + value) 0 (builtins.genList (x: x) 20000)",
                "/",
            )
            .unwrap();
        assert_eq!(result.get_int().unwrap(), 199_990_000);
        if session.gc_stats().collections > collections {
            break;
        }
    }
    assert!(session.gc_stats().collections > collections);
    let argument = session.eval_expr_cursor("null", "/").unwrap();
    assert_eq!(argument.apply(&function).unwrap().get_int().unwrap(), 42);
}

#[test]
fn last_cursor_drop_reclaims_graph_but_child_and_clone_remain_live() {
    let session = Session::new(config()).unwrap();
    let parent = session
        .eval_expr_cursor("builtins.genList (index: index) 12000", "/")
        .unwrap();
    let clone = parent.clone();
    let child = parent
        .find_along_attr_path(&["17".into()])
        .unwrap()
        .unwrap();
    session.collect_garbage().unwrap();
    let retained_bytes = session.heap_bytes();
    drop(parent);
    session.collect_garbage().unwrap();
    assert_eq!(
        session.heap_bytes(),
        retained_bytes,
        "a clone retains the whole list"
    );
    drop(clone);
    session.collect_garbage().unwrap();
    let child_bytes = session.heap_bytes();
    assert!(
        child_bytes < retained_bytes / 2,
        "unreachable siblings must be reclaimed: {retained_bytes} -> {child_bytes}"
    );
    assert_eq!(child.get_int().unwrap(), 17);
    drop(child);
    session.collect_garbage().unwrap();
    assert!(session.heap_bytes() <= child_bytes);
}

#[test]
fn recursive_calls_and_lazy_cursor_traversal_survive_stress_collection() {
    let mut config = config();
    config.gc.stress = true;
    config.gc.threshold_bytes = 1024;
    config.gc.chunk_bytes = 256;
    let session = Session::new(config).unwrap();
    let root = session.eval_expr_cursor(
        "let go = count: acc: if count == 0 then acc else go (count - 1) (acc + count); in { sum = go 120 0; nested = [ { values = builtins.genList (x: toString (x + 1)) 8; } ]; unused = throw \"must stay lazy\"; }",
        "/",
    ).unwrap();
    let nested = root
        .find_along_attr_path(&["nested".into(), "0".into(), "values".into()])
        .unwrap()
        .unwrap();
    let sum = root.get_attr("sum").unwrap();
    drop(root);
    assert_eq!(sum.get_int().unwrap(), 7_260);
    let elements = nested.get_list().unwrap();
    drop(nested);
    session.collect_garbage().unwrap();
    for (index, element) in elements.iter().enumerate() {
        assert_eq!(element.get_string().unwrap(), (index + 1).to_string());
        assert_eq!(
            element.render_bytes(Format::Json).unwrap(),
            format!("\"{}\"", index + 1).as_bytes()
        );
    }
    assert!(session.gc_stats().collections > 120);
}
