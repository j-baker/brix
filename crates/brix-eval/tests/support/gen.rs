//! A grammar-based generator of random Nix expressions, their renderer, and
//! a shrinker, for differential testing against the reference `nix`.
//!
//! The generator produces a small typed tree ([`Expr`]) rather than text so
//! that failing cases can be shrunk structurally. Rendering parenthesises
//! every compound sub-expression: the parser is tested elsewhere (it prints
//! all of nixpkgs identically to `nix-instantiate --parse`), the point here
//! is evaluation semantics, builtins, printers and error messages.
//!
//! Constructs deliberately *not* generated, because the reference and brix
//! legitimately differ (see `KNOWN_DEVIATIONS.md`) or the reference is not
//! deterministic:
//!
//! - regular expressions with alternation (`|`) or nested quantified
//!   groups: `regex` is leftmost-first, POSIX ERE leftmost-longest;
//! - invalid JSON/TOML input to `fromJSON`/`fromTOML`: the parse-error
//!   texts come from different libraries;
//! - `builtins.trace`/`warn` anywhere but inside a `sort` comparator or as
//!   the outermost expression: Determinate Nix's parallel deep-forcing
//!   interleaves traces from sibling attributes nondeterministically;
//! - fetchers, flakes, `import` and file reads (no file system in the
//!   fuzzing host; the corpus and differential table cover them);
//! - duplicate attribute names and other parse errors;
//! - derivation attributes that are anything but literals: Determinate
//!   Nix's `derivationStrict` deep-forces its argument on worker threads
//!   first, which changes traces and thunk markers in error messages;
//! - recursion depths near `max-call-depth`: the exact threshold depends
//!   on the output mode's own frames and, in JSON mode, on which thread
//!   forced the value (the differential table pins the plain-mode
//!   thresholds);
//! - list lengths that would allocate gigabytes (`genList` gets a small
//!   count); an infinitely nested value is still generated and is accepted
//!   when Nix runs out of memory where brix reports `max-call-depth`.

use std::fmt::Write as _;

use rand::seq::IndexedRandom;
use rand::{RngExt, SeedableRng};
use rand_pcg::Pcg64;

