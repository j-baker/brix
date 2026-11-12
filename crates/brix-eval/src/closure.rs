//! Closure compilation: each IR node becomes a Rust closure, built the
//! first time the node is evaluated, that captures what the node needs
//! (its children's closures, slots, constants, positions) and evaluates
//! it without decoding the node again. This is the technique of Feeley and
//! Lapalme, "Using closures for code generation" (1987): no code is
//! generated at run time; every closure body is ordinary Rust compiled
//! into brix, and building a node's closure only chooses a body and fills
//! in its captured data.
//!
//! This replaced a tree walker that matched on the node kind at every
//! step; each closure does what that walker's arm for its node did, in the
//! same order, and the shared pieces (selection, string concatenation,
//! attribute sets with dynamic names) live in [`crate::eval`]. Measured
//! against the walker in the same binary, closures were 2 to 6% faster on
//! nixpkgs evaluations:
//!
//! - Strict children (a select's subject, an `if`'s branches, a `let`'s
//!   body) are compiled together with their parent and called directly.
//! - Lazy children (attribute values, call arguments, list elements, `let`
//!   bindings) are never compiled with their parent: what `maybe_thunk`
//!   would do with them is decided once, at build time ([`Lazy`]), and a
//!   thunk's node is compiled when the thunk is forced. A lambda's body is
//!   compiled on its first call. So only code that runs is compiled.
//!
//! Closures live in the evaluator's arena and die with it. Thunks keep
//! their `(env, GlobalNode)` representation; the closure of a node is
//! found through its file's [`ClosureTable`].

use crate::runtime::cell::Cell;
use std::fmt;

use crate::runtime::Bump;
use smallvec::SmallVec;

use crate::code::LinkedCode;
use crate::error::msg;
use crate::evaluator::Evaluator;
use crate::ir::{AttrKind, BinOp, Node, NodeId, Pos, VarRef};
use crate::stack::{STACK_RED_ZONE, STACK_SEGMENT};
use crate::stats::{LazySite, Stats, ThunkKind};
use crate::symbol::{sym, Symbol};
use crate::value::{Attr, Env, GlobalNode, SrcPos, Value, ValueRef};
use crate::Result;

/// The compiled form of a node: evaluates it in an environment.
#[cfg(not(brix_parallel))]
pub type Eval<'a> = &'a dyn Fn(&Evaluator<'a>, Env<'a>) -> Result<Value<'a>>;
#[cfg(brix_parallel)]
pub type Eval<'a> = &'a (dyn Fn(&Evaluator<'a>, Env<'a>) -> Result<Value<'a>> + Send + Sync);

/// Native closure cache, allocated on first use and indexed by the file's nodes.
pub struct ClosureTable<'a> {
    slots: Cell<&'a [Cell<Option<Eval<'a>>>]>,
    bump: &'a Bump,
    nodes: usize,
}

impl<'a> ClosureTable<'a> {
    pub fn new(bump: &'a Bump, nodes: usize) -> ClosureTable<'a> {
        ClosureTable {
            slots: Cell::new(&[]),
            bump,
            nodes,
        }
    }

    #[inline]
    pub fn get(&self, node: NodeId) -> Option<Eval<'a>> {
        let slots = self.slots.get();
        if slots.is_empty() {
            assert!(
                (node.0 as usize) < self.nodes,
                "closure node index out of bounds"
            );
            None
        } else {
            slots[node.0 as usize].get()
        }
    }

    fn set(&self, node: NodeId, f: Eval<'a>) {
        let mut slots = self.slots.get();
        if slots.is_empty() {
            assert!(
                (node.0 as usize) < self.nodes,
                "closure node index out of bounds"
            );
            // Linking is shared with the managed evaluator, which has its own
            // closure cache. Allocate native slots only when native execution
            // first compiles this file; their arena addresses remain stable.
            slots = self
                .bump
                .alloc_slice_fill_with(self.nodes, |_| Cell::new(None));
            self.slots.set(slots);
        }
        slots[node.0 as usize].set(Some(f));
    }
}

impl fmt::Debug for ClosureTable<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let built = self
            .slots
            .get()
            .iter()
            .filter(|s| s.get().is_some())
            .count();
        write!(f, "ClosureTable({built}/{} built)", self.nodes)
    }
}

#[cfg(test)]
mod table_tests {
    use super::*;

