//! Deep recursion on a small thread stack: evaluation moves to fresh stack
//! segments as it needs them (forcing thunks and calling functions alike),
//! so only `max-call-depth` limits how deep a Nix program may go, and that
//! limit is reported exactly as on a large stack. The printers walk values
//! as deep as they nest, and grow the stack the same way.
//!
//! Each test runs on a thread with a 512 KiB stack, a few times the
//! 128 KiB red zone the evaluator keeps, so every deep evaluation here
//! crosses many segment boundaries, down and back up. Without the growth
//! the thread would overflow its stack and the test process would die.

use brix_eval::string_context::StringContext;
use brix_eval::{Arena, Config, Evaluator, NullHost, PrintOptions, SrcPos};

/// The thread stack the tests run on.
const SMALL_STACK: usize = 512 * 1024;

/// How the result is printed.
#[derive(Clone, Copy)]
enum Output {
    /// `nix-instantiate --eval --strict`: deeply forced, `printAmbiguous`.
    Ambiguous,
    /// `nix eval`: `printValue`, forcing as it goes.
    Value,
    /// `nix-instantiate --eval --strict --json`.
    Json,
    /// `nix-instantiate --eval --strict --xml`.
    Xml,
}

/// Evaluates `src` with `config` on a thread with a [`SMALL_STACK`] and
/// prints the deeply forced result, or the rendered error.
fn eval_on_small_stack(src: String, config: Config) -> Result<String, String> {
    print_on_small_stack(src, config, Output::Ambiguous)
}

/// Evaluates `src` with `config` on a thread with a [`SMALL_STACK`] and
/// prints the result as `output` says, or the rendered error, as text.
fn print_on_small_stack(src: String, config: Config, output: Output) -> Result<String, String> {
    let printed = std::thread::Builder::new()
        .stack_size(SMALL_STACK)
        .spawn(move || {
            let arena = Arena::new();
            let host = NullHost;
            let ev = Evaluator::new(&arena, config, &host);
            let printed = ev.eval_str(src.as_bytes(), "/").and_then(|v| {
                let cell = ev.alloc_cell(v);
                let mut ctx = StringContext::new();
                match output {
                    Output::Ambiguous => {
                        ev.force_deep(cell)?;
                        ev.print_ambiguous(cell).map(Vec::from)
                    }
                    Output::Value => ev
                        .print_value(cell, &PrintOptions::for_eval())
                        .map(Vec::from),
                    Output::Json => ev
                        .to_json(cell, SrcPos::NONE, &mut ctx, true, false)
                        .map(String::into_bytes),
                    Output::Xml => ev.to_xml(cell, SrcPos::NONE, &mut ctx, true, false),
                }
            });
            printed.map_err(|e| Vec::from(e.render(false)))
        })
        .expect("spawn the evaluation thread")
        .join()
        .expect("the evaluation thread panicked");
    let text =
        |bytes: Vec<u8>| String::from_utf8(bytes).expect("the output of these tests is text");
    printed.map(text).map_err(text)
}

fn deep_call_config() -> Config {
    Config {
        max_call_depth: 1_000_000,
        ..Config::default()
    }
}

/// `n` thunks, each forcing the previous one: recursion through forcing
/// alone, which no call-depth limit bounds.
#[test]
fn a_deep_chain_of_thunks_grows_the_stack() {
    let n = 100_000;
    let mut src = String::from("let a0 = 0;");
    for i in 1..=n {
        src.push_str(&format!(" a{i} = a{} + 1;", i - 1));
    }
    src.push_str(&format!(" in a{n}"));
    assert_eq!(
        eval_on_small_stack(src, Config::default()),
        Ok(n.to_string())
    );
}

#[test]
fn deep_calls_within_max_call_depth_grow_the_stack() {
    let src = "let f = n: if n == 0 then 0 else 1 + f (n - 1); in f 100000";
    assert_eq!(
        eval_on_small_stack(src.to_owned(), deep_call_config()),
        Ok("100000".to_owned())
    );
}

/// Ten descents to depth 20,000 one after the other: each returns to the
/// segment it started on, and the next one grows again from there.
#[test]
fn repeated_deep_descents_cross_segment_boundaries() {
    let src = "let f = n: if n == 0 then 0 else 1 + f (n - 1); \
               in builtins.foldl' (acc: i: acc + f 20000) 0 (builtins.genList (i: i) 10)";
    assert_eq!(
        eval_on_small_stack(src.to_owned(), deep_call_config()),
        Ok("200000".to_owned())
    );
}

/// The threshold `tests/differential.rs` pins against Nix (on a 1 GiB
/// stack) is the same on a small one: depth is counted, never measured.
#[test]
fn max_call_depth_is_reported_as_on_a_large_stack() {
    let f = |n: u32| format!("let f = n: if n == 0 then 0 else f (n - 1); in f {n}");
    assert_eq!(
        eval_on_small_stack(f(9999), Config::default()),
        Ok("0".to_owned())
    );
    let err = eval_on_small_stack(f(10000), Config::default())
        .expect_err("one level past max-call-depth");
    assert!(
        err.contains("error: stack overflow; max-call-depth exceeded"),
        "unexpected error: {err}"
    );
}

/// Deeply nested *source*: `n` copies of `open`, then `leaf`, then `n`
/// copies of `close`.
fn nested(open: &str, leaf: &str, close: &str, n: usize) -> String {
    let mut src = String::with_capacity((open.len() + close.len()) * n + leaf.len());
    for _ in 0..n {
        src.push_str(open);
    }
    src.push_str(leaf);
    for _ in 0..n {
        src.push_str(close);
    }
    src
}

