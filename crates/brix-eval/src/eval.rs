//! Evaluation proper: expressions, thunks, function calls.
//!
//! This follows `eval.cc` closely; the comments name the C++ function each
//! piece mirrors so behaviour can be checked against it.

use smallvec::SmallVec;

use crate::code::LinkedCode;
use crate::coerce::Coercion;
use crate::error::{msg, ErrorKind};
use crate::evaluator::Evaluator;
use crate::ir::{AttrKind, AttrName, BinOp, Node, NodeId, Pos, VarRef};
use crate::stats::{LazySite, ThunkKind};
use crate::string_context::StringContext;
use crate::symbol::{sym, Symbol};
use crate::value::{
    Attr, Bindings, ContextElem, Env, GlobalNode, SrcPos, Value, ValueRef, ValueType,
};
use crate::Result;

impl<'a> Evaluator<'a> {
    /// Evaluates `node` in `env` to weak head normal form, through the
    /// node's compiled closure (see `crate::closure`).
    ///
    /// The stack check lives in [`Evaluator::force`] and
    /// [`Evaluator::call`] rather than here: unbounded run-time recursion
    /// always goes through a thunk or a function call. Recursion through
    /// nested closures alone is bounded by the nesting of the source, but
    /// that can be deep too (a generated `else if` chain); the closure
    /// compiler gives every eighth level of such nesting a check of its
    /// own (`Evaluator::compiled` in `closure.rs`), and the places that
    /// enter a separately compiled node other than through a thunk or a
    /// call use `eval_checked`. Checking on every node cost
    /// about 3% of a nixpkgs evaluation.
    ///
    /// The remaining checks (about 2.9 million for firefox.drvPath) are an
    /// inline compare against a window of the current stack segment
    /// (`Evaluator::with_stack` in `stack.rs`). Against calling
    /// `stacker::maybe_grow` every time, that took 1.6-2.9% off the
    /// instructions and 1.3-2.8% off the in-process firefox and NixOS
    /// cycles and wall time; CLI runs were within noise. (An earlier
    /// attempt, before closure compilation, measured 3-5% slower.)
    #[inline]
    pub fn eval_node(
        &self,
        code: &'a LinkedCode<'a>,
        node: NodeId,
        env: Env<'a>,
    ) -> Result<Value<'a>> {
        (self.closure(code, node))(self, env)
    }

    /// [`Evaluator::eval_node`] behind a stack check, for a node compiled on
    /// its own that is entered neither by forcing a thunk nor by calling a
    /// function (a `select … or` default, a dynamic attribute name): its
    /// closure relies on its caller for the check, as a thunk's relies on
    /// [`Evaluator::force`].
    pub(crate) fn eval_checked(
        &self,
        code: &'a LinkedCode<'a>,
        node: NodeId,
        env: Env<'a>,
    ) -> Result<Value<'a>> {
        self.with_stack(|| self.eval_node(code, node, env))
    }

    /// The error of a failed `assert cond; …` (`ExprAssert::eval`): for an
    /// `==` condition, the operands are evaluated again and compared with
    /// `assertEqValues` to explain the difference.
    #[cold]
    #[inline(never)]
    pub(crate) fn assert_failure(
        &self,
        code: &'a LinkedCode<'a>,
        env: Env<'a>,
        cond: NodeId,
        p: SrcPos,
    ) -> crate::Error {
        let expr_str = crate::show::show_node(self, code, cond);
        if let Node::BinOp {
            op: BinOp::Eq,
            lhs,
            rhs,
            ..
        } = *code.node(cond)
        {
            let v1 = match self.eval_node(code, lhs, env) {
                Ok(v) => v,
                Err(e) => return e,
            };
            let v2 = match self.eval_node(code, rhs, env) {
                Ok(v) => v,
                Err(e) => return e,
            };
            let a = self.alloc_cell(v1);
            let b = self.alloc_cell(v2);
            if let Err(e) = self.assert_eq_values(a, b, SrcPos::NONE, "in an equality assertion") {
                if e.kind == ErrorKind::Assertion {
                    self.add_trace(
                        &e,
                        p,
                        msg!(
                            "while evaluating the condition of the assertion '",
                            expr_str,
                            "'"
                        ),
                    );
                }
                return e;
            }
        }
        self.error_at(
            ErrorKind::Assertion,
            msg!("assertion '", expr_str, "' failed"),
            p,
        )
    }

    /// `ExprOpUpdate::eval`: the attributes of both sets, the right one
    /// winning on a shared name. See [`Bindings::update_counted`] for how large
    /// sets avoid copying.
    ///
    /// Out of line: inlined into the `//` closure, the merge machinery made
    /// that closure's frame 648 bytes, paid at every level of evaluation
    /// that recurses through an update's operands.
    #[inline(never)]
    pub fn update_attrs(&self, v1: Bindings<'a>, v2: Bindings<'a>) -> Value<'a> {
        if v1.is_empty() {
            return Value::Attrs(v2);
        }
        if v2.is_empty() {
            return Value::Attrs(v1);
        }
        self.stats.update_left_sizes.record(v1.len());
        self.stats.update_right_sizes.record(v2.len());
        let (merged, written) = Bindings::update_counted(self.bump, v1, v2);
        crate::stats::Stats::add_work(&self.stats.update_attrs_copied, written as u64);
        self.mk_bindings(merged)
    }

    /// `EvalState::concatLists`.
    pub fn concat_lists(
        &self,
        lists: &[ValueRef<'a>],
        pos: SrcPos,
        ctx: &str,
    ) -> Result<Value<'a>> {
        let mut total = 0usize;
        let mut non_empty: Option<Value<'a>> = None;
        let mut forced = Vec::with_capacity(lists.len());
        for &l in lists {
            let v = self.force_list(l, pos, ctx)?;
            let elems = v.as_list().unwrap();
            total += elems.len();
            if !elems.is_empty() {
                non_empty = Some(v);
            }
            forced.push(elems);
        }
        if let Some(v) = non_empty {
            if v.as_list().unwrap().len() == total {
                return Ok(v);
            }
        }
        let mut out = Vec::with_capacity(total);
        for elems in forced {
            out.extend_from_slice(elems);
        }
        Ok(self.mk_list(&out))
    }

    /// `ExprConcatStrings::eval`, with the evaluation of part `i` supplied
    /// by the caller: the compiled closure calls the part's closure.
    pub(crate) fn concat_strings_with(
        &self,
        code: &'a LinkedCode<'a>,
        pos: Pos,
        force_string: bool,
        parts: crate::ir::Range,
        mut eval_part: impl FnMut(usize) -> Result<Value<'a>>,
    ) -> Result<Value<'a>> {
        let mut context = StringContext::new();
        // Coerced parts borrow the arena (a string's own bytes, or bytes
        // the coercion allocated); they are written once into the result
        // after the total length is known.
        let mut strings: SmallVec<[&'a [u8]; 8]> = SmallVec::new();
        // Lazy hashes in the result (see `crate::lazy_tree`), by offset.
        let mut lazy: SmallVec<[crate::lazy_tree::LazyHash; 1]> = SmallVec::new();
        // The value itself when the whole concatenation is one string
        // (`"${s}"`): Nix builds an equal string; returning this one is
        // indistinguishable and allocates nothing.
        let mut only_string: Option<Value<'a>> = None;
        let mut n: i64 = 0;
        let mut nf: f64 = 0.0;
        let mut first = !force_string;
        let mut first_type = ValueType::String;
        let parts = code.parts_in(parts);
        for (i, &(ipos, _)) in parts.iter().enumerate() {
            let v = eval_part(i)?;
            let ipos = self.src_pos(code, ipos);
            if first {
                first_type = v.value_type();
            }
            if first_type == ValueType::Int {
                match v {
                    Value::Int(i) => match n.checked_add(i) {
                        Some(sum) => n = sum,
                        None => {
                            return Err(self.error_at(
                                ErrorKind::Eval,
                                format!("integer overflow in adding {n} + {i}"),
                                ipos,
                            ))
                        }
                    },
                    Value::Float(f) => {
                        first_type = ValueType::Float;
                        nf = n as f64 + f;
                    }
                    _ => {
                        return Err(self.error_at(
                            ErrorKind::Eval,
                            format!("cannot add {} to an integer", v.show_type()),
                            ipos,
                        ))
                    }
                }
            } else if first_type == ValueType::Float {
                match v {
                    Value::Int(i) => nf += i as f64,
                    Value::Float(f) => nf += f,
                    _ => {
                        return Err(self.error_at(
                            ErrorKind::Eval,
                            format!("cannot add {} to a float", v.show_type()),
                            ipos,
                        ))
                    }
                }
            } else {
                let part = match v {
                    // `path + …` stays a path in the same namespace: a path
                    // into a lazy tree keeps its placeholder rather than
                    // being turned into text (which needs the store path).
                    Value::Path(p) if first => p.path,
                    // A string with lazy hashes keeps them lazy, at their
                    // new offsets (and a path made of one may be a path
                    // into the lazy tree: `concat_path_text`).
                    Value::String(s)
                        if s.is_lazy()
                            && matches!(first_type, ValueType::String | ValueType::Path) =>
                    {
                        let base: usize = strings.iter().map(|s| s.len()).sum();
                        lazy.extend(crate::lazy_tree::lazy_hashes(s).map(|h| {
                            crate::lazy_tree::LazyHash {
                                offset: base + h.offset,
                                tree: h.tree,
                            }
                        }));
                        context.extend_from_slice(crate::lazy_tree::context_without_hashes(s));
                        s.raw_bytes()
                    }
                    _ => self.coerce_value_to_string(
                        ipos,
                        v,
                        &mut context,
                        "while evaluating a path segment",
                        Coercion {
                            more: false,
                            copy_to_store: first_type == ValueType::String,
                            canonicalize: !first,
                        },
                    )?,
                };
                if parts.len() == 1 && matches!(v, Value::String(_)) {
                    only_string = Some(v);
                }
                strings.push(part);
            }
            first = false;
        }
        let p = self.src_pos(code, pos);
        match first_type {
            ValueType::Int => Ok(Value::Int(n)),
            ValueType::Float => Ok(Value::Float(nf)),
            ValueType::Path => {
                if context.has_real() {
                    return Err(self.error_at(
                        ErrorKind::Eval,
                        "a string that refers to a store path cannot be appended to a path",
                        p,
                    ));
                }
                // `rootPath(CanonPath(str()))`, the bytes as they are.
                let mut joined: Vec<u8> = strings.concat();
                if !lazy.is_empty() {
                    joined = self.concat_path_text(&joined, &lazy)?;
                }
                let canon = self.canon_path(&joined)?;
                Ok(self.mk_root_path(&canon))
            }
            _ => {
                if let Some(v) = only_string {
                    return Ok(v);
                }
                for h in &lazy {
                    context.insert(ContextElem::LazyHash {
                        offset: h.offset as u32,
                        tree: h.tree,
                    });
                }
                let ctx = context.finish(self.bump);
                if let [single] = strings[..] {
                    return Ok(self.mk_string_view(single, ctx));
                }
                let total: usize = strings.iter().map(|s| s.len()).sum();
                if total == 0 && ctx.is_empty() {
                    return Ok(self.consts.empty_string.get());
                }
                let bytes = self.bump.alloc_slice_fill_copy(total, 0u8);
                let mut offset = 0;
                for s in &strings {
                    bytes[offset..offset + s.len()].copy_from_slice(s);
                    offset += s.len();
                }
                Ok(self.mk_string_view(bytes, ctx))
            }
        }
    }

    /// `ExprSelect::eval` after the subject has been evaluated to `v`.
    pub(crate) fn select_from(
        &self,
        code: &'a LinkedCode<'a>,
        env: Env<'a>,
        mut v: Value<'a>,
        pos: Pos,
        path: crate::ir::Range,
        default: Option<NodeId>,
    ) -> Result<Value<'a>> {
        let p = self.src_pos(code, pos);
        let mut pos2 = SrcPos::NONE;
        let names = code.attr_names_in(path);
        let result: Result<Value<'a>> = (|| {
            // Like `ExprSelect::eval`: a selected value is forced at the
            // *start* of the next step with the select's own position, and
            // only the last one with its attribute's position (`pos2`), so
            // an infinite recursion in `x.a.b` is reported at `x`.
            let mut pending: Option<ValueRef<'a>> = None;
            for name in names.iter() {
                if let Some(cell) = pending.take() {
                    v = self.force(cell, p)?;
                }
                let name = self.attr_name(code, env, *name)?;
                let attr = if let Some(def) = default {
                    match v {
                        Value::Attrs(b) => match b.get(name) {
                            Some(a) => a,
                            None => return self.eval_checked(code, def, env),
                        },
                        _ => return self.eval_checked(code, def, env),
                    }
                } else {
                    let b = self.expect_attrs(v, p, "while selecting an attribute")?;
                    match b.get(name) {
                        Some(a) => a,
                        None => return Err(self.missing_attr_error(b, name, p)),
                    }
                };
                pos2 = attr.pos;
                pending = Some(attr.value);
            }
            match pending {
                Some(cell) => self.force(cell, pos2.or(p)),
                None => Ok(v),
            }
        })();
        match result {
            Ok(v) => Ok(v),
            Err(e) => Err(self.select_failed(code, env, names, pos2, e)),
        }
    }

    /// `attribute 'x' missing`, with Nix's suggestions of similar names.
    #[cold]
    #[inline(never)]
    fn missing_attr_error(&self, b: Bindings<'a>, name: Symbol, p: SrcPos) -> crate::Error {
        let e = self.error_at(
            ErrorKind::Eval,
            msg!("attribute '", self.symbol_ident(name), "' missing"),
            p,
        );
        e.set_suggestions(crate::error::best_suggestions_bytes(
            b.iter().map(|a| self.symbol_name(a.name)),
            self.symbol_name(name),
        ));
        e
    }

    /// Adds `while evaluating the attribute 'a.b'` to an error raised while
    /// selecting, unless the attribute has no (or an internal) position.
    #[cold]
    #[inline(never)]
    fn select_failed(
        &self,
        code: &'a LinkedCode<'a>,
        env: Env<'a>,
        names: &[AttrName],
        pos2: SrcPos,
        e: crate::Error,
    ) -> crate::Error {
        if pos2.is_none() || self.is_internal_pos(pos2) {
            return e;
        }
        let shown = self.show_attr_selection_path(code, env, names);
        self.add_trace(
            &e,
            pos2,
            msg!("while evaluating the attribute '", shown, "'"),
        );
        e
    }

    /// `showAttrSelectionPath`: names joined with dots; a dynamic name
    /// that fails to evaluate is shown as its source.
    fn show_attr_selection_path(
        &self,
        code: &'a LinkedCode<'a>,
        env: Env<'a>,
        names: &[AttrName],
    ) -> Vec<u8> {
        let mut out = Vec::new();
        for (i, name) in names.iter().enumerate() {
            if i > 0 {
                out.push(b'.');
            }
            match self.attr_name(code, env, *name) {
                Ok(s) => out.extend_from_slice(&self.symbol_ident(s)),
                Err(_) => {
                    if let AttrName::Dynamic(e) = name {
                        out.extend_from_slice(b"\"${");
                        out.extend_from_slice(&crate::show::show_node(self, code, *e));
                        out.extend_from_slice(b"}\"");
                    }
                }
            }
        }
        out
    }

    /// `ExprOpHasAttr::eval` after the subject has been evaluated to `v`.
    pub(crate) fn has_attr_from(
        &self,
        code: &'a LinkedCode<'a>,
        env: Env<'a>,
        mut v: Value<'a>,
        getpos: SrcPos,
        path: crate::ir::Range,
    ) -> Result<Value<'a>> {
        let names = code.attr_names_in(path);
        for (i, name) in names.iter().enumerate() {
            let name = self.attr_name(code, env, *name)?;
            match v {
                Value::Attrs(b) => match b.get(name) {
                    // Only the sets along the way are forced; the value of
                    // the last attribute is never looked at
                    // (`rec { f = f; } ? f` is `true`).
                    Some(attr) if i + 1 < names.len() => {
                        v = self.force(attr.value, getpos)?;
                    }
                    Some(_) => {}
                    None => return Ok(Value::Bool(false)),
                },
                _ => return Ok(Value::Bool(false)),
            }
        }
        Ok(Value::Bool(true))
    }

    /// `getName`: the symbol of a static or dynamic attribute name.
    pub fn attr_name(
        &self,
        code: &'a LinkedCode<'a>,
        env: Env<'a>,
        name: AttrName,
    ) -> Result<Symbol> {
        match name {
            AttrName::Static(s) => Ok(s),
            AttrName::Dynamic(e) => {
                let v = self.eval_checked(code, e, env)?;
                let cell = self.alloc_cell(v);
                let s = self.force_string_no_ctx(
                    cell,
                    self.src_pos(code, code.pos_of(e)),
                    "while evaluating an attribute name",
                )?;
                Ok(self.intern(s))
            }
        }
    }

    /// `ExprAttrs::buildInheritFromEnv`.
    #[inline(never)]
    pub(crate) fn build_inherit_env(
        &self,
        code: &'a LinkedCode<'a>,
        set: &'a crate::code::AttrSet<'a>,
        up: Env<'a>,
    ) -> Option<Env<'a>> {
        if set.inherit_from.is_empty() {
            return None;
        }
        let env = self.alloc_env(Some(up), set.inherit_from.len());
        for (i, &from) in set.inherit_from.iter().enumerate() {
            env.set_slot(
                i,
                self.maybe_thunk(code, from, up, LazySite::InheritFromSource),
            );
        }
        Some(env)
    }

    /// `ExprAttrs::eval` for the sets the closure compiler leaves to it
    /// (`compile_attrs` in `closure.rs`): those with dynamic attributes or a
    /// `rec` set's `__overrides`. Static sets, nearly all of them, are
    /// compiled.
    pub(crate) fn eval_attrs(
        &self,
        code: &'a LinkedCode<'a>,
        env: Env<'a>,
        id: crate::ir::AttrsId,
    ) -> Result<Value<'a>> {
        let set = code.attr_set(id);
        let set_pos = self.src_pos(code, set.pos);
        let overrides = if set.recursive {
            set.find(sym::OVERRIDES).map(|d| d.displ)
        } else {
            None
        };
        let mut bindings: Vec<Attr<'a>> =
            Vec::with_capacity(set.attrs.len() + set.dynamic_attrs.len());
        let mut dynamic_env = env;
        if set.recursive {
            let env2 = self.alloc_env(Some(env), set.attrs.len());
            dynamic_env = env2;
            let inherit_env = self.build_inherit_env(code, set, env2);
            // `bindings[k]` is the attribute whose displacement is
            // `set.id_order[k]`: the static part is pushed in symbol order.
            for (i, def) in set.in_id_order() {
                let e = def.kind.choose(env2, env, inherit_env);
                let cell = if overrides.is_some() && def.kind != AttrKind::Inherited {
                    self.mk_thunk_at(code, def.expr, e, LazySite::RecOverrideValue)
                } else {
                    self.maybe_thunk(code, def.expr, e, LazySite::AttrValue)
                };
                env2.set_slot(i, cell);
                bindings.push(Attr::new(def.name, self.src_pos(code, def.pos), cell));
            }
            if let Some(displ) = overrides {
                let index_of = |displ: u32| {
                    set.id_order
                        .iter()
                        .position(|&d| d == displ)
                        .expect("every displacement appears in id_order")
                };
                let ov = bindings[index_of(displ)].value;
                let ovv = self.force_attrs_by(
                    ov,
                    |v| self.determine_pos(v, SrcPos::NONE),
                    "while evaluating the `__overrides` attribute",
                )?;
                for attr in ovv.iter() {
                    match set.find(attr.name) {
                        Some(def) => {
                            bindings[index_of(def.displ)] = *attr;
                            env2.set_slot(def.displ as usize, attr.value);
                        }
                        None => bindings.push(*attr),
                    }
                }
            }
        } else {
            let inherit_env = self.build_inherit_env(code, set, env);
            for (_, def) in set.in_id_order() {
                let e = def.kind.choose(env, env, inherit_env);
                bindings.push(Attr::new(
                    def.name,
                    self.src_pos(code, def.pos),
                    self.maybe_thunk(code, def.expr, e, LazySite::AttrValue),
                ));
            }
        }
        for dyn_attr in set.dynamic_attrs {
            let ipos = self.src_pos(code, dyn_attr.pos);
            let name_v = self.eval_checked(code, dyn_attr.name, dynamic_env)?;
            if matches!(name_v, Value::Null) {
                continue;
            }
            let cell = self.alloc_cell(name_v);
            let name_s = self.force_string_no_ctx(
                cell,
                ipos,
                "while evaluating the name of a dynamic attribute",
            )?;
            let name = self.intern(name_s);
            // The static part is in symbol order (the compiler's); a
            // duplicate is detected by name regardless of order.
            if let Some(prev) = bindings.iter().find(|a| a.name == name) {
                let prev = prev.pos;
                return Err(self.error_at(
                    ErrorKind::Eval,
                    msg!(
                        "dynamic attribute '",
                        self.symbol_ident(name),
                        "' already defined at ",
                        self.position(prev).map(|p| p.display()).unwrap_or_default()
                    ),
                    ipos,
                ));
            }
            bindings.push(Attr::new(
                name,
                ipos,
                self.maybe_thunk(code, dyn_attr.value, dynamic_env, LazySite::DynAttrValue),
            ));
        }
        // `bindings` is in symbol order (static part) followed by overrides
        // and dynamic attributes. Names are unique by construction, so
        // this is a plain sort into canonical order.
        bindings.sort_unstable_by_key(|a| a.name);
        Ok(self.mk_attrs_sorted(&bindings, set_pos))
    }

    /// `mk_thunk` plus the thunk-kind and lazy-site statistics.
    fn mk_thunk_at(
        &self,
        code: &'a LinkedCode<'a>,
        node: NodeId,
        env: Env<'a>,
        site: LazySite,
    ) -> ValueRef<'a> {
        self.stats
            .record_thunk(ThunkKind::of(code.node(node)), site);
        self.mk_thunk(code, node, env)
    }

    /// `EvalState::lookupVar`. With `no_eval`, a `with`-bound variable
    /// yields `None` instead of forcing the `with` set.
    pub fn lookup_var(
        &self,
        code: &'a LinkedCode<'a>,
        env: Env<'a>,
        name: Symbol,
        var: VarRef,
        pos: Pos,
        no_eval: bool,
    ) -> Result<Option<ValueRef<'a>>> {
        match var {
            VarRef::Env { level, displ } => Ok(Some(env.up_by(level).slot(displ))),
            VarRef::With { level, with } => {
                if no_eval {
                    return Ok(None);
                }
                let mut env = env.up_by(level);
                let mut with_node = with;
                loop {
                    let (wpos, prev_with, parent_with) = match *code.node(with_node) {
                        Node::With {
                            pos,
                            prev_with,
                            parent_with,
                            ..
                        } => (pos, prev_with, parent_with),
                        _ => unreachable!("with reference to a non-with node"),
                    };
                    let set = self.force_attrs(
                        env.slot(0),
                        self.src_pos(code, wpos),
                        "while evaluating the first subexpression of a with expression",
                    )?;
                    if let Some(attr) = set.get(name) {
                        return Ok(Some(attr.value));
                    }
                    match parent_with {
                        None => {
                            return Err(self.error_at(
                                ErrorKind::UndefinedVar,
                                msg!("undefined variable '", self.symbol_ident(name), "'"),
                                self.src_pos(code, pos),
                            ))
                        }
                        Some(parent) => {
                            env = env.up_by(prev_with);
                            with_node = parent;
                        }
                    }
                }
            }
        }
    }

    /// `EvalState::evalBool` with the evaluation supplied by the caller.
    #[inline]
    pub(crate) fn bool_from(
        &self,
        eval: impl FnOnce() -> Result<Value<'a>>,
        pos: SrcPos,
        ctx: &str,
    ) -> Result<bool> {
        // Only the success path is inlined into the (many) callers; the
        // errors are built in shared out-of-line functions.
        match eval() {
            Ok(Value::Bool(b)) => Ok(b),
            Ok(v) => Err(self.not_a_bool(v, pos, ctx)),
            Err(e) => Err(self.traced(e, pos, ctx)),
        }
    }

    #[cold]
    #[inline(never)]
    fn not_a_bool(&self, v: Value<'a>, pos: SrcPos, ctx: &str) -> crate::Error {
        let e = self.error_at(
            ErrorKind::TypeError,
            msg!(
                "expected a Boolean but found ",
                v.show_type(),
                ": ",
                self.print_for_error(v)
            ),
            pos,
        );
        self.traced(e, pos, ctx)
    }

    /// Adds the trace `hint` at `pos` to an error on its way out. Kept out
    /// of line so that error handling does not sit in hot code.
    #[cold]
    #[inline(never)]
    pub(crate) fn traced(&self, e: crate::Error, pos: SrcPos, hint: &str) -> crate::Error {
        self.add_trace(&e, pos, hint);
        e
    }

    /// `EvalState::evalAttrs` with the evaluation supplied by the caller.
    #[inline]
    pub(crate) fn attrs_from(
        &self,
        eval: impl FnOnce() -> Result<Value<'a>>,
        pos: SrcPos,
        ctx: &str,
    ) -> Result<Bindings<'a>> {
        match eval() {
            Ok(Value::Attrs(b)) => Ok(b),
            Ok(v) => Err(self.not_a_set(v, pos, ctx)),
            Err(e) => Err(self.traced(e, pos, ctx)),
        }
    }

    #[cold]
    #[inline(never)]
    fn not_a_set(&self, v: Value<'a>, pos: SrcPos, ctx: &str) -> crate::Error {
        let e = self.error(
            ErrorKind::TypeError,
            msg!(
                "expected a set but found ",
                v.show_type(),
                ": ",
                self.print_for_error(v)
            ),
        );
        self.traced(e, pos, ctx)
    }

    /// `Value::determinePos`: the position of a set or lambda, else `pos`.
    pub fn determine_pos(&self, v: Value<'a>, pos: SrcPos) -> SrcPos {
        match v {
            Value::Attrs(b) => b.pos(),
            Value::Lambda { node, .. } => {
                let (code, lam) = self.lambda_def(node);
                self.src_pos(code, lam.pos)
            }
            _ => pos,
        }
    }

    /// `EvalState::mkPos`: `{ file; line; column; }` for `__curPos` and
    /// `unsafeGetAttrPos`.
    pub fn mk_pos_value(&self, pos: SrcPos) -> Result<Value<'a>> {
        let Some(p) = self.position(pos) else {
            return Ok(Value::Null);
        };
        match &p.info.origin {
            crate::ir::Origin::File { path, .. } => {
                // The file's path with the `Path` context of its store path;
                // in a lazy tree, the hash is left lazy.
                let file = self.lazy_path_string(path, true)?;
                let attrs = vec![
                    self.attr(sym::FILE, file),
                    self.attr(sym::LINE, Value::Int(i64::from(p.line))),
                    self.attr(sym::COLUMN, Value::Int(i64::from(p.column))),
                ];
                Ok(self.mk_attrs(attrs, SrcPos::NONE))
            }
            _ => Ok(Value::Null),
        }
    }

    // ----- forcing -----

    /// `forceValue`: evaluates a thunk in place and returns the value.
    ///
    /// Measured: splitting out an inlined "already evaluated" fast path
    /// (checked at all ~90 call sites) was 0.4-2.4% slower than this single
    /// out-of-line function, presumably from the larger callers.
    pub fn force(&self, cell: ValueRef<'a>, pos: SrcPos) -> Result<Value<'a>> {
        match cell.get() {
            Value::Thunk { env, node } => {
                crate::stats::Stats::bump_work(&self.stats.thunk_forces);
                let (code, node) = self.resolve_node(node);
                cell.set(Value::Blackhole);
                match self.with_stack(|| self.eval_node(code, node, env)) {
                    Ok(v) => {
                        cell.set(v);
                        Ok(v)
                    }
                    Err(e) => Err(self.fail_cell(cell, e, pos)),
                }
            }
            Value::App(app) => {
                cell.set(Value::Blackhole);
                match self.call_cell(app.fun, &[app.arg], pos) {
                    Ok(v) => {
                        cell.set(v);
                        Ok(v)
                    }
                    Err(e) => Err(self.fail_cell(cell, e, pos)),
                }
            }
            // The position is filled in by the enclosing force of the
            // thunk that is being re-entered (`tryFixupBlackHolePos`).
            Value::Blackhole => Err(self.error(
                ErrorKind::InfiniteRecursion,
                "infinite recursion encountered",
            )),
            Value::Failed(e) => {
                // A memoised infinite recursion that was found by the
                // parallel pre-force (which forces without a position)
                // takes the position of the force that rethrows it, as the
                // shared exception object does in Nix.
                let e = (*e).clone();
                if e.kind == ErrorKind::InfiniteRecursion && e.pos().is_none() {
                    e.set_pos(self.position(pos));
                }
                Err(e)
            }
            v => Ok(v),
        }
    }

    /// Records a failed evaluation in the cell so that forcing it again
    /// rethrows (`Value::Failed`), fixing up the position of an infinite
    /// recursion error that has none (`tryFixupBlackHolePos`).
    #[cold]
    #[inline(never)]
    fn fail_cell(&self, cell: ValueRef<'a>, e: crate::Error, pos: SrcPos) -> crate::Error {
        if e.kind == ErrorKind::InfiniteRecursion && e.pos().is_none() {
            e.set_pos(self.position(pos));
        }
        cell.set(Value::Failed(self.bump.alloc(e.clone())));
        e
    }

    /// Forces a value held in a temporary rather than a cell.
    pub fn force_value(&self, v: Value<'a>, pos: SrcPos) -> Result<Value<'a>> {
        if v.is_forced() {
            Ok(v)
        } else {
            self.force(self.alloc_cell(v), pos)
        }
    }

    /// `forceValueDeep`.
    pub fn force_deep(&self, cell: ValueRef<'a>) -> Result<()> {
        let mut seen: std::collections::HashSet<usize> = std::collections::HashSet::new();
        self.force_deep_inner(cell, &mut seen)
    }

    /// One level of [`Evaluator::force_deep`]: recurses as deep as the
    /// value nests, until `max-call-depth`, growing the stack as needed.
    fn force_deep_inner(
        &self,
        cell: ValueRef<'a>,
        seen: &mut std::collections::HashSet<usize>,
    ) -> Result<()> {
        self.with_stack(|| self.force_deep_level(cell, seen))
    }

    fn force_deep_level(
        &self,
        cell: ValueRef<'a>,
        seen: &mut std::collections::HashSet<usize>,
    ) -> Result<()> {
        let _guard = self.enter(self.determine_pos(cell.get(), SrcPos::NONE))?;
        if !seen.insert(cell as *const _ as usize) {
            return Ok(());
        }
        let v = self.force(cell, self.determine_pos(cell.get(), SrcPos::NONE))?;
        match v {
            Value::Attrs(b) => {
                // Nix's `Bindings` order: which attribute fails first is
                // observable.
                for attr in b.iter() {
                    if let Err(e) = self.force_deep_inner(attr.value, seen) {
                        self.add_trace(
                            &e,
                            attr.pos,
                            msg!(
                                "while evaluating the attribute '",
                                self.symbol_ident(attr.name),
                                "'"
                            ),
                        );
                        return Err(e);
                    }
                }
            }
            Value::List(l) => {
                for (i, &elem) in l.elems.iter().enumerate() {
                    if let Err(e) = self.force_deep_inner(elem, seen) {
                        e.add_trace(None, format!("while evaluating list element at index {i}"));
                        return Err(e);
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    #[cold]
    #[inline(never)]
    fn type_error(&self, expected: &str, v: Value<'a>, pos: SrcPos) -> crate::Error {
        self.error_at(
            ErrorKind::TypeError,
            msg!(
                "expected ",
                expected,
                " but found ",
                v.show_type(),
                ": ",
                self.print_for_error(v)
            ),
            pos,
        )
    }

    fn with_ctx<T>(&self, r: Result<T>, pos: SrcPos, ctx: &str) -> Result<T> {
        r.map_err(|e| self.traced(e, pos, ctx))
    }

    pub fn force_int(&self, cell: ValueRef<'a>, pos: SrcPos, ctx: &str) -> Result<i64> {
        let r = self.force(cell, pos).and_then(|v| match v {
            Value::Int(i) => Ok(i),
            _ => Err(self.type_error("an integer", v, pos)),
        });
        self.with_ctx(r, pos, ctx)
    }

    pub fn force_float(&self, cell: ValueRef<'a>, pos: SrcPos, ctx: &str) -> Result<f64> {
        let r = self.force(cell, pos).and_then(|v| match v {
            Value::Int(i) => Ok(i as f64),
            Value::Float(f) => Ok(f),
            _ => Err(self.type_error("a float", v, pos)),
        });
        self.with_ctx(r, pos, ctx)
    }

    pub fn force_bool(&self, cell: ValueRef<'a>, pos: SrcPos, ctx: &str) -> Result<bool> {
        let r = self.force(cell, pos).and_then(|v| match v {
            Value::Bool(b) => Ok(b),
            _ => Err(self.type_error("a Boolean", v, pos)),
        });
        self.with_ctx(r, pos, ctx)
    }

    /// `forceAttrs`'s check: the error carries the context as a trace but
    /// no position of its own.
    pub fn expect_attrs(&self, v: Value<'a>, pos: SrcPos, ctx: &str) -> Result<Bindings<'a>> {
        match v {
            Value::Attrs(b) => Ok(b),
            _ => Err(self
                .type_error("a set", v, SrcPos::NONE)
                .with_trace(self.position(pos), ctx)),
        }
    }

    pub fn force_attrs(&self, cell: ValueRef<'a>, pos: SrcPos, ctx: &str) -> Result<Bindings<'a>> {
        let v = self.force(cell, pos)?;
        self.expect_attrs(v, pos, ctx)
    }

    /// `forceAttrs` with a lazily computed position.
    pub fn force_attrs_by(
        &self,
        cell: ValueRef<'a>,
        get_pos: impl Fn(Value<'a>) -> SrcPos,
        ctx: &str,
    ) -> Result<Bindings<'a>> {
        let pos = get_pos(cell.get());
        let v = self.force(cell, pos)?;
        self.expect_attrs(v, pos, ctx)
    }

    pub fn force_list(&self, cell: ValueRef<'a>, pos: SrcPos, ctx: &str) -> Result<Value<'a>> {
        let v = self.force(cell, pos)?;
        match v {
            Value::List(_) => Ok(v),
            _ => Err(self
                .type_error("a list", v, SrcPos::NONE)
                .with_trace(self.position(pos), ctx)),
        }
    }

    pub fn force_list_elems(
        &self,
        cell: ValueRef<'a>,
        pos: SrcPos,
        ctx: &str,
    ) -> Result<&'a [ValueRef<'a>]> {
        Ok(self.force_list(cell, pos, ctx)?.as_list().unwrap())
    }

    pub fn is_functor(&self, v: Value<'a>) -> bool {
        matches!(v, Value::Attrs(b) if b.get(sym::FUNCTOR).is_some())
    }

    pub fn force_function(&self, cell: ValueRef<'a>, pos: SrcPos, ctx: &str) -> Result<Value<'a>> {
        let r = self.force(cell, pos).and_then(|v| {
            if v.value_type() == ValueType::Function || self.is_functor(v) {
                Ok(v)
            } else {
                Err(self.type_error("a function", v, pos))
            }
        });
        self.with_ctx(r, pos, ctx)
    }

    /// `forceString` without collecting context.
    pub fn force_string(
        &self,
        cell: ValueRef<'a>,
        pos: SrcPos,
        ctx: &str,
    ) -> Result<&'a crate::value::NixStr<'a>> {
        let r = self.force(cell, pos).and_then(|v| match v {
            Value::String(s) => Ok(s),
            _ => Err(self.type_error("a string", v, pos)),
        });
        self.with_ctx(r, pos, ctx)
    }

    /// `forceString(v, context, …)`: also merges the string's context.
    pub fn force_string_ctx(
        &self,
        cell: ValueRef<'a>,
        context: &mut StringContext<'a>,
        pos: SrcPos,
        ctx: &str,
    ) -> Result<&'a [u8]> {
        let (bytes, string_context) = self.str_parts(self.force_string(cell, pos, ctx)?)?;
        context.extend_from_slice(string_context);
        Ok(bytes)
    }

    /// `forceStringNoCtx`: the string must not refer to store paths.
    pub fn force_string_no_ctx(
        &self,
        cell: ValueRef<'a>,
        pos: SrcPos,
        ctx: &str,
    ) -> Result<&'a [u8]> {
        let (bytes, string_context) = self.str_parts(self.force_string(cell, pos, ctx)?)?;
        if let Some(elem) = string_context.iter().find(|c| c.is_real()) {
            return Err(self
                .error(
                    ErrorKind::Eval,
                    msg!(
                        "the string '",
                        bytes,
                        "' is not allowed to refer to a store path (such as '",
                        self.display_context_elem(*elem),
                        "')"
                    ),
                )
                .with_trace(self.position(pos), ctx));
        }
        Ok(bytes)
    }

    /// `NixStringContextElem::display`: bytes, as an output name is.
    pub fn display_context_elem(&self, c: ContextElem<'a>) -> Vec<u8> {
        let dir = self.config.store_dir.as_str();
        match c {
            ContextElem::Opaque(p) => format!("{dir}/{p}").into_bytes(),
            ContextElem::DrvDeep(p) => format!("{dir}/{p} (deep)").into_bytes(),
            ContextElem::Built { drv, output } => msg!(dir, "/", drv, "^", output),
            ContextElem::Path(p) => format!("{dir}/{p} (untracked)").into_bytes(),
            ContextElem::LazyHash { .. } | ContextElem::LazyPath(_) => {
                unreachable!("a coerced string's lazy context is resolved")
            }
        }
    }

    /// `isDerivation`: a set with `type = "derivation"`.
    pub fn is_derivation(&self, v: Value<'a>) -> Result<bool> {
        let Value::Attrs(b) = v else { return Ok(false) };
        let Some(t) = b.get(sym::TYPE) else {
            return Ok(false);
        };
        let tv = self.force(t.value, t.pos)?;
        match tv {
            Value::String(s) => Ok(self.str_bytes(s)? == b"derivation"),
            _ => Ok(false),
        }
    }

    // ----- equality -----

    pub fn eq_values_v(
        &self,
        v1: Value<'a>,
        v2: Value<'a>,
        pos: SrcPos,
        ctx: &str,
    ) -> Result<bool> {
        // Two temporaries are never the same cell, so Nix's pointer
        // shortcut cannot apply; forced operands (the usual case) are
        // compared without allocating cells for them.
        if !v1.is_forced() || !v2.is_forced() {
            return self.eq_values(self.alloc_cell(v1), self.alloc_cell(v2), pos, ctx);
        }
        let _guard = self.enter(pos)?;
        self.eq_forced(v1, v2, pos, ctx)
    }

    /// `EvalState::eqValues`. Takes cells so that the pointer-equality
    /// shortcut Nix applies (`&v1 == &v2`) is reproduced. `ctx` is carried
    /// like Nix's `errorCtx`, which only an internal error would show.
    #[allow(clippy::only_used_in_recursion)]
    pub fn eq_values(
        &self,
        a: ValueRef<'a>,
        b: ValueRef<'a>,
        pos: SrcPos,
        ctx: &str,
    ) -> Result<bool> {
        // Nested lists and sets recurse through here as deep as
        // `max-call-depth` lets them, growing the stack as needed.
        self.with_stack(|| {
            let _guard = self.enter(pos)?;
            let v1 = self.force(a, pos)?;
            let v2 = self.force(b, pos)?;
            if std::ptr::eq(a, b) {
                return Ok(true);
            }
            self.eq_forced(v1, v2, pos, ctx)
        })
    }

    /// The comparison of `eqValues` after both operands are forced and
    /// the pointer shortcut has been tried.
    #[allow(clippy::only_used_in_recursion)]
    fn eq_forced(&self, v1: Value<'a>, v2: Value<'a>, pos: SrcPos, ctx: &str) -> Result<bool> {
        Ok(match (v1, v2) {
            (Value::Int(i), Value::Float(f)) => i as f64 == f,
            (Value::Float(f), Value::Int(i)) => f == i as f64,
            (Value::Int(x), Value::Int(y)) => x == y,
            (Value::Float(x), Value::Float(y)) => x == y,
            (Value::Bool(x), Value::Bool(y)) => x == y,
            (Value::String(x), Value::String(y)) => self.str_eq(x, y)?,
            (Value::Path(x), Value::Path(y)) => {
                x.accessor == y.accessor
                    && if x.accessor == self.root_accessor {
                        self.root_paths_equal(x.path, y.path)?
                    } else {
                        x.path == y.path
                    }
            }
            (Value::Null, Value::Null) => true,
            (Value::List(x), Value::List(y)) => {
                if x.elems.len() != y.elems.len() {
                    return Ok(false);
                }
                for (ea, eb) in x.elems.iter().zip(y.elems.iter()) {
                    if !self.eq_values(ea, eb, pos, ctx)? {
                        return Ok(false);
                    }
                }
                true
            }
            (Value::Attrs(x), Value::Attrs(y)) => {
                if self.is_derivation(v1)? && self.is_derivation(v2)? {
                    if let (Some(i), Some(j)) = (x.get(sym::OUT_PATH), y.get(sym::OUT_PATH)) {
                        return self.eq_values(i.value, j.value, pos, ctx);
                    }
                }
                if x.len() != y.len() {
                    return Ok(false);
                }
                // Nix compares in `Bindings` (symbol id) order; which
                // attribute is forced first is observable.
                for (i, j) in x.iter().zip(y.iter()) {
                    if i.name != j.name || !self.eq_values(i.value, j.value, pos, ctx)? {
                        return Ok(false);
                    }
                }
                true
            }
            _ if v1.value_type() == ValueType::Function
                && v2.value_type() == ValueType::Function =>
            {
                false
            }
            _ => false,
        })
    }

    /// `assertEqValues`: like `eqValues` but explains the difference.
    pub fn assert_eq_values(
        &self,
        a: ValueRef<'a>,
        b: ValueRef<'a>,
        pos: SrcPos,
        ctx: &str,
    ) -> Result<()> {
        self.with_stack(|| self.assert_eq_values_inner(a, b, pos, ctx))
    }

    fn assert_eq_values_inner(
        &self,
        a: ValueRef<'a>,
        b: ValueRef<'a>,
        pos: SrcPos,
        ctx: &str,
    ) -> Result<()> {
        let _guard = self.enter(pos)?;
        let v1 = self.force(a, pos)?;
        let v2 = self.force(b, pos)?;
        if std::ptr::eq(a, b) {
            return Ok(());
        }
        let numeric = |v: Value<'a>| matches!(v, Value::Int(_) | Value::Float(_));
        let err = |msg: Vec<u8>| Err(self.error(ErrorKind::Assertion, msg));
        if numeric(v1) && numeric(v2) {
            if self.eq_values(a, b, pos, ctx)? {
                return Ok(());
            }
            return err(msg!(
                v1.show_type(),
                " with value '",
                self.print_for_error(v1),
                "' is not equal to ",
                v2.show_type(),
                " with value '",
                self.print_for_error(v2),
                "'"
            ));
        }
        if v1.value_type() != v2.value_type() {
            return err(msg!(
                v1.show_type(),
                " of value '",
                self.print_for_error(v1),
                "' is not equal to ",
                v2.show_type(),
                " of value '",
                self.print_for_error(v2),
                "'"
            ));
        }
        match (v1, v2) {
            (Value::Int(x), Value::Int(y)) if x != y => {
                return err(format!("integer '{x}' is not equal to integer '{y}'").into());
            }
            (Value::Bool(x), Value::Bool(y)) if x != y => {
                return err(format!("boolean '{x}' is not equal to boolean '{y}'").into());
            }
            (Value::String(x), Value::String(y)) if !self.str_eq(x, y)? => {
                return err(msg!(
                    "string '",
                    self.print_for_error(v1),
                    "' is not equal to string '",
                    self.print_for_error(v2),
                    "'"
                ));
            }
            (Value::Path(x), Value::Path(y)) => {
                if x.accessor != y.accessor {
                    return err(msg!(
                        "path '",
                        self.print_for_error(v1),
                        "' is not equal to path '",
                        self.print_for_error(v2),
                        "' because their accessors are different"
                    ));
                }
                let equal = if x.accessor == self.root_accessor {
                    self.root_paths_equal(x.path, y.path)?
                } else {
                    x.path == y.path
                };
                if !equal {
                    return err(msg!(
                        "path '",
                        self.print_for_error(v1),
                        "' is not equal to path '",
                        self.print_for_error(v2),
                        "'"
                    ));
                }
            }
            (Value::Null, Value::Null) => {}
            (Value::List(x), Value::List(y)) => {
                if x.elems.len() != y.elems.len() {
                    return err(msg!(
                        "list of size '",
                        x.elems.len().to_string(),
                        "' is not equal to list of size '",
                        y.elems.len().to_string(),
                        "', left hand side is '",
                        self.print_for_error(v1),
                        "', right hand side is '",
                        self.print_for_error(v2),
                        "'"
                    ));
                }
                for (n, (ea, eb)) in x.elems.iter().zip(y.elems.iter()).enumerate() {
                    if let Err(e) = self.assert_eq_values(ea, eb, pos, ctx) {
                        self.add_trace(&e, pos, format!("while comparing list element {n}"));
                        return Err(e);
                    }
                }
            }
            (Value::Attrs(x), Value::Attrs(y)) => {
                if self.is_derivation(v1)? && self.is_derivation(v2)? {
                    if let (Some(i), Some(j)) = (x.get(sym::OUT_PATH), y.get(sym::OUT_PATH)) {
                        return self
                            .assert_eq_values(i.value, j.value, pos, ctx)
                            .inspect_err(|e| {
                                self.add_trace(
                                    e,
                                    pos,
                                    "while comparing a derivation by its 'outPath' attribute",
                                );
                            });
                    }
                }
                if x.len() != y.len() {
                    return err(msg!(
                        "attribute names of attribute set '",
                        self.print_for_error(v1),
                        "' differs from attribute set '",
                        self.print_for_error(v2),
                        "'"
                    ));
                }
                for (i, j) in x.iter().zip(y.iter()) {
                    if i.name != j.name {
                        if y.get(i.name).is_none() {
                            return err(msg!(
                                "attribute name '",
                                self.symbol_ident(i.name),
                                "' is contained in '",
                                self.print_for_error(v1),
                                "', but not in '",
                                self.print_for_error(v2),
                                "'"
                            ));
                        }
                        if x.get(j.name).is_none() {
                            return err(msg!(
                                "attribute name '",
                                self.symbol_ident(j.name),
                                "' is missing in '",
                                self.print_for_error(v1),
                                "', but is contained in '",
                                self.print_for_error(v2),
                                "'"
                            ));
                        }
                    }
                    if let Err(e) = self.assert_eq_values(i.value, j.value, pos, ctx) {
                        if j.pos.is_some() {
                            self.add_trace(&e, j.pos, "where right hand side is");
                        }
                        if i.pos.is_some() {
                            self.add_trace(&e, i.pos, "where left hand side is");
                        }
                        self.add_trace(
                            &e,
                            pos,
                            msg!(
                                "while comparing attribute '",
                                self.symbol_ident(i.name),
                                "'"
                            ),
                        );
                        return Err(e);
                    }
                }
            }
            (Value::Float(x), Value::Float(y)) if x != y => {
                return err(format!(
                    "float '{}' is not equal to float '{}'",
                    crate::print::format_float_f(x),
                    crate::print::format_float_f(y)
                )
                .into());
            }
            _ if v1.value_type() == ValueType::Function => {
                return err(
                    "distinct functions and immediate comparisons of identical functions compare as unequal"
                        .into(),
                );
            }
            _ => {}
        }
        Ok(())
    }

    // ----- calls -----

    pub fn call1(&self, fun: Value<'a>, arg: ValueRef<'a>, pos: SrcPos) -> Result<Value<'a>> {
        self.call(fun, &[arg], pos)
    }

    /// `EvalState::callFunction`: one call-depth level, whatever the number
    /// of arguments, applied in a loop (curried results take the remaining
    /// arguments without recursing).
    ///
    /// The shape is deliberately Nix's, because call depth is observable
    /// through `max-call-depth`: forcing an `App` calls this recursively
    /// (mapAttrs's `App (App f name) value` costs two levels), and so does
    /// `__functor`. Measured on firefox.drvPath: 1.69 M calls, 58% with one
    /// argument and 42% with two; 0.38 M continue with a curried result;
    /// `App` chains are at most two long; 21 k partial builtin applications
    /// get saturated. Collapsing any of these would change the depth at
    /// which Nix reports `stack overflow; max-call-depth exceeded`
    /// (tests/differential.rs pins the thresholds).
    pub fn call(&self, fun: Value<'a>, args: &[ValueRef<'a>], pos: SrcPos) -> Result<Value<'a>> {
        self.with_stack(|| self.call_inner(fun, args, pos))
    }

    fn call_inner(&self, fun: Value<'a>, args: &[ValueRef<'a>], pos: SrcPos) -> Result<Value<'a>> {
        let _guard = self.enter(pos)?;
        let cur = self.force_value(fun, pos)?;
        self.apply(cur, args, pos)
    }

    /// [`Evaluator::call`] of the function in the cell `fun`, which is
    /// forced in place after the call's depth level is entered, as
    /// `callFunction(Value & fun, …)` forces it. Forcing a suspended
    /// application `App fun arg` is this call, so when `fun` is itself an
    /// application (mapAttrs's `App (App f name) value`) the inner call is
    /// made one level deeper, as in Nix. Out of line, so that
    /// [`Evaluator::force`] does not take in a copy of the argument loop.
    #[inline(never)]
    fn call_cell(
        &self,
        fun: ValueRef<'a>,
        args: &[ValueRef<'a>],
        pos: SrcPos,
    ) -> Result<Value<'a>> {
        self.with_stack(|| {
            let _guard = self.enter(pos)?;
            let cur = self.force(fun, pos)?;
            self.apply(cur, args, pos)
        })
    }

    /// The argument loop of `callFunction`, inside the call's depth level,
    /// from the forced function `cur`. Inlined into both entries.
    #[inline(always)]
    fn apply(&self, mut cur: Value<'a>, args: &[ValueRef<'a>], pos: SrcPos) -> Result<Value<'a>> {
        let mut args = args;
        while !args.is_empty() {
            match cur {
                Value::Lambda { env, node } => {
                    crate::stats::Stats::bump_work(&self.stats.lambda_calls);
                    cur = self.call_lambda(env, node, args[0], pos)?;
                    args = &args[1..];
                }
                Value::PrimOp(p) => {
                    let arity = p.arity as usize;
                    if args.len() < arity {
                        return Ok(self.make_app_chain(cur, args));
                    }
                    cur = self.call_primop(p, &args[..arity], pos)?;
                    args = &args[arity..];
                }
                Value::PrimOpApp(app) => {
                    // Builtins take at most a handful of arguments.
                    let mut done: SmallVec<[ValueRef<'a>; 4]> = SmallVec::new();
                    let mut head = Value::PrimOpApp(app);
                    while let Value::PrimOpApp(a) = head {
                        done.push(a.arg);
                        head = a.fun.get();
                    }
                    let Value::PrimOp(p) = head else {
                        unreachable!("primop application without a primop")
                    };
                    done.reverse();
                    let left = p.arity as usize - done.len();
                    if args.len() < left {
                        return Ok(self.make_app_chain(cur, args));
                    }
                    done.extend_from_slice(&args[..left]);
                    cur = self.call_primop(p, &done, pos)?;
                    args = &args[left..];
                }
                Value::Attrs(b) if b.get(sym::FUNCTOR).is_some() => {
                    let functor = b.get(sym::FUNCTOR).unwrap();
                    let self_cell = self.alloc_cell(cur);
                    let f = self.force(functor.value, functor.pos)?;
                    cur = self
                        .call(f, &[self_cell, args[0]], functor.pos)
                        .map_err(|e| {
                            self.traced(
                                e,
                                pos,
                                "while calling a functor (an attribute set with a '__functor' attribute)",
                            )
                        })?;
                    args = &args[1..];
                }
                _ => return Err(self.not_a_function(cur, pos)),
            }
        }
        Ok(cur)
    }

    #[cold]
    #[inline(never)]
    fn not_a_function(&self, v: Value<'a>, pos: SrcPos) -> crate::Error {
        self.error_at(
            ErrorKind::TypeError,
            msg!(
                "attempt to call something which is not a function but ",
                v.show_type(),
                ": ",
                self.print_for_error(v)
            ),
            pos,
        )
    }

    fn make_app_chain(&self, mut cur: Value<'a>, args: &[ValueRef<'a>]) -> Value<'a> {
        for &arg in args {
            let fun = self.alloc_cell(cur);
            cur = self.mk_primop_app(fun, arg);
        }
        cur
    }

    fn call_primop(
        &self,
        p: &'a crate::value::PrimOp<'a>,
        args: &[ValueRef<'a>],
        pos: SrcPos,
    ) -> Result<Value<'a>> {
        crate::stats::Stats::bump_work(&self.stats.primop_calls);
        match (p.imp)(self, SrcPos::NONE, args) {
            Ok(v) => Ok(v),
            Err(e) => Err(self.trace_primop(e, p, pos)),
        }
    }

    #[cold]
    #[inline(never)]
    fn trace_primop(
        &self,
        e: crate::Error,
        p: &crate::value::PrimOp<'a>,
        pos: SrcPos,
    ) -> crate::Error {
        if p.add_trace {
            self.add_trace(&e, pos, format!("while calling the '{}' builtin", p.name));
        }
        e
    }

    /// The name used in `function '…'` messages: the symbol or
    /// `anonymous lambda`.
    fn lambda_name(&self, lam: &crate::code::Lambda<'a>) -> &'a [u8] {
        match lam.name {
            Some(n) => self.symbol_name(n),
            None => b"anonymous lambda",
        }
    }

    /// The name used in `while calling …` traces: quoted when named.
    fn lambda_call_name(&self, lam: &crate::code::Lambda<'a>) -> Vec<u8> {
        match lam.name {
            Some(n) => msg!("'", self.symbol_name(n), "'"),
            None => b"anonymous lambda".to_vec(),
        }
    }

    /// Applies the function at `node`, closed over `closure_env`, to `arg`.
    fn call_lambda(
        &self,
        closure_env: Env<'a>,
        node: GlobalNode,
        arg: ValueRef<'a>,
        pos: SrcPos,
    ) -> Result<Value<'a>> {
        let (code, lam) = self.lambda_def(node);
        let lam_pos = self.src_pos(code, lam.pos);
        let env2 = self.alloc_env(Some(closure_env), lam.env_size());
        let mut displ = 0usize;
        match &lam.formals {
            Some(formals) => {
                let attrs = match self.force_attrs(
                    arg,
                    lam_pos,
                    "while evaluating the value passed for the lambda argument",
                ) {
                    Ok(a) => a,
                    Err(e) => return Err(self.trace_call_site(e, pos)),
                };
                if lam.arg.is_some() {
                    env2.set_slot(displ, arg);
                    displ += 1;
                }
                let mut used = 0usize;
                // Formals are visited in symbol order, as Nix's sorted
                // `Formals::formals` are; a formal's slot is its index.
                let first_formal = displ;
                crate::stats::Stats::add_work(
                    &self.stats.formal_lookups,
                    formals.formals.len() as u64,
                );
                for (index, formal) in formals.in_id_order() {
                    let displ = first_formal + index;
                    match attrs.get(formal.name) {
                        Some(a) => {
                            used += 1;
                            env2.set_slot(displ, a.value);
                        }
                        None => match formal.default {
                            Some(def) => env2.set_slot(
                                displ,
                                self.maybe_thunk(code, def, env2, LazySite::FormalDefault),
                            ),
                            None => {
                                return Err(self.missing_argument(lam, formal.name, lam_pos, pos))
                            }
                        },
                    }
                }
                if !formals.ellipsis && used != attrs.len() {
                    if let Some(e) = self.unexpected_argument(lam, formals, attrs, lam_pos, pos) {
                        return Err(e);
                    }
                }
            }
            None => {
                env2.set_slot(0, arg);
            }
        }
        match self.eval_node(code, lam.body, env2) {
            Ok(v) => Ok(v),
            Err(e) => Err(self.trace_lambda_call(e, lam, lam_pos, pos)),
        }
    }

    /// The `from call site` trace of an error raised while matching a
    /// lambda's arguments.
    #[cold]
    #[inline(never)]
    fn trace_call_site(&self, e: crate::Error, pos: SrcPos) -> crate::Error {
        if pos.is_some() {
            self.add_trace(&e, pos, "from call site");
        }
        e
    }

    /// The traces of an error raised in a lambda's body (with
    /// `--show-trace`).
    #[cold]
    #[inline(never)]
    fn trace_lambda_call(
        &self,
        e: crate::Error,
        lam: &crate::code::Lambda<'a>,
        lam_pos: SrcPos,
        pos: SrcPos,
    ) -> crate::Error {
        if self.config.show_trace {
            self.add_trace(
                &e,
                lam_pos,
                msg!("while calling ", self.lambda_call_name(lam)),
            );
            if pos.is_some() {
                self.add_trace(&e, pos, "from call site");
            }
        }
        e
    }

    /// `function 'f' called without required argument 'x'`.
    #[cold]
    #[inline(never)]
    fn missing_argument(
        &self,
        lam: &crate::code::Lambda<'a>,
        name: Symbol,
        lam_pos: SrcPos,
        pos: SrcPos,
    ) -> crate::Error {
        let e = self.error_at(
            ErrorKind::TypeError,
            msg!(
                "function '",
                self.lambda_name(lam),
                "' called without required argument '",
                self.symbol_ident(name),
                "'"
            ),
            lam_pos,
        );
        self.add_trace(&e, pos, "from call site");
        e
    }

    /// `function 'f' called with unexpected argument 'x'`, for the first
    /// such argument in `Bindings` (symbol id) order as Nix reports it, with
    /// suggestions among the formals.
    #[cold]
    #[inline(never)]
    fn unexpected_argument(
        &self,
        lam: &crate::code::Lambda<'a>,
        formals: &crate::code::Formals<'a>,
        attrs: Bindings<'a>,
        lam_pos: SrcPos,
        pos: SrcPos,
    ) -> Option<crate::Error> {
        let a = attrs.iter().find(|a| !formals.has(a.name))?;
        let e = self.error_at(
            ErrorKind::TypeError,
            msg!(
                "function '",
                self.lambda_name(lam),
                "' called with unexpected argument '",
                self.symbol_ident(a.name),
                "'"
            ),
            lam_pos,
        );
        self.add_trace(&e, pos, "from call site");
        e.set_suggestions(crate::error::best_suggestions_bytes(
            formals.formals.iter().map(|f| self.symbol_name(f.name)),
            self.symbol_name(a.name),
        ));
        Some(e)
    }

    /// `autoCallFunction`: calls a function with the formals it declares
    /// taken from `args`, or returns a non-function unchanged.
    pub fn auto_call(&self, fun: ValueRef<'a>, args: Bindings<'a>) -> Result<Value<'a>> {
        let pos = self.determine_pos(fun.get(), SrcPos::NONE);
        let f = self.force(fun, pos)?;
        if let Value::Attrs(b) = f {
            if let Some(functor) = b.get(sym::FUNCTOR) {
                let fv = self.force(functor.value, functor.pos)?;
                let v = self.call(fv, &[fun], pos)?;
                let cell = self.alloc_cell(v);
                return self.auto_call(cell, args);
            }
        }
        let Value::Lambda { node, .. } = f else {
            return Ok(f);
        };
        let (code, lam) = self.lambda_def(node);
        let Some(formals) = &lam.formals else {
            return Ok(f);
        };
        let mut attrs: Vec<Attr<'a>> = Vec::new();
        if formals.ellipsis {
            attrs.extend(args.iter().copied());
        } else {
            for (_, formal) in formals.in_id_order() {
                match args.get(formal.name) {
                    Some(a) => attrs.push(*a),
                    None if formal.default.is_none() => {
                        return Err(self.error_at(
                            ErrorKind::MissingArgument,
                            msg!("cannot evaluate a function that has an argument without a value ('", self.symbol_ident(formal.name), "')\nNix attempted to evaluate a function as a top level expression; in\nthis case it must have its arguments supplied either by default\nvalues, or passed explicitly with '--arg' or '--argstr'. See\nhttps://nix.dev/manual/nix/stable/language/syntax.html#functions."),
                            self.src_pos(code, formal.pos),
                        ))
                    }
                    None => {}
                }
            }
        }
        let arg = self.alloc_cell(self.mk_attrs(attrs, SrcPos::NONE));
        self.call(f, &[arg], pos)
    }
}

