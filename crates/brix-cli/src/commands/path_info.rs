//! `brix path-info` (`src/nix/path-info.cc`).

use std::collections::{BTreeMap, BTreeSet};

use crate::installable::DerivedPath;
use brix::store::PathInfo;
use brix::{Session, StoreDir, StorePath};
use serde_json::{json, Map, Value};

use crate::commands::realise::{to_store_paths, write_to_store};
use crate::commands::{print_stdout, Ctx};
use crate::{CliError, Result};

pub fn run(ctx: &Ctx<'_>) -> Result<()> {
    // Parsed with the command line, `--json` or not.
    let json_format = ctx
        .inv
        .flag_value("json-format")
        .map(parse_json_format)
        .transpose()?;
    if ctx.inv.has("all") {
        return Err(CliError::msg(
            "'--all' is not supported: brix cannot enumerate a store",
        ));
    }
    let session = ctx.session()?;
    let mut resolver = ctx.resolver(&session, &["nix-build"])?;
    let installables = ctx.installables(&resolver)?;
    let store_dir = session.config().store_dir().clone();
    // `BuiltPathsCommand::run`: the installables' paths (`toBuiltPaths`
    // with `Realise::Nothing`) and, with `--recursive`, their closure.
    let mut derived: Vec<(String, DerivedPath)> = Vec::new();
    for i in &installables {
        let what = i.what(&store_dir);
        for d in resolver.derived_paths(i)? {
            derived.push((what.clone(), d));
        }
    }
    let roots: Vec<StorePath> = if ctx.inv.has("derivation") {
        // `Installable::toDerivations`.
        let mut drvs = Vec::new();
        for (what, d) in &derived {
            match d {
                DerivedPath::Built { drv_path, .. } => drvs.push(drv_path.clone()),
                DerivedPath::Opaque(p) if p.is_derivation() => drvs.push(p.clone()),
                // `getDeriver`: the first of the store's derivers.
                DerivedPath::Opaque(p) => match session.store_valid_derivers(p)?.into_iter().next()
                {
                    Some(drv) => drvs.push(drv),
                    None => {
                        return Err(CliError::msg(format!(
                            "'{what}' does not have a known deriver"
                        )))
                    }
                },
            }
        }
        for drv in &drvs {
            write_to_store(&session, drv)?;
        }
        drvs
    } else {
        let paths: Vec<DerivedPath> = derived.into_iter().map(|(_, d)| d).collect();
        to_store_paths(&session, &paths)?
    };
    let mut infos = Infos::new(&session);
    let paths: BTreeSet<StorePath> = if ctx.inv.has("recursive") {
        infos.closure(&roots)?
    } else {
        roots.into_iter().collect()
    };
    let show_closure_size = ctx.inv.has("closure-size");
    if ctx.inv.has("json") {
        let format = json_format.unwrap_or_else(|| {
            brix::log::stderr("warning: '--json' without '--json-format' is deprecated; please specify '--json-format 1' or '--json-format 2'. This will become an error in a future release.");
            1
        });
        return ctx.print_json(&path_infos_json(
            &mut infos,
            &paths,
            show_closure_size,
            format,
        )?);
    }
    // `StorePathsCommand::run`: each path after the paths it refers to.
    let paths = infos.topo_sort(&paths)?;
    let show_size = ctx.inv.has("size");
    let human = ctx.inv.has("human-readable");
    let show_sigs = ctx.inv.has("sigs");
    let path_len = paths
        .iter()
        .map(|p| store_dir.display(p).len())
        .max()
        .unwrap_or(0);
    let mut out = String::new();
    for p in &paths {
        let info = infos.require(p)?;
        let shown = store_dir.display(p);
        out.push_str(&shown);
        if show_size || show_closure_size || show_sigs {
            out.push_str(&" ".repeat(path_len.saturating_sub(shown.len())));
        }
        if show_size {
            out.push_str(&print_size(info.nar_size, human));
        }
        if show_closure_size {
            out.push_str(&print_size(infos.closure_size(p)?, human));
        }
        if show_sigs {
            out.push('\t');
            let mut ss: Vec<String> = Vec::new();
            if info.ultimate {
                ss.push("ultimate".into());
            }
            if let Some(ca) = &info.ca {
                ss.push(format!("ca:{}", ca.render()));
            }
            for (key_name, sig) in signatures(&info)? {
                ss.push(format!("{key_name}:{sig}"));
            }
            out.push_str(&ss.join(" "));
        }
        out.push('\n');
    }
    print_stdout(&out)
}

