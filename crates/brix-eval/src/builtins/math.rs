//! Arithmetic: `add`, `sub`, `mul`, `div`, bit operations, `lessThan`,
//! `ceil`, `floor`.

use crate::builtins::core::compare_values;
use crate::builtins::reg;
use crate::error::ErrorKind;
use crate::evaluator::Evaluator;
use crate::value::{SrcPos, Value, ValueRef};
use crate::Result;

pub fn register(ev: &Evaluator<'_>) {
    reg(ev, "__ceil", 1, prim_ceil);
    reg(ev, "__floor", 1, prim_floor);
    reg(ev, "__add", 2, prim_add);
    reg(ev, "__sub", 2, prim_sub);
    reg(ev, "__mul", 2, prim_mul);
    reg(ev, "__div", 2, prim_div);
    reg(ev, "__bitAnd", 2, prim_bit_and);
    reg(ev, "__bitOr", 2, prim_bit_or);
    reg(ev, "__bitXor", 2, prim_bit_xor);
    reg(ev, "__lessThan", 2, prim_less_than);
}

fn is_float(v: Value<'_>) -> bool {
    matches!(v, Value::Float(_))
}

#[allow(clippy::too_many_arguments)]
fn arith<'a>(
    ev: &Evaluator<'a>,
    pos: SrcPos,
    args: &[ValueRef<'a>],
    name: &str,
    verb: &str,
    sym: char,
    fop: fn(f64, f64) -> f64,
    iop: fn(i64, i64) -> Option<i64>,
) -> Result<Value<'a>> {
    let v1 = ev.force(args[0], pos)?;
    let v2 = ev.force(args[1], pos)?;
    if is_float(v1) || is_float(v2) {
        let f1 = ev.force_float(
            args[0],
            pos,
            &format!("while evaluating the first argument of the {name}"),
        )?;
        let f2 = ev.force_float(
            args[1],
            pos,
            &format!("while evaluating the second argument of the {name}"),
        )?;
        return Ok(Value::Float(fop(f1, f2)));
    }
    let i1 = ev.force_int(
        args[0],
        pos,
        &format!("while evaluating the first argument of the {name}"),
    )?;
    let i2 = ev.force_int(
        args[1],
        pos,
        &format!("while evaluating the second argument of the {name}"),
    )?;
    match iop(i1, i2) {
        Some(r) => Ok(Value::Int(r)),
        None => Err(ev.error_at(
            ErrorKind::Eval,
            format!("integer overflow in {verb} {i1} {sym} {i2}"),
            pos,
        )),
    }
}