impl<'a> Evaluator<'a> {
    /// `forceValueDeepParallel` as Nix's JSON serialisation runs it before
    /// serialising (the reference build has the parallel evaluator on by
    /// default). The walk forces the root, then collects the thunks it
    /// meets as work items: a single item runs inline, so its error
    /// propagates without serialisation traces; two or more are spawned
    /// on worker threads, whose errors are swallowed and surface later,
    /// with traces, when the serialiser forces the failed thunks. Sets
    /// with `__toString` are skipped, of sets with `outPath` only that
    /// attribute is forced, descent stops at depth 1024, and values
    /// already walked (by any earlier walk) are not walked again.
    pub fn force_deep_quiet(&self, cell: ValueRef<'a>, pos: SrcPos) -> Result<()> {
        self.deep_parallel(cell, pos, false)
    }

    fn deep_parallel(&self, root: ValueRef<'a>, pos: SrcPos, swallow: bool) -> Result<()> {
        if let Err(e) = self.force(root, pos) {
            return if swallow { Ok(()) } else { Err(e) };
        }
        let mut work: Vec<DeepWork<'a>> = Vec::new();
        self.deep_parallel_walk(root, pos, true, 0, &mut work);
        match work.len() {
            0 => Ok(()),
            1 => match work.pop().unwrap() {
                DeepWork::Deep(cell, pos) => self.deep_parallel(cell, pos, swallow),
                DeepWork::Force(cell, pos) => match self.force(cell, pos) {
                    Ok(_) => Ok(()),
                    Err(e) => {
                        if swallow {
                            Ok(())
                        } else {
                            Err(e)
                        }
                    }
                },
            },
            _ => {
                // The worker threads force without a position: an infinite
                // recursion found here is memoised without one, and gets
                // the position of whichever later force rethrows it.
                for item in work {
                    match item {
                        DeepWork::Deep(cell, _) => {
                            let _ = self.deep_parallel(cell, SrcPos::NONE, true);
                        }
                        DeepWork::Force(cell, _) => {
                            let _ = self.force(cell, SrcPos::NONE);
                        }
                    }
                }
                Ok(())
            }
        }
    }