/// `--json-format`: `string2IntWithUnitPrefix`, then
/// `parsePathInfoJsonFormat`.
fn parse_json_format(text: &str) -> Result<u64> {
    let version = brix_store::setting::int_with_unit_prefix::<u64>(text)
        .map_err(|e| CliError::usage(e.to_string()))?;
    if !(1..=3).contains(&version) {
        return Err(CliError::msg(format!(
            "unsupported path info JSON format version {version}; supported versions are 1, 2 and 3"
        )));
    }
    Ok(version)
}

/// `pathInfoToJSON`: every path's info by its full path (format 1) or its
/// base name (later formats, which wrap the map with the format and the
/// store directory). A path that is not valid is `null`.
fn path_infos_json(
    infos: &mut Infos<'_>,
    paths: &BTreeSet<StorePath>,
    show_closure_size: bool,
    format: u64,
) -> Result<Value> {
    let store_dir = infos.session.config().store_dir().clone();
    let mut objects = Map::new();
    for p in paths {
        let key = if format == 1 {
            store_dir.display(p)
        } else {
            p.as_str().to_owned()
        };
        let object = match infos.get(p)? {
            Some(info) => {
                let mut object = path_info_json(&store_dir, &info, format)?;
                if show_closure_size {
                    object.insert("closureSize".into(), json!(infos.closure_size(p)?));
                }
                Value::Object(object)
            }
            None => Value::Null,
        };
        objects.insert(key, object);
    }
    Ok(if format == 1 {
        Value::Object(objects)
    } else {
        json!({
            "version": format,
            "storeDir": store_dir.as_str(),
            "info": Value::Object(objects),
        })
    })
}

/// `UnkeyedValidPathInfo::toJSON` with the impure fields, in JSON format
/// `format`: format 1 has full store paths and string content addresses,
/// format 2 base names and `{method, hash}` content addresses, format 3
/// also `{keyName, sig}` signatures. A registration time of 0 is `null`.
/// (`nix path-info` then sets `storeDir` itself, to the same value.)
fn path_info_json(
    store_dir: &StoreDir,
    info: &PathInfo,
    format: u64,
) -> Result<Map<String, Value>> {
    let show = |p: &StorePath| {
        if format == 1 {
            store_dir.display(p)
        } else {
            p.as_str().to_owned()
        }
    };
    let mut obj = Map::new();
    obj.insert(
        "ca".into(),
        match &info.ca {
            None => Value::Null,
            Some(ca) if format == 1 => json!(ca.render()),
            Some(ca) => json!({
                "method": ca.method.render(),
                "hash": ca.hash.to_string(brix_store::HashFormat::Sri, true),
            }),
        },
    );
    obj.insert(
        "deriver".into(),
        info.deriver
            .as_ref()
            .map_or(Value::Null, |d| json!(show(d))),
    );
    obj.insert(
        "narHash".into(),
        json!(info.nar_hash.to_string(brix_store::HashFormat::Sri, true)),
    );
    obj.insert("narSize".into(), json!(info.nar_size));
    let refs: Vec<String> = info.references.iter().map(show).collect();
    obj.insert("references".into(), json!(refs));
    obj.insert(
        "registrationTime".into(),
        match info.registration_time {
            0 => Value::Null,
            t => json!(t),
        },
    );
    let signatures: Vec<Value> = signatures(info)?
        .into_iter()
        .map(|(key_name, sig)| {
            if format >= 3 {
                json!({ "keyName": key_name, "sig": sig })
            } else {
                json!(format!("{key_name}:{sig}"))
            }
        })
        .collect();
    obj.insert("signatures".into(), Value::Array(signatures));
    obj.insert("storeDir".into(), json!(store_dir.as_str()));
    obj.insert("ultimate".into(), json!(info.ultimate));
    obj.insert("version".into(), json!(format));
    Ok(obj)
}

/// The path's signatures as `(key name, base-64 signature)`, in the order
/// of Nix's `std::set<Signature>`: by key name, then signature (Nix
/// compares the decoded bytes, which can only order two signatures by the
/// same key differently).
fn signatures(info: &PathInfo) -> Result<Vec<(&str, &str)>> {
    let mut sigs = info
        .sigs
        .iter()
        .map(|s| {
            s.split_once(':').ok_or_else(|| {
                CliError::msg(format!("signature '{s}' is not of the form NAME:SIG"))
            })
        })
        .collect::<Result<Vec<_>>>()?;
    sigs.sort();
    Ok(sigs)
}

/// The path infos `path-info` needs, each asked of the daemon once.
struct Infos<'s> {
    session: &'s Session,
    known: BTreeMap<StorePath, Option<PathInfo>>,
}

