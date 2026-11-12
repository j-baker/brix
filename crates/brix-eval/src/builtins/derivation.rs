//! `derivationStrict`, the `derivation` wrapper, and importing `.drv`
//! files (`primops.cc`, `derivation.nix`, `imported-drv-to-derivation.nix`).

use std::collections::{BTreeMap, BTreeSet};

use bstr::{BString, ByteSlice};

use brix_store::{
    ContentAddress, ContentAddressMethod, Derivation, DerivationOutput, HashAlgo, StorePath,
};

use crate::builtins::paths::new_hash_allow_empty;
use crate::builtins::strings::xp_settings;
use crate::builtins::{reg, require_attr};
use crate::coerce::Coercion;
use crate::error::{msg, ErrorKind};
use crate::evaluator::Evaluator;
use crate::string_context::StringContext;
use crate::symbol::{sym, Symbol};
use crate::value::{Attr, ContextElem, PathVal, SrcPos, Value, ValueRef};
use crate::Result;

/// The `derivation` function: a thin Nix-level wrapper around
/// `derivationStrict` that lazily exposes `drvPath`, `outPath`, one
/// attribute per output and `all`. Written from the documented behaviour
/// of Nix's `derivation.nix`.
const DERIVATION_WRAPPER: &str = r#"
# brix's implementation of `builtins.derivation`: a wrapper around the
# `derivationStrict` primop.
#
# `derivationStrict` does the real work: it validates the attributes,
# computes the store paths and returns `{ drvPath; <output> = outPath; }`.
# This wrapper keeps that call lazy, so merely mentioning a derivation
# costs nothing, and builds the attribute set Nix users see:
#
#   * every attribute passed in, unchanged (`drvAttrs` keeps the original
#     set as well);
#   * one attribute per output name, each a copy of the derivation whose
#     `outPath` and `outputName` are that output's;
#   * `all`, the list of those per-output copies;
#   * `drvPath`, `outPath` (of the first output) and `type = "derivation"`.
#
# The result for a given output shares one attribute set between `all`,
# the output attribute and the derivation itself, which is why the
# evaluator prints `«repeated»` for them.
#
# Layout note: positions inside this file reach users. `builtins.derivation`
# prints as `«lambda @ «nix-internal»/derivation-internal.nix:31:1»`, and
# errors raised while forcing a derivation quote the surrounding source
# lines in their excerpts. So the code below sits at the same lines, with
# the same text, as the file Nix embeds (which starts one line later than
# the file in its repository); only this comment is ours.
#
#
#
drvFunc:
drvAttrs@{
  outputs ? [ "out" ],
  ...
}:

let

  strict = drvFunc drvAttrs;

  commonAttrs =
    drvAttrs
    // (builtins.listToAttrs outputsList)
    // {
      all = map (x: x.value) outputsList;
      inherit drvAttrs;
    };

  outputToAttrListElement = outputName: {
    name = outputName;
    value = commonAttrs // {
      outPath = strict.${outputName};
      drvPath = strict.drvPath;
      type = "derivation";
      inherit outputName;
    };
  };

  outputsList = map outputToAttrListElement outputs;

in
(builtins.head outputsList).value
"#;

/// Turns the attribute set `import` builds for a `.drv` file into a
/// derivation value, the way `imported-drv-to-derivation.nix` does.
const IMPORTED_DRV_WRAPPER: &str = r#"
info@{ drvPath, outputs, name, ... }:
let
  perOutput = outputName: {
    name = outputName;
    value = shared // {
      outPath = builtins.getAttr outputName info;
      inherit outputName;
    };
  };
  outputsList = map perOutput outputs;
  shared = builtins.listToAttrs outputsList // {
    all = map (o: o.value) outputsList;
    inherit drvPath name;
    type = "derivation";
  };
in
(builtins.head outputsList).value
"#;

pub fn register(ev: &Evaluator<'_>) {
    reg(ev, "derivationStrict", 1, prim_derivation_strict);
    let cell = ev.alloc_cell(Value::Null);
    ev.add_global("derivation", cell);
    ev.derivation_fn.set(Some(cell));
}

/// Evaluates the wrapper and binds `builtins.derivation`.
pub fn install_wrapper(ev: &Evaluator<'_>) -> Result<()> {
    let wrapper = ev.eval_internal("derivation-internal.nix", DERIVATION_WRAPPER)?;
    let strict = ev.builtin("derivationStrict");
    let f = ev.force(wrapper, SrcPos::NONE)?;
    let v = ev.call(f, &[strict], SrcPos::NONE)?;
    ev.derivation_fn.get().expect("derivation cell").set(v);
    Ok(())
}