fn prim_add<'a>(ev: &Evaluator<'a>, pos: SrcPos, args: &[ValueRef<'a>]) -> Result<Value<'a>> {
    arith(
        ev,
        pos,
        args,
        "addition",
        "adding",
        '+',
        |a, b| a + b,
        i64::checked_add,
    )
}

fn prim_sub<'a>(ev: &Evaluator<'a>, pos: SrcPos, args: &[ValueRef<'a>]) -> Result<Value<'a>> {
    arith(
        ev,
        pos,
        args,
        "subtraction",
        "subtracting",
        '-',
        |a, b| a - b,
        i64::checked_sub,
    )
}

fn prim_mul<'a>(ev: &Evaluator<'a>, pos: SrcPos, args: &[ValueRef<'a>]) -> Result<Value<'a>> {
    // Nix's message says "first of the multiplication" (sic) for the
    // first operand.
    let v1 = ev.force(args[0], pos)?;
    let v2 = ev.force(args[1], pos)?;
    if is_float(v1) || is_float(v2) {
        let f1 = ev.force_float(
            args[0],
            pos,
            "while evaluating the first of the multiplication",
        )?;
        let f2 = ev.force_float(
            args[1],
            pos,
            "while evaluating the second argument of the multiplication",
        )?;
        return Ok(Value::Float(f1 * f2));
    }
    let i1 = ev.force_int(
        args[0],
        pos,
        "while evaluating the first argument of the multiplication",
    )?;
    let i2 = ev.force_int(
        args[1],
        pos,
        "while evaluating the second argument of the multiplication",
    )?;
    match i1.checked_mul(i2) {
        Some(r) => Ok(Value::Int(r)),
        None => Err(ev.error_at(
            ErrorKind::Eval,
            format!("integer overflow in multiplying {i1} * {i2}"),
            pos,
        )),
    }
}

fn prim_div<'a>(ev: &Evaluator<'a>, pos: SrcPos, args: &[ValueRef<'a>]) -> Result<Value<'a>> {
    let v1 = ev.force(args[0], pos)?;
    let v2 = ev.force(args[1], pos)?;
    let f2 = ev.force_float(
        args[1],
        pos,
        "while evaluating the second operand of the division",
    )?;
    if f2 == 0.0 {
        return Err(ev.error_at(ErrorKind::Eval, "division by zero", pos));
    }
    if is_float(v1) || is_float(v2) {
        let f1 = ev.force_float(
            args[0],
            pos,
            "while evaluating the first operand of the division",
        )?;
        return Ok(Value::Float(f1 / f2));
    }
    let i1 = ev.force_int(
        args[0],
        pos,
        "while evaluating the first operand of the division",
    )?;
    let i2 = ev.force_int(
        args[1],
        pos,
        "while evaluating the second operand of the division",
    )?;
    match i1.checked_div(i2) {
        Some(r) => Ok(Value::Int(r)),
        None => Err(ev.error_at(
            ErrorKind::Eval,
            format!("integer overflow in dividing {i1} / {i2}"),
            pos,
        )),
    }
}

fn bitop<'a>(
    ev: &Evaluator<'a>,
    pos: SrcPos,
    args: &[ValueRef<'a>],
    name: &str,
    op: fn(i64, i64) -> i64,
) -> Result<Value<'a>> {
    let i1 = ev.force_int(
        args[0],
        pos,
        &format!("while evaluating the first argument passed to builtins.{name}"),
    )?;
    let i2 = ev.force_int(
        args[1],
        pos,
        &format!("while evaluating the second argument passed to builtins.{name}"),
    )?;
    Ok(Value::Int(op(i1, i2)))
}

fn prim_bit_and<'a>(ev: &Evaluator<'a>, pos: SrcPos, args: &[ValueRef<'a>]) -> Result<Value<'a>> {
    bitop(ev, pos, args, "bitAnd", |a, b| a & b)
}

fn prim_bit_or<'a>(ev: &Evaluator<'a>, pos: SrcPos, args: &[ValueRef<'a>]) -> Result<Value<'a>> {
    bitop(ev, pos, args, "bitOr", |a, b| a | b)
}

fn prim_bit_xor<'a>(ev: &Evaluator<'a>, pos: SrcPos, args: &[ValueRef<'a>]) -> Result<Value<'a>> {
    bitop(ev, pos, args, "bitXor", |a, b| a ^ b)
}

fn prim_less_than<'a>(ev: &Evaluator<'a>, pos: SrcPos, args: &[ValueRef<'a>]) -> Result<Value<'a>> {
    ev.force(args[0], pos)?;
    ev.force(args[1], pos)?;
    Ok(Value::Bool(compare_values(
        ev,
        args[0],
        args[1],
        SrcPos::NONE,
        "",
    )?))
}

fn round_to_int<'a>(
    ev: &Evaluator<'a>,
    pos: SrcPos,
    args: &[ValueRef<'a>],
    name: &str,
    op: fn(f64) -> f64,
) -> Result<Value<'a>> {
    let arg_pos = ev.determine_pos(args[0].get(), pos);
    let value = ev.force_float(
        args[0],
        arg_pos,
        &format!("while evaluating the first argument passed to builtins.{name}"),
    )?;
    let rounded = op(value);
    let is_int = matches!(args[0].get(), Value::Int(_));
    let int_min = i64::MIN as f64;
    let result = if rounded >= int_min && rounded < -int_min {
        rounded as i64
    } else if is_int {
        let Value::Int(i) = args[0].get() else {
            unreachable!()
        };
        return Err(ev.error_at(
            ErrorKind::Eval,
            format!(
                "Due to a bug (see https://github.com/NixOS/nix/issues/12899) the NixInt argument {i} caused undefined behavior in previous Nix versions.\n\tFuture Nix versions might implement the correct behavior."
            ),
            pos,
        ));
    } else {
        let Value::Float(f) = args[0].get() else {
            unreachable!()
        };
        return Err(ev.error_at(
            ErrorKind::Eval,
            format!(
                "NixFloat argument {} is not in the range of NixInt",
                brix_syntax::float::format_g(f)
            ),
            pos,
        ));
    };
    if is_int {
        let Value::Int(i) = args[0].get() else {
            unreachable!()
        };
        if i != result {
            return Err(ev.error_at(
                ErrorKind::Eval,
                format!(
                    "Due to a bug (see https://github.com/NixOS/nix/issues/12899) a loss of precision occurred in previous Nix versions because the NixInt argument {i} was rounded to {result}.\n\tFuture Nix versions might implement the correct behavior."
                ),
                pos,
            ));
        }
    }
    Ok(Value::Int(result))
}

fn prim_ceil<'a>(ev: &Evaluator<'a>, pos: SrcPos, args: &[ValueRef<'a>]) -> Result<Value<'a>> {
    round_to_int(ev, pos, args, "ceil", f64::ceil)
}

fn prim_floor<'a>(ev: &Evaluator<'a>, pos: SrcPos, args: &[ValueRef<'a>]) -> Result<Value<'a>> {
    round_to_int(ev, pos, args, "floor", f64::floor)
}
