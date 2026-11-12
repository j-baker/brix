//! Flake schemas: how Determinate Nix understands a flake's outputs
//! (`src/libcmd/flake-schemas.cc`). A schema per output name says how to
//! enumerate it (`inventory`), which commands may use it (`roles`), and
//! what a leaf is (`what`, `derivationAttrPath`, `evalChecks`). `nix flake
//! show`, `nix flake check` and installable resolution all go through it.
//!
//! The schemas themselves are Nix expressions. brix carries its own
//! rendition of Nix's built-in default schemas; a flake providing a
//! `schemas` output replaces them, exactly as in Nix.
//!
//! This module evaluates the schemas and reads their inventory; what
//! `nix flake show` and `nix flake check` make of it is the command
//! line's.

use std::collections::{BTreeMap, BTreeSet};

use brix_eval::Origin;
use brix_flake::FlakeRef;

use crate::cursor::Cursor;
use crate::error::{Error, Result};
use crate::session::{Flake, Session};
use crate::value_slots::Lease;

/// brix's rendering of `call-flake-schemas.nix`: given the default
/// schemas flake (or `null` when the flake brings its own) and the flake,
/// returns `{ outputs; inventory; }` where `inventory.<name>` is the
/// schema plus its `output` (the inventory of that output), or `{ unknown
/// = true; }` for outputs without a schema.
const CALL_FLAKE_SCHEMAS: &str = include_str!("schemas/call-flake-schemas.nix");

/// brix's rendition of the default schemas Nix embeds. The `doc` texts
/// and `what` labels are observable (`nix flake show --json` prints them),
/// so they match Nix's; the structure follows the published
/// flake-schemas format (version 1: `doc`, `inventory`, optional `roles`,
/// `appendSystem`, `defaultAttrPath`, `allowIFD`).
const DEFAULT_SCHEMAS: &str = include_str!("schemas/default-schemas.nix");

/// The `{ outputs; inventory; }` value for `flake`, evaluated once per
/// flake and kept in the session.
pub fn schema_root<'s>(
    session: &'s Session,
    flake: &Flake<'s>,
    default_schemas: Option<&FlakeRef>,
) -> Result<Cursor<'s>> {
    if !std::ptr::eq(session, flake.session()) {
        return Err(Error::Usage(
            "a flake belongs to a different session".into(),
        ));
    }
    if let Some(slot) = flake.schema_slot() {
        return Ok(Cursor::new(session, slot, Vec::new()));
    }
    // A flake that defines `schemas` needs no defaults (and Nix passes
    // `null` then, so the default flake is never fetched).
    let has_own = flake.outputs()?.maybe_get_attr("schemas")?.is_some();
    let default_slot = if has_own {
        None
    } else {
        match default_schemas {
            Some(flake_ref) => Some(session.flake(flake_ref)?.root().slot),
            None => Some(eval_internal(
                session,
                DEFAULT_SCHEMAS,
                "builtin-flake-schemas/flake.nix",
            )?),
        }
    };
    let slot = session.with_slot(&flake.root().slot, |ctx, flake_cell| {
        let default_cell = match &default_slot {
            Some(slot) => ctx.slot(slot),
            None => ctx.ev.null().map_err(|e| session.eval_error(e))?,
        };
        let ev = &mut *ctx.ev;
        let fun = ev
            .eval_source(
                CALL_FLAKE_SCHEMAS.as_bytes(),
                "/",
                Origin::Internal {
                    name: "call-flake-schemas.nix".into(),
                },
            )
            .map_err(|e| session.eval_error(e))?;
        let partial = ev
            .apply(&fun, &default_cell)
            .map_err(|e| session.eval_error(e))?;
        let res = ev
            .apply(&partial, &flake_cell)
            .map_err(|e| session.eval_error(e))?;
        Ok::<_, Error>(ctx.keep_value(res))
    })?;
    flake.set_schema_slot(slot.clone());
    Ok(Cursor::new(session, slot, Vec::new()))
}

fn eval_internal(session: &Session, src: &str, name: &str) -> Result<Lease> {
    session.with(|ctx| {
        let v = ctx
            .ev
            .eval_source(
                src.as_bytes(),
                "/",
                Origin::Internal {
                    name: name.to_owned(),
                },
            )
            .map_err(|e| session.eval_error(e))?;
        Ok(ctx.keep_value(v))
    })
}

/// What a schema says about itself (`SchemaInfo`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SchemaInfo {
    pub roles: BTreeSet<String>,
    pub append_system: bool,
    pub default_attr_path: Option<Vec<String>>,
}

/// The schemas in an inventory (`getSchemas`).
#[derive(Clone, Debug, Default)]
pub struct Schemas {
    pub map: BTreeMap<String, SchemaInfo>,
}

impl Schemas {
    pub fn from_inventory(inventory: &Cursor<'_>) -> Result<Schemas> {
        let mut map = BTreeMap::new();
        for name in inventory.get_attrs()? {
            let schema = inventory.get_attr(&name)?;
            let mut info = SchemaInfo::default();
            if let Some(roles) = schema.maybe_get_attr("roles")? {
                info.roles = roles.get_attrs()?.into_iter().collect();
            }
            if let Some(a) = schema.maybe_get_attr("appendSystem")? {
                info.append_system = a.get_bool()?;
            }
            if let Some(d) = schema.maybe_get_attr("defaultAttrPath")? {
                info.default_attr_path = Some(d.get_list_of_strings()?);
            }
            map.insert(name, info);
        }
        Ok(Schemas { map })
    }

