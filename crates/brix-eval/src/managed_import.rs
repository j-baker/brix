//! Import caches own precise roots; compilation remains permanent metadata.
#![allow(dead_code)]

use crate::error::{msg, ErrorKind};
use crate::managed_eval::{allocation, Machine};
use crate::managed_heap::{BindingsRoot, Roots, ValueRoot};
use crate::managed_language::Language;
use crate::value::{PathVal, SrcPos};
use crate::Result;
use brix_store::SourcePath;
#[cfg(not(brix_parallel))]
use std::panic::{catch_unwind, resume_unwind, AssertUnwindSafe};

pub(crate) fn file<'r, 'h>(
    language: &Language<'_>,
    machine: &mut Machine<'h>,
    roots: &Roots<'r, 'h>,
    path: &PathVal<'_>,
    scope: Option<BindingsRoot<'_, 'h>>,
    must_be_attrs: bool,
) -> Result<ValueRoot<'r, 'h>> {
    let ev = language.legacy();
    let key = (path.accessor, path.path.to_vec());
    let resolved = if scope.is_none() {
        if let Some(path) = machine.imports.get(&key) {
            path.clone()
        } else {
            ev.resolve_expr_path(path)?
        }
    } else {
        ev.resolve_expr_path(path)?
    };
    if let Some(scope) = scope {
        let vars = machine
            .heap
            .bindings_view(scope)
            .iter()
            .enumerate()
            .map(|(i, a)| (a.name, i as u32))
            .collect();
        let static_scope = crate::compile::StaticEnv::new(None, vars);
        let base = language.base_env(machine, roots)?;
        let cells: Vec<_> = machine
            .heap
            .bindings_view(scope)
            .iter()
            .map(|a| a.value.root(roots))
            .collect();
        let env = allocation(machine.heap.env(roots, Some(base), &cells))?;
        let code = ev.load_file_code(&resolved, Some(&static_scope), must_be_attrs)?;
        return language.eval_code(machine, roots, code, env);
    }
    #[cfg(brix_parallel)]
    {
        // Publish a replayable thunk before evaluating it. All workers use the
        // same cell and its normal ownership/wait/unwind protocol, including
        // recursive imports. No cache lock is held at a heap safepoint.
        let cached = machine
            .shared_file_cache
            .lock()
            .unwrap()
            .get(&resolved)
            .cloned();
        let cell = if let Some(cached) = cached {
            roots.copy(cached.root())
        } else {
            let code = language.import_code(&resolved, must_be_attrs)?;
            let env = language.base_env(machine, roots)?;
            let candidate = allocation(machine.heap.thunk_cell(
                roots,
                env,
                crate::value::GlobalNode(code.node_base + code.root.0),
            ))?;
            let lease = machine.heap.persist(candidate);
            let chosen = machine
                .shared_file_cache
                .lock()
                .unwrap()
                .entry(resolved.clone())
                .or_insert(lease)
                .clone();
            roots.copy(chosen.root())
        };
        machine.imports.insert(key, resolved.clone());
        language
            .force(machine, roots, cell, SrcPos::NONE)
            .map_err(|error| {
                let error = crate::error::snapshot(&error);
                if error.kind != ErrorKind::Parse {
                    error.add_trace(
                        None,
                        msg!(
                            "while evaluating the file '",
                            ev.accessor(resolved.accessor).show_path(&resolved.path),
                            "':"
                        ),
                    );
                }
                error
            })
    }
    #[cfg(not(brix_parallel))]
    {
        if let Some(cached) = machine.file_cache.get(&resolved) {
            let cell = roots.copy(cached.root());
            machine.imports.insert(key, resolved);
            return language.force(machine, roots, cell, SrcPos::NONE);
        }
        let cell = allocation(machine.heap.cell(roots, roots.blackhole()))?;
        machine
            .file_cache
            .insert(resolved.clone(), machine.heap.persist(cell));
        machine.imports.insert(key.clone(), resolved.clone());
        let evaluated = catch_unwind(AssertUnwindSafe(|| {
            let code = ev.load_file_code(&resolved, None, must_be_attrs)?;
            let env = language.base_env(machine, roots)?;
            language.eval_code(machine, roots, code, env)
        }));
        match evaluated {
            Ok(Ok(value)) => {
                machine.heap.store(cell, value);
                Ok(value)
            }
            Ok(Err(error)) => {
                if error.kind != ErrorKind::Parse {
                    error.add_trace(
                        None,
                        msg!(
                            "while evaluating the file '",
                            ev.accessor(resolved.accessor).show_path(&resolved.path),
                            "':"
                        ),
                    );
                }
                match machine.heap.failed(roots, error.clone()) {
                    Ok(value) => machine.heap.store(cell, value),
                    Err(oom) => {
                        machine.file_cache.remove(&resolved);
                        machine.imports.remove(&key);
                        return allocation(Err(oom));
                    }
                }
                Err(error)
            }
            Err(panic) => {
                machine.file_cache.remove(&resolved);
                machine.imports.remove(&key);
                resume_unwind(panic)
            }
        }
    }
}

pub(crate) fn main_file<'r, 'h>(
    language: &Language<'_>,
    machine: &mut Machine<'h>,
    roots: &Roots<'r, 'h>,
    path: &SourcePath,
) -> Result<ValueRoot<'r, 'h>> {
    let code = language.legacy().load_file_code(path, None, false)?;
    let env = language.base_env(machine, roots)?;
    language.eval_code(machine, roots, code, env)
}
