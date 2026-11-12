//! Facade handles retain only their reachable runtime graph.

use brix::{Config, FetchConfig, Session};

#[test]
fn last_cursor_clone_releases_a_large_graph_and_repeated_calls_stay_bounded() {
    let session = Session::new(Config {
        fetch: FetchConfig {
            enabled: false,
            ..Default::default()
        },
        ..Default::default()
    })
    .unwrap();
    drop(session.eval_expr_cursor("0", "/").unwrap());
    session.collect_garbage().unwrap();
    let baseline = session.heap_bytes();
    for _ in 0..3 {
        let parent = session.eval_expr_cursor(
            r#"{ child = builtins.concatStringsSep "" (builtins.genList (_: "abcdefgh") 8192); }"#,
            "/",
        ).unwrap();
        let child = parent.get_attr("child").unwrap();
        assert_eq!(child.get_string().unwrap().len(), 65_536);
        let retained = child.clone();
        drop(parent);
        drop(child);
        session.collect_garbage().unwrap();
        assert!(session.heap_bytes() >= baseline + 65_536);
        assert_eq!(retained.get_string().unwrap().len(), 65_536);
        drop(retained);
        session.collect_garbage().unwrap();
        assert!(
            session.heap_bytes() <= baseline + 4096,
            "released graph remains reachable: {} bytes above baseline",
            session.heap_bytes().saturating_sub(baseline)
        );
    }
}
