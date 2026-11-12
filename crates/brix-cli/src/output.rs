//! Output formats shared with Nix's CLI: `nix derivation show` and
//! `nix flake metadata --json`.

use std::collections::BTreeMap;

use brix::derivation_json::DERIVATION_JSON_VERSION;
use brix::{Error, LockedFlake, Result, TreeHandle};
use brix_flake::attrs::attrs_to_json;
use brix_store::{Derivation, DerivationOutput, HashFormat, StorePath};
use serde_json::{json, Map, Value};

/// Inserts into a JSON object whose keys end up sorted, as nlohmann's
/// `std::map`-backed objects are.
fn sorted_object(entries: Vec<(String, Value)>) -> Value {
    let mut map: BTreeMap<String, Value> = BTreeMap::new();
    for (k, v) in entries {
        map.insert(k, v);
    }
    Value::Object(map.into_iter().collect::<Map<String, Value>>())
}

/// `adl_serializer<DerivationOutput>::to_json`.
fn output_json(output: &DerivationOutput) -> Value {
    match output {
        DerivationOutput::InputAddressed(p) => {
            sorted_object(vec![("path".into(), json!(p.as_str()))])
        }
        DerivationOutput::CAFixed(ca) => sorted_object(vec![
            (
                "hash".into(),
                json!(ca.hash.to_string(HashFormat::Sri, true)),
            ),
            ("method".into(), json!(ca.method.render())),
        ]),
        DerivationOutput::CAFloating { method, hash_algo } => sorted_object(vec![
            ("hashAlgo".into(), json!(hash_algo.to_string())),
            ("method".into(), json!(method.render())),
        ]),
        DerivationOutput::Deferred => sorted_object(Vec::new()),
        DerivationOutput::Impure { method, hash_algo } => sorted_object(vec![
            ("hashAlgo".into(), json!(hash_algo.to_string())),
            ("impure".into(), json!(true)),
            ("method".into(), json!(method.render())),
        ]),
    }
}

/// A string of a derivation (or an output name), which is bytes, as JSON
/// text. nlohmann's `dump()` refuses to write one that is not UTF-8, and
/// the command (`nix derivation show`, `nix build --json`) fails with its
/// error.
pub(crate) fn json_text(s: &[u8]) -> Result<&str> {
    std::str::from_utf8(s).map_err(|_| {
        Error::Store(
            brix::derivation_json::utf8_error(s)
                .expect("not UTF-8")
                .into(),
        )
    })
}

/// `adl_serializer<Derivation>::to_json`: one derivation as `nix
/// derivation show` prints it (store paths as basenames, structured attrs
/// split out of `env`). The strings are converted in the order nlohmann
/// writes them (keys sorted: `args`, `builder`, `env`, `inputs` with the
/// input derivations' output names, `name`, `outputs` keyed by output
/// name, `system`), so the error for one that is not UTF-8 is the one Nix
/// reports first.
pub fn derivation_json(drv: &Derivation) -> Result<Value> {
    let args = drv
        .args
        .iter()
        .map(|a| json_text(a).map(|a| json!(a)))
        .collect::<Result<Vec<_>>>()?;
    let builder = json!(json_text(&drv.builder)?);
    let mut env: Vec<(String, Value)> = Vec::new();
    let mut structured: Option<Value> = None;
    for (k, v) in &drv.env {
        if k == brix_store::derivation::STRUCTURED_ATTRS_ENV {
            structured = Some(serde_json::from_slice(v).map_err(|e| {
                Error::Store(
                    format!(
                        "derivation '{}' has malformed structured attrs: {e}",
                        drv.name
                    )
                    .into(),
                )
            })?);
        } else {
            env.push((json_text(k)?.to_owned(), json!(json_text(v)?)));
        }
    }
    // Output names are bytes, as Nix's: an input's come before the
    // derivation's own in the document.
    let drvs: Vec<(String, Value)> = drv
        .input_drvs
        .iter()
        .map(|(path, outs)| {
            let outs = outs
                .iter()
                .map(|o| json_text(o).map(|o| json!(o)))
                .collect::<Result<Vec<_>>>()?;
            Ok((
                path.as_str().to_owned(),
                sorted_object(vec![
                    ("dynamicOutputs".into(), sorted_object(Vec::new())),
                    ("outputs".into(), Value::Array(outs)),
                ]),
            ))
        })
        .collect::<Result<_>>()?;
    let srcs: Vec<Value> = drv.input_srcs.iter().map(|p| json!(p.as_str())).collect();
    let outputs: Vec<(String, Value)> = drv
        .outputs
        .iter()
        .map(|(name, o)| Ok((json_text(name)?.to_owned(), output_json(o))))
        .collect::<Result<_>>()?;
    let system = json!(json_text(&drv.platform)?);
    let mut entries = vec![
        ("args".into(), Value::Array(args)),
        ("builder".into(), builder),
        ("env".into(), sorted_object(env)),
        (
            "inputs".into(),
            sorted_object(vec![
                ("drvs".into(), sorted_object(drvs)),
                ("srcs".into(), Value::Array(srcs)),
            ]),
        ),
        ("name".into(), json!(drv.name)),
        ("outputs".into(), sorted_object(outputs)),
        ("system".into(), system),
        ("version".into(), json!(DERIVATION_JSON_VERSION)),
    ];
    if let Some(s) = structured {
        entries.push(("structuredAttrs".into(), s));
    }
    Ok(sorted_object(entries))
}