fn prim_derivation_strict<'a>(
    ev: &Evaluator<'a>,
    pos: SrcPos,
    args: &[ValueRef<'a>],
) -> Result<Value<'a>> {
    let attrs = ev.force_attrs(
        args[0],
        pos,
        "while evaluating the argument passed to builtins.derivationStrict",
    )?;
    let name_attr = require_attr(
        ev,
        attrs,
        sym::NAME,
        "in the attrset passed as argument to builtins.derivationStrict",
    )?;
    let drv_name = ev
        .force_string_no_ctx(
            name_attr.value,
            pos,
            "while evaluating the `name` attribute passed to builtins.derivationStrict",
        )
        .inspect_err(|e| {
            trace(
                ev,
                e,
                name_attr.pos,
                "while evaluating the derivation attribute 'name'",
            )
        })?;
    derivation_strict_internal(ev, drv_name, attrs)
        .inspect_err(|e| trace_derivation(ev, e, drv_name, name_attr.pos))
}

// Errors and traces live out of line: `derivationStrict` runs once per
// derivation between long stretches of ordinary evaluation, so its code is
// fetched afresh each time, and the less of it there is the better.

#[cold]
#[inline(never)]
fn trace(ev: &Evaluator<'_>, e: &crate::Error, pos: SrcPos, hint: &str) {
    ev.add_trace(e, pos, hint);
}

#[cold]
#[inline(never)]
fn trace_derivation(ev: &Evaluator<'_>, e: &crate::Error, drv_name: &[u8], name_pos: SrcPos) {
    let pos = ev
        .position(name_pos)
        .map(|p| p.display())
        .unwrap_or_default();
    e.add_trace(
        None,
        msg!(
            "while evaluating derivation '",
            drv_name,
            "'\n  whose name attribute is located at ",
            pos
        ),
    );
}

#[cold]
#[inline(never)]
fn trace_attribute(ev: &Evaluator<'_>, e: &crate::Error, attr: &Attr<'_>, drv_name: &str) {
    ev.add_trace(
        e,
        attr.pos,
        msg!(
            "while evaluating attribute '",
            ev.symbol_name(attr.name),
            "' of derivation '",
            drv_name,
            "'"
        ),
    );
}

#[cold]
#[inline(never)]
fn eval_error(ev: &Evaluator<'_>, msg: &str) -> crate::Error {
    ev.error(ErrorKind::Eval, msg)
}

#[cold]
#[inline(never)]
fn store_error(ev: &Evaluator<'_>, e: impl std::fmt::Display) -> crate::Error {
    ev.error(ErrorKind::Eval, e.to_string())
}

/// A failure to add the derivation, with the bytes of a name or an
/// environment variable it quotes as they are.
#[cold]
#[inline(never)]
fn add_error(ev: &Evaluator<'_>, e: brix_store::VirtualStoreError) -> crate::Error {
    ev.error(ErrorKind::Eval, e.message_bytes())
}

/// `tokenizeString` on the separators of `outputs`: space, tab, newline
/// and carriage return.
fn tokenize(s: &[u8]) -> Vec<BString> {
    s.split(|b| matches!(b, b' ' | b'\t' | b'\n' | b'\r'))
        .filter(|t| !t.is_empty())
        .map(BString::from)
        .collect()
}

/// What `derivationStrict` gathers from the attributes, in name order.
/// Strings stay bytes, as they are in Nix: the environment, the builder,
/// the platform and the arguments go into the derivation as they are.
struct DrvAttrs<'a> {
    drv: Derivation,
    /// The environment, in attribute order and so sorted, which lets the
    /// map be built in one go rather than by repeated insertion.
    env: Vec<(BString, BString)>,
    /// With structured attrs: each attribute as JSON.
    json_attrs: BTreeMap<BString, String>,
    /// With structured attrs: nlohmann's error for the first name or
    /// string in them that is not UTF-8, in the order its dump writes
    /// them. Nix raises it where it first serialises the derivation, after
    /// every attribute has been evaluated (see [`pending_json_error`]).
    json_error: Option<String>,
    context: StringContext<'a>,
    content_addressed: bool,
    is_impure: bool,
    output_hash: Option<BString>,
    output_hash_algo: Option<HashAlgo>,
    ingestion_method: Option<ContentAddressMethod>,
    /// Names as given, which need not be UTF-8: as in Nix, one that is
    /// not a valid store path name fails only where an output path is
    /// made from it.
    outputs: BTreeSet<BString>,
}