/// A generated expression.
#[derive(Clone, Debug, PartialEq)]
pub enum Expr {
    Int(i64),
    Float(f64),
    Bool(bool),
    Null,
    /// `"…"`, with interpolations.
    Str(Vec<StrPart>),
    /// `''…''`: the string is rendered with the given indentation so that
    /// `stripIndentation` has work to do.
    IndStr {
        indent: usize,
        parts: Vec<StrPart>,
    },
    /// A path literal such as `./a` or `/abs/x`.
    Path(String),
    Var(String),
    List(Vec<Expr>),
    Attrs {
        rec: bool,
        binds: Vec<Bind>,
    },
    Select {
        subject: Box<Expr>,
        path: Vec<AttrKey>,
        default: Option<Box<Expr>>,
    },
    HasAttr(Box<Expr>, Vec<AttrKey>),
    Lambda {
        arg: Option<String>,
        formals: Option<Formals>,
        body: Box<Expr>,
    },
    Call(Box<Expr>, Vec<Expr>),
    Let(Vec<Bind>, Box<Expr>),
    With(Box<Expr>, Box<Expr>),
    If(Box<Expr>, Box<Expr>, Box<Expr>),
    Assert(Box<Expr>, Box<Expr>),
    /// `!e` or `-e`.
    Unary(&'static str, Box<Expr>),
    Binary(&'static str, Box<Expr>, Box<Expr>),
    /// `builtins.<name> args…`
    Builtin(String, Vec<Expr>),
}

#[derive(Clone, Debug, PartialEq)]
pub enum StrPart {
    Lit(String),
    Interp(Expr),
}

#[derive(Clone, Debug, PartialEq)]
pub enum AttrKey {
    /// `a`
    Ident(String),
    /// `"a b"`
    Quoted(String),
    /// `${e}`
    Dynamic(Expr),
}

#[derive(Clone, Debug, PartialEq)]
pub enum Bind {
    Plain(Vec<AttrKey>, Expr),
    Inherit(Vec<String>),
    InheritFrom(Expr, Vec<String>),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Formals {
    pub params: Vec<(String, Option<Expr>)>,
    pub ellipsis: bool,
}

/// The kind of value a builtin argument position expects. Generation
/// honours it most of the time and deliberately violates it sometimes, so
/// type-error messages are compared too.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ty {
    Any,
    Int,
    Num,
    Str,
    Bool,
    List,
    ListOfStr,
    ListOfAttrs,
    ListOfLists,
    /// A list of `{ name; value; }` sets.
    NameValueList,
    Attrs,
    /// A small non-negative count (occasionally -1): the length given to
    /// allocating builtins such as `genList`, so that no case asks for
    /// gigabytes.
    SmallInt,
    /// A one-argument function.
    Fn1,
    /// A two-argument function.
    Fn2,
    /// A comparator for `sort`: two arguments, boolean result.
    Comparator,
    /// A string usable as a POSIX ERE without alternation.
    Regex,
    /// Valid JSON text.
    Json,
    /// Valid TOML text.
    Toml,
    HashAlgo,
    /// A `{ hash; toHashFormat; hashAlgo?; }` set for `convertHash`.
    ConvertHashArgs,
    /// Arguments for `derivation`.
    DerivationArgs,
    /// A `{ startSet; operator; }` set for `genericClosure`.
    ClosureArgs,
}

/// Builtins called by the generator, with their argument types. Impure and
/// file-system builtins are left out (see the module documentation).
pub const BUILTINS: &[(&str, &[Ty])] = &[
    ("abort", &[Ty::Str]),
    ("add", &[Ty::Num, Ty::Num]),
    ("addErrorContext", &[Ty::Str, Ty::Any]),
    ("all", &[Ty::Fn1, Ty::List]),
    ("any", &[Ty::Fn1, Ty::List]),
    ("attrNames", &[Ty::Attrs]),
    ("attrValues", &[Ty::Attrs]),
    ("baseNameOf", &[Ty::Str]),
    ("bitAnd", &[Ty::Int, Ty::Int]),
    ("bitOr", &[Ty::Int, Ty::Int]),
    ("bitXor", &[Ty::Int, Ty::Int]),
    ("catAttrs", &[Ty::Str, Ty::ListOfAttrs]),
    ("ceil", &[Ty::Num]),
    ("compareVersions", &[Ty::Str, Ty::Str]),
    ("concatLists", &[Ty::ListOfLists]),
    ("concatMap", &[Ty::Fn1, Ty::List]),
    ("concatStringsSep", &[Ty::Str, Ty::ListOfStr]),
    ("convertHash", &[Ty::ConvertHashArgs]),
    ("deepSeq", &[Ty::Any, Ty::Any]),
    ("derivation", &[Ty::DerivationArgs]),
    ("dirOf", &[Ty::Str]),
    ("div", &[Ty::Num, Ty::Num]),
    ("elem", &[Ty::Any, Ty::List]),
    ("elemAt", &[Ty::List, Ty::Int]),
    ("filter", &[Ty::Fn1, Ty::List]),
    ("filterAttrs", &[Ty::Fn2, Ty::Attrs]),
    ("floor", &[Ty::Num]),
    ("foldl'", &[Ty::Fn2, Ty::Any, Ty::List]),
    ("fromJSON", &[Ty::Json]),
    ("fromTOML", &[Ty::Toml]),
    ("functionArgs", &[Ty::Fn1]),
    ("genList", &[Ty::Fn1, Ty::SmallInt]),
    ("genericClosure", &[Ty::ClosureArgs]),
    ("getAttr", &[Ty::Str, Ty::Attrs]),
    ("getContext", &[Ty::Str]),
    ("getEnv", &[Ty::Str]),
    ("groupBy", &[Ty::Fn1, Ty::List]),
    ("hasAttr", &[Ty::Str, Ty::Attrs]),
    ("hasContext", &[Ty::Str]),
    ("hashString", &[Ty::HashAlgo, Ty::Str]),
    ("head", &[Ty::List]),
    ("intersectAttrs", &[Ty::Attrs, Ty::Attrs]),
    ("isAttrs", &[Ty::Any]),
    ("isBool", &[Ty::Any]),
    ("isFloat", &[Ty::Any]),
    ("isFunction", &[Ty::Any]),
    ("isInt", &[Ty::Any]),
    ("isList", &[Ty::Any]),
    ("isNull", &[Ty::Any]),
    ("isPath", &[Ty::Any]),
    ("isString", &[Ty::Any]),
    ("langVersion", &[]),
    ("length", &[Ty::List]),
    ("lessThan", &[Ty::Any, Ty::Any]),
    ("listToAttrs", &[Ty::NameValueList]),
    ("map", &[Ty::Fn1, Ty::List]),
    ("mapAttrs", &[Ty::Fn2, Ty::Attrs]),
    ("match", &[Ty::Regex, Ty::Str]),
    ("mul", &[Ty::Num, Ty::Num]),
    ("nixPath", &[]),
    ("nixVersion", &[]),
    ("parseDrvName", &[Ty::Str]),
    ("partition", &[Ty::Fn1, Ty::List]),
    ("placeholder", &[Ty::Str]),
    ("removeAttrs", &[Ty::Attrs, Ty::ListOfStr]),
    ("replaceStrings", &[Ty::ListOfStr, Ty::ListOfStr, Ty::Str]),
    ("seq", &[Ty::Any, Ty::Any]),
    ("sort", &[Ty::Comparator, Ty::List]),
    ("split", &[Ty::Regex, Ty::Str]),
    ("splitVersion", &[Ty::Str]),
    ("storeDir", &[]),
    ("stringLength", &[Ty::Str]),
    ("sub", &[Ty::Num, Ty::Num]),
    ("substring", &[Ty::Int, Ty::Int, Ty::Str]),
    ("tail", &[Ty::List]),
    ("throw", &[Ty::Str]),
    ("toFile", &[Ty::Str, Ty::Str]),
    ("toJSON", &[Ty::Any]),
    ("toPath", &[Ty::Str]),
    ("toString", &[Ty::Any]),
    ("toXML", &[Ty::Any]),
    ("tryEval", &[Ty::Any]),
    ("typeOf", &[Ty::Any]),
    ("unsafeDiscardStringContext", &[Ty::Str]),
    ("unsafeGetAttrPos", &[Ty::Str, Ty::Attrs]),
    ("zipAttrsWith", &[Ty::Fn2, Ty::ListOfAttrs]),
];

const IDENTS: &[&str] = &[
    "a", "b", "c", "x", "y", "f", "g", "foo", "bar", "z1", "a-b", "_u",
];

/// Pieces strings are built from: plain text plus every character the
/// lexer and the printers treat specially.
const STR_PIECES: &[&str] = &[
    "a",
    "b",
    "foo",
    "bar",
    " ",
    "  ",
    "\n",
    "\t",
    "\r",
    "\"",
    "\\",
    "$",
    "${",
    "'",
    "''",
    "é",
    "ü",
    "日本",
    "0",
    "1.5",
    "-",
    "/",
    ".",
    ":",
    "\u{1}",
    "\u{7f}",
    "x y",
    "a\nb",
    "/nix/store/",
    "😀",
];

const HASH_ALGOS: &[&str] = &["md5", "sha1", "sha256", "sha512", "sha3", ""];

/// Interesting integers: around zero, around the 64-bit limits, and some
/// ordinary ones.
const INTS: &[i64] = &[
    0,
    1,
    -1,
    2,
    7,
    10,
    42,
    -7,
    100,
    1000,
    65536,
    i64::MAX,
    i64::MIN,
    i64::MAX - 1,
    4611686018427387904,
    -4611686018427387904,
    3037000499,
    3037000500,
];

const FLOATS: &[f64] = &[
    0.0,
    -0.0,
    0.5,
    1.0,
    1.5,
    -1.5,
    0.1,
    0.2,
    0.3,
    1e-5,
    1.5e-7,
    123456.0,
    1234567.0,
    12345678.9,
    1e15,
    1e16,
    1e21,
    1e30,
    1e308,
    2.5e-300,
    std::f64::consts::PI,
    100.0,
    1e6,
    999999.5,
    // Rendered as an overflowing product, and in JSON text as `1e400`,
    // which `fromJSON` rejects as Nix does.
    f64::INFINITY,
];

/// The random generator. One per case, seeded, so every expression is
/// reproducible from its seed (PCG: a seed gives the same numbers with
/// every rand version, which StdRng does not promise).
pub struct Gen {
    rng: Pcg64,
    /// Remaining node budget; the tree grows until it is spent.
    budget: usize,
    /// Variables bound by enclosing `let`/`rec`/lambdas.
    scope: Vec<String>,
    /// Names made available by enclosing `with` expressions.
    with_scope: Vec<String>,
    depth: usize,
}

impl Gen {
    pub fn new(seed: u64, budget: usize) -> Gen {
        Gen {
            rng: Pcg64::seed_from_u64(seed),
            budget,
            scope: Vec::new(),
            with_scope: Vec::new(),
            depth: 0,
        }
    }

    /// Generates one complete expression.
    pub fn expr(&mut self) -> Expr {
        let mut e = self.any();
        // Sometimes wrap the whole thing: a trace at the outermost position
        // is deterministic, and a deep-force through tryEval is a classic
        // source of differences.
        match self.rng.random_range(0..12) {
            0 => e = Expr::Builtin("trace".into(), vec![self.leaf(Ty::Str), e]),
            1 => e = Expr::Builtin("tryEval".into(), vec![e]),
            2 => e = Expr::Builtin("deepSeq".into(), vec![e.clone(), e]),
            _ => {}
        }
        e
    }

    fn spend(&mut self) -> bool {
        if self.budget == 0 || self.depth > 14 {
            return false;
        }
        self.budget -= 1;
        true
    }

    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        items.choose(&mut self.rng).expect("non-empty")
    }

    #[allow(clippy::explicit_auto_deref)] // `T` would be inferred as `str` otherwise
    fn pick_str(&mut self, items: &[&'static str]) -> &'static str {
        *items.choose(&mut self.rng).expect("non-empty")
    }

    fn chance(&mut self, p: f64) -> bool {
        self.rng.random_bool(p)
    }

    fn ident(&mut self) -> String {
        (*self.pick(IDENTS)).to_owned()
    }

    fn nested<T>(&mut self, f: impl FnOnce(&mut Gen) -> T) -> T {
        self.depth += 1;
        let r = f(self);
        self.depth -= 1;
        r
    }

    /// A builtin call with arguments of the types in the table.
    fn table_call(&mut self, name: &str) -> Expr {
        let (_, tys) = BUILTINS
            .iter()
            .find(|(n, _)| *n == name)
            .expect("builtin in table");
        let args = tys.iter().map(|t| self.nested(|g| g.of_type(*t))).collect();
        Expr::Builtin(name.to_owned(), args)
    }

    fn lit_str(s: &str) -> Expr {
        Expr::Str(vec![StrPart::Lit(s.to_owned())])
    }

    fn bind(name: &str, value: Expr) -> Bind {
        Bind::Plain(vec![AttrKey::Ident(name.to_owned())], value)
    }

    // ----- leaves ---------------------------------------------------------

    /// A small expression of the given type with no sub-expressions worth
    /// shrinking (used when the budget is spent).
    fn leaf(&mut self, ty: Ty) -> Expr {
        match ty {
            Ty::Int => Expr::Int(*self.pick(INTS)),
            Ty::SmallInt => {
                if self.chance(0.05) {
                    Expr::Int(-1)
                } else {
                    Expr::Int(self.rng.random_range(0..=10))
                }
            }
            Ty::Num => {
                if self.chance(0.7) {
                    Expr::Int(*self.pick(INTS))
                } else {
                    Expr::Float(*self.pick(FLOATS))
                }
            }
            Ty::Str => Expr::Str(vec![StrPart::Lit(self.text(3))]),
            Ty::Bool => Expr::Bool(self.chance(0.5)),
            Ty::List | Ty::ListOfStr | Ty::ListOfAttrs | Ty::ListOfLists | Ty::NameValueList => {
                Expr::List(vec![])
            }
            Ty::Attrs => Expr::Attrs {
                rec: false,
                binds: vec![],
            },
            Ty::Fn1 => Expr::Lambda {
                arg: Some("v".into()),
                formals: None,
                body: Box::new(Expr::Var("v".into())),
            },
            Ty::Fn2 | Ty::Comparator => Expr::Lambda {
                arg: Some("p".into()),
                formals: None,
                body: Box::new(Expr::Lambda {
                    arg: Some("q".into()),
                    formals: None,
                    body: Box::new(if ty == Ty::Comparator {
                        Expr::Binary(
                            "<",
                            Box::new(Expr::Var("p".into())),
                            Box::new(Expr::Var("q".into())),
                        )
                    } else {
                        Expr::Var("p".into())
                    }),
                }),
            },
            Ty::Regex => {
                let r = self.regex();
                Gen::lit_str(&r)
            }
            Ty::Json => {
                let j = self.json(2);
                Gen::lit_str(&j)
            }
            Ty::Toml => {
                let t = self.toml();
                Gen::lit_str(&t)
            }
            Ty::HashAlgo => Gen::lit_str(self.pick_str(HASH_ALGOS)),
            Ty::ConvertHashArgs => self.convert_hash_args(),
            Ty::DerivationArgs => self.derivation_args(),
            Ty::ClosureArgs => self.closure_args(),
            Ty::Any => match self.rng.random_range(0..7) {
                0 => Expr::Null,
                1 => Expr::Bool(self.chance(0.5)),
                2 => Expr::Int(*self.pick(INTS)),
                3 => Expr::Float(*self.pick(FLOATS)),
                4 => Expr::Str(vec![StrPart::Lit(self.text(3))]),
                5 => self.path(),
                _ => self.var_or(Ty::Int),
            },
        }
    }

    fn var_or(&mut self, fallback: Ty) -> Expr {
        let all: Vec<String> = self
            .scope
            .iter()
            .chain(self.with_scope.iter())
            .cloned()
            .collect();
        if all.is_empty() || self.chance(0.1) {
            if self.chance(0.03) {
                // An undefined variable: a compile-time error in Nix.
                return Expr::Var("undefinedVariable".into());
            }
            return self.leaf(fallback);
        }
        Expr::Var(self.pick(&all).clone())
    }

    fn text(&mut self, max_pieces: usize) -> String {
        let n = self.rng.random_range(0..=max_pieces);
        (0..n).map(|_| *self.pick(STR_PIECES)).collect()
    }

    fn path(&mut self) -> Expr {
        let p = self.pick(&[
            "./a",
            "./a/b.nix",
            "/abs/x",
            "../up",
            "./.",
            "./a/../b",
            "/nix/store/x",
        ]);
        Expr::Path((*p).to_owned())
    }

    /// A POSIX ERE without alternation (see the module documentation).
    fn regex(&mut self) -> String {
        let atoms = [
            "a",
            "b",
            ".",
            "[a-z]",
            "[^0-9]",
            "[[:space:]]",
            "x",
            "(a)",
            "(b*)",
            "([a-c]+)",
            "\\$",
            "\\.",
            "o",
        ];
        let quants = ["", "", "*", "+", "?", "{1,2}"];
        let n = self.rng.random_range(1..=4);
        let mut s = String::new();
        for _ in 0..n {
            s.push_str(self.pick_str(&atoms));
            s.push_str(self.pick_str(&quants));
        }
        if self.chance(0.2) {
            s.insert(0, '^');
        }
        s
    }

    fn json(&mut self, depth: usize) -> String {
        match self.rng.random_range(0..if depth == 0 { 6 } else { 9 }) {
            0 => "null".into(),
            1 => "true".into(),
            2 => (*self.pick(INTS)).to_string(),
            3 => format!("{:?}", *self.pick(FLOATS)).replace("inf", "1e400"),
            4 => {
                let t = self.text(3);
                format!("\"{}\"", json_escape(&t))
            }
            5 => "-0".into(),
            6 => {
                let n = self.rng.random_range(0..3);
                let items: Vec<String> = (0..n).map(|_| self.json(depth - 1)).collect();
                format!("[{}]", items.join(", "))
            }
            _ => {
                let n = self.rng.random_range(0..3);
                let items: Vec<String> = (0..n)
                    .map(|_| {
                        let k = self.ident();
                        format!("\"{k}\": {}", self.json(depth - 1))
                    })
                    .collect();
                format!("{{{}}}", items.join(", "))
            }
        }
    }

    fn toml(&mut self) -> String {
        let lines = [
            "a = 1",
            "b = \"x\"",
            "c = [1, 2, 3]",
            "d = true",
            "e = 1.5",
            "f = 1979-05-27T07:32:00Z",
            "g = { x = 1, y = \"z\" }",
            "[t]\nh = \"i\"",
            "[[arr]]\nk = 1\n[[arr]]\nk = 2",
            "neg = -0.0",
            "big = 9223372036854775807",
            "s = '''\nmulti\nline\n'''",
            "u = \"\\u00e9\"",
            "inf = inf",
            "nan = nan",
            "date = 1979-05-27",
            "time = 07:32:00",
        ];
        let n = self.rng.random_range(0..=4);
        let mut used: Vec<&str> = Vec::new();
        for _ in 0..n {
            let l = *self.pick(&lines);
            if !used.contains(&l) {
                used.push(l);
            }
        }
        // Tables must come after plain keys.
        used.sort_by_key(|l| l.starts_with('['));
        used.join("\n") + "\n"
    }

    fn convert_hash_args(&mut self) -> Expr {
        let hashes = [
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824",
            "sha256-LPJNul+wow4m6DsqxbninhsWHlwfp0JecwQzYpOLmCQ=",
            "sha256:2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824",
            "d41d8cd98f00b204e9800998ecf8427e",
            "a9993e364706816aba3e25717850c26c9cd0d89d",
            "0000000000000000000000000000000000000000000000000000000000000000",
            "not-a-hash",
            "",
        ];
        let formats = ["base16", "nix32", "base32", "base64", "sri", "hex", ""];
        let mut binds = vec![
            Gen::bind("hash", Gen::lit_str(self.pick_str(&hashes))),
            Gen::bind("toHashFormat", Gen::lit_str(self.pick_str(&formats))),
        ];
        if self.chance(0.5) {
            let algo = self.leaf(Ty::HashAlgo);
            binds.push(Gen::bind("hashAlgo", algo));
        }
        Expr::Attrs { rec: false, binds }
    }

    fn derivation_args(&mut self) -> Expr {
        let mut binds = Vec::new();
        if self.chance(0.95) {
            let name = *self.pick(&["x", "hello-1.0", "a.drv", "foo bar", "", "-bad", "ok_name"]);
            binds.push(Gen::bind("name", Gen::lit_str(name)));
        }
        if self.chance(0.95) {
            binds.push(Gen::bind("builder", Gen::lit_str("/bin/sh")));
        }
        if self.chance(0.95) {
            binds.push(Gen::bind("system", Gen::lit_str("x86_64-linux")));
        }
        if self.chance(0.4) {
            #[allow(clippy::explicit_auto_deref)] // inference needs the deref
            let outs: &[&str] = *self.pick(&[
                &["out"][..],
                &["out", "dev"],
                &["lib"],
                &[],
                &["drv"],
                &["out", "out"],
            ]);
            let list = outs.iter().map(|s| Gen::lit_str(s)).collect();
            binds.push(Gen::bind("outputs", Expr::List(list)));
        }
        if self.chance(0.4) {
            let arg = self.leaf(Ty::Str);
            binds.push(Gen::bind("args", Expr::List(vec![Gen::lit_str("-c"), arg])));
        }
        if self.chance(0.5) {
            // Literal values only: Determinate Nix's `derivationStrict`
            // deep-forces its argument on other threads first, so an
            // attribute that refers back to the derivation, or a nested set
            // holding unevaluated thunks, produces traces and «potential
            // infinite recursion» markers that depend on scheduling (see
            // KNOWN_DEVIATIONS.md, "Parallel evaluation").
            let v = match self.rng.random_range(0..4) {
                0 => Expr::List(vec![self.leaf(Ty::Str), self.leaf(Ty::Int)]),
                1 => Expr::Attrs {
                    rec: false,
                    binds: vec![Gen::bind("x", Gen::lit_str(""))],
                },
                _ => self.leaf(Ty::Any),
            };
            let name = *self.pick(&["extra", "dep", "env1"]);
            binds.push(Gen::bind(name, v));
        }
        if self.chance(0.2) {
            binds.push(Gen::bind(
                "outputHash",
                Gen::lit_str("sha256-LPJNul+wow4m6DsqxbninhsWHlwfp0JecwQzYpOLmCQ="),
            ));
            if self.chance(0.5) {
                let mode = *self.pick(&["flat", "recursive", "nar", "text", "git", "bad"]);
                binds.push(Gen::bind("outputHashMode", Gen::lit_str(mode)));
            }
            if self.chance(0.5) {
                let algo = self.leaf(Ty::HashAlgo);
                binds.push(Gen::bind("outputHashAlgo", algo));
            }
        }
        if self.chance(0.15) {
            binds.push(Gen::bind("__structuredAttrs", Expr::Bool(true)));
        }
        if self.chance(0.15) {
            binds.push(Gen::bind("__ignoreNulls", Expr::Bool(true)));
            binds.push(Gen::bind("n", Expr::Null));
        }
        if self.chance(0.15) {
            binds.push(Gen::bind(
                "passAsFile",
                Expr::List(vec![Gen::lit_str("extra")]),
            ));
        }
        Expr::Attrs { rec: false, binds }
    }

    fn closure_args(&mut self) -> Expr {
        let key = self.leaf(Ty::Any);
        let start = Expr::List(vec![Expr::Attrs {
            rec: false,
            binds: vec![Gen::bind("key", key)],
        }]);
        let n_key = || Expr::Select {
            subject: Box::new(Expr::Var("n".into())),
            path: vec![AttrKey::Ident("key".into())],
            default: None,
        };
        let op = Expr::Lambda {
            arg: Some("n".into()),
            formals: None,
            body: Box::new(Expr::If(
                Box::new(Expr::Binary("<", Box::new(n_key()), Box::new(Expr::Int(3)))),
                Box::new(Expr::List(vec![Expr::Attrs {
                    rec: false,
                    binds: vec![Gen::bind(
                        "key",
                        Expr::Binary("+", Box::new(n_key()), Box::new(Expr::Int(1))),
                    )],
                }])),
                Box::new(Expr::List(vec![])),
            )),
        };
        Expr::Attrs {
            rec: false,
            binds: vec![Gen::bind("startSet", start), Gen::bind("operator", op)],
        }
    }

    // ----- compound expressions -------------------------------------------

    /// An expression that should have type `ty` (with a small chance of
    /// deliberately being of another type).
    pub fn of_type(&mut self, ty: Ty) -> Expr {
        if ty != Ty::Any && self.chance(0.08) {
            return match ty {
                // A random *string* here would be invalid JSON/TOML or an
                // invalid regex, whose parse-error texts come from
                // different libraries (KNOWN_DEVIATIONS.md); a non-string
                // still exercises the type error.
                Ty::Json | Ty::Toml | Ty::Regex => match self.rng.random_range(0..3) {
                    0 => Expr::Null,
                    1 => Expr::Int(*self.pick(INTS)),
                    _ => Expr::Bool(true),
                },
                _ => self.any(),
            };
        }
        if !self.spend() {
            return self.leaf(ty);
        }
        match ty {
            Ty::Any => self.any_compound(),
            Ty::Int | Ty::Num => self.number(ty),
            Ty::SmallInt => self.leaf(ty),
            Ty::Str => self.string(),
            Ty::Bool => self.boolean(),
            Ty::List => self.list(Ty::Any),
            Ty::ListOfStr => self.list(Ty::Str),
            Ty::ListOfAttrs => self.list(Ty::Attrs),
            Ty::ListOfLists => self.list(Ty::List),
            Ty::NameValueList => self.name_value_list(),
            Ty::Attrs => self.attrs(),
            Ty::Fn1 => self.lambda(1, false),
            Ty::Fn2 => self.lambda(2, false),
            Ty::Comparator => self.lambda(2, true),
            Ty::Regex
            | Ty::Json
            | Ty::Toml
            | Ty::HashAlgo
            | Ty::ConvertHashArgs
            | Ty::DerivationArgs
            | Ty::ClosureArgs => self.leaf(ty),
        }
    }

    fn any(&mut self) -> Expr {
        if !self.spend() {
            return self.leaf(Ty::Any);
        }
        self.any_compound()
    }

    fn any_compound(&mut self) -> Expr {
        match self.rng.random_range(0..22) {
            0 | 1 => self.number(Ty::Num),
            2 | 3 => self.string(),
            4 => self.boolean(),
            5 => self.list(Ty::Any),
            6 | 7 => self.attrs(),
            8 => self.select(),
            9 => {
                let arity = self.rng.random_range(1..=2);
                self.lambda(arity, false)
            }
            10 => self.call(),
            11 | 12 => self.let_(),
            13 => self.with(),
            14 => self.if_(),
            15 => self.assert_(),
            16..=18 => self.builtin_call(),
            19 => self.recursion(),
            20 => self.path(),
            _ => self.var_or(Ty::Any),
        }
    }

    fn name_value_list(&mut self) -> Expr {
        let n = self.rng.random_range(0..=3);
        let items = (0..n)
            .map(|_| {
                let name = if self.chance(0.9) {
                    let i = self.ident();
                    Gen::lit_str(&i)
                } else {
                    self.leaf(Ty::Any)
                };
                let value = self.nested(|g| g.of_type(Ty::Any));
                let mut binds = vec![Gen::bind("name", name)];
                if self.chance(0.95) {
                    binds.push(Gen::bind("value", value));
                }
                Expr::Attrs { rec: false, binds }
            })
            .collect();
        Expr::List(items)
    }

    fn number(&mut self, ty: Ty) -> Expr {
        match self.rng.random_range(0..8) {
            0..=3 => {
                let op = *self.pick(&["+", "-", "*", "/"]);
                let l = self.nested(|g| g.of_type(ty));
                let r = self.nested(|g| g.of_type(ty));
                Expr::Binary(op, Box::new(l), Box::new(r))
            }
            4 => Expr::Unary("-", Box::new(self.nested(|g| g.of_type(ty)))),
            5 => {
                let name = *self.pick(&["add", "sub", "mul", "div", "bitAnd", "bitOr", "bitXor"]);
                let l = self.nested(|g| g.of_type(ty));
                let r = self.nested(|g| g.of_type(ty));
                Expr::Builtin(name.into(), vec![l, r])
            }
            6 => {
                let name = *self.pick(&["length", "stringLength", "floor", "ceil"]);
                self.table_call(name)
            }
            _ => self.leaf(ty),
        }
    }

    fn string(&mut self) -> Expr {
        match self.rng.random_range(0..7) {
            0 | 1 => Expr::Str(self.str_parts(false)),
            2 => {
                let parts = self.str_parts(true);
                let indent = self.rng.random_range(0..5);
                Expr::IndStr { indent, parts }
            }
            6 => {
                // A string carrying a derivation's context: `"${drv}"`,
                // `drv.drvPath` or `drv.outPath`, so the context builtins
                // (`getContext`, `hasContext`, `unsafeDiscardStringContext`,
                // `derivation` inputs) see real contexts.
                let drv = Expr::Builtin("derivation".into(), vec![self.derivation_args()]);
                match self.rng.random_range(0..3) {
                    0 => Expr::Str(vec![StrPart::Interp(drv)]),
                    1 => Expr::Select {
                        subject: Box::new(drv),
                        path: vec![AttrKey::Ident("drvPath".into())],
                        default: None,
                    },
                    _ => Expr::Select {
                        subject: Box::new(drv),
                        path: vec![AttrKey::Ident("outPath".into())],
                        default: None,
                    },
                }
            }
            3 => {
                let l = self.nested(|g| g.of_type(Ty::Str));
                let r = self.nested(|g| g.of_type(Ty::Str));
                Expr::Binary("+", Box::new(l), Box::new(r))
            }
            4 => {
                let a = self.nested(|g| g.of_type(Ty::Any));
                Expr::Builtin("toString".into(), vec![a])
            }
            _ => {
                let name = *self.pick(&[
                    "substring",
                    "concatStringsSep",
                    "replaceStrings",
                    "toJSON",
                    "baseNameOf",
                    "dirOf",
                    "hashString",
                    "unsafeDiscardStringContext",
                ]);
                self.table_call(name)
            }
        }
    }

    fn str_parts(&mut self, indented: bool) -> Vec<StrPart> {
        let n = self.rng.random_range(0..=4);
        (0..n)
            .map(|_| {
                if self.chance(0.3) {
                    StrPart::Interp(self.nested(|g| g.of_type(Ty::Str)))
                } else {
                    let mut t = self.text(3);
                    if indented && self.chance(0.5) {
                        t.push('\n');
                        let pad = self.rng.random_range(0..6);
                        t.push_str(&" ".repeat(pad));
                    }
                    StrPart::Lit(t)
                }
            })
            .collect()
    }

    fn boolean(&mut self) -> Expr {
        match self.rng.random_range(0..7) {
            0 | 1 => {
                let op = *self.pick(&["==", "!=", "<", ">", "<=", ">="]);
                let l = self.nested(|g| g.of_type(Ty::Any));
                let r = self.nested(|g| g.of_type(Ty::Any));
                Expr::Binary(op, Box::new(l), Box::new(r))
            }
            2 => {
                let op = *self.pick(&["&&", "||", "->"]);
                let l = self.nested(|g| g.of_type(Ty::Bool));
                let r = self.nested(|g| g.of_type(Ty::Bool));
                Expr::Binary(op, Box::new(l), Box::new(r))
            }
            3 => Expr::Unary("!", Box::new(self.nested(|g| g.of_type(Ty::Bool)))),
            4 => {
                let subject = self.nested(|g| g.of_type(Ty::Attrs));
                let path = self.attr_path();
                Expr::HasAttr(Box::new(subject), path)
            }
            5 => {
                let name = *self.pick(&[
                    "isNull",
                    "isInt",
                    "isString",
                    "isList",
                    "isAttrs",
                    "isFunction",
                    "isBool",
                    "isFloat",
                    "isPath",
                    "elem",
                    "hasAttr",
                    "any",
                    "all",
                    "hasContext",
                ]);
                self.table_call(name)
            }
            _ => self.leaf(Ty::Bool),
        }
    }

    fn list(&mut self, elem: Ty) -> Expr {
        match self.rng.random_range(0..6) {
            0..=2 => {
                let n = self.rng.random_range(0..=4);
                Expr::List((0..n).map(|_| self.nested(|g| g.of_type(elem))).collect())
            }
            3 => {
                let l = self.nested(|g| g.of_type(Ty::List));
                let r = self.nested(|g| g.of_type(Ty::List));
                Expr::Binary("++", Box::new(l), Box::new(r))
            }
            _ => {
                let name = *self.pick(&[
                    "map",
                    "filter",
                    "genList",
                    "sort",
                    "concatLists",
                    "concatMap",
                    "tail",
                    "attrNames",
                    "attrValues",
                    "catAttrs",
                    "splitVersion",
                    "match",
                    "split",
                    "partition",
                    "genericClosure",
                ]);
                self.table_call(name)
            }
        }
    }

    fn attr_key(&mut self) -> AttrKey {
        match self.rng.random_range(0..10) {
            0 => AttrKey::Quoted(self.text(2)),
            1 => {
                let mut e = self.nested(|g| g.of_type(Ty::Str));
                // `${"lit"}` is a static key to the parser and could duplicate
                // another; keep dynamic keys genuinely dynamic.
                if matches!(&e, Expr::Str(parts) if parts.iter().all(|p| matches!(p, StrPart::Lit(_))))
                {
                    e = Expr::Binary("+", Box::new(e), Box::new(Gen::lit_str("")));
                }
                AttrKey::Dynamic(e)
            }
            2 => AttrKey::Quoted(self.ident()),
            _ => AttrKey::Ident(self.ident()),
        }
    }

    fn attr_path(&mut self) -> Vec<AttrKey> {
        let n = self.rng.random_range(1..=2);
        (0..n).map(|_| self.attr_key()).collect()
    }

    /// Bindings whose static names avoid `used` (a duplicate attribute is a
    /// parse error, which is not what this harness tests).
    fn binds(&mut self, allow_dynamic: bool, used: &mut Vec<String>) -> Vec<Bind> {
        let n = self.rng.random_range(0..=4);
        let mut binds = Vec::new();
        for _ in 0..n {
            match self.rng.random_range(0..10) {
                0 if !self.scope.is_empty() => {
                    let v = self.scope[self.rng.random_range(0..self.scope.len())].clone();
                    if used.contains(&v) {
                        continue;
                    }
                    used.push(v.clone());
                    binds.push(Bind::Inherit(vec![v]));
                }
                1 => {
                    let from = self.nested(|g| g.of_type(Ty::Attrs));
                    let name = self.ident();
                    if used.contains(&name) {
                        continue;
                    }
                    used.push(name.clone());
                    binds.push(Bind::InheritFrom(from, vec![name]));
                }
                _ => {
                    let mut path = self.attr_path();
                    if !allow_dynamic {
                        for k in &mut path {
                            if matches!(k, AttrKey::Dynamic(_)) {
                                *k = AttrKey::Ident(self.ident());
                            }
                        }
                    }
                    if let AttrKey::Ident(n) | AttrKey::Quoted(n) = &path[0] {
                        if used.contains(n) {
                            continue;
                        }
                        used.push(n.clone());
                    }
                    let value = self.nested(|g| g.of_type(Ty::Any));
                    binds.push(Bind::Plain(path, value));
                }
            }
        }
        binds
    }

    /// The statically known names a set of bindings introduces.
    fn bound_names(binds: &[Bind]) -> Vec<String> {
        let mut names = Vec::new();
        for b in binds {
            match b {
                Bind::Plain(path, _) => match &path[0] {
                    AttrKey::Ident(n) => names.push(n.clone()),
                    // A quoted name can only be referred to as a variable
                    // if it happens to be an identifier (`"foo"`, not `""`).
                    AttrKey::Quoted(n) if IDENTS.contains(&n.as_str()) => names.push(n.clone()),
                    AttrKey::Quoted(_) | AttrKey::Dynamic(_) => {}
                },
                Bind::Inherit(ns) | Bind::InheritFrom(_, ns) => names.extend(ns.iter().cloned()),
            }
        }
        names
    }

    /// Distinct identifiers for a binding block.
    fn fresh_names(&mut self) -> Vec<String> {
        let n = self.rng.random_range(1..=3);
        let mut names: Vec<String> = (0..n).map(|_| self.ident()).collect();
        names.sort();
        names.dedup();
        names
    }

    fn attrs(&mut self) -> Expr {
        if self.chance(0.3) {
            // Names are in scope inside the set itself; pick them first so
            // the values can refer to each other.
            let names = self.fresh_names();
            let saved = self.scope.len();
            self.scope.extend(names.iter().cloned());
            let mut binds: Vec<Bind> = names
                .iter()
                .map(|n| {
                    let v = self.nested(|g| g.of_type(Ty::Any));
                    Gen::bind(n, v)
                })
                .collect();
            let mut used = names.clone();
            binds.extend(self.binds(true, &mut used));
            self.scope.truncate(saved);
            Expr::Attrs { rec: true, binds }
        } else if self.chance(0.15) {
            let name = *self.pick(&[
                "intersectAttrs",
                "removeAttrs",
                "mapAttrs",
                "filterAttrs",
                "listToAttrs",
                "zipAttrsWith",
                "groupBy",
                "functionArgs",
                "parseDrvName",
                "fromJSON",
                "fromTOML",
                "tryEval",
                "unsafeGetAttrPos",
                "getContext",
            ]);
            self.table_call(name)
        } else if self.chance(0.15) {
            let l = self.nested(|g| g.of_type(Ty::Attrs));
            let r = self.nested(|g| g.of_type(Ty::Attrs));
            Expr::Binary("//", Box::new(l), Box::new(r))
        } else {
            let binds = self.binds(true, &mut Vec::new());
            Expr::Attrs { rec: false, binds }
        }
    }

    fn select(&mut self) -> Expr {
        let subject = self.nested(|g| g.of_type(Ty::Attrs));
        let path = self.attr_path();
        let default = if self.chance(0.4) {
            Some(Box::new(self.nested(|g| g.of_type(Ty::Any))))
        } else {
            None
        };
        Expr::Select {
            subject: Box::new(subject),
            path,
            default,
        }
    }

    fn lambda(&mut self, arity: usize, comparator: bool) -> Expr {
        if arity == 0 {
            return if comparator {
                let l = self.nested(|g| g.of_type(Ty::Any));
                let r = self.nested(|g| g.of_type(Ty::Any));
                let op = *self.pick(&["<", ">", "<=", "==", "!="]);
                let cmp = Expr::Binary(op, Box::new(l), Box::new(r));
                if self.chance(0.3) {
                    let msg = self.leaf(Ty::Str);
                    Expr::Builtin("trace".into(), vec![msg, cmp])
                } else {
                    cmp
                }
            } else {
                self.nested(|g| g.of_type(Ty::Any))
            };
        }
        let saved = self.scope.len();
        let (arg, formals) = if !comparator && self.chance(0.3) {
            let names = {
                let n = self.rng.random_range(0..=3);
                let mut v: Vec<String> = (0..n).map(|_| self.ident()).collect();
                v.sort();
                v.dedup();
                v
            };
            let params: Vec<(String, Option<Expr>)> = names
                .into_iter()
                .map(|name| {
                    let default = if self.chance(0.4) {
                        Some(self.nested(|g| g.of_type(Ty::Any)))
                    } else {
                        None
                    };
                    (name, default)
                })
                .collect();
            for (n, _) in &params {
                self.scope.push(n.clone());
            }
            let arg = if self.chance(0.3) {
                let a = format!("args{}", self.depth);
                self.scope.push(a.clone());
                Some(a)
            } else {
                None
            };
            let ellipsis = self.chance(0.5);
            (arg, Some(Formals { params, ellipsis }))
        } else {
            let name = if comparator {
                if arity == 2 {
                    "p".to_owned()
                } else {
                    "q".to_owned()
                }
            } else {
                self.ident()
            };
            self.scope.push(name.clone());
            (Some(name), None)
        };
        let body = self.lambda(arity - 1, comparator);
        self.scope.truncate(saved);
        Expr::Lambda {
            arg,
            formals,
            body: Box::new(body),
        }
    }

    fn call(&mut self) -> Expr {
        match self.rng.random_range(0..4) {
            0 => {
                // Calling a set with `__functor`.
                let body = self.nested(|g| g.of_type(Ty::Any));
                let functor = Expr::Lambda {
                    arg: Some("self".into()),
                    formals: None,
                    body: Box::new(Expr::Lambda {
                        arg: Some("arg".into()),
                        formals: None,
                        body: Box::new(body),
                    }),
                };
                let set = Expr::Attrs {
                    rec: false,
                    binds: vec![Gen::bind("__functor", functor)],
                };
                let arg = self.nested(|g| g.of_type(Ty::Any));
                Expr::Call(Box::new(set), vec![arg])
            }
            1 => {
                // Calling with an attribute set against formals.
                let f = self.nested(|g| g.of_type(Ty::Fn1));
                let arg = self.nested(|g| g.of_type(Ty::Attrs));
                Expr::Call(Box::new(f), vec![arg])
            }
            _ => {
                let arity = self.rng.random_range(1..=2);
                let f = self.nested(|g| g.of_type(if arity == 1 { Ty::Fn1 } else { Ty::Fn2 }));
                let args = (0..arity)
                    .map(|_| self.nested(|g| g.of_type(Ty::Any)))
                    .collect();
                Expr::Call(Box::new(f), args)
            }
        }
    }

    fn let_(&mut self) -> Expr {
        let names = self.fresh_names();
        let saved = self.scope.len();
        self.scope.extend(names.iter().cloned());
        let mut binds: Vec<Bind> = names
            .iter()
            .map(|n| {
                let v = self.nested(|g| g.of_type(Ty::Any));
                Gen::bind(n, v)
            })
            .collect();
        let mut used = names.clone();
        binds.extend(self.binds(false, &mut used));
        let body = self.nested(|g| g.of_type(Ty::Any));
        self.scope.truncate(saved);
        Expr::Let(binds, Box::new(body))
    }

    fn with(&mut self) -> Expr {
        let attrs = self.nested(|g| g.of_type(Ty::Attrs));
        let saved = self.with_scope.len();
        if let Expr::Attrs { binds, .. } = &attrs {
            let names = Gen::bound_names(binds);
            self.with_scope.extend(names);
        }
        let body = self.nested(|g| g.of_type(Ty::Any));
        self.with_scope.truncate(saved);
        Expr::With(Box::new(attrs), Box::new(body))
    }

    fn if_(&mut self) -> Expr {
        let c = self.nested(|g| g.of_type(Ty::Bool));
        let t = self.nested(|g| g.of_type(Ty::Any));
        let e = self.nested(|g| g.of_type(Ty::Any));
        Expr::If(Box::new(c), Box::new(t), Box::new(e))
    }

    fn assert_(&mut self) -> Expr {
        let c = self.nested(|g| g.of_type(Ty::Bool));
        let b = self.nested(|g| g.of_type(Ty::Any));
        Expr::Assert(Box::new(c), Box::new(b))
    }

    fn builtin_call(&mut self) -> Expr {
        let (name, _) = *self.pick(BUILTINS);
        let mut call = self.table_call(name);
        // Occasionally pass too few or too many arguments.
        if let Expr::Builtin(_, args) = &mut call {
            if self.chance(0.04) {
                args.pop();
            } else if self.chance(0.03) {
                let extra = self.leaf(Ty::Any);
                args.push(extra);
            }
        }
        call
    }

    /// Recursive definitions: deep (bounded) recursion, infinite recursion
    /// and self-referential data.
    fn recursion(&mut self) -> Expr {
        let var = |n: &str| Expr::Var(n.to_owned());
        match self.rng.random_range(0..5) {
            0 => {
                // let f = n: if n == 0 then 0 else f (n - 1); in f N
                // Well below `max-call-depth`: near the limit the outcome
                // depends on how many frames the output mode adds, and in
                // JSON mode on which thread Determinate Nix forced the value
                // (see tests/differential.rs for the exact-threshold cases).
                let depth = *self.pick(&[10, 100, 1000, 5000, 9000]);
                Expr::Let(
                    vec![Gen::bind(
                        "rec_f",
                        Expr::Lambda {
                            arg: Some("n".into()),
                            formals: None,
                            body: Box::new(Expr::If(
                                Box::new(Expr::Binary(
                                    "==",
                                    Box::new(var("n")),
                                    Box::new(Expr::Int(0)),
                                )),
                                Box::new(Expr::Int(0)),
                                Box::new(Expr::Call(
                                    Box::new(var("rec_f")),
                                    vec![Expr::Binary(
                                        "-",
                                        Box::new(var("n")),
                                        Box::new(Expr::Int(1)),
                                    )],
                                )),
                            )),
                        },
                    )],
                    Box::new(Expr::Call(Box::new(var("rec_f")), vec![Expr::Int(depth)])),
                )
            }
            1 => Expr::Let(vec![Gen::bind("inf", var("inf"))], Box::new(var("inf"))),
            2 => {
                let set = Expr::Attrs {
                    rec: true,
                    binds: vec![
                        Gen::bind(
                            "a",
                            Expr::Select {
                                subject: Box::new(var("b")),
                                path: vec![AttrKey::Ident("x".into())],
                                default: None,
                            },
                        ),
                        Gen::bind(
                            "b",
                            Expr::Attrs {
                                rec: false,
                                binds: vec![Gen::bind("x", var("a"))],
                            },
                        ),
                    ],
                };
                Expr::Select {
                    subject: Box::new(set),
                    path: vec![AttrKey::Ident("a".into())],
                    default: None,
                }
            }
            3 => {
                // A value with shared sub-values, printed with «repeated».
                Expr::Attrs {
                    rec: true,
                    binds: vec![
                        Gen::bind("self", var("self_")),
                        Gen::bind(
                            "self_",
                            Expr::Attrs {
                                rec: false,
                                binds: vec![Gen::bind("v", Expr::Int(1))],
                            },
                        ),
                        Gen::bind("twice", Expr::List(vec![var("self_"), var("self_")])),
                    ],
                }
            }
            _ => {
                // Mutual recursion through a list.
                Expr::Let(
                    vec![
                        Gen::bind("xs", Expr::List(vec![Expr::Int(1), var("ys")])),
                        Gen::bind("ys", Expr::List(vec![Expr::Int(2), var("xs")])),
                    ],
                    Box::new(Expr::Builtin(
                        "length".into(),
                        vec![Expr::Builtin(
                            "elemAt".into(),
                            vec![var("xs"), Expr::Int(1)],
                        )],
                    )),
                )
            }
        }
    }
}

fn json_escape(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c if (c as u32) < 0x20 || c == '\u{7f}' => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out
}

// ----- rendering ------------------------------------------------------------

impl Expr {
    /// The expression as Nix source.
    pub fn render(&self) -> String {
        let mut out = String::new();
        self.write(&mut out);
        out
    }

    fn is_atomic(&self) -> bool {
        match self {
            Expr::Int(n) => *n >= 0,
            Expr::Float(f) => f.is_finite() && !f.is_sign_negative(),
            Expr::Bool(_)
            | Expr::Null
            | Expr::Str(_)
            | Expr::IndStr { .. }
            | Expr::Path(_)
            | Expr::Var(_)
            | Expr::List(_)
            | Expr::Attrs { .. } => true,
            _ => false,
        }
    }

    /// Writes the expression parenthesised unless it is atomic.
    fn write_operand(&self, out: &mut String) {
        if self.is_atomic() {
            self.write(out);
        } else {
            out.push('(');
            self.write(out);
            out.push(')');
        }
    }

    /// Writes the subject of `.attr` or `? attr`. A path, integer or float
    /// literal would swallow the dot (`../up.x` is a path, `1000.x` a float
    /// followed by `x`), so those are parenthesised too.
    fn write_subject(&self, out: &mut String) {
        if matches!(self, Expr::Path(_) | Expr::Int(_) | Expr::Float(_)) {
            out.push('(');
            self.write(out);
            out.push(')');
        } else {
            self.write_operand(out);
        }
    }

    fn write(&self, out: &mut String) {
        match self {
            Expr::Int(n) => {
                if *n == i64::MIN {
                    // `9223372036854775808` is not a valid literal.
                    out.push_str("(-9223372036854775807 - 1)");
                } else if *n < 0 {
                    let _ = write!(out, "(-{})", n.unsigned_abs());
                } else {
                    let _ = write!(out, "{n}");
                }
            }
            // No literal is infinite (`1.0e400` is an invalid float); this
            // is how a Nix expression gets there.
            Expr::Float(f) if f.is_infinite() => out.push_str(if f.is_sign_positive() {
                "1.0e308 * 10.0"
            } else {
                "-1.0e308 * 10.0"
            }),
            Expr::Float(f) => {
                let lit = float_literal(f.abs());
                if f.is_sign_negative() {
                    let _ = write!(out, "(-{lit})");
                } else {
                    out.push_str(&lit);
                }
            }
            Expr::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Expr::Null => out.push_str("null"),
            Expr::Str(parts) => {
                out.push('"');
                for p in parts {
                    match p {
                        StrPart::Lit(s) => out.push_str(&escape_string(s)),
                        StrPart::Interp(e) => {
                            // A literal `$` right before `${` would lex as
                            // `$$`, the text "$$", and take the `{` along.
                            if out.ends_with('$') {
                                out.pop();
                                out.push_str("\\$");
                            }
                            out.push_str("${");
                            e.write(out);
                            out.push('}');
                        }
                    }
                }
                out.push('"');
            }
            Expr::IndStr { indent, parts } => {
                let pad = " ".repeat(*indent);
                out.push_str("''\n");
                out.push_str(&pad);
                for p in parts {
                    match p {
                        StrPart::Lit(s) => {
                            let text = escape_indented(s).replace('\n', &format!("\n{pad}"));
                            // A quote at the end of the previous piece must not
                            // run into one starting this piece: `''` would end
                            // the string. An empty interpolation keeps them
                            // apart without changing the value.
                            if out.ends_with('\'') && text.starts_with('\'') {
                                out.push_str("${\"\"}");
                            }
                            out.push_str(&text);
                        }
                        StrPart::Interp(e) => {
                            // As in `"…"`, a literal `$` must not run into
                            // the `${`: it becomes the escape `''$`.
                            if out.ends_with('$') {
                                out.pop();
                                if out.ends_with('\'') {
                                    out.push_str("${\"\"}");
                                }
                                out.push_str("''$");
                            }
                            out.push_str("${");
                            e.write(out);
                            out.push('}');
                        }
                    }
                }
                // Likewise before the closing `''`: `'` + `''` would lex as
                // the escape `'''`.
                if out.ends_with('\'') {
                    out.push_str("${\"\"}");
                }
                out.push_str("''");
            }
            Expr::Path(p) => out.push_str(p),
            Expr::Var(v) => out.push_str(v),
            Expr::List(items) => {
                out.push('[');
                for i in items {
                    out.push(' ');
                    i.write_operand(out);
                }
                out.push_str(" ]");
            }
            Expr::Attrs { rec, binds } => {
                if *rec {
                    out.push_str("rec ");
                }
                out.push('{');
                write_binds(binds, out);
                out.push_str(" }");
            }
            Expr::Select {
                subject,
                path,
                default,
            } => {
                subject.write_subject(out);
                out.push('.');
                write_attr_path(path, out);
                if let Some(d) = default {
                    out.push_str(" or ");
                    d.write_operand(out);
                }
            }
            Expr::HasAttr(subject, path) => {
                subject.write_subject(out);
                out.push_str(" ? ");
                write_attr_path(path, out);
            }
            Expr::Lambda { arg, formals, body } => {
                match (arg, formals) {
                    (Some(a), None) => out.push_str(a),
                    (arg, Some(f)) => {
                        if let Some(a) = arg {
                            out.push_str(a);
                            out.push_str(" @ ");
                        }
                        out.push_str("{ ");
                        for (i, (name, default)) in f.params.iter().enumerate() {
                            if i > 0 {
                                out.push_str(", ");
                            }
                            out.push_str(name);
                            if let Some(d) = default {
                                out.push_str(" ? ");
                                d.write_operand(out);
                            }
                        }
                        if f.ellipsis {
                            if !f.params.is_empty() {
                                out.push_str(", ");
                            }
                            out.push_str("...");
                        }
                        out.push_str(" }");
                    }
                    (None, None) => out.push('_'),
                }
                out.push_str(": ");
                body.write(out);
            }
            Expr::Call(f, args) => {
                f.write_operand(out);
                for a in args {
                    out.push(' ');
                    a.write_operand(out);
                }
            }
            Expr::Let(binds, body) => {
                out.push_str("let");
                write_binds(binds, out);
                out.push_str(" in ");
                body.write(out);
            }
            Expr::With(attrs, body) => {
                out.push_str("with ");
                attrs.write(out);
                out.push_str("; ");
                body.write(out);
            }
            Expr::If(c, t, e) => {
                out.push_str("if ");
                c.write(out);
                out.push_str(" then ");
                t.write(out);
                out.push_str(" else ");
                e.write(out);
            }
            Expr::Assert(c, b) => {
                out.push_str("assert ");
                c.write(out);
                out.push_str("; ");
                b.write(out);
            }
            Expr::Unary(op, e) => {
                out.push_str(op);
                e.write_operand(out);
            }
            Expr::Binary(op, l, r) => {
                l.write_operand(out);
                let _ = write!(out, " {op} ");
                r.write_operand(out);
            }
            Expr::Builtin(name, args) => {
                out.push_str("builtins.");
                out.push_str(name);
                for a in args {
                    out.push(' ');
                    a.write_operand(out);
                }
            }
        }
    }
}

fn write_binds(binds: &[Bind], out: &mut String) {
    for b in binds {
        out.push(' ');
        match b {
            Bind::Plain(path, value) => {
                write_attr_path(path, out);
                out.push_str(" = ");
                value.write(out);
                out.push(';');
            }
            Bind::Inherit(names) => {
                out.push_str("inherit");
                for n in names {
                    out.push(' ');
                    out.push_str(n);
                }
                out.push(';');
            }
            Bind::InheritFrom(from, names) => {
                out.push_str("inherit (");
                from.write(out);
                out.push(')');
                for n in names {
                    out.push(' ');
                    out.push_str(n);
                }
                out.push(';');
            }
        }
    }
}

fn write_attr_path(path: &[AttrKey], out: &mut String) {
    for (i, k) in path.iter().enumerate() {
        if i > 0 {
            out.push('.');
        }
        match k {
            AttrKey::Ident(n) => out.push_str(n),
            AttrKey::Quoted(s) => {
                out.push('"');
                out.push_str(&escape_string(s));
                out.push('"');
            }
            AttrKey::Dynamic(e) => {
                out.push_str("${");
                e.write(out);
                out.push('}');
            }
        }
    }
}

/// A float as a Nix literal: Nix requires digits on both sides of the
/// point (`1.0e30`, not `1e30`), and no sign.
fn float_literal(f: f64) -> String {
    debug_assert!(f >= 0.0 && f.is_finite());
    let s = format!("{f:e}");
    // `{:e}` gives `1.5e-7` or `1e30`; make the mantissa carry a point.
    let (mantissa, exp) = s.split_once('e').expect("exponent form");
    let mantissa = if mantissa.contains('.') {
        mantissa.to_owned()
    } else {
        format!("{mantissa}.0")
    };
    format!("{mantissa}e{exp}")
}

/// Escapes text for a `"…"` literal.
fn escape_string(s: &str) -> String {
    let mut out = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            '$' if chars.peek() == Some(&'{') => out.push_str("\\$"),
            c => out.push(c),
        }
    }
    out
}

/// Escapes text for an `''…''` literal.
fn escape_indented(s: &str) -> String {
    let mut out = String::new();
    // An escape starts with `''`; right after a lone `'` the lexer would
    // read `'''` instead, so an empty interpolation separates them.
    let escape = |out: &mut String, esc: &str| {
        if out.ends_with('\'') {
            out.push_str("${\"\"}");
        }
        out.push_str(esc);
    };
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\'' if chars.peek() == Some(&'\'') => {
                chars.next();
                escape(&mut out, "'''");
            }
            '$' if chars.peek() == Some(&'{') => escape(&mut out, "''$"),
            '\t' => escape(&mut out, "''\\t"),
            '\r' => escape(&mut out, "''\\r"),
            c => out.push(c),
        }
    }
    out
}

