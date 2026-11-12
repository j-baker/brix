//! Compiling the AST into [`crate::ir::Code`], resolving variables.
//!
//! This is Nix's `bindVars` pass (`nixexpr.cc`) fused with a flattening of
//! the tree. Scopes are tracked with a chain of [`StaticEnv`]s exactly as
//! Nix does: each `let`, `rec { … }`, lambda and `with` pushes one. A
//! variable that no static scope binds is resolved through the nearest
//! enclosing `with`, or rejected as undefined right here, before anything
//! is evaluated, which is also when Nix reports it.

use brix_syntax::ast::{self, AttrValue, Expr};
use bstr::BString;
use thiserror::Error;

use crate::ir::{
    AttrDef, AttrKind, AttrName, AttrSet, AttrsId, Code, DynamicAttrDef, Formal, Formals, Lambda,
    LambdaId, LitId, Node, NodeId, Pos, Range, VarRef,
};
use crate::stack::{STACK_RED_ZONE, STACK_SEGMENT};
use crate::symbol::{Symbol, SymbolTable};

/// A compile-time scope. (`StaticEnv` in Nix.)
#[derive(Debug, Default)]
pub struct StaticEnv<'p> {
    /// Set for the scope a `with` introduces; the node is the `with` itself.
    pub is_with: Option<NodeId>,
    pub up: Option<&'p StaticEnv<'p>>,
    /// Sorted by symbol.
    pub vars: Vec<(Symbol, u32)>,
}

impl<'p> StaticEnv<'p> {
    pub fn new(up: Option<&'p StaticEnv<'p>>, mut vars: Vec<(Symbol, u32)>) -> StaticEnv<'p> {
        vars.sort_by_key(|&(s, _)| s);
        StaticEnv {
            is_with: None,
            up,
            vars,
        }
    }

    fn with(up: &'p StaticEnv<'p>, node: NodeId) -> StaticEnv<'p> {
        StaticEnv {
            is_with: Some(node),
            up: Some(up),
            vars: Vec::new(),
        }
    }

    pub fn find(&self, name: Symbol) -> Option<u32> {
        self.vars
            .binary_search_by_key(&name, |&(s, _)| s)
            .ok()
            .map(|i| self.vars[i].1)
    }
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum CompileError {
    /// The name is bytes, as written (`inherit "\xff";` names a
    /// variable too).
    #[error("{}", String::from_utf8_lossy(&undefined_variable(name)))]
    UndefinedVariable { name: BString, pos: Pos },
}

/// "undefined variable 'x'", with the name as `printIdentifier` shows a
/// symbol: quoted when it is not an identifier (`inherit "a b";`).
pub fn undefined_variable(name: &[u8]) -> Vec<u8> {
    let mut msg = b"undefined variable '".to_vec();
    brix_syntax::printer::print_identifier(&mut msg, name);
    msg.push(b'\'');
    msg
}

/// Compiles a parsed file. `base_env` binds the names visible at top
/// level (the builtins, and for `scopedImport` the extra scope).
pub fn compile(
    expr: &Expr,
    symbols: &mut SymbolTable<'_>,
    base_env: &StaticEnv<'_>,
) -> Result<Code, CompileError> {
    let mut compiler = Compiler {
        symbols,
        code: Code {
            root: NodeId(0),
            nodes: Vec::new(),
            strings: Vec::new(),
            attr_sets: Vec::new(),
            lambdas: Vec::new(),
            attr_names: Vec::new(),
            node_lists: Vec::new(),
            parts: Vec::new(),
        },
    };
    // Intern every name in source order first, so that symbol ids (and
    // with them the order of attributes in sets) follow the order in which
    // Nix's parser would have created them.
    compiler.intern_names(expr);
    let root = compiler.expr(expr, base_env, None)?;
    compiler.code.root = root;
    Ok(compiler.code)
}

struct Compiler<'s, 'a> {
    symbols: &'s mut SymbolTable<'a>,
    code: Code,
}

impl Compiler<'_, '_> {
    /// Interns the names in `e` in source order. Recurses as deep as the
    /// source nests, on a fresh stack segment when this one runs low (as
    /// the parser does).
    fn intern_names(&mut self, e: &Expr) {
        stacker::maybe_grow(STACK_RED_ZONE, STACK_SEGMENT, || self.intern_names_inner(e))
    }