impl<'a> DrvAttrs<'a> {
    fn new(drv_name: &str, attrs: usize) -> DrvAttrs<'a> {
        DrvAttrs {
            drv: Derivation {
                name: drv_name.to_owned(),
                ..Derivation::default()
            },
            env: Vec::with_capacity(attrs),
            json_attrs: BTreeMap::new(),
            json_error: None,
            context: StringContext::new(),
            content_addressed: false,
            is_impure: false,
            output_hash: None,
            output_hash_algo: None,
            ingestion_method: None,
            outputs: BTreeSet::from([BString::from("out")]),
        }
    }

    /// One attribute. Most become an environment variable; the few with a
    /// meaning of their own are handled out of line.
    fn add(
        &mut self,
        ev: &Evaluator<'a>,
        attr: &Attr<'a>,
        structured: bool,
        ignore_nulls: bool,
    ) -> Result<()> {
        let pos = SrcPos::NONE;
        if ignore_nulls && matches!(ev.force(attr.value, pos)?, Value::Null) {
            return Ok(());
        }
        match attr.name {
            sym::CONTENT_ADDRESSED | sym::IMPURE => return self.add_flag(ev, attr),
            sym::ARGS => return self.add_args(ev, attr),
            _ if structured => return self.add_structured(ev, attr),
            _ => {}
        }
        let s = ev.coerce_to_string(pos, attr.value, &mut self.context, "", Coercion::MORE_COPY)?;
        match attr.name {
            sym::JSON => return self.add_json(ev, s),
            sym::BUILDER
            | sym::SYSTEM
            | sym::OUTPUT_HASH
            | sym::OUTPUT_HASH_ALGO
            | sym::OUTPUT_HASH_MODE
            | sym::OUTPUTS => self.note_special(ev, attr.name, s)?,
            _ => {}
        }
        // The env map owns its copy; only the few special attributes are
        // also copied into their own fields.
        self.env
            .push((BString::from(ev.symbol_name(attr.name)), BString::from(s)));
        Ok(())
    }

    /// `__contentAddressed` and `__impure`.
    #[inline(never)]
    fn add_flag(&mut self, ev: &Evaluator<'a>, attr: &Attr<'a>) -> Result<()> {
        if ev.force_bool(attr.value, SrcPos::NONE, "")? {
            if attr.name == sym::CONTENT_ADDRESSED {
                self.content_addressed = true;
                ev.require_experimental("ca-derivations", "")?;
            } else {
                self.is_impure = true;
                ev.require_experimental("impure-derivations", "")?;
            }
        }
        Ok(())
    }

    #[inline(never)]
    fn add_args(&mut self, ev: &Evaluator<'a>, attr: &Attr<'a>) -> Result<()> {
        let pos = SrcPos::NONE;
        let list = ev.force_list_elems(attr.value, pos, "")?;
        self.drv.args.reserve(list.len());
        for &elem in list {
            let s = ev.coerce_to_string(
                pos,
                elem,
                &mut self.context,
                "while evaluating an element of the argument list",
                Coercion::MORE_COPY,
            )?;
            self.drv.args.push(BString::from(s));
        }
        Ok(())
    }

    /// The attributes that also set a field (or are checked), given
    /// their string value.
    #[inline(never)]
    fn note_special(&mut self, ev: &Evaluator<'a>, name: Symbol, s: &[u8]) -> Result<()> {
        match name {
            sym::BUILDER => self.drv.builder = BString::from(s),
            sym::SYSTEM => self.drv.platform = BString::from(s),
            sym::OUTPUT_HASH => self.output_hash = Some(BString::from(s)),
            sym::OUTPUT_HASH_ALGO => self.output_hash_algo = hash_algo(ev, s)?,
            sym::OUTPUT_HASH_MODE => {
                self.ingestion_method = Some(hash_mode(ev, s, &self.drv.name)?)
            }
            sym::OUTPUTS => set_outputs(ev, tokenize(s), &mut self.outputs)?,
            _ => {}
        }
        Ok(())
    }

    /// `__json`: deprecated structured attributes, stored canonically.
    #[cold]
    #[inline(never)]
    fn add_json(&mut self, ev: &Evaluator<'a>, s: &[u8]) -> Result<()> {
        ev.host.warn(format!(
            "In derivation '{}': setting structured attributes via '__json' is deprecated, and may be disallowed in future versions of Nix. Set '__structuredAttrs = true' instead.",
            self.drv.name
        ).as_bytes());
        let parsed: serde_json::Value = serde_json::from_slice(s).map_err(|e| {
            ev.error(
                ErrorKind::Eval,
                format!("cannot process __json attribute: {e}"),
            )
        })?;
        self.env
            .push(("__json".into(), crate::json::dump(&parsed).into()));
        Ok(())
    }

    /// An attribute of a derivation with `__structuredAttrs`.
    #[inline(never)]
    fn add_structured(&mut self, ev: &Evaluator<'a>, attr: &Attr<'a>) -> Result<()> {
        if attr.name == sym::STRUCTURED_ATTRS {
            return Ok(());
        }
        let pos = SrcPos::NONE;
        let key = ev.symbol_name(attr.name);
        let (json, error) = ev.to_json_undumped(attr.value, pos, &mut self.context, true, true)?;
        // Attributes come in name order, the dump's: a name, then its value.
        if self.json_error.is_none() {
            self.json_error = crate::json::utf8_error(key).or(error);
        }
        self.json_attrs.insert(BString::from(key), json);
        match attr.name {
            sym::BUILDER => {
                let s = ev.force_string_ctx(attr.value, &mut self.context, pos, "")?;
                self.drv.builder = BString::from(s);
            }
            sym::SYSTEM => {
                let s = ev.force_string_no_ctx(attr.value, pos, "")?;
                self.drv.platform = BString::from(s);
            }
            sym::OUTPUT_HASH => {
                let s = ev.force_string_no_ctx(attr.value, pos, "")?;
                self.output_hash = Some(BString::from(s));
            }
            sym::OUTPUT_HASH_ALGO => {
                let s = ev.force_string_no_ctx(attr.value, pos, "")?;
                self.output_hash_algo = hash_algo(ev, s)?;
            }
            sym::OUTPUT_HASH_MODE => {
                let s = ev.force_string_no_ctx(attr.value, pos, "")?;
                self.ingestion_method = Some(hash_mode(ev, s, &self.drv.name)?);
            }
            sym::OUTPUTS => {
                let list = ev.force_list_elems(attr.value, pos, "")?;
                let mut names = Vec::with_capacity(list.len());
                for &elem in list {
                    names.push(BString::from(ev.force_string_no_ctx(elem, pos, "")?));
                }
                set_outputs(ev, names, &mut self.outputs)?;
            }
            _ => {}
        }
        let ignored = match attr.name {
            sym::ALLOWED_REFERENCES => "allowedReferences",
            sym::ALLOWED_REQUISITES => "allowedRequisites",
            sym::DISALLOWED_REFERENCES => "disallowedReferences",
            sym::DISALLOWED_REQUISITES => "disallowedRequisites",
            sym::MAX_SIZE => "maxSize",
            sym::MAX_CLOSURE_SIZE => "maxClosureSize",
            _ => return Ok(()),
        };
        ev.host.warn(format!(
            "In a derivation named '{}', 'structuredAttrs' disables the effect of the derivation attribute '{ignored}'; use 'outputChecks.<output>.{ignored}' instead",
            self.drv.name
        ).as_bytes());
        Ok(())
    }
}