    #[test]
    fn linking_reserves_no_native_slots_and_first_use_reuses_stable_storage() {
        let bump = Bump::new();
        let table = ClosureTable::new(&bump, 257);
        assert_eq!(bump.allocated_bytes(), 0);
        assert!(table.get(NodeId(0)).is_none());
        assert!(table.get(NodeId(256)).is_none());
        assert_eq!(format!("{table:?}"), "ClosureTable(0/257 built)");
        assert_eq!(bump.allocated_bytes(), 0);
        fn constant<'a>(bump: &'a Bump, value: i64) -> Eval<'a> {
            bump.alloc(move |_: &Evaluator<'a>, _: Env<'a>| Ok(Value::Int(value)))
        }
        let first = constant(&bump, 1);
        let second = constant(&bump, 2);
        let before = bump.allocated_bytes();
        table.set(NodeId(256), first);
        let after = bump.allocated_bytes();
        assert!(
            after > before,
            "the first native closure allocates its table"
        );
        let slots = table.slots.get().as_ptr();
        table.set(NodeId(1), second);
        assert_eq!(table.slots.get().as_ptr(), slots);
        assert_eq!(bump.allocated_bytes(), after);
        assert!(std::ptr::eq(table.get(NodeId(256)).unwrap(), first));
        assert!(std::ptr::eq(table.get(NodeId(1)).unwrap(), second));
        assert!(table.get(NodeId(0)).is_none());
        assert_eq!(format!("{table:?}"), "ClosureTable(2/257 built)");
    }

    #[test]
    #[should_panic(expected = "out of bounds")]
    fn uninitialized_native_table_preserves_index_bounds() {
        let bump = Bump::new();
        ClosureTable::new(&bump, 2).get(NodeId(2));
    }
}

/// How many levels of strict children call each other's closures directly
/// before one of them checks the stack (see [`Evaluator::compiled`]).
/// Small enough that so many closure frames, even unoptimised ones, take
/// a fraction of the red zone the check keeps, and large enough that
/// hardly any closure of real code needs a check.
const STACK_CHECK_LEVELS: u32 = 8;

/// What `maybe_thunk` does with a node in a lazy position, decided when the
/// parent is compiled: hand out a shared constant cell, share a variable's
/// cell when its slot is filled, or allocate a thunk.
#[derive(Clone, Copy)]
enum Lazy<'a> {
    /// A literal's pre-built cell, or the shared empty list.
    Const(ValueRef<'a>),
    /// A statically bound variable: its cell if the slot is filled, else a
    /// thunk (a `rec` attribute or formal not filled in yet).
    Var {
        level: u32,
        displ: u32,
        node: GlobalNode,
        site: LazySite,
    },
    Thunk {
        node: GlobalNode,
        kind: ThunkKind,
        site: LazySite,
    },
}

impl<'a> Lazy<'a> {
    /// The cell for the node in `env`. Out of line: it is used by every
    /// closure that makes cells (lists, calls, `let`, `with`, attribute
    /// sets), and one shared copy keeps those closures small.
    #[inline(never)]
    fn cell(self, ev: &Evaluator<'a>, env: Env<'a>) -> ValueRef<'a> {
        self.cell_inline(ev, env)
    }

    /// [`Lazy::cell`]'s body, for [`Evaluator::maybe_thunk`], which decides
    /// and makes the cell in one go: inlined there, the plan folds away.
    #[inline(always)]
    fn cell_inline(self, ev: &Evaluator<'a>, env: Env<'a>) -> ValueRef<'a> {
        match self {
            Lazy::Const(c) => c,
            Lazy::Var {
                level,
                displ,
                node,
                site,
            } => {
                let cell = env.up_by(level).slot(displ);
                if !std::ptr::eq(cell, ev.consts.uninit) {
                    cell
                } else {
                    ev.stats.record_thunk(ThunkKind::UninitVar, site);
                    ev.mk_thunk_global(env, node)
                }
            }
            Lazy::Thunk { node, kind, site } => {
                ev.stats.record_thunk(kind, site);
                ev.mk_thunk_global(env, node)
            }
        }
    }
}

