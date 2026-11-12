//! Miscellaneous built-ins: type tests, `seq`, `trace`, `throw`,
//! `tryEval`, `import`, `genericClosure`, `functionArgs`.

use crate::builtins::{reg, require_attr};
use crate::coerce::Coercion;
use crate::error::{msg, ErrorKind};
use crate::evaluator::Evaluator;
use crate::string_context::StringContext;
use crate::symbol::sym;
use crate::value::{Attr, SrcPos, Value, ValueRef, ValueType};
use crate::Result;

pub fn register(ev: &Evaluator<'_>) {
    reg(ev, "__traceVerbose", 2, prim_trace_verbose);
    reg(ev, "scopedImport", 2, prim_scoped_import);
    reg(ev, "import", 1, prim_import);
    reg(ev, "__typeOf", 1, prim_type_of);
    reg(ev, "isNull", 1, prim_is_null);
    reg(ev, "__isFunction", 1, prim_is_function);
    reg(ev, "__isInt", 1, prim_is_int);
    reg(ev, "__isFloat", 1, prim_is_float);
    reg(ev, "__isString", 1, prim_is_string);
    reg(ev, "__isBool", 1, prim_is_bool);
    reg(ev, "__isPath", 1, prim_is_path);
    reg(ev, "__genericClosure", 1, prim_generic_closure);
    reg(ev, "break", 1, prim_break);
    reg(ev, "abort", 1, prim_abort);
    reg(ev, "throw", 1, prim_throw);
    let c = reg(ev, "__addErrorContext", 2, prim_add_error_context);
    if let Value::PrimOp(p) = c.get() {
        // `addTrace = false` in Nix.
        let p2 = ev.bump.alloc(crate::value::PrimOp {
            name: p.name,
            arity: p.arity,
            add_trace: false,
            imp: p.imp,
        });
        c.set(Value::PrimOp(p2));
    }
    reg(ev, "__tryEval", 1, prim_try_eval);
    reg(ev, "__getEnv", 1, prim_get_env);
    reg(ev, "__seq", 2, prim_seq);
    reg(ev, "__deepSeq", 2, prim_deep_seq);
    reg(ev, "__trace", 2, prim_trace);
    reg(ev, "__warn", 2, prim_warn);
    reg(ev, "__functionArgs", 1, prim_function_args);
}

fn type_test<'a>(
    ev: &Evaluator<'a>,
    pos: SrcPos,
    arg: ValueRef<'a>,
    t: ValueType,
) -> Result<Value<'a>> {
    let v = ev.force(arg, pos)?;
    Ok(Value::Bool(v.value_type() == t))
}

