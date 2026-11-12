//! The compiler's scope resolution, checked against what Nix's `bindVars`
//! produces: levels count environments outwards, displacements follow
//! symbol order, and `with` lookups point at the nearest `with`.

use brix_eval::ir::{Node, VarRef};
use brix_eval::{Arena, Config, Evaluator, NullHost, Origin};
use bstr::ByteSlice;

fn load<'a>(ev: &Evaluator<'a>, src: &str) -> &'a brix_eval::LinkedCode<'a> {
    let expr = ev
        .parse_source(src.as_bytes(), b"/", &Origin::String)
        .expect("parses");
    ev.load(&expr, Origin::String, src.as_bytes().to_vec(), None, false)
        .expect("compiles")
}

/// All `Var` nodes of a file as (name, resolution), in node order.
fn vars<'a>(ev: &Evaluator<'a>, code: &'a brix_eval::LinkedCode<'a>) -> Vec<(String, VarRef)> {
    code.nodes
        .iter()
        .filter_map(|n| match *n {
            Node::Var { name, var, .. } => Some((ev.symbol_str(name), var)),
            _ => None,
        })
        .collect()
}

#[test]
fn let_and_lambda_levels() {
    let arena = Arena::new();
    let host = NullHost;
    let ev = Evaluator::new(&arena, Config::default(), &host);
    // `let` binds b and a with displacements in symbol order; the lambda
    // body sees x at level 0 and the let at level 1.
    let code = load(&ev, "let b = 1; a = 2; in x: x + a + b");
    let v = vars(&ev, code);
    let find = |n: &str| {
        v.iter()
            .find(|(name, _)| name == n)
            .map(|(_, r)| *r)
            .unwrap()
    };
    assert_eq!(find("x"), VarRef::Env { level: 0, displ: 0 });
    let a = find("a");
    let b = find("b");
    assert!(matches!(a, VarRef::Env { level: 1, .. }));
    assert!(matches!(b, VarRef::Env { level: 1, .. }));
    // b was interned before a (source order), so it has the lower
    // displacement, as Nix's std::map<Symbol, …> would give it.
    assert_eq!(b, VarRef::Env { level: 1, displ: 0 });
    assert_eq!(a, VarRef::Env { level: 1, displ: 1 });
}

#[test]
fn with_lookups_point_at_the_nearest_with() {
    let arena = Arena::new();
    let host = NullHost;
    let ev = Evaluator::new(&arena, Config::default(), &host);
    let code = load(&ev, "with { a = 1; }; with { b = 2; }; a + b");
    let v = vars(&ev, code);
    let find = |n: &str| {
        v.iter()
            .find(|(name, _)| name == n)
            .map(|(_, r)| *r)
            .unwrap()
    };
    // Both are unresolved statically and go through the inner `with`
    // (level 0); `a` then continues to the outer one via `parent_with`.
    assert!(matches!(find("a"), VarRef::With { level: 0, .. }));
    assert!(matches!(find("b"), VarRef::With { level: 0, .. }));
    let inner = match find("a") {
        VarRef::With { with, .. } => with,
        _ => unreachable!(),
    };
    match *code.node(inner) {
        Node::With {
            prev_with,
            parent_with,
            ..
        } => {
            // The enclosing `with`'s environment is one frame up.
            assert_eq!(prev_with, 1);
            assert!(parent_with.is_some());
        }
        _ => panic!("not a with node"),
    }
}

#[test]
fn static_binding_shadows_with() {
    let arena = Arena::new();
    let host = NullHost;
    let ev = Evaluator::new(&arena, Config::default(), &host);
    let code = load(&ev, "let a = 1; in with { a = 2; }; a");
    let v = vars(&ev, code);
    // The `let` wins: resolved statically, level 1 (through the with frame).
    assert_eq!(v.last().unwrap().1, VarRef::Env { level: 1, displ: 0 });
}

#[test]
fn formals_are_in_symbol_order() {
    let arena = Arena::new();
    let host = NullHost;
    let ev = Evaluator::new(&arena, Config::default(), &host);
    let code = load(&ev, "args@{ z, a ? z, ... }: a");
    let lambda = code.lambdas.first().expect("one lambda");
    let names: Vec<String> = lambda
        .formals
        .as_ref()
        .unwrap()
        .formals
        .iter()
        .map(|f| ev.symbol_str(f.name))
        .collect();
    // `z` was interned before `a`, so it comes first; `args` has slot 0.
    assert_eq!(names, vec!["z", "a"]);
    let v = vars(&ev, code);
    let find = |n: &str| {
        v.iter()
            .find(|(name, _)| name == n)
            .map(|(_, r)| *r)
            .unwrap()
    };
    assert_eq!(find("z"), VarRef::Env { level: 0, displ: 1 });
    assert_eq!(find("a"), VarRef::Env { level: 0, displ: 2 });
}

#[test]
fn undefined_variables_are_compile_errors() {
    let arena = Arena::new();
    let host = NullHost;
    let ev = Evaluator::new(&arena, Config::default(), &host);
    let err = ev.eval_str(b"let a = 1; in b", "/").unwrap_err();
    assert_eq!(
        err.render(true),
        "error: undefined variable 'b'\n       at «string»:1:15:\n            1| let a = 1; in b\n             |               ^"
    );
    // Even when the body is never evaluated.
    let err = ev.eval_str(b"let f = x: nope; in 1", "/").unwrap_err();
    assert!(err
        .render(true)
        .starts_with_str("error: undefined variable 'nope'"));
}

#[test]
fn lambdas_are_named_after_their_attribute() {
    let arena = Arena::new();
    let host = NullHost;
    let ev = Evaluator::new(&arena, Config::default(), &host);
    let code = load(&ev, "{ f = x: y: x; g = { a }: a; }");
    let names: Vec<Option<String>> = code
        .lambdas
        .iter()
        .map(|l| l.name.map(|n| ev.symbol_str(n)))
        .collect();
    // Both lambdas of the curried `f` are named `f`, as `setName` does.
    assert_eq!(
        names,
        vec![Some("f".into()), Some("f".into()), Some("g".into())]
    );
}