    fn intern_names_inner(&mut self, e: &Expr) {
        match e {
            Expr::Int { .. }
            | Expr::Float { .. }
            | Expr::String { .. }
            | Expr::Path { .. }
            | Expr::CurPos { .. } => {}
            Expr::Var { name, .. } => {
                self.symbols.intern(name.as_bytes());
            }
            Expr::Select {
                subject,
                path,
                default,
                ..
            } => {
                self.intern_names(subject);
                self.intern_attr_names(path);
                if let Some(d) = default {
                    self.intern_names(d);
                }
            }
            Expr::HasAttr { subject, path } => {
                self.intern_names(subject);
                self.intern_attr_names(path);
            }
            Expr::Attrs(set) => self.intern_attr_set(set),
            Expr::List { elems, .. } => elems.iter().for_each(|e| self.intern_names(e)),
            Expr::Lambda {
                arg, formals, body, ..
            } => {
                // Bison creates a formal's symbol when the `formal` rule
                // reduces, i.e. after its default expression; the `@`/plain
                // argument's symbol is created in the lambda rule's action,
                // after the body has been parsed.
                if let Some(f) = formals {
                    for formal in &f.formals {
                        if let Some(d) = &formal.default {
                            self.intern_names(d);
                        }
                        self.symbols.intern(formal.name.as_bytes());
                    }
                }
                self.intern_names(body);
                if let Some(a) = arg {
                    self.symbols.intern(a.as_bytes());
                }
            }
            Expr::Call { fun, args, .. } => {
                self.intern_names(fun);
                args.iter().for_each(|e| self.intern_names(e));
            }
            Expr::Let { attrs, body } => {
                self.intern_attr_set(attrs);
                self.intern_names(body);
            }
            Expr::With { attrs, body, .. } => {
                self.intern_names(attrs);
                self.intern_names(body);
            }
            Expr::If {
                cond, then_, else_, ..
            } => {
                self.intern_names(cond);
                self.intern_names(then_);
                self.intern_names(else_);
            }
            Expr::Assert { cond, body, .. } => {
                self.intern_names(cond);
                self.intern_names(body);
            }
            Expr::Not(e) => self.intern_names(e),
            Expr::BinOp { lhs, rhs, .. } => {
                self.intern_names(lhs);
                self.intern_names(rhs);
            }
            Expr::ConcatStrings { parts, .. } => {
                parts.iter().for_each(|(_, e)| self.intern_names(e))
            }
        }
    }

    fn intern_attr_names(&mut self, path: &[ast::AttrName]) {
        for p in path {
            match p {
                ast::AttrName::Static(n) => {
                    self.symbols.intern(n.as_bytes());
                }
                ast::AttrName::Dynamic(e) => self.intern_names(e),
            }
        }
    }

    fn intern_attr_set(&mut self, set: &ast::AttrSet) {
        // Per binding, in source order: an attribute path's symbols are
        // created when the path reduces, before its value is parsed, and
        // `inherit (from) a b;` reduces `from` before the names. (The
        // position of dynamic attributes among the static ones is not kept
        // by the AST; they come last: see "Symbol order" in
        // KNOWN_DEVIATIONS.md.)
        let mut from_done = vec![false; set.inherit_from.len()];
        for def in &set.attrs {
            if let AttrValue::InheritedFrom { index } = def.value {
                let index = index as usize;
                if !from_done[index] {
                    from_done[index] = true;
                    self.intern_names(&set.inherit_from[index]);
                }
            }
            self.symbols.intern(def.name.as_bytes());
            if let AttrValue::Plain(e) = &def.value {
                self.intern_names(e);
            }
        }
        for d in &set.dynamic_attrs {
            self.intern_names(&d.name);
            self.intern_names(&d.value);
        }
        for (i, e) in set.inherit_from.iter().enumerate() {
            if !from_done[i] {
                self.intern_names(e);
            }
        }
    }

    fn push(&mut self, node: Node) -> NodeId {
        let id = NodeId(self.code.nodes.len() as u32);
        self.code.nodes.push(node);
        id
    }

    fn push_string(&mut self, s: &[u8]) -> LitId {
        let id = LitId(self.code.strings.len() as u32);
        self.code.strings.push(s.to_vec());
        id
    }

    fn push_node_list(&mut self, ids: &[NodeId]) -> Range {
        let start = self.code.node_lists.len() as u32;
        self.code.node_lists.extend_from_slice(ids);
        Range {
            start,
            len: ids.len() as u32,
        }
    }

    fn sym(&mut self, name: &ast::Name) -> Symbol {
        self.symbols.intern(name.as_bytes())
    }

