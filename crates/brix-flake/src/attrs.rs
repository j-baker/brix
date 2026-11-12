//! Fetcher input attributes (`src/libfetchers/attrs.cc`).
//!
//! An input is described by a flat map from attribute name to a string,
//! unsigned integer or boolean. This is the shape `builtins.fetchTree`
//! takes, the shape stored in `flake.lock`, and the shape flake references
//! are converted to and from.

use std::collections::BTreeMap;

use serde_json::{Map, Value};

use crate::{err, Error, Result};

/// One attribute value. (`fetchers::Attr`, without the lazy variant, which
/// only exists inside Nix's git fetcher.)
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Attr {
    String(String),
    Int(u64),
    Bool(bool),
}

impl From<&str> for Attr {
    fn from(s: &str) -> Self {
        Attr::String(s.to_string())
    }
}

impl From<String> for Attr {
    fn from(s: String) -> Self {
        Attr::String(s)
    }
}

impl From<u64> for Attr {
    fn from(n: u64) -> Self {
        Attr::Int(n)
    }
}

impl From<bool> for Attr {
    fn from(b: bool) -> Self {
        Attr::Bool(b)
    }
}

/// `fetchers::Attrs`: sorted by name, like `std::map`.
pub type Attrs = BTreeMap<String, Attr>;

/// Build an [`Attrs`] from pairs.
pub fn attrs<const N: usize>(pairs: [(&str, Attr); N]) -> Attrs {
    pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect()
}

/// `jsonToAttrs`.
pub fn json_to_attrs(json: &Value) -> Result<Attrs> {
    let Some(obj) = json.as_object() else {
        return Err(err!("unsupported input attribute type in lock file"));
    };
    let mut out = Attrs::new();
    for (k, v) in obj {
        let attr = match v {
            Value::Number(n) => match n.as_u64() {
                Some(n) => Attr::Int(n),
                None => return Err(err!("unsupported input attribute type in lock file")),
            },
            Value::String(s) => Attr::String(s.clone()),
            Value::Bool(b) => Attr::Bool(*b),
            _ => return Err(err!("unsupported input attribute type in lock file")),
        };
        out.insert(k.clone(), attr);
    }
    Ok(out)
}

/// `attrsToJSON`. The internal `__final` attribute is never serialised.
/// Keys come out sorted, as nlohmann's `json` (an ordered map) prints them.
pub fn attrs_to_json(attrs: &Attrs) -> Value {
    let mut obj = Map::new();
    for (k, v) in attrs {
        if k == "__final" {
            continue;
        }
        let v = match v {
            Attr::String(s) => Value::String(s.clone()),
            Attr::Int(n) => Value::Number((*n).into()),
            Attr::Bool(b) => Value::Bool(*b),
        };
        obj.insert(k.clone(), v);
    }
    Value::Object(obj)
}

/// `attrsToJSON(attrs).dump()`: the compact JSON Nix embeds in messages.
pub fn attrs_to_json_string(attrs: &Attrs) -> String {
    attrs_to_json(attrs).to_string()
}

/// `maybeGetStrAttr`.
pub fn maybe_get_str_attr<'a>(attrs: &'a Attrs, name: &str) -> Result<Option<&'a str>> {
    match attrs.get(name) {
        None => Ok(None),
        Some(Attr::String(s)) => Ok(Some(s)),
        Some(_) => Err(err!(
            "input attribute '{}' is not a string {}",
            name,
            attrs_to_json_string(attrs)
        )),
    }
}

/// `getStrAttr`.
pub fn get_str_attr<'a>(attrs: &'a Attrs, name: &str) -> Result<&'a str> {
    maybe_get_str_attr(attrs, name)?.ok_or_else(|| err!("input attribute '{}' is missing", name))
}

/// `maybeGetIntAttr`.
pub fn maybe_get_int_attr(attrs: &Attrs, name: &str) -> Result<Option<u64>> {
    match attrs.get(name) {
        None => Ok(None),
        Some(Attr::Int(n)) => Ok(Some(*n)),
        Some(_) => Err(err!("input attribute '{}' is not an integer", name)),
    }
}

/// `getIntAttr`.
pub fn get_int_attr(attrs: &Attrs, name: &str) -> Result<u64> {
    maybe_get_int_attr(attrs, name)?.ok_or_else(|| err!("input attribute '{}' is missing", name))
}

/// `maybeGetBoolAttr`.
pub fn maybe_get_bool_attr(attrs: &Attrs, name: &str) -> Result<Option<bool>> {
    match attrs.get(name) {
        None => Ok(None),
        Some(Attr::Bool(b)) => Ok(Some(*b)),
        Some(_) => Err(err!("input attribute '{}' is not a Boolean", name)),
    }
}

/// `getBoolAttr`.
pub fn get_bool_attr(attrs: &Attrs, name: &str) -> Result<bool> {
    maybe_get_bool_attr(attrs, name)?.ok_or_else(|| err!("input attribute '{}' is missing", name))
}

/// `attrsToQuery`: every attribute as a URL query parameter (booleans as
/// `1`/`0`).
pub fn attrs_to_query(attrs: &Attrs) -> BTreeMap<String, String> {
    attrs
        .iter()
        .map(|(k, v)| {
            let s = match v {
                Attr::String(s) => s.clone(),
                Attr::Int(n) => n.to_string(),
                Attr::Bool(b) => (if *b { "1" } else { "0" }).to_string(),
            };
            (k.clone(), s)
        })
        .collect()
}

/// Parse a JSON document into attrs, with Nix's error for non-objects.
pub fn parse_attrs_json(text: &str) -> Result<Attrs> {
    let json: Value = serde_json::from_str(text).map_err(|e| Error::Json(e.to_string()))?;
    json_to_attrs(&json)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_round_trip_drops_final_and_sorts() {
        let a = attrs([
            ("type", "github".into()),
            ("owner", "NixOS".into()),
            ("__final", true.into()),
            ("lastModified", 123u64.into()),
            ("flag", false.into()),
        ]);
        let json = attrs_to_json(&a);
        assert_eq!(
            json.to_string(),
            r#"{"flag":false,"lastModified":123,"owner":"NixOS","type":"github"}"#
        );
        let mut back = json_to_attrs(&json).unwrap();
        back.insert("__final".into(), Attr::Bool(true));
        assert_eq!(back, a);
    }

    #[test]
    fn typed_getters() {
        let a = attrs([("s", "x".into()), ("n", 7u64.into()), ("b", true.into())]);
        assert_eq!(get_str_attr(&a, "s").unwrap(), "x");
        assert_eq!(get_int_attr(&a, "n").unwrap(), 7);
        assert!(get_bool_attr(&a, "b").unwrap());
        assert_eq!(
            get_str_attr(&a, "n").unwrap_err().to_string(),
            r#"input attribute 'n' is not a string {"b":true,"n":7,"s":"x"}"#
        );
        assert_eq!(
            get_int_attr(&a, "missing").unwrap_err().to_string(),
            "input attribute 'missing' is missing"
        );
        assert_eq!(
            attrs_to_query(&a),
            [("b", "1"), ("n", "7"), ("s", "x")]
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect()
        );
    }
}