// ----- shrinking ------------------------------------------------------------

impl Expr {
    /// The expressions one step smaller than this one at the root: the
    /// expression replaced by one of its sub-expressions or by a literal,
    /// or an element removed from a list/set/call/let. Sub-expressions are
    /// shrunk in place by [`shrink`], not enumerated here (enumerating
    /// every combination is exponential).
    pub fn shrink_candidates(&self) -> Vec<Expr> {
        let mut out = Vec::new();
        for child in self.children() {
            out.push(child.clone());
        }
        if !matches!(self, Expr::Int(1) | Expr::Null) {
            out.push(Expr::Int(1));
            out.push(Expr::Null);
        }
        match self {
            Expr::List(items) if !items.is_empty() => {
                for i in 0..items.len() {
                    let mut v = items.clone();
                    v.remove(i);
                    out.push(Expr::List(v));
                }
            }
            Expr::Attrs { rec, binds } if !binds.is_empty() => {
                for i in 0..binds.len() {
                    let mut v = binds.clone();
                    v.remove(i);
                    out.push(Expr::Attrs {
                        rec: *rec,
                        binds: v,
                    });
                }
                if *rec {
                    out.push(Expr::Attrs {
                        rec: false,
                        binds: binds.clone(),
                    });
                }
            }
            Expr::Let(binds, body) if binds.len() > 1 => {
                for i in 0..binds.len() {
                    let mut v = binds.clone();
                    v.remove(i);
                    out.push(Expr::Let(v, body.clone()));
                }
            }
            Expr::Call(f, args) if args.len() > 1 => {
                for i in 0..args.len() {
                    let mut v = args.clone();
                    v.remove(i);
                    out.push(Expr::Call(f.clone(), v));
                }
            }
            Expr::Builtin(name, args) if !args.is_empty() => {
                // Fewer arguments: a partial application or an arity error.
                let mut v = args.clone();
                v.pop();
                out.push(Expr::Builtin(name.clone(), v));
            }
            Expr::Str(parts) if parts.len() > 1 => {
                for i in 0..parts.len() {
                    let mut v = parts.clone();
                    v.remove(i);
                    out.push(Expr::Str(v));
                }
            }
            Expr::IndStr { indent, parts } if parts.len() > 1 => {
                for i in 0..parts.len() {
                    let mut v = parts.clone();
                    v.remove(i);
                    out.push(Expr::IndStr {
                        indent: *indent,
                        parts: v,
                    });
                }
            }
            Expr::Select {
                subject,
                path,
                default: Some(_),
            } => {
                out.push(Expr::Select {
                    subject: subject.clone(),
                    path: path.clone(),
                    default: None,
                });
            }
            _ => {}
        }
        out
    }