/// How deep the source nests in the tests below. Parsing, compiling,
/// evaluating and dropping such a file recurses once per level (strict
/// children are evaluated by calling their closures directly, with no
/// thunk or call in between); all of it has to grow the stack.
const DEEP: usize = 100_000;

/// `if … then … else if …`, 100,000 deep. Nix evaluates this one too.
#[test]
fn a_deep_else_if_chain() {
    let src = nested("if false then 0 else ", "1", "", DEEP);
    assert_eq!(
        eval_on_small_stack(src, Config::default()),
        Ok("1".to_owned())
    );
}

/// `1 + (1 + (… + 1))` and `1 + 1 + … + 1`. Nix gives 30001 for 30,000
/// levels and runs out of stack at 100,000 ("stack overflow (possible
/// infinite recursion)"); brix finishes.
#[test]
fn deep_additions() {
    let right = nested("1 + (", "1", ")", DEEP);
    let left = nested("", "1", " + 1", DEEP);
    for src in [right, left] {
        assert_eq!(
            eval_on_small_stack(src, Config::default()),
            Ok((DEEP + 1).to_string())
        );
    }
}

/// `{ a = 0; } // ({ a = 0; } // (… // { b = 1; }))` and the
/// left-nested chain. Nix's limits are those of `+`.
#[test]
fn deep_updates() {
    let right = nested("{ a = 0; } // (", "{ b = 1; }", ")", DEEP);
    let left = nested("", "{ a = 0; }", " // { b = 1; }", DEEP);
    for src in [right, left] {
        assert_eq!(
            eval_on_small_stack(src, Config::default()),
            Ok("{ a = 0; b = 1; }".to_owned())
        );
    }
}

/// `!!…!true` and `"${"${…"x"…}"}"`: unary nesting, and interpolation,
/// whose parts are evaluated inside the string concatenation. The
/// negations are fewer: each one's position is its innermost operand's
/// (`ExprOpNot::getPos`), found by walking down, so the chain costs time
/// quadratic in its length (in Nix too, on every evaluation).
#[test]
fn deep_negation_and_interpolation() {
    let not = nested("!", "true", "", DEEP / 4);
    assert_eq!(
        eval_on_small_stack(not, Config::default()),
        Ok("true".to_owned())
    );
    let interpolation = nested("\"${", "\"x\"", "}\"", DEEP);
    assert_eq!(
        eval_on_small_stack(interpolation, Config::default()),
        Ok("\"x\"".to_owned())
    );
}

/// `{ }.a or ({ }.a or (… 1))`: a default is compiled on its own and
/// entered from the selection, not through a thunk or a call. Nix runs
/// out of stack at this depth.
#[test]
fn deep_select_defaults() {
    let src = nested("{ }.a or (", "1", ")", DEEP);
    assert_eq!(
        eval_on_small_stack(src, Config::default()),
        Ok("1".to_owned())
    );
}

/// `{ a = { a = … { a = 1; } … }; }`: walking it with `genericClosure`
/// visits every level without recursing (Nix gives 100001); `deepSeq`
/// recurses and stops at `max-call-depth`, as in Nix.
#[test]
fn deep_attribute_nesting() {
    let set = nested("{ a = ", "1", "; }", DEEP);
    let walk = format!(
        "builtins.length (builtins.genericClosure {{ startSet = [ {{ key = 0; v = {set}; }} ]; \
         operator = e: if builtins.isAttrs e.v then [ {{ key = e.key + 1; v = e.v.a; }} ] else [ ]; }})"
    );
    assert_eq!(
        eval_on_small_stack(walk, Config::default()),
        Ok((DEEP + 1).to_string())
    );
    let err = eval_on_small_stack(format!("builtins.deepSeq ({set}) 1"), Config::default())
        .expect_err("deepSeq runs out of call depth");
    assert!(
        err.contains("error: stack overflow; max-call-depth exceeded"),
        "unexpected error: {err}"
    );
}

/// `[ [ … [ 1 ] … ] ]`, as deep as the source: every printer walks it
/// down to the `1`, one recursion per level (and the parser reads the
/// lists one recursion per level too).
#[test]
fn print_ambiguous_walks_deeply_nested_values() {
    let list = nested("[ ", "1", " ]", DEEP);
    assert_eq!(
        print_on_small_stack(list.clone(), deep_call_config(), Output::Ambiguous),
        Ok(list)
    );
}

#[test]
fn print_value_walks_deeply_nested_values() {
    let list = nested("[ ", "1", " ]", DEEP);
    assert_eq!(
        print_on_small_stack(list.clone(), deep_call_config(), Output::Value),
        Ok(list)
    );
}

#[test]
fn to_json_walks_deeply_nested_values() {
    assert_eq!(
        print_on_small_stack(
            nested("[ ", "1", " ]", DEEP),
            deep_call_config(),
            Output::Json
        ),
        Ok(nested("[", "1", "]", DEEP))
    );
}

#[test]
fn to_xml_walks_deeply_nested_values() {
    // Every level of the document is indented, so it grows with the
    // square of the depth: a few thousand levels are enough here.
    let depth = 3000;
    let xml = print_on_small_stack(
        nested("[ ", "1", " ]", depth),
        deep_call_config(),
        Output::Xml,
    )
    .expect("the list prints as XML");
    assert_eq!(xml.matches("<list>").count(), depth);
    assert!(xml.contains(&format!("{}<int value=\"1\" />", " ".repeat(2 * depth + 2))));
}