    fn deep_parallel_walk(
        &self,
        cell: ValueRef<'a>,
        pos: SrcPos,
        is_root: bool,
        depth: usize,
        work: &mut Vec<DeepWork<'a>>,
    ) {
        const MAX_DEPTH: usize = 1024;
        if depth > MAX_DEPTH {
            return;
        }
        let v = cell.get();
        if matches!(
            v,
            Value::String(_)
                | Value::Path(_)
                | Value::Null
                | Value::Int(_)
                | Value::Float(_)
                | Value::Bool(_)
                | Value::Failed(_)
        ) {
            return;
        }
        if !self
            .deep_seen
            .borrow_mut()
            .insert(cell as *const _ as usize)
            && !is_root
        {
            return;
        }
        if !v.is_forced() {
            work.push(DeepWork::Deep(cell, pos));
            return;
        }
        match v {
            Value::Attrs(b) => {
                if b.get(sym::TO_STRING).is_some() {
                    return;
                }
                if let Some(out) = b.get(sym::OUT_PATH) {
                    if !out.value.get().is_forced() {
                        work.push(DeepWork::Force(out.value, out.pos));
                    }
                } else {
                    for a in b.iter() {
                        self.deep_parallel_walk(a.value, a.pos, false, depth + 1, work);
                    }
                }
            }
            Value::List(l) => {
                for &e in l.elems {
                    self.deep_parallel_walk(e, pos, false, depth + 1, work);
                }
            }
            _ => {}
        }
    }
}

/// One unit of Nix's parallel deep-force walk.
enum DeepWork<'a> {
    /// `forceValueDeepParallel` on a thunk.
    Deep(ValueRef<'a>, SrcPos),
    /// `forceValue` on a set's `outPath`.
    Force(ValueRef<'a>, SrcPos),
}