    /// Compiles `e` in scope `env`. `name` is the attribute name `e` is
    /// the value of, used to name lambdas. Recurses as deep as the source
    /// nests, on a fresh stack segment when this one runs low.
    fn expr(
        &mut self,
        e: &Expr,
        env: &StaticEnv<'_>,
        name: Option<Symbol>,
    ) -> Result<NodeId, CompileError> {
        stacker::maybe_grow(STACK_RED_ZONE, STACK_SEGMENT, || {
            self.expr_inner(e, env, name)
        })
    }

    fn expr_inner(
        &mut self,
        e: &Expr,
        env: &StaticEnv<'_>,
        name: Option<Symbol>,
    ) -> Result<NodeId, CompileError> {
        Ok(match e {
            Expr::Int { value, .. } => self.push(Node::Int(*value)),
            Expr::Float { value, .. } => self.push(Node::Float(*value)),
            Expr::String { value, .. } => {
                let lit = self.push_string(value);
                self.push(Node::Str(lit))
            }
            Expr::Path { anchor, path, .. } => {
                let lit = self.push_string(path);
                self.push(Node::Path {
                    anchor: *anchor,
                    lit,
                })
            }
            Expr::Var { pos, name } => {
                let name = self.sym(name);
                let var =
                    resolve_var(env, name).ok_or_else(|| CompileError::UndefinedVariable {
                        name: self.symbols.name(name).into(),
                        pos: *pos,
                    })?;
                self.push(Node::Var {
                    pos: *pos,
                    name,
                    var,
                })
            }
            Expr::Select {
                pos,
                subject,
                path,
                default,
            } => {
                let subject = self.expr(subject, env, None)?;
                let default = match default {
                    Some(d) => Some(self.expr(d, env, None)?),
                    None => None,
                };
                let path = self.attr_path(path, env)?;
                self.push(Node::Select {
                    pos: *pos,
                    subject,
                    path,
                    default,
                })
            }
            Expr::HasAttr { subject, path } => {
                let subject = self.expr(subject, env, None)?;
                let path = self.attr_path(path, env)?;
                self.push(Node::HasAttr { subject, path })
            }
            Expr::Attrs(set) => {
                let id = self.attr_set(set, env, set.recursive)?;
                self.push(Node::Attrs(id))
            }
            Expr::List { elems, .. } => {
                let ids = elems
                    .iter()
                    .map(|e| self.expr(e, env, None))
                    .collect::<Result<Vec<_>, _>>()?;
                let elems = self.push_node_list(&ids);
                self.push(Node::List { elems })
            }
            Expr::Lambda {
                pos,
                arg,
                formals,
                body,
            } => {
                let id = self.lambda(*pos, arg.as_ref(), formals.as_deref(), body, env, name)?;
                self.push(Node::Lambda(id))
            }
            Expr::Call { pos, fun, args } => {
                let fun = self.expr(fun, env, None)?;
                let ids = args
                    .iter()
                    .map(|e| self.expr(e, env, None))
                    .collect::<Result<Vec<_>, _>>()?;
                let args = self.push_node_list(&ids);
                self.push(Node::Call {
                    pos: *pos,
                    fun,
                    args,
                })
            }
            Expr::Let { attrs, body } => {
                // A let is a recursive set whose scope the body sees.
                let (id, new_env_vars) = self.attr_set_with_env(attrs, env, true)?;
                let new_env = StaticEnv::new(Some(env), new_env_vars);
                let body = self.expr(body, &new_env, None)?;
                self.push(Node::Let { attrs: id, body })
            }
            Expr::With { pos, attrs, body } => {
                let parent_with = nearest_with(env);
                let prev_with = enclosing_with_level(env);
                let attrs = self.expr(attrs, env, None)?;
                let node = self.push(Node::With {
                    pos: *pos,
                    attrs,
                    body: NodeId(u32::MAX),
                    prev_with,
                    parent_with,
                });
                let new_env = StaticEnv::with(env, node);
                let body = self.expr(body, &new_env, None)?;
                if let Node::With { body: b, .. } = &mut self.code.nodes[node.0 as usize] {
                    *b = body;
                }
                node
            }
            Expr::If {
                pos,
                cond,
                then_,
                else_,
            } => {
                let cond = self.expr(cond, env, None)?;
                let then_ = self.expr(then_, env, None)?;
                let else_ = self.expr(else_, env, None)?;
                self.push(Node::If {
                    pos: *pos,
                    cond,
                    then_,
                    else_,
                })
            }
            Expr::Assert { pos, cond, body } => {
                let cond = self.expr(cond, env, None)?;
                let body = self.expr(body, env, None)?;
                self.push(Node::Assert {
                    pos: *pos,
                    cond,
                    body,
                })
            }
            Expr::Not(e) => {
                let e = self.expr(e, env, None)?;
                self.push(Node::Not(e))
            }
            Expr::BinOp { pos, op, lhs, rhs } => {
                let lhs = self.expr(lhs, env, None)?;
                let rhs = self.expr(rhs, env, None)?;
                // parser.y builds `ExprOpEq`/`ExprOpNEq` without a position
                // (`noPos`), unlike the other operators; the difference shows
                // in traces such as "in the argument of the not operator".
                let pos = match op {
                    ast::BinOp::Eq | ast::BinOp::NEq => ast::Pos::NONE,
                    _ => *pos,
                };
                self.push(Node::BinOp {
                    pos,
                    op: *op,
                    lhs,
                    rhs,
                })
            }
            Expr::ConcatStrings {
                pos,
                force_string,
                parts,
            } => {
                let compiled = parts
                    .iter()
                    .map(|(p, e)| Ok((*p, self.expr(e, env, None)?)))
                    .collect::<Result<Vec<_>, CompileError>>()?;
                let start = self.code.parts.len() as u32;
                self.code.parts.extend_from_slice(&compiled);
                self.push(Node::ConcatStrings {
                    pos: *pos,
                    force_string: *force_string,
                    parts: Range {
                        start,
                        len: compiled.len() as u32,
                    },
                })
            }
            Expr::CurPos { pos } => self.push(Node::CurPos(*pos)),
        })
    }

