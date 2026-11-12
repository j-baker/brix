//! Source positions and call depth where they are observable: which
//! positions count as the evaluator's own (hidden from traces), which
//! values carry one, and how deep forcing goes before `max-call-depth`.

mod common;

use brix_eval::{Arena, Config, Evaluator, NullHost, Origin, SrcPos};
use bstr::{BString, ByteSlice};

/// Selects `a` from `helper`, an attribute set whose `a` throws, in user
/// code, and renders the error with traces.
fn select_throwing_attr<'a>(ev: &Evaluator<'a>, helper: brix_eval::ValueRef<'a>) -> BString {
    let select = ev
        .eval_str(b"h: h.a", "/")
        .expect("the selecting function evaluates");
    ev.call(select, &[helper], SrcPos::NONE)
        .expect_err("selecting `a` throws")
        .render(true)
}

const HELPER: &str = "{ a = throw \"boom\"; }";
const ATTR_TRACE: &str = "while evaluating the attribute 'a'";

/// A file that fails to compile is still registered (its undefined
/// variable has a position), but it is never linked. Positions in the
/// evaluator's own helpers loaded after it must still be recognised as
/// internal, so the "while evaluating the attribute" trace Nix leaves out
/// for them (`isDerivationInternalPos`) stays out.
#[test]
fn internal_positions_survive_a_failed_load() {
    let arena = Arena::new();
    let host = NullHost;
    let ev = Evaluator::new(&arena, Config::default(), &host);
    let err = ev
        .eval_str(b"undefinedVariable", "/")
        .expect_err("an undefined variable fails to compile");
    assert!(err
        .render(false)
        .contains_str("undefined variable 'undefinedVariable'"));

    let internal = ev
        .eval_internal("helper.nix", HELPER)
        .expect("the helper evaluates");
    let rendered = select_throwing_attr(&ev, internal);
    assert!(
        rendered.contains_str("boom"),
        "unexpected error: {rendered}"
    );
    assert!(
        !rendered.contains_str(ATTR_TRACE),
        "an internal position was shown: {rendered}"
    );

    // The same helper loaded as ordinary code keeps the trace.
    let ordinary = ev
        .eval_source(HELPER.as_bytes(), "/", Origin::String)
        .expect("the helper evaluates");
    let rendered = select_throwing_attr(&ev, ev.alloc_cell(ordinary));
    assert!(
        rendered.contains_str(ATTR_TRACE),
        "the trace of an ordinary position is missing: {rendered}"
    );
}

/// `nix-instantiate --eval --strict` and brix on `expr` with a
/// `max-call-depth` of 10, compared.
fn compare_shallow(expr: &str) -> Result<(), String> {
    let pwd = tempfile::tempdir().expect("create a working directory");
    let host = common::TestHost::new(&[("/pwd", pwd.path())]);
    let config = Config {
        max_call_depth: 10,
        eval_cores: 0,
        ..Config::default()
    };
    let format = common::Format::Plain;
    let ours = common::run_brix_expr(&host, config, expr, "/pwd", format);
    let theirs = common::run_nix_expr(
        expr,
        pwd.path(),
        format,
        &["--option", "max-call-depth", "10"],
        false,
    );
    common::same_outcome(expr, &ours, &theirs)
}

/// `deepSeq` of 30 sets nested through `a`, each made by `set` from the
/// previous one (`acc`) and a counter (`i`).
fn deep(set: &str) -> String {
    format!(
        "builtins.deepSeq (builtins.foldl' (acc: i: {set}) {{}} (builtins.genList (x: x) 30)) 1"
    )
}

fn compare_all(cases: &[String]) {
    common::compare_all("positions", cases, compare_shallow);
}