/// `parseHashAlgoOpt` of `outputHashAlgo`: an unknown name, whatever its
/// bytes, is no algorithm at all.
#[inline(never)]
fn hash_algo(ev: &Evaluator<'_>, s: &[u8]) -> Result<Option<HashAlgo>> {
    HashAlgo::parse_opt(s, xp_settings(ev)).map_err(|e| store_error(ev, e))
}

/// `outputHashMode`.
#[inline(never)]
fn hash_mode(ev: &Evaluator<'_>, s: &[u8], drv_name: &str) -> Result<ContentAddressMethod> {
    let m = if s == b"recursive" {
        ContentAddressMethod::NixArchive
    } else {
        // Nix replaces the parser's own error; a value that is not UTF-8
        // names no method either.
        std::str::from_utf8(s)
            .ok()
            .and_then(|s| ContentAddressMethod::parse(s).ok())
            .ok_or_else(|| {
                ev.error(
                    ErrorKind::Eval,
                    msg!("invalid value '", s, "' for 'outputHashMode' attribute"),
                )
            })?
    };
    if m == ContentAddressMethod::Text {
        ev.require_experimental(
            "dynamic-derivations",
            &format!("text-hashed derivation '{drv_name}', outputHashMode = \"text\""),
        )?;
    }
    if m == ContentAddressMethod::Git {
        ev.require_experimental("git-hashing", "")?;
    }
    Ok(m)
}

/// `outputs`: replaces the set, rejecting duplicates and `drvPath`.
#[inline(never)]
fn set_outputs(
    ev: &Evaluator<'_>,
    names: Vec<BString>,
    outputs: &mut BTreeSet<BString>,
) -> Result<()> {
    outputs.clear();
    for j in names {
        if outputs.contains(&j) {
            return Err(ev.error(
                ErrorKind::Eval,
                msg!("duplicate derivation output '", j, "'"),
            ));
        }
        if j == "drvPath" {
            return Err(eval_error(ev, "invalid derivation output name 'drvPath'"));
        }
        outputs.insert(j);
    }
    if outputs.is_empty() {
        return Err(eval_error(
            ev,
            "derivation cannot have an empty set of outputs",
        ));
    }
    Ok(())
}

/// `__structuredAttrs`/`__ignoreNulls`: false when absent.
fn flag_attr<'a>(
    ev: &Evaluator<'a>,
    attrs: crate::value::Bindings<'a>,
    name: Symbol,
    ctx: &str,
) -> Result<bool> {
    match attrs.get(name) {
        Some(a) => ev.force_bool(a.value, SrcPos::NONE, ctx),
        None => Ok(false),
    }
}

#[cold]
#[inline(never)]
fn invalid_name(ev: &Evaluator<'_>, e: brix_store::StorePathError) -> crate::Error {
    ev.error(
        ErrorKind::Eval,
        msg!(
            "invalid derivation name: ",
            e.message_bytes(),
            ". Please pass a different 'name'."
        ),
    )
}

