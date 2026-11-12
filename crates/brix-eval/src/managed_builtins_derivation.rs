//! Derivation construction over precise rooted values. Store operations consume
//! owned data; no evaluator heap references cross calls that may collect.

use crate::builtins::paths::new_hash_allow_empty;
use crate::builtins::strings::xp_settings;
use crate::error::{msg, ErrorKind};
use crate::evaluator::Evaluator;
use crate::managed_bindings;
use crate::managed_builtins::{
    coerce, force_attrs, force_bool, force_list, force_string, force_string_no_ctx,
};
use crate::managed_builtins_strings::{self as strings, OwnedContext};
use crate::managed_eval::{allocation, Machine};
use crate::managed_heap::{
    BindingsRoot, CellKind, CellRoot, RootedAttr, Roots, ValueRoot, ValueView,
};
use crate::managed_language::Language;
use crate::string_context::StringContext;
use crate::symbol::{sym, Symbol};
use crate::value::{ContextElem, SrcPos};
use crate::Result;
use brix_store::{
    ContentAddress, ContentAddressMethod, Derivation, DerivationOutput, HashAlgo, StorePath,
};
use bstr::{BString, ByteSlice};
use std::collections::{BTreeMap, BTreeSet};

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

struct DrvAttrs {
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
    context: Vec<OwnedContext>,
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

impl DrvAttrs {
    fn new(drv_name: &str, attrs: usize) -> DrvAttrs {
        DrvAttrs {
            drv: Derivation {
                name: drv_name.to_owned(),
                ..Derivation::default()
            },
            env: Vec::with_capacity(attrs),
            json_attrs: BTreeMap::new(),
            json_error: None,
            context: Vec::new(),
            content_addressed: false,
            is_impure: false,
            output_hash: None,
            output_hash_algo: None,
            ingestion_method: None,
            outputs: BTreeSet::from([BString::from("out")]),
        }
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
        crate::runtime::ordered_effect()?;
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
fn add_floating_outputs(ev: &Evaluator<'_>, drv: &mut Derivation, a: &DrvAttrs) -> Result<()> {
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

pub(crate) fn call<'r, 'h>(
    language: &Language<'_>,
    machine: &mut Machine<'h>,
    roots: &Roots<'r, 'h>,
    name: &str,
    args: &[CellRoot<'_, 'h>],
    pos: SrcPos,
) -> Option<Result<ValueRoot<'r, 'h>>> {
    Some(resolve(name)?(language, machine, roots, name, args, pos))
}

pub(crate) fn resolve(name: &str) -> Option<super::managed_builtins::Handler> {
    (name == "derivationStrict").then_some(dispatch)
}

fn dispatch<'r, 'h>(
    language: &Language<'_>,
    machine: &mut Machine<'h>,
    roots: &Roots<'r, 'h>,
    _name: &str,
    args: &[CellRoot<'_, 'h>],
    pos: SrcPos,
) -> Result<ValueRoot<'r, 'h>> {
    let attrs = force_attrs(
        language,
        machine,
        roots,
        args[0],
        pos,
        "while evaluating the argument passed to builtins.derivationStrict",
    )?;
    let name = machine
        .heap
        .bindings_view(attrs)
        .get(sym::NAME)
        .map(|attr| attr.root(roots))
        .ok_or_else(|| {
            language
                .legacy()
                .error(ErrorKind::TypeError, "attribute 'name' missing")
                .with_trace(
                    None,
                    "in the attrset passed as argument to builtins.derivationStrict",
                )
        })?;
    let drv_name = force_string_no_ctx(
        language,
        machine,
        roots,
        name.value,
        pos,
        "while evaluating the `name` attribute passed to builtins.derivationStrict",
    )
    .inspect_err(|error| {
        language.legacy().add_trace(
            error,
            name.pos,
            "while evaluating the derivation attribute 'name'",
        )
    })?;
    let value =
        derivation_strict(language, machine, roots, &drv_name, attrs).inspect_err(|error| {
            let position = language
                .legacy()
                .position(name.pos)
                .map(|p| p.display())
                .unwrap_or_default();
            error.add_trace(
                None,
                msg!(
                    "while evaluating derivation '",
                    drv_name,
                    "'\n  whose name attribute is located at ",
                    position
                ),
            );
        })?;
    Ok(value)
}

fn flag<'h>(
    language: &Language<'_>,
    machine: &mut Machine<'h>,
    roots: &Roots<'_, 'h>,
    attrs: BindingsRoot<'_, 'h>,
    name: Symbol,
    context: &str,
) -> Result<bool> {
    let field = machine
        .heap
        .bindings_view(attrs)
        .get(name)
        .map(|a| a.root(roots));
    match field {
        Some(field) => force_bool(language, machine, roots, field.value, SrcPos::NONE, context),
        None => Ok(false),
    }
}
fn field<'r, 'h>(
    machine: &mut Machine<'h>,
    roots: &Roots<'r, 'h>,
    name: Symbol,
    value: ValueRoot<'_, 'h>,
) -> Result<RootedAttr<'r, 'h>> {
    Ok(RootedAttr {
        name,
        pos: SrcPos::NONE,
        value: allocation(machine.heap.cell(roots, value))?,
    })
}
fn set<'r, 'h>(
    machine: &mut Machine<'h>,
    roots: &Roots<'r, 'h>,
    mut attrs: Vec<RootedAttr<'_, 'h>>,
) -> Result<ValueRoot<'r, 'h>> {
    managed_bindings::sort_dedup(&mut attrs);
    let set = allocation(managed_bindings::from_sorted(
        &mut machine.heap,
        roots,
        &attrs,
        SrcPos::NONE,
    ))?;
    Ok(machine.heap.bindings_value(roots, set))
}

impl DrvAttrs {
    fn add<'h>(
        &mut self,
        language: &Language<'_>,
        machine: &mut Machine<'h>,
        roots: &Roots<'_, 'h>,
        attr: RootedAttr<'_, 'h>,
        structured: bool,
        ignore_nulls: bool,
    ) -> Result<()> {
        let ev = language.legacy();
        let pos = SrcPos::NONE;
        if ignore_nulls {
            let value = language.force(machine, roots, attr.value, pos)?;
            if matches!(machine.heap.view(value), ValueView::Null) {
                return Ok(());
            }
        }
        match attr.name {
            sym::CONTENT_ADDRESSED | sym::IMPURE => {
                if force_bool(language, machine, roots, attr.value, pos, "")? {
                    if attr.name == sym::CONTENT_ADDRESSED {
                        self.content_addressed = true;
                        ev.require_experimental("ca-derivations", "")?;
                    } else {
                        self.is_impure = true;
                        ev.require_experimental("impure-derivations", "")?;
                    }
                }
                return Ok(());
            }
            sym::ARGS => {
                let list = force_list(language, machine, roots, attr.value, pos, "")?;
                let ValueView::List(view) = machine.heap.view(list) else {
                    unreachable!()
                };
                let len = view.len();
                self.drv.args.reserve(len);
                for index in 0..len {
                    machine.with_roots(|machine, temporary| -> Result<()> {
                        let ValueView::List(view) = machine.heap.view(list) else {
                            unreachable!()
                        };
                        let elem = view.get(index).unwrap().root(temporary);
                        let value = crate::managed_builtins::coerce_with_context(
                            language,
                            machine,
                            temporary,
                            elem,
                            pos,
                            crate::coerce::Coercion::MORE_COPY,
                            "while evaluating an element of the argument list",
                        )?;
                        let value = strings::materialize(language, machine, temporary, value)?;
                        strings::extend_context(machine, value, &mut self.context);
                        self.drv.args.push(strings::bytes(machine, value).into());
                        Ok(())
                    })?;
                }
                return Ok(());
            }
            _ if structured => return self.add_structured(language, machine, roots, attr),
            _ => {}
        }
        let value = coerce(language, machine, roots, attr.value, pos, true, true)?;
        let value = strings::materialize(language, machine, roots, value)?;
        strings::extend_context(machine, value, &mut self.context);
        let bytes = strings::bytes(machine, value);
        if attr.name == sym::JSON {
            #[cfg(brix_parallel)]
            machine.ordered_effect()?;
            return self.add_json(ev, bytes);
        }
        self.note_special(ev, attr.name, bytes)?;
        self.env
            .push((ev.symbol_name(attr.name).into(), bytes.into()));
        Ok(())
    }

    fn note_special(&mut self, ev: &Evaluator<'_>, name: Symbol, bytes: &[u8]) -> Result<()> {
        match name {
            sym::BUILDER => self.drv.builder = bytes.into(),
            sym::SYSTEM => self.drv.platform = bytes.into(),
            sym::OUTPUT_HASH => self.output_hash = Some(bytes.into()),
            sym::OUTPUT_HASH_ALGO => self.output_hash_algo = hash_algo(ev, bytes)?,
            sym::OUTPUT_HASH_MODE => {
                self.ingestion_method = Some(hash_mode(ev, bytes, &self.drv.name)?)
            }
            sym::OUTPUTS => set_outputs(ev, tokenize(bytes), &mut self.outputs)?,
            _ => {}
        }
        Ok(())
    }

    fn add_json(&mut self, ev: &Evaluator<'_>, bytes: &[u8]) -> Result<()> {
        ev.host.warn(format!("In derivation '{}': setting structured attributes via '__json' is deprecated, and may be disallowed in future versions of Nix. Set '__structuredAttrs = true' instead.", self.drv.name).as_bytes());
        let parsed: serde_json::Value = serde_json::from_slice(bytes).map_err(|error| {
            ev.error(
                ErrorKind::Eval,
                format!("cannot process __json attribute: {error}"),
            )
        })?;
        self.env
            .push(("__json".into(), crate::json::dump(&parsed).into()));
        Ok(())
    }

    fn add_structured<'h>(
        &mut self,
        language: &Language<'_>,
        machine: &mut Machine<'h>,
        roots: &Roots<'_, 'h>,
        attr: RootedAttr<'_, 'h>,
    ) -> Result<()> {
        if attr.name == sym::STRUCTURED_ATTRS {
            return Ok(());
        }
        let ev = language.legacy();
        let pos = SrcPos::NONE;
        let key = ev.symbol_name(attr.name);
        let (json, context, error) =
            strings::json_undumped(language, machine, roots, attr.value, pos)?;
        self.context.extend(context);
        if self.json_error.is_none() {
            self.json_error = crate::json::utf8_error(key).or(error);
        }
        self.json_attrs.insert(key.into(), json);
        match attr.name {
            sym::BUILDER => {
                let value = force_string(language, machine, roots, attr.value, pos, "")?;
                let value = strings::materialize(language, machine, roots, value)?;
                strings::extend_context(machine, value, &mut self.context);
                self.drv.builder = strings::bytes(machine, value).into();
            }
            sym::SYSTEM | sym::OUTPUT_HASH | sym::OUTPUT_HASH_ALGO | sym::OUTPUT_HASH_MODE => {
                let bytes = force_string_no_ctx(language, machine, roots, attr.value, pos, "")?;
                self.note_special(ev, attr.name, &bytes)?;
            }
            sym::OUTPUTS => {
                let list = force_list(language, machine, roots, attr.value, pos, "")?;
                let ValueView::List(view) = machine.heap.view(list) else {
                    unreachable!()
                };
                let len = view.len();
                let mut names = Vec::with_capacity(len);
                for index in 0..len {
                    let bytes = machine.with_roots(|machine, temporary| {
                        let ValueView::List(view) = machine.heap.view(list) else {
                            unreachable!()
                        };
                        let elem = view.get(index).unwrap().root(temporary);
                        force_string_no_ctx(language, machine, temporary, elem, pos, "")
                    })?;
                    names.push(bytes.into());
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
        #[cfg(brix_parallel)]
        machine.ordered_effect()?;
        ev.host.warn(format!("In a derivation named '{}', 'structuredAttrs' disables the effect of the derivation attribute '{ignored}'; use 'outputChecks.<output>.{ignored}' instead", self.drv.name).as_bytes());
        Ok(())
    }
}

fn derivation_strict<'r, 'h>(
    language: &Language<'_>,
    machine: &mut Machine<'h>,
    roots: &Roots<'r, 'h>,
    name: &[u8],
    attrs: BindingsRoot<'_, 'h>,
) -> Result<ValueRoot<'r, 'h>> {
    let ev = language.legacy();
    brix_store::store_path::validate_name(name).map_err(|error| invalid_name(ev, error))?;
    let name = std::str::from_utf8(name).expect("validated store name is ASCII");
    let structured = flag(
        language,
        machine,
        roots,
        attrs,
        sym::STRUCTURED_ATTRS,
        "while evaluating the `__structuredAttrs` attribute passed to builtins.derivationStrict",
    )?;
    let ignore_nulls = flag(
        language,
        machine,
        roots,
        attrs,
        sym::IGNORE_NULLS,
        "while evaluating the `__ignoreNulls` attribute passed to builtins.derivationStrict",
    )?;
    // Instantiating the dependencies is most of the work below: let idle
    // workers start on them while this one coerces the attributes in order.
    #[cfg(brix_parallel)]
    let _walk = machine
        .sparks
        .clone()
        .and_then(|sparks| sparks.offer_walk(machine, attrs, ev.call_depth.get()));
    let fields = managed_bindings::lexicographic(&machine.heap, roots, attrs, ev.keys);
    let mut state = DrvAttrs::new(name, fields.len());
    for attr in fields {
        if attr.name == sym::IGNORE_NULLS {
            continue;
        }
        machine
            .with_roots(|machine, temporary| {
                state.add(language, machine, temporary, attr, structured, ignore_nulls)
            })
            .inspect_err(|error| {
                ev.add_trace(
                    error,
                    attr.pos,
                    msg!(
                        "while evaluating attribute '",
                        ev.symbol_name(attr.name),
                        "' of derivation '",
                        name,
                        "'"
                    ),
                )
            })?;
    }
    let mut drv = std::mem::take(&mut state.drv);
    drv.env = std::mem::take(&mut state.env).into_iter().collect();
    if structured {
        drv.env
            .insert("__json".into(), structured_json(&state.json_attrs));
    }
    state.context.sort();
    state.context.dedup();
    add_context_inputs(
        ev,
        &mut drv,
        &strings::borrowed_context(&state.context)?,
        &state.json_error,
    )?;
    if drv.builder.is_empty() {
        return Err(eval_error(ev, "required attribute 'builder' missing"));
    }
    if drv.platform.is_empty() {
        return Err(eval_error(ev, "required attribute 'system' missing"));
    }
    if name.ends_with(".drv")
        && !(state.ingestion_method == Some(ContentAddressMethod::Text)
            && state.outputs.len() == 1
            && state.outputs.contains(b"out".as_slice()))
    {
        return Err(eval_error(ev, "derivation names are allowed to end in '.drv' only if they produce a single derivation file"));
    }
    let mut fixed_path = None;
    let deferred = if let Some(hash) = &state.output_hash {
        fixed_path = Some(add_fixed_output(
            ev,
            &mut drv,
            hash,
            state.output_hash_algo,
            state.ingestion_method,
            &state.outputs,
        )?);
        pending_json_error(ev, &state.json_error)?;
        false
    } else if state.content_addressed || state.is_impure {
        add_floating_outputs(ev, &mut drv, &state)?;
        false
    } else {
        pending_json_error(ev, &state.json_error)?;
        for output in &state.outputs {
            drv.env.insert(output.clone(), BString::default());
            drv.outputs
                .insert(output.clone(), DerivationOutput::Deferred);
        }
        true
    };
    let path = ev
        .add_derivation(drv, deferred)
        .map_err(|error| add_error(ev, error))?;
    let outputs = ev
        .store
        .borrow()
        .derivation(&path)
        .expect("derivation just added")
        .outputs
        .clone();
    let value = strings::string(
        machine,
        roots,
        ev.config.store_dir.display(&path).as_bytes(),
        &[OwnedContext::DrvDeep(path.as_str().as_bytes().to_vec())],
    )?;
    let mut attrs = vec![field(machine, roots, sym::DRV_PATH, value)?];
    for (output_name, output) in &outputs {
        let value = match (output, &fixed_path) {
            (DerivationOutput::CAFixed(_), Some(fixed)) => strings::string(
                machine,
                roots,
                ev.config.store_dir.display(fixed).as_bytes(),
                &[OwnedContext::Built {
                    drv: path.as_str().as_bytes().to_vec(),
                    output: output_name.to_vec(),
                }],
            )?,
            _ => output_string(language, machine, roots, &path, name, output_name, output)?,
        };
        attrs.push(field(machine, roots, ev.intern(output_name), value)?);
    }
    set(machine, roots, attrs)
}

pub(crate) fn output_string<'r, 'h>(
    language: &Language<'_>,
    machine: &mut Machine<'h>,
    roots: &Roots<'r, 'h>,
    drv_path: &StorePath,
    drv_name: &str,
    output_name: &[u8],
    output: &DerivationOutput,
) -> Result<ValueRoot<'r, 'h>> {
    let ev = language.legacy();
    let path = output
        .path(&ev.config.store_dir, drv_name, output_name)
        .map_err(|error| store_error(ev, error))?;
    let bytes = match path {
        Some(path) => ev.config.store_dir.display(&path),
        None => brix_store::derivation::downstream_placeholder(drv_path, output_name)
            .map_err(|error| store_error(ev, error))?,
    };
    strings::string(
        machine,
        roots,
        bytes.as_bytes(),
        &[OwnedContext::Built {
            drv: drv_path.as_str().as_bytes().to_vec(),
            output: output_name.to_vec(),
        }],
    )
}

pub(crate) fn imported_drv_to_value<'r, 'h>(
    language: &Language<'_>,
    machine: &mut Machine<'h>,
    roots: &Roots<'r, 'h>,
    pos: SrcPos,
    path: &[u8],
    store_path: &StorePath,
) -> Result<ValueRoot<'r, 'h>> {
    machine.with_roots(|machine, temporary| {
        let ev = language.legacy();
        let drv = ev
            .store
            .borrow()
            .derivation(store_path)
            .cloned()
            .ok_or_else(|| {
                ev.error_at(
                    ErrorKind::Eval,
                    msg!("path '", path, "' is not a valid derivation"),
                    pos,
                )
            })?;
        let value = strings::string(
            machine,
            temporary,
            path,
            &[OwnedContext::DrvDeep(
                store_path.as_str().as_bytes().to_vec(),
            )],
        )?;
        let mut attrs = vec![field(machine, temporary, sym::DRV_PATH, value)?];
        let name = drv
            .env
            .get(b"name".as_slice())
            .map_or(b"".as_slice(), |name| name.as_slice());
        let value = allocation(machine.heap.string(temporary, name))?;
        attrs.push(field(machine, temporary, sym::NAME, value)?);
        let names = temporary.vector::<CellKind>();
        for (name, output) in &drv.outputs {
            machine.with_roots(|machine, item_roots| -> Result<()> {
                let value = output_string(
                    language, machine, item_roots, store_path, &drv.name, name, output,
                )?;
                let attribute = field(machine, item_roots, ev.intern(name), value)?;
                attrs.push(RootedAttr {
                    name: attribute.name,
                    pos: attribute.pos,
                    value: temporary.copy(attribute.value),
                });
                let name = allocation(machine.heap.string(item_roots, name))?;
                names.push(allocation(machine.heap.cell(item_roots, name))?);
                Ok(())
            })?;
        }
        let names: Vec<_> = (0..names.len())
            .map(|index| names.get(temporary, index).unwrap())
            .collect();
        let names = allocation(machine.heap.list(temporary, &names))?;
        attrs.push(field(machine, temporary, sym::OUTPUTS, names)?);
        let info = set(machine, temporary, attrs)?;
        let info = allocation(machine.heap.cell(temporary, info))?;
        let wrapper = if let Some(wrapper) = machine.internal.get("imported-drv-to-derivation") {
            temporary.copy(wrapper.root())
        } else {
            let wrapper = language.eval_internal(
                machine,
                temporary,
                "imported-drv-to-derivation.nix",
                IMPORTED_DRV_WRAPPER,
            )?;
            machine
                .internal
                .insert("imported-drv-to-derivation", machine.heap.persist(wrapper));
            wrapper
        };
        let result = language.call_value(machine, temporary, wrapper, &[info], pos)?;
        let cell = allocation(machine.heap.cell(temporary, result))?;
        force_attrs(
            language,
            machine,
            temporary,
            cell,
            pos,
            "while calling imported-drv-to-derivation.nix.gen.hh",
        )?;
        Ok(roots.copy(result))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::managed_heap::with_owned_heap;

    #[test]
    fn derivations_match_legacy_with_collection_at_every_allocation() {
        let sources = [
            r#"builtins.toJSON (builtins.derivationStrict { name = "hello"; system = "x86_64-linux"; builder = "/bin/sh"; args = [ "-c" "echo hello" ]; })"#,
            r#"let d = derivation { name = "multi"; system = "x86_64-linux"; builder = "/bin/sh"; outputs = [ "dev" "out" ]; }; in builtins.toJSON { inherit (d) drvPath outPath outputName; output = d.out.outPath; all = map (x: x.outputName) d.all; ctx = builtins.getContext d.drvPath; }"#,
            r#"builtins.toJSON (builtins.derivationStrict { name = "fixed"; system = "builtin"; builder = "builtin:fetchurl"; outputHashAlgo = "sha256"; outputHash = "0000000000000000000000000000000000000000000000000000000000000000"; outputHashMode = "recursive"; })"#,
            r#"builtins.toJSON (builtins.derivationStrict { name = "structured"; system = "x86_64-linux"; builder = "/bin/sh"; __structuredAttrs = true; outputs = [ "out" "dev" ]; settings = { z = [ 1 true null ]; a = "value"; }; args = [ "hello" ]; })"#,
            r#"let dependency = derivation { name = "input"; system = "x86_64-linux"; builder = "/bin/sh"; }; in builtins.toJSON (builtins.derivationStrict { name = "uses-input"; system = "x86_64-linux"; builder = "/bin/sh"; source = dependency; closure = dependency.drvPath; })"#,
            r#"builtins.toJSON (builtins.derivationStrict { name = "ignore"; system = "x86_64-linux"; builder = "/bin/sh"; __ignoreNulls = true; ignored = null; values = [ 1 false null "value" ]; })"#,
            r#"builtins.toJSON (builtins.derivationStrict { name = "old-json"; system = "x86_64-linux"; builder = "/bin/sh"; __json = ''{"z":2,"a":1}''; })"#,
        ];
        for source in sources {
            let old_arena = crate::Arena::new();
            let new_arena = crate::Arena::new();
            let host = crate::NullHost;
            let legacy = crate::Evaluator::new(&old_arena, crate::Config::default(), &host);
            let expected = legacy
                .eval_str(source.as_bytes(), "/")
                .unwrap_or_else(|error| panic!("legacy {source}: {}", error.render(true)));
            let crate::value::Value::String(expected) = expected else {
                panic!("JSON result")
            };
            let expected = legacy.str_bytes(expected).unwrap().to_vec();
            let language = Language::new(crate::Evaluator::new(
                &new_arena,
                crate::Config::default(),
                &host,
            ));
            with_owned_heap(
                brix_gc::Config {
                    workers: 0,
                    stress: true,
                    chunk_bytes: 256,
                    steady_threshold_bytes: None,
                    trace_collections: false,
                    threshold_bytes: 1024,
                },
                |heap| {
                    let mut machine = Machine::new(heap);
                    machine.with_roots(|machine, roots| {
                        let actual = language
                            .eval_str(machine, roots, source.as_bytes(), "/")
                            .unwrap_or_else(|error| {
                                panic!("managed {source}: {}", error.render(true))
                            });
                        assert_eq!(strings::snapshot(machine, actual).0, expected, "{source}");
                        machine.heap.collect().unwrap();
                        assert_eq!(
                            strings::snapshot(machine, actual).0,
                            expected,
                            "after collection: {source}"
                        );
                    });
                    assert!(machine.heap.stats().collections > 0);
                },
            )
            .unwrap();
        }
    }

    #[test]
    fn derivation_errors_match_legacy() {
        let sources = [
            r#"builtins.derivationStrict {}"#,
            r#"builtins.derivationStrict { name = 1; }"#,
            r#"builtins.derivationStrict { name = "bad/name"; }"#,
            r#"builtins.derivationStrict { name = "missing"; }"#,
            r#"builtins.derivationStrict { name = "bad"; builder = "b"; system = "s"; outputs = [ "out" "out" ]; }"#,
            r#"builtins.derivationStrict { name = "bad"; builder = "b"; system = "s"; __structuredAttrs = true; outputs = []; }"#,
            r#"builtins.derivationStrict { name = "bad"; builder = "b"; system = "s"; a = {}; }"#,
            r#"builtins.derivationStrict { name = "bad"; builder = "b"; system = "s"; outputHashMode = "invalid"; }"#,
        ];
        for source in sources {
            let old_arena = crate::Arena::new();
            let new_arena = crate::Arena::new();
            let host = crate::NullHost;
            let legacy = crate::Evaluator::new(&old_arena, crate::Config::default(), &host);
            let expected = legacy
                .eval_str(source.as_bytes(), "/")
                .expect_err("legacy error");
            let language = Language::new(crate::Evaluator::new(
                &new_arena,
                crate::Config::default(),
                &host,
            ));
            with_owned_heap(
                brix_gc::Config {
                    stress: true,
                    ..Default::default()
                },
                |heap| {
                    let mut machine = Machine::new(heap);
                    machine.with_roots(|machine, roots| {
                        let actual = language
                            .eval_str(machine, roots, source.as_bytes(), "/")
                            .expect_err("managed error");
                        assert_eq!(actual.render(true), expected.render(true), "{source}");
                    });
                },
            )
            .unwrap();
        }
    }

    #[test]
    fn imported_derivation_wrapper_is_rooted_and_cached() {
        let arena = crate::Arena::new();
        let host = crate::NullHost;
        let language = Language::new(crate::Evaluator::new(
            &arena,
            crate::Config::default(),
            &host,
        ));
        with_owned_heap(brix_gc::Config { stress: true, chunk_bytes: 256, ..Default::default() }, |heap| {
            let mut machine = Machine::new(heap);
            machine.with_roots(|machine, roots| {
                let value = language.eval_str(machine, roots, br#"builtins.derivationStrict { name = "imported"; system = "x86_64-linux"; builder = "/bin/sh"; outputs = [ "out" "dev" ]; }"#, "/").unwrap();
                let ValueView::Attrs(set) = machine.heap.view(value) else { panic!("attrs") };
                let cell = set.get(sym::DRV_PATH).unwrap().value.root(roots);
                let path = machine.heap.value(roots, cell);
                let bytes = strings::snapshot(machine, path).0;
                let path = language.legacy().config.store_dir.maybe_parse(std::str::from_utf8(&bytes).unwrap()).unwrap();
                for _ in 0..2 {
                    let imported = imported_drv_to_value(&language, machine, roots, SrcPos::NONE, &bytes, &path).unwrap();
                    let cell = allocation(machine.heap.cell(roots, imported)).unwrap();
                    let (json, _, _) = strings::json_undumped(&language, machine, roots, cell, SrcPos::NONE).unwrap();
                    // Derivations serialize through outPath.
                    assert!(json.starts_with("\"/nix/store/"));
                    assert!(json.ends_with("-imported-dev\""));
                    machine.heap.collect().unwrap();
                }
                assert_eq!(machine.internal.len(), 1);
            });
        }).unwrap();
    }
}