/// A set built by `//` or `removeAttrs` is a fresh `Bindings`, whose
/// position Nix leaves unset; only sets written in the source have one.
/// The position shows where `deepSeq` runs out of call depth inside the
/// set (`addCallDepth(v.determinePos(noPos))`).
#[test]
fn updated_and_filtered_sets_have_no_position() {
    compare_all(&[
        deep("{ a = acc; b = i; }"),
        deep("{ a = acc; } // { b = i; }"),
        deep(r#"removeAttrs { a = acc; b = i; c = 1; } [ "c" ]"#),
    ]);
}

/// Forcing a suspended application `App fun arg` is `callFunction(fun,
/// arg)`, which forces `fun` inside its own call-depth level: mapAttrs's
/// `App (App f name) value` reaches one level deeper than forcing the
/// inner application first would, so `deepSeq` runs out of depth one set
/// earlier.
#[test]
fn forcing_a_nested_application_costs_a_level() {
    compare_all(&[deep("builtins.mapAttrs (n: v: v) { a = acc; b = i; }")]);
}

/// `deepSeq` of 30 lists nested through their first element, each made
/// by `list` from the previous one (`acc`) and a counter (`i`).
fn deep_list(list: &str) -> String {
    format!(
        "builtins.deepSeq (builtins.foldl' (acc: i: {list}) [ ] (builtins.genList (x: x) 30)) 1"
    )
}

/// Sets and lists that built-ins make have no position, and their lazy
/// elements (`map`'s, `genList`'s, `zipAttrsWith`'s applications) are
/// forced as Nix forces them: `deepSeq` runs out of depth at the same
/// place, with the same position.
#[test]
fn built_in_sets_and_lists_are_forced_as_in_nix() {
    compare_all(&[
        deep("builtins.zipAttrsWith (n: vs: builtins.head vs) [ { a = acc; b = i; } ]"),
        deep("builtins.zipAttrsWith (n: builtins.head) [ { a = acc; } { b = i; } ]"),
        deep(r#"builtins.listToAttrs [ { name = "a"; value = acc; } ]"#),
        deep("builtins.intersectAttrs { a = 1; } { a = acc; b = i; }"),
        deep("builtins.filterAttrs (n: v: true) { a = acc; b = i; }"),
        deep_list("builtins.map (x: x) [ acc ]"),
        deep_list("builtins.genList (x: acc) 1"),
        deep_list("builtins.concatMap (x: [ x ]) [ acc ]"),
        deep_list("builtins.filter (x: true) [ acc ]"),
        deep_list("builtins.sort (a: b: false) [ acc ]"),
        deep_list("(builtins.partition (x: true) [ acc ]).right"),
    ]);
}

/// A built-in forces the function it is given (or leaves it to the
/// applications it makes) at the call depth Nix does: the function here
/// is a thunk `n` calls deep, and every `n` is tried up to where the
/// depth runs out.
#[test]
fn functions_given_to_built_ins_are_forced_at_nix_call_depth() {
    let calls = [
        "builtins.map F [ 1 ]",
        "builtins.mapAttrs F { a = 1; }",
        "builtins.zipAttrsWith F [ { a = 1; } ]",
        "builtins.filterAttrs F { a = 1; }",
        "builtins.foldl' F 0 [ 1 ]",
        "builtins.sort F [ 1 2 ]",
        "builtins.genList F 1",
        "builtins.filter F [ 1 ]",
        "builtins.concatMap F [ 1 ]",
        "builtins.partition F [ 1 ]",
        "builtins.groupBy F [ 1 ]",
        "builtins.any F [ 1 ]",
        "builtins.all F [ 1 ]",
        "builtins.genericClosure { startSet = [ { key = 1; } ]; operator = F; }",
    ];
    let cases: Vec<String> = calls
        .iter()
        .flat_map(|call| {
            (0..9).map(move |n| {
                format!(
                    "let mk = n: if n == 0 then (x: y: x) else mk (n - 1); in builtins.deepSeq ({}) 1",
                    call.replace('F', &format!("(mk {n})"))
                )
            })
        })
        .collect();
    compare_all(&cases);
}

/// The overflow the review found: `deepSeq` through 20,000 nested
/// `zipAttrsWith` results runs out of `max-call-depth` at the frame and
/// position Nix reports.
#[test]
fn deep_seq_through_zip_attrs_with_overflows_as_in_nix() {
    let expr = "let f = n: if n == 0 then 1 else builtins.zipAttrsWith (name: vs: f (n - 1)) [ { a = 1; } ]; in builtins.deepSeq (f 20000) 1";
    let pwd = tempfile::tempdir().expect("create a working directory");
    let host = common::TestHost::new(&[("/pwd", pwd.path())]);
    let config = Config {
        eval_cores: 0,
        ..Config::default()
    };
    let ours = common::run_brix_expr(&host, config, expr, "/pwd", common::Format::Plain);
    let theirs = common::run_nix_expr(expr, pwd.path(), common::Format::Plain, &[], false);
    assert_eq!(ours, theirs);
}