fn derivation_strict_internal<'a>(
    ev: &Evaluator<'a>,
    drv_name: &[u8],
    attrs: crate::value::Bindings<'a>,
) -> Result<Value<'a>> {
    if let Err(e) = brix_store::store_path::validate_name(drv_name) {
        return Err(invalid_name(ev, e));
    }
    let drv_name = std::str::from_utf8(drv_name).expect("a valid store path name is ASCII");
    let structured = flag_attr(
        ev,
        attrs,
        sym::STRUCTURED_ATTRS,
        "while evaluating the `__structuredAttrs` attribute passed to builtins.derivationStrict",
    )?;
    let ignore_nulls = flag_attr(
        ev,
        attrs,
        sym::IGNORE_NULLS,
        "while evaluating the `__ignoreNulls` attribute passed to builtins.derivationStrict",
    )?;
    let mut a = DrvAttrs::new(drv_name, attrs.len());
    // In name order, as Nix (`lexicographicOrder`): which attribute fails
    // first is observable, and the environment is built sorted.
    for attr in attrs.lexicographic(ev.keys) {
        if attr.name == sym::IGNORE_NULLS {
            continue;
        }
        a.add(ev, attr, structured, ignore_nulls)
            .inspect_err(|e| trace_attribute(ev, e, attr, drv_name))?;
    }
    let mut drv = std::mem::take(&mut a.drv);
    // Sorted already, so this builds the map without searching it.
    drv.env = std::mem::take(&mut a.env).into_iter().collect();
    if structured {
        drv.env
            .insert("__json".into(), structured_json(&a.json_attrs));
    }
    add_context_inputs(ev, &mut drv, &a.context, &a.json_error)?;

    if drv.builder.is_empty() {
        return Err(eval_error(ev, "required attribute 'builder' missing"));
    }
    if drv.platform.is_empty() {
        return Err(eval_error(ev, "required attribute 'system' missing"));
    }
    if drv_name.ends_with(".drv")
        && !(a.ingestion_method == Some(ContentAddressMethod::Text)
            && a.outputs.len() == 1
            && a.outputs.contains(b"out".as_slice()))
    {
        return Err(eval_error(
            ev,
            "derivation names are allowed to end in '.drv' only if they produce a single derivation file",
        ));
    }

    let mut fixed_path = None;
    let deferred = if let Some(hash) = &a.output_hash {
        fixed_path = Some(add_fixed_output(
            ev,
            &mut drv,
            hash,
            a.output_hash_algo,
            a.ingestion_method,
            &a.outputs,
        )?);
        pending_json_error(ev, &a.json_error)?;
        false
    } else if a.content_addressed || a.is_impure {
        add_floating_outputs(ev, &mut drv, &a)?;
        false
    } else {
        pending_json_error(ev, &a.json_error)?;
        // Any bytes: a name that is not a store path name fails where its
        // output path is computed, with Nix's message.
        for o in &a.outputs {
            drv.env.insert(o.clone(), BString::default());
            drv.outputs.insert(o.clone(), DerivationOutput::Deferred);
        }
        true
    };
    // `fillInOutputPaths` if the outputs are deferred, then
    // `writeDerivation`.
    let drv_path = ev
        .add_derivation(drv, deferred)
        .map_err(|e| add_error(ev, e))?;

    let interned = ev.intern_store_path(&drv_path);
    // The derivation now lives in the store; its outputs are read back from
    // there rather than cloned before the move.
    let store = ev.store.borrow();
    let drv_outputs = &store
        .derivation(&drv_path)
        .expect("derivation was just added")
        .outputs;
    let mut result: Vec<Attr<'a>> = Vec::with_capacity(1 + drv_outputs.len());
    let drv_context = ev
        .bump
        .alloc_slice_copy(&[ContextElem::DrvDeep(interned.base)]);
    result.push(ev.attr(
        sym::DRV_PATH,
        ev.mk_string_view(interned.full.as_bytes(), drv_context),
    ));
    for (name, output) in drv_outputs {
        let value = match (output, &fixed_path) {
            // Computed above; no need to hash it again.
            (DerivationOutput::CAFixed(_), Some(path)) => {
                ev.output_path_string(path, interned.base, name)
            }
            _ => ev.output_string(&drv_path, interned.base, drv_name, name, output)?,
        };
        result.push(ev.attr(ev.intern(name), value));
    }
    drop(store);
    Ok(ev.mk_attrs(result, SrcPos::NONE))
}

/// With structured attrs, the environment's `__json`: the attributes as
/// one JSON object. A name that is not UTF-8 is written lossily, as
/// strings are by [`Evaluator::to_json_undumped`]: the derivation is never
/// serialised with it (see [`pending_json_error`]).
#[inline(never)]
fn structured_json(json_attrs: &BTreeMap<BString, String>) -> BString {
    let mut out = String::from("{");
    for (i, (k, v)) in json_attrs.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        crate::json::push_escaped(&mut out, &String::from_utf8_lossy(k));
        out.push(':');
        out.push_str(v);
    }
    out.push('}');
    out.into()
}