    /// The direct sub-expressions, in a fixed order that `with_child`
    /// shares.
    fn children(&self) -> Vec<&Expr> {
        let mut v = Vec::new();
        match self {
            Expr::Int(_)
            | Expr::Float(_)
            | Expr::Bool(_)
            | Expr::Null
            | Expr::Path(_)
            | Expr::Var(_) => {}
            Expr::Str(parts) | Expr::IndStr { parts, .. } => {
                for p in parts {
                    if let StrPart::Interp(e) = p {
                        v.push(e);
                    }
                }
            }
            Expr::List(items) => v.extend(items.iter()),
            Expr::Attrs { binds, .. } => binds_children(binds, &mut v),
            Expr::Select {
                subject,
                path,
                default,
            } => {
                v.push(subject);
                path_children(path, &mut v);
                if let Some(d) = default {
                    v.push(d);
                }
            }
            Expr::HasAttr(subject, path) => {
                v.push(subject);
                path_children(path, &mut v);
            }
            Expr::Lambda { formals, body, .. } => {
                if let Some(f) = formals {
                    for d in f.params.iter().filter_map(|(_, d)| d.as_ref()) {
                        v.push(d);
                    }
                }
                v.push(body);
            }
            Expr::Call(f, args) => {
                v.push(f);
                v.extend(args.iter());
            }
            Expr::Let(binds, body) => {
                binds_children(binds, &mut v);
                v.push(body);
            }
            Expr::With(a, b) | Expr::Assert(a, b) | Expr::Binary(_, a, b) => {
                v.push(a);
                v.push(b);
            }
            Expr::If(a, b, c) => {
                v.push(a);
                v.push(b);
                v.push(c);
            }
            Expr::Unary(_, e) => v.push(e),
            Expr::Builtin(_, args) => v.extend(args.iter()),
        }
        v
    }