    /// Nix's precedence between outputs: the historical ones first, then
    /// the rest alphabetically.
    pub fn sorted_names(&self) -> Vec<String> {
        let mut out = Vec::new();
        let mut seen = BTreeSet::new();
        for name in [
            "apps",
            "defaultApp",
            "devShells",
            "devShell",
            "packages",
            "defaultPackage",
            "legacyPackages",
        ] {
            if self.map.contains_key(name) {
                out.push(name.to_owned());
                seen.insert(name.to_owned());
            }
        }
        for name in self.map.keys() {
            if !seen.contains(name) {
                out.push(name.clone());
            }
        }
        out
    }
}

/// One flake output as `forEachOutput` yields it.
pub struct OutputEntry<'s> {
    pub name: String,
    /// `None` for an output without a schema.
    pub output: Option<Cursor<'s>>,
    pub doc: String,
}

/// `forEachOutput`: the inventory's outputs in Nix's order (those whose
/// schema forbids import-from-derivation first), skipping schemas with
/// no corresponding output.
pub fn for_each_output<'s>(inventory: &Cursor<'s>) -> Result<Vec<OutputEntry<'s>>> {
    let names = inventory.get_attrs()?;
    let mut out = Vec::new();
    for allow_ifd in [false, true] {
        for name in &names {
            let info = inventory.get_attr(name)?;
            let allows = match info.maybe_get_attr("allowIFD")? {
                Some(a) => a.get_bool()?,
                None => true,
            };
            if allow_ifd != allows {
                continue;
            }
            let is_unknown = info.maybe_get_attr("unknown")?.is_some();
            let output = info.maybe_get_attr("output")?;
            if output.is_none() && !is_unknown {
                continue;
            }
            let doc = if is_unknown {
                String::new()
            } else {
                info.get_attr("doc")?.get_string()?
            };
            out.push(OutputEntry {
                name: name.clone(),
                output: if is_unknown { None } else { output },
                doc,
            });
        }
    }
    Ok(out)
}

/// `Node::forSystems`.
pub fn for_systems(node: &impl crate::cursor::ReadCursor) -> Result<Option<Vec<String>>> {
    match node.maybe_get_attr("forSystems")? {
        Some(f) => Ok(Some(f.get_list_of_strings()?)),
        None => Ok(None),
    }
}

/// The derivation a leaf refers to, if any (`Leaf::derivation`).
pub fn leaf_derivation<C: crate::cursor::ReadCursor>(leaf: &C, outputs: &C) -> Result<Option<C>> {
    match leaf.maybe_get_attr("derivationAttrPath")? {
        None => leaf.maybe_get_attr("derivation"),
        Some(p) => {
            let path = p.get_list_of_strings()?;
            // Inventory nodes carry the flake output attribute path they
            // describe (`cleanupAttrPath`; see `Cursor::renamed`).
            let output = outputs
                .find_along_attr_path(leaf.attr_path())?
                .ok_or_else(|| {
                    Error::Usage(
                        format!(
                        "flake output '{}' should exist according to its schema, but it doesn't",
                        leaf.attr_path_str()
                    )
                        .into(),
                    )
                })?;
            match output.find_along_attr_path(&path)? {
                Some(d) => Ok(Some(d)),
                None => Err(Error::Usage(
                    format!(
                        "flake output '{}' does not have a derivation attribute '{}'",
                        leaf.attr_path_str(),
                        crate::cursor::attr_path_to_string(&path)
                    )
                    .into(),
                )),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    /// The files the schemas are read from, by their source path.
    const FILES: [(&str, &str); 2] = [
        ("schemas/default-schemas.nix", super::DEFAULT_SCHEMAS),
        ("schemas/call-flake-schemas.nix", super::CALL_FLAKE_SCHEMAS),
    ];

    #[test]
    fn default_schemas_parse() {
        for (name, src) in FILES {
            let mut warnings = Vec::new();
            if let Err(e) = brix_syntax::parse(
                src.as_bytes(),
                "/",
                &brix_syntax::ParseOptions::default(),
                &mut warnings,
            ) {
                panic!("{name} is not valid Nix: {e}");
            }
            assert_eq!(warnings, vec![], "{name} parses without warnings");
        }
    }

    /// Nix's own parser accepts them too (`nix-instantiate --parse`).
    #[test]
    fn nix_parses_the_schemas() {
        let nix = std::env::var("BRIX_NIX_BIN").expect(
            "BRIX_NIX_BIN must point at the reference nix binary (run inside `nix develop`)",
        );
        let instantiate = std::path::Path::new(&nix).with_file_name("nix-instantiate");
        for (name, _) in FILES {
            let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("src")
                .join(name);
            let out = std::process::Command::new(&instantiate)
                .arg("--parse")
                .arg(&path)
                .output()
                .unwrap_or_else(|e| panic!("running '{}': {e}", instantiate.display()));
            assert!(
                out.status.success(),
                "nix-instantiate --parse {name}:\n{}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }
}