    fn attr_path(
        &mut self,
        path: &[ast::AttrName],
        env: &StaticEnv<'_>,
    ) -> Result<Range, CompileError> {
        let names = path
            .iter()
            .map(|p| {
                Ok(match p {
                    ast::AttrName::Static(n) => AttrName::Static(self.sym(n)),
                    ast::AttrName::Dynamic(e) => AttrName::Dynamic(self.expr(e, env, None)?),
                })
            })
            .collect::<Result<Vec<_>, CompileError>>()?;
        let start = self.code.attr_names.len() as u32;
        self.code.attr_names.extend_from_slice(&names);
        Ok(Range {
            start,
            len: names.len() as u32,
        })
    }

    fn attr_set(
        &mut self,
        set: &ast::AttrSet,
        env: &StaticEnv<'_>,
        recursive: bool,
    ) -> Result<AttrsId, CompileError> {
        Ok(self.attr_set_with_env(set, env, recursive)?.0)
    }

    /// Compiles the bindings of a set or `let`; returns the scope
    /// variables (name, displacement) the bindings introduce when
    /// recursive, in symbol order.
    fn attr_set_with_env(
        &mut self,
        set: &ast::AttrSet,
        env: &StaticEnv<'_>,
        recursive: bool,
    ) -> Result<(AttrsId, Vec<(Symbol, u32)>), CompileError> {
        // Sort definitions by symbol (Nix keeps them in a map keyed by
        // Symbol) and assign displacements in that order.
        let mut defs: Vec<(Symbol, &ast::AttrDef)> = set
            .attrs
            .iter()
            .map(|d| (self.symbols.intern(d.name.as_bytes()), d))
            .collect();
        defs.sort_by_key(|&(s, _)| s);
        let vars: Vec<(Symbol, u32)> = defs
            .iter()
            .enumerate()
            .map(|(i, &(s, _))| (s, i as u32))
            .collect();

        let new_env = StaticEnv::new(Some(env), vars.clone());
        // Plain attributes of a recursive set see the set's own scope;
        // inherited ones see the enclosing scope; `inherit (from)` ones
        // see an empty scope that sits on top of the set's scope and holds
        // the `from` values.
        let plain_env: &StaticEnv<'_> = if recursive { &new_env } else { env };

        let inherit_from = set
            .inherit_from
            .iter()
            .map(|e| self.expr(e, plain_env, None))
            .collect::<Result<Vec<_>, _>>()?;

        let mut attrs = Vec::with_capacity(defs.len());
        for (displ, &(name, def)) in defs.iter().enumerate() {
            let (kind, expr) = match &def.value {
                AttrValue::Plain(e) => (AttrKind::Plain, self.expr(e, plain_env, Some(name))?),
                AttrValue::Inherited => {
                    let var =
                        resolve_var(env, name).ok_or_else(|| CompileError::UndefinedVariable {
                            name: def.name.as_bytes().into(),
                            pos: def.pos,
                        })?;
                    (
                        AttrKind::Inherited,
                        self.push(Node::Var {
                            pos: def.pos,
                            name,
                            var,
                        }),
                    )
                }
                AttrValue::InheritedFrom { index } => {
                    let subject = self.push(Node::InheritFrom {
                        pos: def.pos,
                        displ: *index,
                    });
                    let start = self.code.attr_names.len() as u32;
                    self.code.attr_names.push(AttrName::Static(name));
                    (
                        AttrKind::InheritedFrom,
                        self.push(Node::Select {
                            pos: def.pos,
                            subject,
                            path: Range { start, len: 1 },
                            default: None,
                        }),
                    )
                }
            };
            attrs.push(AttrDef {
                name,
                pos: def.pos,
                kind,
                expr,
                displ: displ as u32,
            });
        }

        let dynamic_attrs = set
            .dynamic_attrs
            .iter()
            .map(|d| {
                Ok(DynamicAttrDef {
                    name: self.expr(&d.name, plain_env, None)?,
                    value: self.expr(&d.value, plain_env, None)?,
                    pos: d.pos,
                })
            })
            .collect::<Result<Vec<_>, CompileError>>()?;

        let id = AttrsId(self.code.attr_sets.len() as u32);
        self.code.attr_sets.push(AttrSet {
            recursive,
            pos: set.pos,
            attrs,
            dynamic_attrs,
            inherit_from,
        });
        Ok((id, vars))
    }

