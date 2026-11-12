//! Derivations as JSON (`adl_serializer<Derivation>`, format version 4),
//! read back for `nix derivation add`. Writing them (`nix derivation
//! show`) is the command line's.

use std::collections::{BTreeMap, BTreeSet};

use brix_eval::Evaluator;
use brix_store::{Derivation, DerivationOutput, StorePath};
use serde_json::{Map, Value};

/// `expectedJsonVersionDerivation`.
pub const DERIVATION_JSON_VERSION: u64 = 4;

/// nlohmann's compact `dump()` (keys sorted), the form of a derivation's
/// structured attributes (`__json`).
pub fn json_dump(value: &Value) -> String {
    brix_eval::json::dump(value)
}

/// nlohmann's `type_error` 316 for writing `s` as a JSON string, or `None`
/// when it is UTF-8: what `dump()` throws for a derivation's string that
/// is not (`nix derivation show`).
pub fn utf8_error(s: &[u8]) -> Option<String> {
    brix_eval::json::utf8_error(s)
}

/// `adl_serializer<Derivation>::from_json` (format version 4): the
/// inverse of `nix derivation show`'s format, as strict as Nix's. Store paths are
/// base names, every field but `structuredAttrs` is required, an output
/// has exactly the keys of one kind, and the errors are Nix's, with its
/// "while reading key '…'" traces. Structured attributes are stored the
/// way Nix writes `__json`: nlohmann's compact dump, keys sorted.
///
/// The derivation is not checked here; see
/// [`crate::Session::derivation_from_json`].
pub(crate) fn derivation_from_json<'a>(
    ev: &Evaluator<'a>,
    j: &Value,
) -> brix_eval::Result<Derivation> {
    let json = Json { ev };
    let obj = json.object(j)?;
    let name = json.string(json.key(obj, "name")?)?.to_owned();
    let version = json.key(obj, "version")?;
    let v = json.unsigned(version)?;
    if v != DERIVATION_JSON_VERSION {
        return Err(json.fail(format!(
            "Unsupported derivation JSON format version {v}, only format version {DERIVATION_JSON_VERSION} is currently supported."
        )));
    }
    let outputs = (|| {
        let mut outputs: BTreeMap<bstr::BString, DerivationOutput> = BTreeMap::new();
        for (out_name, o) in json.object(json.key(obj, "outputs")?)? {
            outputs.insert(out_name.as_str().into(), json.output(o)?);
        }
        Ok(outputs)
    })()
    .map_err(|e| traced(e, "while reading key 'outputs'"))?;
    let platform = json.string(json.key(obj, "system")?)?.into();
    let builder = json.string(json.key(obj, "builder")?)?.into();
    let args = json
        .array(json.key(obj, "args")?)?
        .iter()
        .map(|a| json.string(a).map(Into::into))
        .collect::<brix_eval::Result<Vec<_>>>()?;
    let env_json = json.key(obj, "env")?;
    let mut env = (|| {
        json.object(env_json)?
            .iter()
            .map(|(k, v)| Ok((k.as_str().into(), json.string(v)?.into())))
            .collect::<brix_eval::Result<BTreeMap<_, _>>>()
    })()
    .map_err(|e| traced(e, "while reading key 'env'"))?;
    if let Some(structured) = obj.get("structuredAttrs") {
        // `StructuredAttrs` holds an object.
        if !structured.is_object() {
            return Err(json.fail(format!(
                "[json.exception.type_error.302] type must be object, but is {}",
                type_name(structured)
            )));
        }
        env.insert(
            brix_store::derivation::STRUCTURED_ATTRS_ENV.into(),
            json_dump(structured).into(),
        );
    }
    let (input_srcs, input_drvs) = (|| {
        let inputs = json.object(json.key(obj, "inputs")?)?;
        let srcs = (|| {
            json.array(json.key(inputs, "srcs")?)?
                .iter()
                .map(|s| json.store_path(s))
                .collect::<brix_eval::Result<BTreeSet<_>>>()
        })()
        .map_err(|e| traced(e, "while reading key 'srcs'"))?;
        let drvs = (|| {
            let mut drvs = BTreeMap::new();
            for (drv, node) in json.object(json.key(inputs, "drvs")?)? {
                let path = StorePath::new(drv).map_err(|e| json.fail(e.to_string()))?;
                let node = json.object(node)?;
                let outputs = json
                    .array(json.key(node, "outputs")?)?
                    .iter()
                    .map(|o| json.string(o).map(bstr::BString::from))
                    .collect::<brix_eval::Result<BTreeSet<_>>>()?;
                if let Some((output, _)) = json
                    .object(json.key(node, "dynamicOutputs")?)?
                    .iter()
                    .next()
                {
                    ev.require_experimental(
                        "dynamic-derivations",
                        &format!("dynamic output '{output}' in JSON"),
                    )?;
                    return Err(json
                        .fail("dynamic derivation outputs are not supported by brix".to_owned()));
                }
                drvs.insert(path, outputs);
            }
            Ok(drvs)
        })()
        .map_err(|e| traced(e, "while reading key 'drvs'"))?;
        Ok((srcs, drvs))
    })()
    .map_err(|e| traced(e, "while reading key 'inputs'"))?;
    Ok(Derivation {
        name,
        outputs,
        input_srcs,
        input_drvs,
        platform,
        builder,
        args,
        env,
    })
}