impl<'s> Infos<'s> {
    fn new(session: &'s Session) -> Infos<'s> {
        Infos {
            session,
            known: BTreeMap::new(),
        }
    }

    /// `queryPathInfo`, `None` for a path that is not valid.
    fn get(&mut self, path: &StorePath) -> Result<Option<PathInfo>> {
        if let Some(info) = self.known.get(path) {
            return Ok(info.clone());
        }
        let info = self.session.store_path_info(path)?;
        self.known.insert(path.clone(), info.clone());
        Ok(info)
    }

    /// `queryPathInfo` of a path that must be valid.
    fn require(&mut self, path: &StorePath) -> Result<PathInfo> {
        self.get(path)?.ok_or_else(|| {
            CliError::msg(format!(
                "path '{}' is not valid",
                self.session.config().store_dir().display(path)
            ))
        })
    }

    /// `computeFSClosure`: `roots` and everything they refer to.
    fn closure(&mut self, roots: &[StorePath]) -> Result<BTreeSet<StorePath>> {
        let mut closure = BTreeSet::new();
        let mut todo: Vec<StorePath> = roots.to_vec();
        while let Some(path) = todo.pop() {
            if closure.contains(&path) {
                continue;
            }
            todo.extend(self.require(&path)?.references);
            closure.insert(path);
        }
        Ok(closure)
    }

    /// `getStoreObjectsTotalSize` of the closure of `path`.
    fn closure_size(&mut self, path: &StorePath) -> Result<u64> {
        let mut total = 0;
        for p in self.closure(std::slice::from_ref(path))? {
            total += self.require(&p)?.nar_size;
        }
        Ok(total)
    }

    /// `Store::topoSortPaths`, reversed: each path after the paths of the
    /// set it refers to. A depth-first walk over the set in order, as in
    /// Nix's `topoSort`, so the order matches Nix's where the references
    /// leave it open.
    fn topo_sort(&mut self, paths: &BTreeSet<StorePath>) -> Result<Vec<StorePath>> {
        let mut sorted = Vec::new();
        let mut visited = BTreeSet::new();
        let mut parents = Vec::new();
        for path in paths {
            self.visit(path, paths, &mut visited, &mut parents, &mut sorted)?;
        }
        Ok(sorted)
    }

    fn visit(
        &mut self,
        path: &StorePath,
        paths: &BTreeSet<StorePath>,
        visited: &mut BTreeSet<StorePath>,
        parents: &mut Vec<StorePath>,
        sorted: &mut Vec<StorePath>,
    ) -> Result<()> {
        if let Some(parent) = parents.last().filter(|_| parents.contains(path)) {
            let store_dir = self.session.config().store_dir();
            return Err(CliError::msg(format!(
                "cycle detected in the references of '{}' from '{}'",
                store_dir.display(path),
                store_dir.display(parent)
            )));
        }
        if !visited.insert(path.clone()) {
            return Ok(());
        }
        parents.push(path.clone());
        // A path that is not valid refers to nothing here.
        let references = self.get(path)?.map(|i| i.references).unwrap_or_default();
        for reference in &references {
            if reference != path && paths.contains(reference) {
                self.visit(reference, paths, visited, parents, sorted)?;
            }
        }
        parents.pop();
        sorted.push(path.clone());
        Ok(())
    }
}

fn print_size(value: u64, human: bool) -> String {
    if human {
        format!("\t{}", render_size(value, true))
    } else {
        format!("\t{value:11}")
    }
}

/// `renderSize`: `1.5 MiB`-style, always at least KiB.
pub fn render_size(value: u64, align: bool) -> String {
    let units = ['K', 'M', 'G', 'T', 'P', 'E'];
    let mut power = 1usize;
    while power < units.len() && (value as f64) >= 1024f64.powi(power as i32 + 1) {
        power += 1;
    }
    let result = value as f64 / 1024f64.powi(power as i32);
    if align {
        format!("{result:6.1} {}iB", units[power - 1])
    } else {
        format!("{result:.1} {}iB", units[power - 1])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_render_like_nix() {
        assert_eq!(render_size(128, true), "   0.1 KiB");
        assert_eq!(render_size(128, false), "0.1 KiB");
        assert_eq!(render_size(5 * 1024 * 1024, false), "5.0 MiB");
        assert_eq!(render_size(1536, false), "1.5 KiB");
    }

    #[test]
    fn json_formats_parse_like_nix() {
        assert_eq!(parse_json_format("2").unwrap(), 2);
        let message = |text: &str| match parse_json_format(text) {
            Err(CliError::UsageWithHint(m)) => {
                bstr::BString::from([b"usage: ", m.as_slice()].concat())
            }
            Err(CliError::Message(m)) => m,
            other => panic!("{text}: {other:?}"),
        };
        assert_eq!(message("x"), "usage: invalid unit specifier 'X'");
        assert_eq!(message("-1"), "usage: '-1' is not an integer");
        assert_eq!(
            message("1k"),
            "unsupported path info JSON format version 1024; supported versions are 1, 2 and 3"
        );
    }
}