    /// A copy with the `i`th child (in `children` order) replaced.
    fn with_child(&self, i: usize, new: Expr) -> Expr {
        let mut copy = self.clone();
        let mut k = 0;
        copy.visit_children_mut(&mut |child| {
            if k == i {
                *child = new.clone();
            }
            k += 1;
        });
        copy
    }

    fn visit_children_mut(&mut self, f: &mut dyn FnMut(&mut Expr)) {
        match self {
            Expr::Int(_)
            | Expr::Float(_)
            | Expr::Bool(_)
            | Expr::Null
            | Expr::Path(_)
            | Expr::Var(_) => {}
            Expr::Str(parts) | Expr::IndStr { parts, .. } => {
                for p in parts {
                    if let StrPart::Interp(e) = p {
                        f(e);
                    }
                }
            }
            Expr::List(items) => {
                for e in items.iter_mut() {
                    f(e);
                }
            }
            Expr::Attrs { binds, .. } => binds_children_mut(binds, f),
            Expr::Select {
                subject,
                path,
                default,
            } => {
                f(subject);
                path_children_mut(path, f);
                if let Some(d) = default {
                    f(d);
                }
            }
            Expr::HasAttr(subject, path) => {
                f(subject);
                path_children_mut(path, f);
            }
            Expr::Lambda { formals, body, .. } => {
                if let Some(fm) = formals {
                    for d in fm.params.iter_mut().filter_map(|(_, d)| d.as_mut()) {
                        f(d);
                    }
                }
                f(body);
            }
            Expr::Call(func, args) => {
                f(func);
                for e in args.iter_mut() {
                    f(e);
                }
            }
            Expr::Let(binds, body) => {
                binds_children_mut(binds, f);
                f(body);
            }
            Expr::With(a, b) | Expr::Assert(a, b) | Expr::Binary(_, a, b) => {
                f(a);
                f(b);
            }
            Expr::If(a, b, c) => {
                f(a);
                f(b);
                f(c);
            }
            Expr::Unary(_, e) => f(e),
            Expr::Builtin(_, args) => {
                for e in args.iter_mut() {
                    f(e);
                }
            }
        }
    }