/// Structured attributes that are not UTF-8 make nlohmann's dump fail
/// where Nix first serialises the derivation (`unparse`): after the
/// attributes, the inputs, the checks of `builder`, `system` and the
/// name, and a fixed output's hash. Its exception is not a Nix error, so
/// no trace is added to it ([`ErrorKind::Foreign`]).
fn pending_json_error(ev: &Evaluator<'_>, error: &Option<String>) -> Result<()> {
    match error {
        Some(e) => Err(foreign_error(ev, e)),
        None => Ok(()),
    }
}

#[cold]
#[inline(never)]
fn foreign_error(ev: &Evaluator<'_>, e: &str) -> crate::Error {
    ev.error(ErrorKind::Foreign, e)
}

/// Everything the attribute strings refer to is an input.
fn add_context_inputs(
    ev: &Evaluator<'_>,
    drv: &mut Derivation,
    context: &StringContext<'_>,
    json_error: &Option<String>,
) -> Result<()> {
    let mut masked_text: Option<Vec<u8>> = None;
    for c in context {
        match *c {
            ContextElem::DrvDeep(d) => add_drv_closure(ev, drv, d)?,
            ContextElem::Built { drv: d, output } => {
                let d = ev.store_path_of(d)?;
                drv.input_drvs
                    .entry(d)
                    .or_default()
                    .insert(BString::from(output));
            }
            ContextElem::Opaque(p) => {
                drv.input_srcs.insert(ev.store_path_of(p)?);
            }
            ContextElem::Path(p) => {
                warn_unreferenced_path(ev, drv, &mut masked_text, json_error, p)?
            }
            // Only a known store path can be in the derivation's text.
            ContextElem::LazyPath(tree) => {
                if let Some(p) = ev.known_lazy_path(tree) {
                    warn_unreferenced_path(ev, drv, &mut masked_text, json_error, p)?;
                }
            }
            ContextElem::LazyHash { .. } => {
                unreachable!("an accumulated context never holds lazy hashes")
            }
        }
    }
    Ok(())
}

/// A `drvPath` in the context: the derivation's whole closure is an
/// input, every derivation in it with all its outputs.
#[cold]
#[inline(never)]
fn add_drv_closure(ev: &Evaluator<'_>, drv: &mut Derivation, d: &str) -> Result<()> {
    let d = ev.store_path_of(d)?;
    for j in ev.closure_of(&d) {
        if j.is_derivation() {
            if let Some(inner) = ev.store.borrow().derivation(&j) {
                drv.input_drvs.insert(j.clone(), inner.output_names());
            }
        }
        drv.input_srcs.insert(j);
    }
    Ok(())
}

/// A store path mentioned without context: Nix warns if the derivation
/// refers to it (in its text with the outputs masked).
#[cold]
#[inline(never)]
fn warn_unreferenced_path(
    ev: &Evaluator<'_>,
    drv: &Derivation,
    masked_text: &mut Option<Vec<u8>>,
    json_error: &Option<String>,
    p: &str,
) -> Result<()> {
    let store_dir = &ev.config.store_dir;
    let text = match masked_text {
        Some(text) => text,
        None => {
            pending_json_error(ev, json_error)?;
            masked_text.insert(
                drv.unparse(store_dir, true)
                    .map_err(|e| store_error(ev, e))?,
            )
        }
    };
    if text.contains_str(p) {
        ev.host.warn(format!(
            "Using 'builtins.derivation' to create a derivation named '{}' that references the store path '{}/{p}' without a proper context. The resulting derivation will not have a correct store reference, so this is unreliable and may stop working in the future.",
            drv.name,
            store_dir.as_str()
        ).as_bytes());
    }
    Ok(())
}

/// `outputHash`: a single fixed output `out`; returns its path.
#[inline(never)]
fn add_fixed_output(
    ev: &Evaluator<'_>,
    drv: &mut Derivation,
    hash: &[u8],
    algo: Option<HashAlgo>,
    method: Option<ContentAddressMethod>,
    outputs: &BTreeSet<BString>,
) -> Result<StorePath> {
    if outputs.len() != 1 || !outputs.contains(b"out".as_slice()) {
        return Err(eval_error(
            ev,
            "multiple outputs are not supported in fixed-output derivations",
        ));
    }
    // The hash is bytes, quoted as they are when it is not valid.
    let h = new_hash_allow_empty(ev, hash, algo)?;
    let method = method.unwrap_or(ContentAddressMethod::Flat);
    let out = DerivationOutput::CAFixed(ContentAddress { method, hash: h });
    let store_dir = &ev.config.store_dir;
    let path = out
        .path(store_dir, &drv.name, b"out")
        .map_err(|e| store_error(ev, e))?
        .expect("fixed output has a path");
    drv.env
        .insert("out".into(), store_dir.display(&path).into());
    drv.outputs.insert("out".into(), out);
    Ok(path)
}