impl<'a> Evaluator<'a> {
    /// The closure for `node`, compiling it on first use.
    #[inline]
    pub(crate) fn closure(&self, code: &'a LinkedCode<'a>, node: NodeId) -> Eval<'a> {
        match code.closures.get(node) {
            Some(f) => f,
            None => self.build_closure(code, node),
        }
    }

    #[inline(never)]
    fn build_closure(&self, code: &'a LinkedCode<'a>, node: NodeId) -> Eval<'a> {
        let started = self.timings.then(crate::platform::Instant::now);
        let f = self.compiled(code, node, 0);
        if let Some(t) = started {
            Stats::add_time(&self.stats.closure_build_time, t.elapsed());
        }
        f
    }

    /// The closure for `node`, compiling it (and its strict children)
    /// if needed. Untimed: called while a parent is being compiled.
    ///
    /// `depth` is how many strict parents call this closure directly
    /// before a closure is entered through a stack check: a node compiled
    /// on its own (a thunk's, a function body, a `select … or` default, a
    /// dynamic attribute name) is entered through [`Evaluator::force`],
    /// [`Evaluator::call`] or another [`Evaluator::with_stack`], and its
    /// strict children are compiled with it, one level deeper each. Every
    /// [`STACK_CHECK_LEVELS`]th level gets a stack check of its own, so
    /// deeply nested source (a long `else if` chain, `a + (b + …)`) grows
    /// the stack like deep recursion does, while the closures of the
    /// shallow code nearly all programs consist of have no check at all.
    fn compiled(&self, code: &'a LinkedCode<'a>, node: NodeId, depth: u32) -> Eval<'a> {
        if let Some(f) = code.closures.get(node) {
            return f;
        }
        let f = stacker::maybe_grow(STACK_RED_ZONE, STACK_SEGMENT, || {
            self.compile_node(code, node, depth)
        });
        let f = if depth % STACK_CHECK_LEVELS == 0 && depth > 0 {
            self.mk(move |ev, env| ev.with_stack(|| f(ev, env)))
        } else {
            f
        };
        code.closures.set(node, f);
        f
    }

    /// [`Evaluator::force`] with its common case, an already evaluated
    /// cell, inlined into the caller.
    #[inline(always)]
    fn force_fast(&self, cell: ValueRef<'a>, pos: SrcPos) -> Result<Value<'a>> {
        let v = cell.get();
        if v.is_forced() {
            Ok(v)
        } else {
            self.force(cell, pos)
        }
    }

    /// The `while evaluating the attribute` trace of the select fast path.
    #[cold]
    #[inline(never)]
    fn trace_selected_attr(&self, e: crate::Error, pos: SrcPos, name: Symbol) -> crate::Error {
        if !pos.is_none() && !self.is_internal_pos(pos) {
            self.add_trace(
                &e,
                pos,
                msg!(
                    "while evaluating the attribute '",
                    self.symbol_ident(name),
                    "'"
                ),
            );
        }
        e
    }

    /// Moves a closure into the arena.
    fn mk<F>(&self, f: F) -> Eval<'a>
    where
        F: Fn(&Evaluator<'a>, Env<'a>) -> Result<Value<'a>> + crate::runtime::ThreadSafe + 'a,
    {
        Stats::bump(&self.stats.closures_built);
        Stats::add(&self.stats.closure_bytes, std::mem::size_of::<F>() as u64);
        self.code_bump.alloc(f)
    }

    /// What `maybe_thunk(code, node, env, site)` will do, decided now.
    #[inline]
    fn lazy(&self, code: &'a LinkedCode<'a>, node: NodeId, site: LazySite) -> Lazy<'a> {
        let global = GlobalNode(code.node_base + node.0);
        match *code.node(node) {
            Node::Int(_) | Node::Float(_) | Node::Str(_) | Node::Path { .. } => {
                Lazy::Const(code.consts[node.0 as usize].expect("literal constant"))
            }
            Node::Var {
                var: VarRef::Env { level, displ },
                ..
            } => Lazy::Var {
                level,
                displ,
                node: global,
                site,
            },
            Node::List { elems } if elems.is_empty() => Lazy::Const(self.consts.empty_list),
            ref n => Lazy::Thunk {
                node: global,
                kind: ThunkKind::of(n),
                site,
            },
        }
    }

    /// `Expr::maybeThunk`: a cell for a sub-expression in the lazy position
    /// `site`, avoiding a thunk for literals and for variables whose cell
    /// already exists. The same decision as [`Evaluator::lazy`], made and
    /// carried out at once, for the code that is not compiled into
    /// closures (formal defaults, `inherit (from)` sources, sets with
    /// dynamic attributes).
    #[inline]
    pub(crate) fn maybe_thunk(
        &self,
        code: &'a LinkedCode<'a>,
        node: NodeId,
        env: Env<'a>,
        site: LazySite,
    ) -> ValueRef<'a> {
        self.lazy(code, node, site).cell_inline(self, env)
    }

    fn lazies(&self, code: &'a LinkedCode<'a>, nodes: &[NodeId], site: LazySite) -> &'a [Lazy<'a>] {
        let v: Vec<Lazy<'a>> = nodes.iter().map(|&n| self.lazy(code, n, site)).collect();
        self.code_bump.alloc_slice_copy(&v)
    }

    fn compile_node(&self, code: &'a LinkedCode<'a>, node: NodeId, depth: u32) -> Eval<'a> {
        let sp = |pos: Pos| self.src_pos(code, pos);
        // The closure of a strict child, called directly from this one.
        let strict = |child: NodeId| self.compiled(code, child, depth + 1);
        match *code.node(node) {
            Node::Int(_) | Node::Float(_) | Node::Str(_) | Node::Path { .. } => {
                let v = code.consts[node.0 as usize]
                    .expect("literal has a constant")
                    .get();
                self.mk(move |_, _| Ok(v))
            }
            Node::Var {
                pos,
                var: VarRef::Env { level, displ },
                ..
            } => {
                let p = sp(pos);
                match level {
                    0 => self.mk(move |ev, env| ev.force_fast(env.slot(displ), p)),
                    1 => self.mk(move |ev, env| {
                        let up = env
                            .up()
                            .expect("environment chain shorter than resolved level");
                        ev.force_fast(up.slot(displ), p)
                    }),
                    _ => self.mk(move |ev, env| ev.force_fast(env.up_by(level).slot(displ), p)),
                }
            }
            Node::Var { pos, name, var } => {
                let p = sp(pos);
                self.mk(move |ev, env| {
                    let cell = ev
                        .lookup_var(code, env, name, var, pos, false)?
                        .expect("eval lookup");
                    ev.force_fast(cell, p)
                })
            }
            // Level 0 of the environment the inherit sources live in.
            Node::InheritFrom { displ, .. } => {
                self.mk(move |ev, env| ev.force_fast(env.slot(displ), SrcPos::NONE))
            }
            Node::Select {
                pos,
                subject,
                path,
                default,
            } => {
                let s = strict(subject);
                match (code.attr_names_in(path), default) {
                    // `x.name`, the commonest select: one static name, no
                    // `or`. The successful path is inlined; anything that
                    // fails re-enters the general code, which produces the
                    // error and trace (`select_from` on a non-set or a
                    // missing name forces nothing).
                    (&[crate::ir::AttrName::Static(name)], None) => {
                        let p = sp(pos);
                        self.mk(move |ev, env| {
                            let v = s(ev, env)?;
                            let attr = match v {
                                Value::Attrs(b) => b.get(name),
                                _ => None,
                            };
                            let Some(attr) = attr else {
                                return ev.select_from(code, env, v, pos, path, None);
                            };
                            let pos2 = attr.pos;
                            ev.force_fast(attr.value, pos2.or(p))
                                .map_err(|e| ev.trace_selected_attr(e, pos2, name))
                        })
                    }
                    _ => self.mk(move |ev, env| {
                        let v = s(ev, env)?;
                        ev.select_from(code, env, v, pos, path, default)
                    }),
                }
            }
            Node::HasAttr { subject, path } => {
                let s = strict(subject);
                let getpos = sp(code.pos_of(node));
                self.mk(move |ev, env| {
                    let v = s(ev, env)?;
                    ev.has_attr_from(code, env, v, getpos, path)
                })
            }
            Node::Attrs(id) => self.compile_attrs(code, id),
            Node::List { elems } => {
                let lazies = self.lazies(code, code.nodes_in(elems), LazySite::ListElem);
                self.mk(move |ev, env| {
                    Ok(ev.mk_list_from_iter(lazies.len(), lazies.iter().map(|l| l.cell(ev, env))))
                })
            }
            Node::Lambda(_) => {
                // Inline in the value, like a thunk: no allocation.
                let node = GlobalNode(code.node_base + node.0);
                self.mk(move |_, env| Ok(Value::Lambda { env, node }))
            }
            Node::Call { pos, fun, args } => {
                let f = strict(fun);
                let p = sp(pos);
                let lazies = self.lazies(code, code.nodes_in(args), LazySite::CallArg);
                // The function is evaluated before the argument cells are
                // made, as in `ExprCall::eval`.
                match *lazies {
                    [a] => self.mk(move |ev, env| {
                        let fv = f(ev, env)?;
                        ev.call(fv, &[a.cell(ev, env)], p)
                    }),
                    [a, b] => self.mk(move |ev, env| {
                        let fv = f(ev, env)?;
                        ev.call(fv, &[a.cell(ev, env), b.cell(ev, env)], p)
                    }),
                    _ => self.mk(move |ev, env| {
                        let fv = f(ev, env)?;
                        let cells: SmallVec<[ValueRef<'a>; 4]> =
                            lazies.iter().map(|l| l.cell(ev, env)).collect();
                        ev.call(fv, &cells, p)
                    }),
                }
            }
            Node::Let { attrs, body } => {
                let set = code.attr_set(attrs);
                let defs = self.bindings(code, set, LazySite::LetBinding);
                let size = set.attrs.len();
                let b = strict(body);
                self.mk(move |ev, env| {
                    let env2 = ev.alloc_env(Some(env), size);
                    let inherit_env = ev.build_inherit_env(code, set, env2);
                    // Slots are filled in symbol order, as `ExprLet::eval`
                    // walks its `std::map`.
                    for &(i, kind, lazy) in defs {
                        let e = kind.choose(env2, env, inherit_env);
                        env2.set_slot(i as usize, lazy.cell(ev, e));
                    }
                    b(ev, env2)
                })
            }
            Node::With { attrs, body, .. } => {
                let a = self.lazy(code, attrs, LazySite::WithSet);
                let b = strict(body);
                self.mk(move |ev, env| {
                    let env2 = ev.alloc_env(Some(env), 1);
                    env2.set_slot(0, a.cell(ev, env));
                    b(ev, env2)
                })
            }
            Node::If {
                pos,
                cond,
                then_,
                else_,
            } => {
                let p = sp(pos);
                let c = strict(cond);
                let t = strict(then_);
                let e = strict(else_);
                self.mk(move |ev, env| {
                    if ev.bool_from(|| c(ev, env), p, "while evaluating a branch condition")? {
                        t(ev, env)
                    } else {
                        e(ev, env)
                    }
                })
            }
            Node::Not(e) => {
                let p = sp(code.pos_of(e));
                let c = strict(e);
                self.mk(move |ev, env| {
                    Ok(Value::Bool(!ev.bool_from(
                        || c(ev, env),
                        p,
                        "in the argument of the not operator",
                    )?))
                })
            }
            Node::BinOp { pos, op, lhs, rhs } => {
                self.compile_binop(code, pos, op, strict(lhs), strict(rhs))
            }
            Node::ConcatStrings {
                pos,
                force_string,
                parts,
            } => {
                let cs: Vec<Eval<'a>> = code
                    .parts_in(parts)
                    .iter()
                    .map(|&(_, n)| strict(n))
                    .collect();
                let cs = self.code_bump.alloc_slice_copy(&cs);
                self.mk(move |ev, env| {
                    ev.concat_strings_with(code, pos, force_string, parts, |i| cs[i](ev, env))
                })
            }
            Node::CurPos(pos) => {
                let p = sp(pos);
                self.mk(move |ev, _| ev.mk_pos_value(p))
            }
            Node::Assert { pos, cond, body } => {
                let p = sp(pos);
                let c = strict(cond);
                let b = strict(body);
                self.mk(move |ev, env| {
                    if !ev.bool_from(
                        || c(ev, env),
                        p,
                        "in the condition of the assert statement",
                    )? {
                        return Err(ev.assert_failure(code, env, cond, p));
                    }
                    b(ev, env)
                })
            }
        }
    }

    /// The static bindings of a set or `let` in symbol-id order (the order
    /// Nix fills them in), each with its displacement and lazy plan.
    fn bindings(
        &self,
        code: &'a LinkedCode<'a>,
        set: &'a crate::code::AttrSet<'a>,
        site: LazySite,
    ) -> &'a [(u32, AttrKind, Lazy<'a>)] {
        let v: Vec<(u32, AttrKind, Lazy<'a>)> = set
            .in_id_order()
            .map(|(i, def)| (i as u32, def.kind, self.lazy(code, def.expr, site)))
            .collect();
        self.code_bump.alloc_slice_copy(&v)
    }

    /// `ExprAttrs::eval`. Sets with dynamic attributes or `__overrides` go
    /// through the general path in [`crate::eval`]; the static case, nearly
    /// every set in nixpkgs, is compiled.
    fn compile_attrs(&self, code: &'a LinkedCode<'a>, id: crate::ir::AttrsId) -> Eval<'a> {
        let set = code.attr_set(id);
        let overrides = set.recursive && set.find(sym::OVERRIDES).is_some();
        if !set.dynamic_attrs.is_empty() || overrides {
            return self.mk(move |ev, env| ev.eval_attrs(code, env, id));
        }
        let set_pos = self.src_pos(code, set.pos);
        let defs = self.bindings(code, set, LazySite::AttrValue);
        // Each attribute of the result in canonical (symbol) order: its
        // name, position and displacement.
        let meta: Vec<(Symbol, SrcPos, u32)> = set
            .id_order
            .iter()
            .map(|&i| {
                let def = &set.attrs[i as usize];
                (def.name, self.src_pos(code, def.pos), i)
            })
            .collect();
        let meta = self.code_bump.alloc_slice_copy(&meta);
        let size = set.attrs.len();
        let recursive = set.recursive;
        self.mk(move |ev, env| {
            let env2 = if recursive {
                ev.alloc_env(Some(env), size)
            } else {
                env
            };
            let inherit_env = ev.build_inherit_env(code, set, env2);
            // The cells are made in symbol order, as Nix fills a `rec`
            // environment (observable through `maybe_thunk` sharing).
            let mut cells: SmallVec<[ValueRef<'a>; 32]> =
                SmallVec::from_elem(ev.consts.uninit, size);
            for &(i, kind, lazy) in defs {
                let cell = lazy.cell(ev, kind.choose(env2, env, inherit_env));
                if recursive {
                    env2.set_slot(i as usize, cell);
                }
                cells[i as usize] = cell;
            }
            Ok(ev.mk_attrs_from_fn(size, set_pos, |k| {
                let (name, pos, i) = meta[k];
                Attr::new(name, pos, cells[i as usize])
            }))
        })
    }

    /// A binary operator, given its operands' closures.
    fn compile_binop(
        &self,
        code: &'a LinkedCode<'a>,
        pos: Pos,
        op: BinOp,
        l: Eval<'a>,
        r: Eval<'a>,
    ) -> Eval<'a> {
        let p = self.src_pos(code, pos);
        match op {
            // Nix builds `ExprOpEq` without a position.
            BinOp::Eq => self.mk(move |ev, env| {
                let v1 = l(ev, env)?;
                let v2 = r(ev, env)?;
                Ok(Value::Bool(ev.eq_values_v(
                    v1,
                    v2,
                    SrcPos::NONE,
                    "while testing two values for equality",
                )?))
            }),
            BinOp::NEq => self.mk(move |ev, env| {
                let v1 = l(ev, env)?;
                let v2 = r(ev, env)?;
                Ok(Value::Bool(!ev.eq_values_v(
                    v1,
                    v2,
                    SrcPos::NONE,
                    "while testing two values for inequality",
                )?))
            }),
            BinOp::And => self.mk(move |ev, env| {
                Ok(Value::Bool(
                    ev.bool_from(
                        || l(ev, env),
                        p,
                        "in the left operand of the AND (&&) operator",
                    )? && ev.bool_from(
                        || r(ev, env),
                        p,
                        "in the right operand of the AND (&&) operator",
                    )?,
                ))
            }),
            BinOp::Or => self.mk(move |ev, env| {
                Ok(Value::Bool(
                    ev.bool_from(
                        || l(ev, env),
                        p,
                        "in the left operand of the OR (||) operator",
                    )? || ev.bool_from(
                        || r(ev, env),
                        p,
                        "in the right operand of the OR (||) operator",
                    )?,
                ))
            }),
            BinOp::Impl => self.mk(move |ev, env| {
                Ok(Value::Bool(
                    !ev.bool_from(
                        || l(ev, env),
                        p,
                        "in the left operand of the IMPL (->) operator",
                    )? || ev.bool_from(
                        || r(ev, env),
                        p,
                        "in the right operand of the IMPL (->) operator",
                    )?,
                ))
            }),
            BinOp::Update => self.mk(move |ev, env| {
                let v1 = ev.attrs_from(
                    || l(ev, env),
                    p,
                    "in the left operand of the update (//) operator",
                )?;
                let v2 = ev.attrs_from(
                    || r(ev, env),
                    p,
                    "in the right operand of the update (//) operator",
                )?;
                Ok(ev.update_attrs(v1, v2))
            }),
            BinOp::ConcatLists => self.mk(move |ev, env| {
                let v1 = l(ev, env)?;
                let v2 = r(ev, env)?;
                let a = ev.alloc_cell(v1);
                let b = ev.alloc_cell(v2);
                ev.concat_lists(
                    &[a, b],
                    p,
                    "while evaluating one of the elements to concatenate",
                )
            }),
        }
    }
}
