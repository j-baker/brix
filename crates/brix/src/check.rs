//! `nix flake check` over a flake's schemas ([`crate::schemas`]): the
//! walk that evaluates every output and finds what to build ([`check`]).
//! Building is the caller's.

use std::collections::{BTreeMap, BTreeSet};

use crate::schemas::{for_each_output, for_systems, leaf_derivation};
use crate::{Cursor, Error, Result, StorePath};

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
    pub to_build: BTreeMap<StorePath, Vec<String>>,
    pub has_errors: bool,
    /// Checking stopped at the first reported failure (no `--keep-going`):
    /// nothing else is reported. Parallel checks report in completion order.
    pub aborted: bool,
}

/// The evaluation half of `nix flake check` (`CmdFlakeCheck::run` before
/// `buildPathsWithResults`): runs every schema's `evalChecks`, validates
/// derivations, and collects what would be built. Reports each completed
/// check immediately, including checks preceding a later evaluation error.
///
/// Nothing in [`CheckReport::to_build`] is built here, or waited for: the
/// only builds this waits on are the ones evaluation reads the results of
/// (import-from-derivation), through the session's daemon. Nor are those
/// derivations in any store yet. A caller that builds them, now or after
/// brix is gone, first makes each valid there with its closure
/// ([`crate::Session::store_upload`]; [`crate::Session::store_realise`]
/// does so itself).
///
/// `on_event` is called while evaluation runs: with parallel evaluation,
/// from the thread that evaluated, one call at a time. It must not use
/// the session.
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
            Node(crate::BatchValue),
            Children {
                value: crate::BatchValue,
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
                    let step = (|| -> Result<Option<crate::BatchValue>> {
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
fn visit_leaf<C: crate::cursor::ReadCursor>(
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
            let shown = crate::attr_path_to_string(&check_path);
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