/// The document `nix derivation show` prints: `{"derivations": {…},
/// "version": 4}`. Derivations are converted in path order, the order of
/// the document.
pub fn derivation_show_json(drvs: &[(StorePath, Derivation)]) -> Result<Value> {
    let mut sorted: Vec<&(StorePath, Derivation)> = drvs.iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(&b.0));
    let mut entries = Vec::with_capacity(drvs.len());
    for (path, drv) in sorted {
        entries.push((path.as_str().to_owned(), derivation_json(drv)?));
    }
    Ok(sorted_object(vec![
        ("derivations".into(), sorted_object(entries)),
        ("version".into(), json!(DERIVATION_JSON_VERSION)),
    ]))
}

/// `nix flake metadata --json`, without `fingerprint` (brix keeps no
/// evaluation cache to key) and `path` (the caller knows whether the tree
/// is in a store).
pub fn flake_metadata_json(locked: &LockedFlake<TreeHandle>) -> Result<Value> {
    let flake = &locked.flake;
    let mut entries: Vec<(String, Value)> = Vec::new();
    if let Some(d) = &flake.metadata.description {
        entries.push(("description".into(), json!(d)));
    }
    entries.push(("originalUrl".into(), json!(flake.original_ref.to_string())));
    entries.push((
        "original".into(),
        attrs_to_json(&flake.original_ref.to_attrs()),
    ));
    entries.push(("resolvedUrl".into(), json!(flake.resolved_ref.to_string())));
    entries.push((
        "resolved".into(),
        attrs_to_json(&flake.resolved_ref.to_attrs()),
    ));
    entries.push(("url".into(), json!(flake.locked_ref.to_string())));
    entries.push(("locked".into(), attrs_to_json(&flake.locked_ref.to_attrs())));
    let input = &flake.locked_ref.input;
    if let Some(rev) = input.rev()? {
        entries.push(("revision".into(), json!(rev.to_hex())));
    }
    if let Some(brix_flake::Attr::String(dirty)) = input.attrs.get("dirtyRev") {
        entries.push(("dirtyRevision".into(), json!(dirty)));
    }
    if let Some(n) = input.rev_count()? {
        entries.push(("revCount".into(), json!(n)));
    }
    if let Some(t) = input.last_modified()? {
        entries.push(("lastModified".into(), json!(t)));
    }
    let (locks, _) = locked.lock_file.to_json();
    entries.push(("locks".into(), locks));
    Ok(sorted_object(entries))
}

/// Serialises like nlohmann: compact, or `dump(2)` when `pretty`. Keys
/// keep the order `value` has.
pub fn json_to_string(value: &Value, pretty: bool) -> String {
    if pretty {
        brix_flake::lockfile::pretty_json(value)
    } else {
        value.to_string()
    }
}
