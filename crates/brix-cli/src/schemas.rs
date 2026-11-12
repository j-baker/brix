//! `nix flake show --json`'s inventory and `nix flake check`'s walk,
//! over the facade's flake schemas ([`brix::schemas`]).

use std::collections::{BTreeMap, BTreeSet};

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

/// One line of `nix flake check`'s report.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CheckEvent {
    /// `✅ <attr path>`, with `build_skipped` for derivations not built.
    Passed {
        attr_path: String,
        build_skipped: bool,
    },
    /// `❌ <attr path>` followed by the error.
    Failed { attr_path: String, error: String },
}

/// What `nix flake check` found before any building.
#[derive(Clone, Debug, Default)]
pub struct CheckReport {
    /// Outputs without a schema.
    pub unchecked_outputs: BTreeSet<String>,
    /// Systems skipped for lack of `--all-systems`.
    pub omitted_systems: BTreeSet<String>,
    /// Derivations to build, with the attribute paths that want them.
    pub to_build: BTreeMap<brix_store::StorePath, Vec<String>>,
    pub has_errors: bool,
    /// Checking stopped at the first reported failure (no `--keep-going`):
    /// nothing else is reported. Parallel checks report in completion order.
    pub aborted: bool,
}

/// The evaluation half of `nix flake check` (`CmdFlakeCheck::run` before
/// `buildPathsWithResults`): runs every schema's `evalChecks`, validates
/// derivations, and collects what would be built. Reports each completed
/// check immediately, including checks preceding a later evaluation error.
pub fn check(
    root: &Cursor<'_>,
    local_system: Option<&str>,
    build: bool,
    build_all: bool,
    keep_going: bool,
    mut on_event: impl FnMut(CheckEvent) + Send,
) -> Result<CheckReport> {
    let inventory = root.get_attr("inventory")?;
    let outputs = root.get_attr("outputs")?.renamed(Vec::new());
    let mut walk = CheckWalk {
        outputs: &outputs,
        local_system,
        build,
        build_all,
        keep_going,
    };
    let mut report = CheckReport::default();
    for entry in for_each_output(&inventory)? {
        match entry.output {
            Some(output) => {
                let node = output.renamed(vec![entry.name.clone()]);
                let visited = walk.visit(&node, &mut report, &mut on_event);
                if !visited? {
                    break;
                }
            }
            None => {
                report.unchecked_outputs.insert(entry.name);
            }
        }
    }
    Ok(report)
}

/// What one `nix flake check` walk over the inventory needs at every node.
struct CheckWalk<'c, 's> {
    /// The flake's outputs, for `derivationAttrPath` lookups.
    outputs: &'c Cursor<'s>,
    /// The system checked; `None` checks all (`--all-systems`).
    local_system: Option<&'c str>,
    /// Derivations to build are left unreported until they are built.
    build: bool,
    /// Build every derivation, not only `isFlakeCheck` ones.
    build_all: bool,
    keep_going: bool,
}