    /// The number of nodes, used to prefer smaller shrink candidates.
    pub fn size(&self) -> usize {
        1 + self.children().iter().map(|c| c.size()).sum::<usize>()
    }
}

fn binds_children<'a>(binds: &'a [Bind], v: &mut Vec<&'a Expr>) {
    for b in binds {
        match b {
            Bind::Plain(path, value) => {
                path_children(path, v);
                v.push(value);
            }
            Bind::Inherit(_) => {}
            Bind::InheritFrom(from, _) => v.push(from),
        }
    }
}

fn binds_children_mut(binds: &mut [Bind], f: &mut dyn FnMut(&mut Expr)) {
    for b in binds {
        match b {
            Bind::Plain(path, value) => {
                path_children_mut(path, f);
                f(value);
            }
            Bind::Inherit(_) => {}
            Bind::InheritFrom(from, _) => f(from),
        }
    }
}

fn path_children<'a>(path: &'a [AttrKey], v: &mut Vec<&'a Expr>) {
    for k in path {
        if let AttrKey::Dynamic(e) = k {
            v.push(e);
        }
    }
}

fn path_children_mut(path: &mut [AttrKey], f: &mut dyn FnMut(&mut Expr)) {
    for k in path {
        if let AttrKey::Dynamic(e) = k {
            f(e);
        }
    }
}

