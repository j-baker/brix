//! Quick end-to-end checks of the evaluator on small expressions.

use brix_eval::{Arena, Config, Evaluator, NullHost};
use bstr::BString;

fn eval_print(src: &str) -> Result<BString, BString> {
    let arena = Arena::new();
    let host = NullHost;
    let ev = Evaluator::new(&arena, Config::default(), &host);
    match ev.eval_str(src.as_bytes(), "/") {
        Ok(v) => {
            let cell = ev.alloc_cell(v);
            ev.force_deep(cell).map_err(|e| e.render(true))?;
            ev.print_ambiguous(cell).map_err(|e| e.render(true))
        }
        Err(e) => Err(e.render(true)),
    }
}

/// Like `eval_print`, but without forcing: what `nix-instantiate --eval`
/// shows for nested values that nothing has evaluated yet.
fn eval_print_lazy(src: &str) -> Result<BString, BString> {
    let arena = Arena::new();
    let host = NullHost;
    let ev = Evaluator::new(&arena, Config::default(), &host);
    match ev.eval_str(src.as_bytes(), "/") {
        Ok(v) => {
            let cell = ev.alloc_cell(v);
            ev.print_ambiguous(cell).map_err(|e| e.render(true))
        }
        Err(e) => Err(e.render(true)),
    }
}

/// A lambda in a lazy position is built directly rather than thunked, but
/// until something forces it the non-forcing printer shows it as Nix shows
/// the thunk it would have been.
#[test]
fn unforced_lambdas_print_as_code() {
    assert_eq!(
        eval_print_lazy("{ f = x: x; g = 1 + 1; }").unwrap(),
        "{ f = <CODE>; g = <CODE>; }"
    );
    assert_eq!(
        eval_print_lazy("let s = { f = x: x; }; in builtins.seq (s.f 1) s").unwrap(),
        "{ f = <LAMBDA>; }"
    );
    assert_eq!(
        eval_print_lazy("let f = x: x; in { inherit f; a = f; }").unwrap(),
        "{ a = <CODE>; f = <CODE>; }"
    );
}

#[test]
fn arithmetic_and_strings() {
    assert_eq!(eval_print("1 + 2 * 3").unwrap(), "7");
    assert_eq!(eval_print("\"a\" + \"b\"").unwrap(), "\"ab\"");
    assert_eq!(eval_print("[ 1 2 ] ++ [ 3 ]").unwrap(), "[ 1 2 3 ]");
    assert_eq!(
        eval_print("{ a = 1; } // { b = 2; }").unwrap(),
        "{ a = 1; b = 2; }"
    );
    assert_eq!(eval_print("let x = 1; y = x + 1; in y").unwrap(), "2");
    assert_eq!(eval_print("rec { a = 1; b = a + 1; }.b").unwrap(), "2");
    assert_eq!(eval_print("with { a = 1; }; a").unwrap(), "1");
    assert_eq!(eval_print("(x: y: x + y) 1 2").unwrap(), "3");
    assert_eq!(eval_print("({ a, b ? 2 }: a + b) { a = 1; }").unwrap(), "3");
    assert_eq!(
        eval_print("builtins.map (x: x * 2) [ 1 2 3 ]").unwrap(),
        "[ 2 4 6 ]"
    );
    assert_eq!(eval_print("builtins.length [ 1 2 3 ]").unwrap(), "3");
    assert_eq!(
        eval_print("if 1 < 2 then \"yes\" else \"no\"").unwrap(),
        "\"yes\""
    );
    assert_eq!(
        eval_print("builtins.attrNames { b = 1; a = 2; }").unwrap(),
        "[ \"a\" \"b\" ]"
    );
    assert_eq!(eval_print("toString 1.5").unwrap(), "\"1.500000\"");
    assert_eq!(eval_print("1.0e30").unwrap(), "1e+30");
    assert_eq!(
        eval_print("builtins.toJSON { a = [ 1 2.5 null ]; }").unwrap(),
        "\"{\\\"a\\\":[1,2.5,null]}\""
    );
    assert_eq!(
        eval_print("builtins.sort builtins.lessThan [ 3 1 2 ]").unwrap(),
        "[ 1 2 3 ]"
    );
    assert_eq!(
        eval_print("(builtins.tryEval (throw \"x\")).success").unwrap(),
        "false"
    );
    assert_eq!(
        eval_print("builtins.foldl' (a: b: a + b) 0 [ 1 2 3 ]").unwrap(),
        "6"
    );
    assert_eq!(
        eval_print("let f = n: if n == 0 then 0 else 1 + f (n - 1); in f 2000").unwrap(),
        "2000"
    );
}

#[test]
fn errors_render_like_nix() {
    assert_eq!(
        eval_print("let f = x: x; in f 1 2").unwrap_err(),
        "error: attempt to call something which is not a function but an integer: 1\n       at «string»:1:18:\n            1| let f = x: x; in f 1 2\n             |                  ^"
    );
    assert_eq!(
        eval_print("builtins.elemAt [ 1 ] 5").unwrap_err(),
        "error:\n       … while calling the 'elemAt' builtin\n         at «string»:1:1:\n            1| builtins.elemAt [ 1 ] 5\n             | ^\n\n       error: 'builtins.elemAt' called with index 5 on a list of size 1"
    );
}

#[test]
fn derivations() {
    let out = eval_print(
        "let d = derivation { name = \"x\"; builder = \"/bin/sh\"; system = \"x\"; }; in { a = d; b = \"${d}\"; }",
    )
    .unwrap();
    assert_eq!(
        out,
        "{ a = { all = [ «repeated» ]; builder = \"/bin/sh\"; drvAttrs = { builder = \"/bin/sh\"; name = \"x\"; system = \"x\"; }; drvPath = \"/nix/store/safnr1ywsblyl5qfbca0178m104hzxaw-x.drv\"; name = \"x\"; out = «repeated»; outPath = \"/nix/store/wrjy0zpx12zvynnga8wrzngbbg6yq3vr-x\"; outputName = \"out\"; system = \"x\"; type = \"derivation\"; }; b = \"/nix/store/wrjy0zpx12zvynnga8wrzngbbg6yq3vr-x\"; }"
    );
}