impl<'s> CheckWalk<'_, 's> {
    /// Checks `node` and what is below it; `false` when checking stopped
    /// at a failure.
    #[cfg(not(brix_parallel))]
    fn visit(
        &mut self,
        node: &Cursor<'s>,
        report: &mut CheckReport,
        on_event: &mut impl FnMut(CheckEvent),
    ) -> Result<bool> {
        if let Some(b) = node.maybe_get_attr("isLegacy")? {
            if b.get_bool()? {
                return Ok(true);
            }
        }
        if let Some(system) = self.local_system {
            if let Some(systems) = for_systems(node)? {
                if !systems.iter().any(|s| s == system) {
                    for s in systems {
                        report.omitted_systems.insert(s);
                    }
                    return Ok(true);
                }
            }
        }
        if let Some(children) = node.maybe_get_attr("children")? {
            let is_legacy_packages = node
                .attr_path()
                .first()
                .is_some_and(|a| a == "legacyPackages");
            for name in children.get_attrs()? {
                let child = children.get_attr(&name)?;
                let mut path = node.attr_path().to_vec();
                path.push(name.clone());
                let child = child.renamed(path);
                match self.visit(&child, report, on_event) {
                    Ok(true) => {}
                    Ok(false) => return Ok(false),
                    Err(Error::Eval { .. }) if is_legacy_packages => {}
                    Err(e) => return Err(e),
                }
            }
            return Ok(true);
        }
        #[cfg(not(brix_parallel))]
        {
            let attr_path = node.attr_path_str();
            match visit_leaf(
                node,
                self.outputs,
                self.build,
                self.build_all,
                &attr_path,
                report,
            ) {
                Ok((done, build_skipped)) => {
                    if done {
                        on_event(CheckEvent::Passed {
                            attr_path,
                            build_skipped,
                        });
                    }
                    Ok(true)
                }
                Err(error) => {
                    on_event(CheckEvent::Failed {
                        attr_path,
                        error: error.render(node.session().config().eval.show_trace),
                    });
                    report.has_errors = true;
                    if self.keep_going {
                        Ok(true)
                    } else {
                        report.aborted = true;
                        Ok(false)
                    }
                }
            }
        }
    }

    #[cfg(brix_parallel)]
    fn visit(
        &mut self,
        node: &Cursor<'s>,
        report: &mut CheckReport,
        on_event: &mut (impl FnMut(CheckEvent) + Send),
    ) -> Result<bool> {
        enum Frame {
            Node(brix::BatchValue),
            Children {
                value: brix::BatchValue,
                names: std::vec::IntoIter<String>,
                path: Vec<String>,
            },
        }
        let mut stack = vec![Frame::Node(node.batch_value())];
        let outputs = self.outputs.batch_value();
        let build = self.build;
        let build_all = self.build_all;
        let keep_going = self.keep_going;
        let local_system = self.local_system;
        let show_trace = node.session().config().eval.show_trace;
        let mut omitted_systems = BTreeSet::new();
        let mut worker_error = None;
        let discovered = node.session().evaluate_stream(
            |context| {
                while let Some(frame) = stack.pop() {
                    let path = match &frame {
                        Frame::Node(value) => context.cursor(value).attr_path().to_vec(),
                        Frame::Children { path, .. } => path.clone(),
                    };
                    let legacy_child = path.len() > 1 && path[0] == "legacyPackages";
                    let step = (|| -> Result<Option<brix::BatchValue>> {
                        match frame {
                            Frame::Children {
                                value,
                                mut names,
                                path,
                            } => {
                                if let Some(name) = names.next() {
                                    let mut child_path = path.clone();
                                    child_path.push(name.clone());
                                    let child = context
                                        .cursor(&value)
                                        .get_attr(&name)?
                                        .renamed(child_path)
                                        .batch_value();
                                    stack.push(Frame::Children { value, names, path });
                                    stack.push(Frame::Node(child));
                                }
                            }
                            Frame::Node(value) => {
                                let node = context.cursor(&value);
                                if let Some(legacy) = node.maybe_get_attr("isLegacy")? {
                                    if legacy.get_bool()? {
                                        return Ok(None);
                                    }
                                }
                                if let Some(system) = local_system {
                                    if let Some(systems) = for_systems(&node)? {
                                        if !systems.iter().any(|s| s == system) {
                                            omitted_systems.extend(systems);
                                            return Ok(None);
                                        }
                                    }
                                }
                                if let Some(children) = node.maybe_get_attr("children")? {
                                    stack.push(Frame::Children {
                                        names: children.get_attrs()?.into_iter(),
                                        value: children.batch_value(),
                                        path: node.attr_path().to_vec(),
                                    });
                                } else {
                                    return Ok(Some(value));
                                }
                            }
                        }
                        Ok(None)
                    })();
                    match step {
                        Ok(Some(value)) => return Ok(Some(value)),
                        Ok(None) => {}
                        Err(Error::Eval { .. }) if legacy_child => {}
                        Err(error) => return Err(error),
                    }
                }
                Ok(None)
            },
            |context, value| {
                let node = context.cursor(&value);
                let outputs = context.cursor(&outputs);
                let attr_path = node.attr_path_str();
                let mut leaf_report = CheckReport::default();
                let outcome = visit_leaf(
                    &node,
                    &outputs,
                    build,
                    build_all,
                    &attr_path,
                    &mut leaf_report,
                )
                .map(|(done, skipped)| (done, skipped, leaf_report.to_build));
                Ok((attr_path, outcome))
            },
            |result| {
                let (attr_path, result) = match result {
                    Ok(result) => result,
                    Err(error) => {
                        worker_error = Some(error);
                        return false;
                    }
                };
                match result {
                    Ok((done, build_skipped, paths)) => {
                        for (path, attrs) in paths {
                            report.to_build.entry(path).or_default().extend(attrs);
                        }
                        if done {
                            on_event(CheckEvent::Passed {
                                attr_path,
                                build_skipped,
                            });
                        }
                        true
                    }
                    Err(error) => {
                        on_event(CheckEvent::Failed {
                            attr_path,
                            error: error.render(show_trace),
                        });
                        report.has_errors = true;
                        report.aborted = !keep_going;
                        keep_going
                    }
                }
            },
        );
        report.omitted_systems.extend(omitted_systems);
        if let Some(error) = worker_error {
            return Err(error);
        }
        if report.aborted {
            return Ok(false);
        }
        discovered?;
        Ok(true)
    }
}

/// The checks of one leaf: whether it is done (nothing left to build
/// before it passes) and whether its build was skipped.
fn visit_leaf<C: brix::cursor::ReadCursor>(
    node: &C,
    outputs: &C,
    build: bool,
    build_all: bool,
    attr_path: &str,
    report: &mut CheckReport,
) -> Result<(bool, bool)> {
    let mut done = true;
    let mut build_skipped = false;
    if let Some(eval_checks) = node.maybe_get_attr("evalChecks")? {
        for check_name in eval_checks.get_attrs()? {
            let c = eval_checks.get_attr(&check_name)?;
            let mut check_path = node.attr_path().to_vec();
            check_path.push(check_name.clone());
            let shown = brix::attr_path_to_string(&check_path);
            let ok = c.get_bool().map_err(|e| match e {
                Error::Usage(_) => Error::Usage(format!("'{shown}' is not a Boolean").into()),
                other => other,
            })?;
            if !ok {
                return Err(Error::Usage(
                    format!("Evaluation check '{shown}' failed.").into(),
                ));
            }
        }
    }
    if let Some(drv) = leaf_derivation(node, outputs)? {
        let is_drv = drv.maybe_get_attr("drvPath")?.is_some()
            && drv.get_attr("type")?.get_string()? == "derivation";
        if !is_drv {
            return Err(Error::Usage(
                format!(
                    "Flake output '{}' is not a derivation.",
                    drv.attr_path_str()
                )
                .into(),
            ));
        }
        // Nix parses the name (`DrvName`) without using it, so a name
        // that fails to evaluate fails the check.
        drv.get_attr("name")?.get_string()?;
        let is_flake_check = match node.maybe_get_attr("isFlakeCheck")? {
            Some(b) => b.get_bool()?,
            None => false,
        };
        if build_all || is_flake_check {
            let drv_path = drv.force_derivation()?;
            report
                .to_build
                .entry(drv_path)
                .or_default()
                .push(attr_path.to_owned());
            if build {
                done = false;
            }
        } else {
            build_skipped = true;
        }
    }
    Ok((done, build_skipped))
}