    fn lambda(
        &mut self,
        pos: Pos,
        arg: Option<&ast::Name>,
        formals: Option<&ast::Formals>,
        body: &Expr,
        env: &StaticEnv<'_>,
        name: Option<Symbol>,
    ) -> Result<LambdaId, CompileError> {
        let arg = arg.map(|a| self.sym(a));
        let mut vars = Vec::new();
        let mut displ = 0u32;
        if let Some(a) = arg {
            vars.push((a, displ));
            displ += 1;
        }
        let sorted_formals = formals.map(|f| {
            let mut fs: Vec<(Symbol, &ast::Formal)> = f
                .formals
                .iter()
                .map(|formal| (self.symbols.intern(formal.name.as_bytes()), formal))
                .collect();
            fs.sort_by_key(|&(s, formal)| (s, formal.pos));
            fs
        });
        if let Some(fs) = &sorted_formals {
            for &(s, _) in fs {
                vars.push((s, displ));
                displ += 1;
            }
        }
        let new_env = StaticEnv::new(Some(env), vars);
        let formals = match (formals, sorted_formals) {
            (Some(f), Some(fs)) => {
                let mut compiled = Vec::with_capacity(fs.len());
                for (s, formal) in fs {
                    let default = match &formal.default {
                        Some(d) => Some(self.expr(d, &new_env, None)?),
                        None => None,
                    };
                    compiled.push(Formal {
                        pos: formal.pos,
                        name: s,
                        default,
                    });
                }
                Some(Formals {
                    formals: compiled,
                    ellipsis: f.ellipsis,
                })
            }
            _ => None,
        };
        // `setName` names the lambda and, through it, a lambda directly in
        // its body (curried definitions).
        let body = self.expr(body, &new_env, name)?;
        let id = LambdaId(self.code.lambdas.len() as u32);
        self.code.lambdas.push(Lambda {
            pos,
            name,
            arg,
            formals,
            body,
        });
        Ok(id)
    }
}

/// `ExprVar::bindVars`: find the scope binding `name`, or the nearest
/// `with` to ask at run time.
fn resolve_var(env: &StaticEnv<'_>, name: Symbol) -> Option<VarRef> {
    let mut cur = Some(env);
    let mut level = 0u32;
    let mut with_level: Option<u32> = None;
    while let Some(e) = cur {
        if e.is_with.is_some() {
            if with_level.is_none() {
                with_level = Some(level);
            }
        } else if let Some(displ) = e.find(name) {
            return Some(VarRef::Env { level, displ });
        }
        cur = e.up;
        level += 1;
    }
    let level = with_level?;
    let with = nearest_with(env)?;
    Some(VarRef::With { level, with })
}

fn nearest_with(env: &StaticEnv<'_>) -> Option<NodeId> {
    let mut cur = Some(env);
    while let Some(e) = cur {
        if let Some(w) = e.is_with {
            return Some(w);
        }
        cur = e.up;
    }
    None
}

/// `ExprWith::prevWith`: the level of the enclosing `with`'s environment,
/// counted from the new `with`'s own environment (hence starting at 1).
fn enclosing_with_level(env: &StaticEnv<'_>) -> u32 {
    let mut cur = Some(env);
    let mut level = 1u32;
    while let Some(e) = cur {
        if e.is_with.is_some() {
            return level;
        }
        cur = e.up;
        level += 1;
    }
    0
}