/// `__contentAddressed` / `__impure`: floating outputs, whose env vars
/// are hash placeholders.
#[cold]
#[inline(never)]
fn add_floating_outputs(ev: &Evaluator<'_>, drv: &mut Derivation, a: &DrvAttrs<'_>) -> Result<()> {
    if a.content_addressed && a.is_impure {
        return Err(eval_error(
            ev,
            "derivation cannot be both content-addressed and impure",
        ));
    }
    pending_json_error(ev, &a.json_error)?;
    let hash_algo = a.output_hash_algo.unwrap_or(HashAlgo::Sha256);
    let method = a
        .ingestion_method
        .unwrap_or(ContentAddressMethod::NixArchive);
    // Any bytes: these outputs have no path, so their names are kept.
    for o in &a.outputs {
        drv.env.insert(
            o.clone(),
            brix_store::derivation::hash_placeholder(o).into(),
        );
        let out = if a.is_impure {
            DerivationOutput::Impure { method, hash_algo }
        } else {
            DerivationOutput::CAFloating { method, hash_algo }
        };
        drv.outputs.insert(o.clone(), out);
    }
    Ok(())
}

/// A store path as the evaluator renders it: its base name and its full
/// path, both in the arena, and the validated [`StorePath`].
#[derive(Clone, Debug)]
pub struct InternedStorePath<'a> {
    pub base: &'a str,
    pub full: &'a str,
    pub path: StorePath,
}

/// Every store path named during an evaluation, by base name. A path is
/// referenced from many places (the `drvPath` and `outPath` strings of its
/// derivation, the context of every string that mentions it, the inputs
/// of every dependent derivation), and each would otherwise copy and
/// re-validate the name; here that happens once.
#[derive(Default)]
pub struct StorePathInterner<'a> {
    by_base: hashbrown::HashMap<&'a str, InternedStorePath<'a>>,
}

impl<'a> Evaluator<'a> {
    /// Add a derivation to the store, first filling in its output paths
    /// if `fill_in`: `VirtualStore::fill_in_and_add_derivation` or
    /// `VirtualStore::add_derivation`, holding the store only to look up
    /// the inputs' hashes and to record the result. Serialising and
    /// hashing the derivation, which is most of the work, happens in
    /// between, so evaluation threads do not queue for the store here.
    pub(crate) fn add_derivation(
        &self,
        drv: Derivation,
        fill_in: bool,
    ) -> std::result::Result<StorePath, brix_store::VirtualStoreError> {
        let (known, xp) = {
            let store = self.store.borrow();
            (store.input_hashes(&drv), store.xp())
        };
        let inputs = match known {
            Some(inputs) => inputs,
            None => self.store.borrow_mut().memoise_input_hashes(&drv),
        };
        let hashed = brix_store::HashedDerivation::new(
            &self.config.store_dir,
            xp,
            drv,
            fill_in,
            &inputs,
            |path| self.store.borrow().is_hashed_derivation(path),
        )?;
        if hashed.is_stored() {
            // Nothing to record: leave the store to its readers.
            return Ok(hashed.path().clone());
        }
        Ok(self.store.borrow_mut().add_hashed_derivation(hashed))
    }