/// `e` with Nix's `e.addTrace({}, trace)`.
fn traced(e: brix_eval::Error, trace: &str) -> brix_eval::Error {
    e.add_trace(None, trace);
    e
}

/// nlohmann's `type_name()`.
fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// Nix's `json-utils.cc` accessors, with their errors.
struct Json<'e, 'a> {
    ev: &'e Evaluator<'a>,
}

impl Json<'_, '_> {
    fn fail(&self, msg: String) -> brix_eval::Error {
        self.ev.error(brix_eval::ErrorKind::Eval, msg)
    }

    /// `ensureType`.
    fn wrong_type(&self, v: &Value, expected: &str) -> brix_eval::Error {
        self.fail(format!(
            "Expected JSON value to be of type '{expected}' but it is of type '{}': {}",
            type_name(v),
            json_dump(v)
        ))
    }

    fn object<'v>(&self, v: &'v Value) -> brix_eval::Result<&'v Map<String, Value>> {
        v.as_object().ok_or_else(|| self.wrong_type(v, "object"))
    }

    fn array<'v>(&self, v: &'v Value) -> brix_eval::Result<&'v Vec<Value>> {
        v.as_array().ok_or_else(|| self.wrong_type(v, "array"))
    }

    fn string<'v>(&self, v: &'v Value) -> brix_eval::Result<&'v str> {
        v.as_str().ok_or_else(|| self.wrong_type(v, "string"))
    }

    /// `getUnsigned`.
    fn unsigned(&self, v: &Value) -> brix_eval::Result<u64> {
        match v {
            Value::Number(n) => n.as_u64().ok_or_else(|| {
                let kind = if n.is_i64() {
                    "signed integral number"
                } else {
                    "floating point number"
                };
                self.fail(format!(
                    "Expected JSON value to be an unsigned integral number but it is of type '{kind}': {}",
                    json_dump(v)
                ))
            }),
            other => Err(self.fail(format!(
                "Expected JSON value to be an unsigned integral number but it is of type '{}': {}",
                type_name(other),
                json_dump(other)
            ))),
        }
    }

    /// `valueAt`.
    fn key<'v>(&self, obj: &'v Map<String, Value>, key: &str) -> brix_eval::Result<&'v Value> {
        obj.get(key).ok_or_else(|| {
            self.fail(format!(
                "Expected JSON object to contain key '{key}' but it doesn't: {}",
                json_dump(&Value::Object(obj.clone()))
            ))
        })
    }

    /// `adl_serializer<StorePath>::from_json`: a base name.
    fn store_path(&self, v: &Value) -> brix_eval::Result<StorePath> {
        StorePath::new(self.string(v)?).map_err(|e| self.fail(e.to_string()))
    }

    /// `adl_serializer<DerivationOutput>::from_json`: the output kind is
    /// told by its exact set of keys.
    fn output(&self, v: &Value) -> brix_eval::Result<DerivationOutput> {
        use brix_store::{ContentAddress, ContentAddressMethod, HashAlgo};
        let obj = self.object(v)?;
        let keys: BTreeSet<&str> = obj.keys().map(String::as_str).collect();
        let keys: Vec<&str> = keys.into_iter().collect();
        let store_err = |e: &dyn std::fmt::Display| self.fail(e.to_string());
        // Hashes parse under the session's experimental features, as Nix's
        // global settings do for `derivation add`.
        let xp = brix_store::XpSettings::from_names(
            self.ev
                .config
                .experimental_features
                .iter()
                .map(String::as_str),
        );
        let method = || {
            ContentAddressMethod::parse(self.string(self.key(obj, "method")?)?)
                .map_err(|e| store_err(&e))
        };
        // Text-hashed outputs come only with dynamic derivations.
        let no_text = |method: ContentAddressMethod| -> brix_eval::Result<ContentAddressMethod> {
            if method == ContentAddressMethod::Text {
                self.ev.require_experimental(
                    "dynamic-derivations",
                    "text-hashed derivation output in JSON",
                )?;
            }
            Ok(method)
        };
        let hash_algo = || {
            HashAlgo::parse(self.string(self.key(obj, "hashAlgo")?)?, xp).map_err(|e| store_err(&e))
        };
        Ok(match keys.as_slice() {
            ["path"] => DerivationOutput::InputAddressed(self.store_path(&obj["path"])?),
            ["hash", "method"] => {
                let method = method()?;
                let hash = brix_store::Hash::parse_sri(self.string(&obj["hash"])?, xp)
                    .map_err(|e| store_err(&e))?;
                DerivationOutput::CAFixed(ContentAddress {
                    method: no_text(method)?,
                    hash,
                })
            }
            ["hashAlgo", "method"] => {
                self.ev.require_experimental("ca-derivations", "")?;
                DerivationOutput::CAFloating {
                    method: no_text(method()?)?,
                    hash_algo: hash_algo()?,
                }
            }
            [] => DerivationOutput::Deferred,
            ["hashAlgo", "impure", "method"] => {
                self.ev.require_experimental("impure-derivations", "")?;
                DerivationOutput::Impure {
                    method: no_text(method()?)?,
                    hash_algo: hash_algo()?,
                }
            }
            _ => return Err(self.fail("invalid JSON for derivation output".to_owned())),
        })
    }
}
