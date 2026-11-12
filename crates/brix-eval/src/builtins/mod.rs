//! The `builtins` set (`primops.cc` and `primops/*.cc`).
//!
//! Each module registers one group. Functions are named after their Nix
//! counterparts (`prim_x` for `builtins.x`) and documented with the
//! behaviour they reproduce, including error messages and the order in
//! which arguments are forced, which is observable through `trace` and
//! through which error is reported first.

pub mod attrs;
pub mod context;
pub mod core;
pub mod derivation;
pub mod fetch;
pub mod lists;
pub mod math;
pub mod paths;
pub mod strings;

use crate::error::{msg, ErrorKind};
use crate::evaluator::Evaluator;
use crate::symbol::Symbol;
use crate::value::{Attr, Bindings, SrcPos, Value, ValueRef};
use crate::Result;

/// The signature every built-in has.
pub type BuiltinFn = for<'a> fn(&Evaluator<'a>, SrcPos, &[ValueRef<'a>]) -> Result<Value<'a>>;

/// Registers a built-in function.
pub fn reg<'a>(ev: &Evaluator<'a>, name: &'static str, arity: u8, f: BuiltinFn) -> ValueRef<'a> {
    let boxed: &'a crate::value::PrimOpFn<'a> = ev
        .code_bump
        .alloc(move |ev: &Evaluator<'a>, pos, args: &[ValueRef<'a>]| f(ev, pos, args));
    ev.add_primop(name, arity, boxed)
}

/// `EvalState::getAttr`: attribute `name` of `attrs`, or the type error
/// "attribute 'name' missing" with `error_ctx` as its trace.
#[inline]
pub(crate) fn require_attr<'a>(
    ev: &Evaluator<'a>,
    attrs: Bindings<'a>,
    name: Symbol,
    error_ctx: &str,
) -> Result<&'a Attr<'a>> {
    match attrs.get(name) {
        Some(attr) => Ok(attr),
        None => Err(missing_attr(ev, name, error_ctx)),
    }
}

#[cold]
#[inline(never)]
fn missing_attr(ev: &Evaluator<'_>, name: Symbol, error_ctx: &str) -> crate::Error {
    ev.error(
        ErrorKind::TypeError,
        msg!("attribute '", ev.symbol_ident(name), "' missing"),
    )
    .with_trace(None, error_ctx)
}

/// Registers a constant.
pub fn constant<'a>(ev: &Evaluator<'a>, name: &'static str, v: Value<'a>) {
    let cell = ev.alloc_cell(v);
    ev.add_global(name, cell);
}

/// `createBaseEnv`: everything in the order Nix adds it. `builtins` must
/// be first (it is `baseEnv.values[0]`).
pub fn register_all(ev: &Evaluator<'_>) {
    ev.add_global("builtins", ev.builtins_cell);
    ev.add_global("true", ev.consts.true_);
    ev.add_global("false", ev.consts.false_);
    ev.add_global("null", ev.consts.null);
    constant(
        ev,
        "__nixVersion",
        ev.mk_string(&ev.config.nix_version.clone()),
    );
    constant(ev, "__storeDir", ev.mk_string(ev.config.store_dir.as_str()));
    constant(ev, "__langVersion", Value::Int(6));
    // Pure evaluation: no search path.
    constant(ev, "__nixPath", ev.consts.empty_list.get());
    core::register(ev);
    math::register(ev);
    strings::register(ev);
    attrs::register(ev);
    lists::register(ev);
    paths::register(ev);
    context::register(ev);
    derivation::register(ev);
    fetch::register(ev);
}

/// The `derivation` wrapper and friends are Nix expressions; they are
/// loaded after the primops exist (`createBaseEnv` does the same).
pub fn finish(ev: &Evaluator<'_>) -> Result<()> {
    derivation::install_wrapper(ev)
}
