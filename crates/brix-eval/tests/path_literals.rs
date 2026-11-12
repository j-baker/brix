//! `Evaluator::path_literal_anchor`: how the path literal a path value is
//! was written, which `readFlake` asks of an input's `url` (Nix tells the
//! two kinds apart by their accessor; brix gives both the root one).

use brix_eval::ir::PathAnchor;
use brix_eval::value::Value;
use brix_eval::{Arena, Config, Evaluator, NullHost, Origin, SrcPos};

#[test]
fn path_literals_are_told_apart_by_how_they_are_written() {
    let arena = Arena::new();
    let host = NullHost;
    let ev = Evaluator::new(&arena, Config::default(), &host);
    let src: &[u8] =
        b"rec { rel = ./x; abs = /x; up = ../../../y; via = abs; computed = ./. + \"/x\"; }";
    let Value::Attrs(attrs) = ev
        .eval_source(src, "/dir/sub", Origin::String)
        .expect("the set evaluates")
    else {
        panic!("a set")
    };
    let anchor = |name: &str| {
        let attr = attrs
            .iter()
            .find(|a| ev.symbol_name(a.name) == name.as_bytes())
            .expect("the attribute exists");
        let Value::Path(p) = ev.force(attr.value, SrcPos::NONE).expect("it forces") else {
            panic!("{name} is a path")
        };
        ev.path_literal_anchor(attr.pos, p)
    };
    assert_eq!(anchor("rel"), Some(PathAnchor::Source));
    assert_eq!(anchor("abs"), Some(PathAnchor::Root));
    // Above the root, as Nix's `CanonPath` keeps it there.
    assert_eq!(anchor("up"), Some(PathAnchor::Source));
    // The literal's value, through a variable.
    assert_eq!(anchor("via"), Some(PathAnchor::Root));
    // A path computed from a literal is none.
    assert_eq!(anchor("computed"), None);
}