fn prim_type_of<'a>(ev: &Evaluator<'a>, pos: SrcPos, args: &[ValueRef<'a>]) -> Result<Value<'a>> {
    let v = ev.force(args[0], pos)?;
    Ok(ev.mk_string(v.type_of()))
}

fn prim_is_null<'a>(ev: &Evaluator<'a>, pos: SrcPos, args: &[ValueRef<'a>]) -> Result<Value<'a>> {
    type_test(ev, pos, args[0], ValueType::Null)
}

fn prim_is_function<'a>(
    ev: &Evaluator<'a>,
    pos: SrcPos,
    args: &[ValueRef<'a>],
) -> Result<Value<'a>> {
    type_test(ev, pos, args[0], ValueType::Function)
}

fn prim_is_int<'a>(ev: &Evaluator<'a>, pos: SrcPos, args: &[ValueRef<'a>]) -> Result<Value<'a>> {
    type_test(ev, pos, args[0], ValueType::Int)
}

fn prim_is_float<'a>(ev: &Evaluator<'a>, pos: SrcPos, args: &[ValueRef<'a>]) -> Result<Value<'a>> {
    type_test(ev, pos, args[0], ValueType::Float)
}

fn prim_is_string<'a>(ev: &Evaluator<'a>, pos: SrcPos, args: &[ValueRef<'a>]) -> Result<Value<'a>> {
    type_test(ev, pos, args[0], ValueType::String)
}

fn prim_is_bool<'a>(ev: &Evaluator<'a>, pos: SrcPos, args: &[ValueRef<'a>]) -> Result<Value<'a>> {
    type_test(ev, pos, args[0], ValueType::Bool)
}

fn prim_is_path<'a>(ev: &Evaluator<'a>, pos: SrcPos, args: &[ValueRef<'a>]) -> Result<Value<'a>> {
    type_test(ev, pos, args[0], ValueType::Path)
}

/// `import`: a `.drv` file in the store becomes a derivation value, any
/// other file is parsed and evaluated (once).
fn import<'a>(
    ev: &Evaluator<'a>,
    pos: SrcPos,
    path_arg: ValueRef<'a>,
    scope: Option<ValueRef<'a>>,
) -> Result<Value<'a>> {
    let p = ev.realise_path(pos, path_arg)?;
    let p = ev.bump.alloc(p);
    if p.path.ends_with(b".drv") {
        if let Some(sp) = ev.maybe_store_path(p.path) {
            if ev.config.store_dir.display(&sp).as_bytes() == p.path
                && ev.store.borrow().contains(&sp)
            {
                return crate::builtins::derivation::imported_drv_to_value(ev, pos, p, &sp);
            }
        }
    }
    match scope {
        None => ev.eval_file(p),
        Some(scope) => {
            let attrs = ev.force_attrs(
                scope,
                pos,
                "while evaluating the first argument passed to builtins.scopedImport",
            )?;
            let vars: Vec<(crate::symbol::Symbol, u32)> = attrs
                .iter()
                .enumerate()
                .map(|(i, a)| (a.name, i as u32))
                .collect();
            let senv = crate::compile::StaticEnv::new(None, vars);
            let values: Vec<ValueRef<'a>> = attrs.iter().map(|a| a.value).collect();
            let env = ev.alloc_env_with(Some(ev.base_env), &values);
            let resolved = ev.resolve_expr_path(p)?;
            ev.parse_and_eval_file(&resolved, Some((&senv, env)))
        }
    }
}

fn prim_import<'a>(ev: &Evaluator<'a>, pos: SrcPos, args: &[ValueRef<'a>]) -> Result<Value<'a>> {
    import(ev, pos, args[0], None)
}

fn prim_scoped_import<'a>(
    ev: &Evaluator<'a>,
    pos: SrcPos,
    args: &[ValueRef<'a>],
) -> Result<Value<'a>> {
    import(ev, pos, args[1], Some(args[0]))
}

/// `CompareValues`: the `<` used by `lessThan`, `sort` and
/// `genericClosure`.
pub fn compare_values<'a>(
    ev: &Evaluator<'a>,
    a: ValueRef<'a>,
    b: ValueRef<'a>,
    pos: SrcPos,
    error_ctx: &str,
) -> Result<bool> {
    let r: Result<bool> = (|| {
        let v1 = a.get();
        let v2 = b.get();
        match (v1, v2) {
            (Value::Float(f), Value::Int(i)) => return Ok(f < i as f64),
            (Value::Int(i), Value::Float(f)) => return Ok((i as f64) < f),
            _ => {}
        }
        if v1.value_type() != v2.value_type() {
            return Err(ev.error(
                ErrorKind::Eval,
                msg!(
                    "cannot compare ",
                    v1.show_type(),
                    " with ",
                    v2.show_type(),
                    "; values are ",
                    ev.print_for_error(v1),
                    " and ",
                    ev.print_for_error(v2)
                ),
            ));
        }
        match (v1, v2) {
            (Value::Int(x), Value::Int(y)) => Ok(x < y),
            (Value::Float(x), Value::Float(y)) => Ok(x < y),
            (Value::String(x), Value::String(y)) => ev.str_less(x, y),
            (Value::Path(x), Value::Path(y)) => {
                if x.accessor == ev.root_accessor && y.accessor == ev.root_accessor {
                    ev.root_path_less(x.path, y.path)
                } else {
                    Ok(x.path < y.path)
                }
            }
            (Value::List(x), Value::List(y)) => {
                let mut i = 0;
                loop {
                    if i == y.elems.len() {
                        return Ok(false);
                    }
                    if i == x.elems.len() {
                        return Ok(true);
                    }
                    if !ev.eq_values(x.elems[i], y.elems[i], pos, error_ctx)? {
                        return compare_values(
                            ev,
                            x.elems[i],
                            y.elems[i],
                            pos,
                            "while comparing two list elements",
                        );
                    }
                    i += 1;
                }
            }
            _ => Err(ev.error(
                ErrorKind::Eval,
                msg!(
                    "cannot compare ",
                    v1.show_type(),
                    " with ",
                    v2.show_type(),
                    "; values of that type are incomparable (values are ",
                    ev.print_for_error(v1),
                    " and ",
                    ev.print_for_error(v2),
                    ")"
                ),
            )),
        }
    })();
    r.inspect_err(|e| {
        if !error_ctx.is_empty() {
            e.add_trace(None, error_ctx);
        }
    })
}

fn prim_generic_closure<'a>(
    ev: &Evaluator<'a>,
    _pos: SrcPos,
    args: &[ValueRef<'a>],
) -> Result<Value<'a>> {
    let pos = SrcPos::NONE;
    let attrs = ev.force_attrs(
        args[0],
        pos,
        "while evaluating the first argument passed to builtins.genericClosure",
    )?;
    let start_set = require_attr(
        ev,
        attrs,
        sym::START_SET,
        "in the attrset passed as argument to builtins.genericClosure",
    )?;
    let start = ev.force_list_elems(
        start_set.value,
        pos,
        "while evaluating the 'startSet' attribute passed as argument to builtins.genericClosure",
    )?;
    let mut work: std::collections::VecDeque<ValueRef<'a>> = start.iter().copied().collect();
    if start.is_empty() {
        return Ok(start_set.value.get());
    }
    let op = require_attr(
        ev,
        attrs,
        sym::OPERATOR,
        "in the attrset passed as argument to builtins.genericClosure",
    )?;
    let op_v = ev.force_function(
        op.value,
        pos,
        "while evaluating the 'operator' attribute passed as argument to builtins.genericClosure",
    )?;
    let mut res: Vec<ValueRef<'a>> = Vec::new();
    // Keys seen so far, kept in the order `std::map` with `CompareValues`
    // would, so that comparison errors surface the same way.
    let mut keys: Vec<(ValueRef<'a>, ValueRef<'a>)> = Vec::new();
    while let Some(e) = work.pop_front() {
        let eb = ev.force_attrs(e, pos, "").inspect_err(|err| {
            err.add_trace(
                None,
                msg!("in genericClosure element ", ev.print_for_error(e.get())),
            );
        })?;
        let key = require_attr(ev, eb, sym::KEY, "").inspect_err(|err| {
            err.add_trace(
                None,
                msg!("in genericClosure element ", ev.print_for_error(e.get())),
            );
        })?;
        ev.force(key.value, pos)?;
        // std::map::insert: binary search with the comparator.
        let mut lo = 0usize;
        let mut hi = keys.len();
        let mut found = false;
        let mut cmp_error: Option<(crate::Error, ValueRef<'a>)> = None;
        while lo < hi {
            let mid = (lo + hi) / 2;
            let (k, elem) = keys[mid];
            let less = match compare_values(ev, key.value, k, pos, "") {
                Ok(b) => b,
                Err(err) => {
                    cmp_error = Some((err, elem));
                    break;
                }
            };
            if less {
                hi = mid;
            } else {
                let greater = match compare_values(ev, k, key.value, pos, "") {
                    Ok(b) => b,
                    Err(err) => {
                        cmp_error = Some((err, elem));
                        break;
                    }
                };
                if greater {
                    lo = mid + 1;
                } else {
                    found = true;
                    break;
                }
            }
        }
        if let Some((err, other)) = cmp_error {
            err.add_trace(None, msg!("with element ", ev.print_for_error(other.get())));
            err.add_trace(
                None,
                msg!("while comparing element ", ev.print_for_error(e.get())),
            );
            return Err(err);
        }
        if found {
            continue;
        }
        keys.insert(lo, (key.value, e));
        res.push(e);
        let r: Result<()> = (|| {
            let new_elems = ev.call(op_v, &[e], pos)?;
            let cell = ev.alloc_cell(new_elems);
            let list = ev.force_list_elems(
                cell,
                pos,
                "while evaluating the return value of the `operator` passed to builtins.genericClosure",
            )?;
            for &elem in list {
                ev.force(elem, pos)?;
                work.push_back(elem);
            }
            Ok(())
        })();
        r.inspect_err(|err| {
            err.add_trace(
                None,
                msg!(
                    "while calling operator on genericClosure element ",
                    ev.print_for_error(e.get())
                ),
            );
        })?;
    }
    Ok(ev.mk_list(&res))
}

fn prim_break<'a>(ev: &Evaluator<'a>, pos: SrcPos, args: &[ValueRef<'a>]) -> Result<Value<'a>> {
    ev.force(args[0], pos)
}

fn prim_abort<'a>(ev: &Evaluator<'a>, pos: SrcPos, args: &[ValueRef<'a>]) -> Result<Value<'a>> {
    let mut ctx = StringContext::new();
    let s = ev.coerce_to_string(
        pos,
        args[0],
        &mut ctx,
        "while evaluating the error message passed to builtins.abort",
        Coercion::COPY,
    )?;
    Err(ev.error(
        ErrorKind::Abort,
        msg!(
            "evaluation aborted with the following error message: '",
            s,
            "'"
        ),
    ))
}

fn prim_throw<'a>(ev: &Evaluator<'a>, pos: SrcPos, args: &[ValueRef<'a>]) -> Result<Value<'a>> {
    let mut ctx = StringContext::new();
    let s = ev.coerce_to_string(
        pos,
        args[0],
        &mut ctx,
        "while evaluating the error message passed to builtin.throw",
        Coercion::COPY,
    )?;
    Err(ev.error(ErrorKind::Thrown, s))
}

fn prim_add_error_context<'a>(
    ev: &Evaluator<'a>,
    pos: SrcPos,
    args: &[ValueRef<'a>],
) -> Result<Value<'a>> {
    match ev.force(args[1], pos) {
        Ok(v) => Ok(v),
        // `std::bad_alloc` and nlohmann's exceptions are not Nix `Error`s,
        // so the handler never sees them and the message is not evaluated.
        Err(e) if matches!(e.kind, ErrorKind::OutOfMemory | ErrorKind::Foreign) => Err(e),
        Err(e) => {
            let mut ctx = StringContext::new();
            let msg = ev.coerce_to_string(
                pos,
                args[0],
                &mut ctx,
                "while evaluating the error message passed to builtins.addErrorContext",
                Coercion::NO_COPY,
            )?;
            e.add_trace_always(msg);
            Err(e)
        }
    }
}

fn prim_try_eval<'a>(ev: &Evaluator<'a>, pos: SrcPos, args: &[ValueRef<'a>]) -> Result<Value<'a>> {
    let success = ev.sym("success");
    let attrs = match ev.force(args[0], pos) {
        Ok(_) => vec![
            ev.attr_cell(sym::VALUE, args[0]),
            ev.attr_cell(success, ev.consts.true_),
        ],
        Err(e) if e.is_catchable() => vec![
            ev.attr_cell(sym::VALUE, ev.consts.false_),
            ev.attr_cell(success, ev.consts.false_),
        ],
        Err(e) => return Err(e),
    };
    Ok(ev.mk_attrs(attrs, SrcPos::NONE))
}

fn prim_get_env<'a>(ev: &Evaluator<'a>, pos: SrcPos, args: &[ValueRef<'a>]) -> Result<Value<'a>> {
    ev.force_string_no_ctx(
        args[0],
        pos,
        "while evaluating the first argument passed to builtins.getEnv",
    )?;
    // Pure evaluation: the environment is empty.
    Ok(ev.consts.empty_string.get())
}

fn prim_seq<'a>(ev: &Evaluator<'a>, pos: SrcPos, args: &[ValueRef<'a>]) -> Result<Value<'a>> {
    ev.force(args[0], pos)?;
    ev.force(args[1], pos)
}

fn prim_deep_seq<'a>(ev: &Evaluator<'a>, pos: SrcPos, args: &[ValueRef<'a>]) -> Result<Value<'a>> {
    ev.force_deep(args[0])?;
    ev.force(args[1], pos)
}

fn prim_trace<'a>(ev: &Evaluator<'a>, pos: SrcPos, args: &[ValueRef<'a>]) -> Result<Value<'a>> {
    let v = ev.force(args[0], pos)?;
    let msg = match v {
        Value::String(s) => ev.str_bytes(s)?.into(),
        _ => ev.print_value(args[0], &crate::print::PrintOptions::default())?,
    };
    ev.host.trace(&msg);
    ev.force(args[1], pos)
}

fn prim_trace_verbose<'a>(
    ev: &Evaluator<'a>,
    pos: SrcPos,
    args: &[ValueRef<'a>],
) -> Result<Value<'a>> {
    if ev.config.trace_verbose {
        prim_trace(ev, pos, args)
    } else {
        ev.force(args[1], pos)
    }
}

fn prim_warn<'a>(ev: &Evaluator<'a>, pos: SrcPos, args: &[ValueRef<'a>]) -> Result<Value<'a>> {
    let s = ev.force_string(
        args[0],
        pos,
        "while evaluating the first argument; the message passed to builtins.warn",
    )?;
    ev.host.log(&crate::terminal::indent_message(
        "evaluation warning",
        ev.str_bytes(s)?,
    ));
    ev.force(args[1], pos)
}

fn prim_function_args<'a>(
    ev: &Evaluator<'a>,
    pos: SrcPos,
    args: &[ValueRef<'a>],
) -> Result<Value<'a>> {
    let v = ev.force(args[0], pos)?;
    match v {
        Value::PrimOp(_) | Value::PrimOpApp(_) => Ok(Value::Attrs(ev.consts.empty_attrs)),
        Value::Lambda { node, .. } => {
            let (code, lam) = ev.lambda_def(node);
            match &lam.formals {
                Some(formals) => {
                    let attrs: Vec<crate::value::Attr<'a>> = formals
                        .formals
                        .iter()
                        .map(|f| {
                            Attr::new(
                                f.name,
                                ev.src_pos(code, f.pos),
                                if f.default.is_some() {
                                    ev.consts.true_
                                } else {
                                    ev.consts.false_
                                },
                            )
                        })
                        .collect();
                    Ok(ev.mk_attrs(attrs, SrcPos::NONE))
                }
                None => Ok(Value::Attrs(ev.consts.empty_attrs)),
            }
        }
        _ => Err(ev.error_at(
            ErrorKind::TypeError,
            "'functionArgs' requires a function",
            pos,
        )),
    }
}