/// Greedily shrinks `expr` while `still_fails` holds: root-level
/// candidates first (smallest first), then each sub-expression in place,
/// repeated until nothing smaller still fails. `max_steps` bounds the
/// number of checks.
pub fn shrink(expr: Expr, max_steps: usize, mut still_fails: impl FnMut(&Expr) -> bool) -> Expr {
    let steps = std::cell::Cell::new(0usize);
    shrink_with(expr, &steps, max_steps, &mut still_fails)
}

fn shrink_with(
    mut expr: Expr,
    steps: &std::cell::Cell<usize>,
    max_steps: usize,
    still_fails: &mut dyn FnMut(&Expr) -> bool,
) -> Expr {
    let mut check = |candidate: &Expr| -> bool {
        if steps.get() >= max_steps {
            return false;
        }
        steps.set(steps.get() + 1);
        still_fails(candidate)
    };
    'outer: loop {
        let mut candidates = expr.shrink_candidates();
        candidates.sort_by_key(Expr::size);
        candidates.dedup();
        for c in candidates {
            if c == expr || c.size() >= expr.size() {
                continue;
            }
            if check(&c) {
                expr = c;
                continue 'outer;
            }
        }
        // Nothing at the root; shrink each child where it stands.
        let mut progressed = false;
        for i in 0..expr.children().len() {
            let child = expr.children()[i].clone();
            let parent = expr.clone();
            let shrunk = shrink_with(child.clone(), steps, max_steps, &mut |c| {
                check(&parent.with_child(i, c.clone()))
            });
            if shrunk != child {
                expr = expr.with_child(i, shrunk);
                progressed = true;
            }
        }
        if !progressed || steps.get() >= max_steps {
            return expr;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn floats_are_valid_literals() {
        assert_eq!(float_literal(1e30), "1.0e30");
        assert_eq!(float_literal(1.5e-7), "1.5e-7");
        assert_eq!(float_literal(0.5), "5.0e-1");
        assert_eq!(Expr::Float(f64::INFINITY).render(), "1.0e308 * 10.0");
        let list = Expr::List(vec![Expr::Float(f64::INFINITY), Expr::Float(-0.5)]);
        assert_eq!(list.render(), "[ (1.0e308 * 10.0) ((-5.0e-1)) ]");
    }

    /// `children` and `with_child` (through `visit_children_mut`) agree
    /// on the order of an expression's children: putting a child back
    /// where it came from changes nothing, and replacing child `i`
    /// replaces `children()[i]`. The shrinker relies on both.
    #[test]
    fn with_child_matches_children() {
        fn check(e: &Expr) {
            let marker = Expr::Var("replaced".to_owned());
            for (i, child) in e.children().into_iter().enumerate() {
                assert_eq!(e.with_child(i, child.clone()), *e, "{}", e.render());
                let replaced = e.with_child(i, marker.clone());
                assert_eq!(replaced.children()[i], &marker, "{}", e.render());
                check(child);
            }
        }
        for seed in 0..300 {
            check(&Gen::new(seed, 40).expr());
        }
    }
}