    /// The interned form of `path`, rendered on first use.
    pub fn intern_store_path(&self, path: &StorePath) -> InternedStorePath<'a> {
        if let Some(interned) = self.store_paths.borrow().by_base.get(path.as_str()) {
            return interned.clone();
        }
        let base = self.bump.alloc_str(path.as_str());
        let dir = self.config.store_dir.as_str();
        let mut full = String::with_capacity(dir.len() + 1 + base.len());
        full.push_str(dir);
        full.push('/');
        full.push_str(base);
        let interned = InternedStorePath {
            base,
            full: self.bump.alloc_str(&full),
            path: path.clone(),
        };
        self.store_paths
            .borrow_mut()
            .by_base
            .insert(base, interned.clone());
        interned
    }

    /// The store path for a base name taken from a string context,
    /// validated once per distinct name.
    pub fn store_path_of(&self, base: &str) -> Result<StorePath> {
        if let Some(interned) = self.store_paths.borrow().by_base.get(base) {
            return Ok(interned.path.clone());
        }
        let path = StorePath::new(base).map_err(|e| self.error(ErrorKind::Eval, e.to_string()))?;
        Ok(self.intern_store_path(&path).path)
    }

    /// A name as arena bytes shared with the symbol table (output names
    /// in context elements: a handful of distinct values).
    fn intern_name(&self, name: &[u8]) -> &'a [u8] {
        self.symbol_name(self.intern(name))
    }

    /// `mkOutputString`: the output path (or a placeholder for a
    /// floating output) with its `Built` context. The output name is
    /// bytes, as Nix's are.
    pub fn output_string(
        &self,
        drv_path: &StorePath,
        drv_base: &'a str,
        drv_name: &str,
        output_name: &[u8],
        output: &DerivationOutput,
    ) -> Result<Value<'a>> {
        let path = output
            .path(&self.config.store_dir, drv_name, output_name)
            .map_err(|e| store_error(self, e))?;
        let bytes: &'a [u8] = match path {
            Some(p) => self.intern_store_path(&p).full.as_bytes(),
            None => {
                let placeholder =
                    brix_store::derivation::downstream_placeholder(drv_path, output_name)
                        .map_err(|e| store_error(self, e))?;
                self.bump.alloc_str(&placeholder).as_bytes()
            }
        };
        Ok(self.built_string(bytes, drv_base, output_name))
    }

    /// [`Evaluator::output_string`] for an output whose path is known.
    fn output_path_string(
        &self,
        path: &StorePath,
        drv_base: &'a str,
        output_name: &[u8],
    ) -> Value<'a> {
        let bytes = self.intern_store_path(path).full.as_bytes();
        self.built_string(bytes, drv_base, output_name)
    }

    /// `bytes` with the context of output `output_name` of `drv_base`.
    fn built_string(&self, bytes: &'a [u8], drv_base: &'a str, output_name: &[u8]) -> Value<'a> {
        let ctx = self.bump.alloc_slice_copy(&[ContextElem::Built {
            drv: drv_base,
            output: self.intern_name(output_name),
        }]);
        self.mk_string_view(bytes, ctx)
    }

    /// `computeFSClosure` over the virtual store: the path and everything
    /// it references, transitively.
    pub fn closure_of(&self, start: &StorePath) -> BTreeSet<StorePath> {
        let store = self.store.borrow();
        let mut seen: BTreeSet<StorePath> = BTreeSet::new();
        let mut todo = vec![start.clone()];
        while let Some(p) = todo.pop() {
            if !seen.insert(p.clone()) {
                continue;
            }
            match store.get(&p) {
                Some(brix_store::StoreObject::Derivation(d)) => todo.extend(d.references()),
                Some(brix_store::StoreObject::Text { references, .. }) => {
                    todo.extend(references.iter().cloned())
                }
                _ => {}
            }
        }
        seen
    }
}

/// `derivationToValue`: importing a `.drv` file.
pub fn imported_drv_to_value<'a>(
    ev: &Evaluator<'a>,
    pos: SrcPos,
    path: &PathVal<'a>,
    store_path: &StorePath,
) -> Result<Value<'a>> {
    let drv = ev
        .store
        .borrow()
        .derivation(store_path)
        .cloned()
        .ok_or_else(|| {
            ev.error_at(
                ErrorKind::Eval,
                msg!("path '", path.path, "' is not a valid derivation"),
                pos,
            )
        })?;
    let drv_base = ev.intern_store_path(store_path).base;
    let mut attrs: Vec<Attr<'a>> = Vec::new();
    attrs.push(ev.attr(
        sym::DRV_PATH,
        ev.alloc_string_value(path.path, &[ContextElem::DrvDeep(drv_base)]),
    ));
    attrs.push(
        ev.attr(
            sym::NAME,
            ev.alloc_string_value(
                drv.env
                    .get(b"name".as_slice())
                    .map_or(&b""[..], |name| name.as_slice()),
                &[],
            ),
        ),
    );
    let mut names: Vec<Value<'a>> = Vec::new();
    for (name, output) in &drv.outputs {
        attrs.push(ev.attr(
            ev.intern(name),
            ev.output_string(store_path, drv_base, &drv.name, name, output)?,
        ));
        names.push(ev.alloc_string_value(name, &[]));
    }
    attrs.push(ev.attr(sym::OUTPUTS, ev.mk_list_of_values(names)));
    let info = ev.alloc_cell(ev.mk_attrs(attrs, SrcPos::NONE));
    let wrapper = ev.imported_drv_wrapper()?;
    let f = ev.force(wrapper, pos)?;
    let v = ev.call(f, &[info], pos)?;
    let cell = ev.alloc_cell(v);
    ev.force_attrs(
        cell,
        pos,
        "while calling imported-drv-to-derivation.nix.gen.hh",
    )?;
    Ok(v)
}

impl<'a> Evaluator<'a> {
    fn imported_drv_wrapper(&self) -> Result<ValueRef<'a>> {
        if let Some(c) = self.internal.borrow().get("imported-drv-to-derivation") {
            return Ok(*c);
        }
        let cell = self.eval_internal("imported-drv-to-derivation.nix", IMPORTED_DRV_WRAPPER)?;
        self.internal
            .borrow_mut()
            .insert("imported-drv-to-derivation", cell);
        Ok(cell)
    }
}
