//! `nix flake show --json`'s inventory over the facade's flake schemas
//! ([`brix::schemas`]), and `nix flake check`'s walk ([`brix::check`]).

use brix::schemas::{for_each_output, for_systems, leaf_derivation};
use brix::{Cursor, Error, Result, Session};
use serde_json::{json, Map, Value as Json};

/// `FlakeInventoryOptions`.
#[derive(Clone, Debug, Default)]
pub struct InventoryOptions {
    pub show_legacy: bool,
    pub show_all_systems: bool,
    pub show_output_paths: bool,
    pub show_drv_paths: bool,
    pub show_drv_names: bool,
}

/// `getFlakeInventory`: the JSON `nix flake show --json` prints under
/// `inventory`.
pub fn inventory_json(
    session: &Session,
    root: &Cursor<'_>,
    options: &InventoryOptions,
    local_system: &str,
) -> Result<Json> {
    let inventory = root.get_attr("inventory")?;
    let outputs = root.get_attr("outputs")?.renamed(Vec::new());
    // nlohmann sorts object keys; the `allowIFD` ordering of the walk is
    // not meant to show.
    let mut inv: std::collections::BTreeMap<String, Json> = std::collections::BTreeMap::new();
    for entry in for_each_output(&inventory)? {
        let mut j = Map::new();
        match entry.output {
            Some(output) => {
                j.insert("doc".into(), json!(entry.doc));
                let mut obj = Map::new();
                // The node's attribute path within the output, for
                // `derivationAttrPath` lookups and error messages.
                let node = output.renamed(vec![entry.name.clone()]);
                visit_json(session, &node, &outputs, options, local_system, &mut obj)?;
                j.insert("output".into(), Json::Object(obj));
            }
            None => {
                j.insert("unknown".into(), json!(true));
            }
        }
        inv.insert(entry.name, Json::Object(j));
    }
    Ok(Json::Object(inv.into_iter().collect()))
}

fn visit_json(
    session: &Session,
    node: &Cursor<'_>,
    outputs: &Cursor<'_>,
    options: &InventoryOptions,
    local_system: &str,
    obj: &mut Map<String, Json>,
) -> Result<()> {
    if let Some(children) = inventory_node(session, node, outputs, options, local_system, obj)? {
        let mut kids = Map::new();
        let is_legacy_packages = node
            .attr_path()
            .first()
            .is_some_and(|a| a == "legacyPackages");
        for name in children.get_attrs()? {
            let child = children.get_attr(&name)?;
            let mut path = node.attr_path().to_vec();
            path.push(name.clone());
            let child = child.renamed(path);
            let mut j = Map::new();
            match visit_json(session, &child, outputs, options, local_system, &mut j) {
                Ok(()) => {}
                Err(Error::Eval { .. }) if is_legacy_packages => {
                    j.insert("failed".into(), json!(true));
                }
                Err(e) => return Err(e),
            }
            kids.insert(name, Json::Object(j));
        }
        obj.insert("children".into(), Json::Object(kids));
    }
    Ok(())
}

/// Read one inventory node without evaluating its descendants. Text output
/// uses this to print each parent and leaf before visiting the next node.
pub fn inventory_node<'s>(
    session: &Session,
    node: &Cursor<'s>,
    outputs: &Cursor<'s>,
    options: &InventoryOptions,
    local_system: &str,
    obj: &mut Map<String, Json>,
) -> Result<Option<Cursor<'s>>> {
    if !options.show_legacy {
        if let Some(b) = node.maybe_get_attr("isLegacy")? {
            if b.get_bool()? {
                obj.insert("isLegacy".into(), json!(true));
                return Ok(None);
            }
        }
    }
    if !options.show_all_systems {
        if let Some(systems) = for_systems(node)? {
            if !systems.iter().any(|s| s == local_system) {
                obj.insert("filtered".into(), json!(true));
                return Ok(None);
            }
        }
    }
    if let Some(children) = node.maybe_get_attr("children")? {
        return Ok(Some(children));
    }
    // A leaf.
    if let Some(what) = node.maybe_get_attr("what")? {
        obj.insert("what".into(), json!(what.get_string()?));
    }
    if let Some(d) = node.maybe_get_attr("shortDescription")? {
        obj.insert("shortDescription".into(), json!(d.get_string()?));
    }
    if let Some(drv) = leaf_derivation(node, outputs)? {
        let mut drv_obj = Map::new();
        if options.show_drv_names {
            drv_obj.insert("name".into(), json!(drv.get_attr("name")?.get_string()?));
        }
        if options.show_drv_paths {
            let drv_path = drv.force_derivation()?;
            drv_obj.insert(
                "path".into(),
                json!(session.config().store_dir().display(&drv_path)),
            );
        }
        if options.show_output_paths {
            let drv_path = drv.force_derivation()?;
            let derivation = session.derivation(&drv_path).ok_or_else(|| {
                Error::Store(
                    format!(
                        "derivation '{}' was not produced by this evaluation",
                        session.config().store_dir().display(&drv_path)
                    )
                    .into(),
                )
            })?;
            let mut outs = Map::new();
            for (name, output) in &derivation.outputs {
                let v = match output {
                    brix_store::DerivationOutput::InputAddressed(p) => {
                        json!(session.config().store_dir().display(p))
                    }
                    _ => Json::Null,
                };
                // Output names are bytes; nlohmann's dump refuses one
                // that is not UTF-8.
                outs.insert(crate::output::json_text(name)?.to_owned(), v);
            }
            drv_obj.insert("outputs".into(), Json::Object(outs));
        }
        obj.insert("derivation".into(), Json::Object(drv_obj));
    }
    if let Some(systems) = for_systems(node)? {
        obj.insert("forSystems".into(), json!(systems));
    }
    Ok(None)
}

pub use brix::check::{check, CheckEvent};
