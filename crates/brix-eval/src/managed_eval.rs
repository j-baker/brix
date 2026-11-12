//! Rooted thunk forcing. Code and source metadata stay outside the moving heap.
//! The legacy arena-reference entry points still need to migrate to this core.

#![allow(dead_code)]

use std::collections::HashMap;
use std::panic::{catch_unwind, resume_unwind, AssertUnwindSafe};

use crate::error::{ErrorKind, EvalError, Position};
use crate::managed_heap::{
    CellKind, CellRoot, EnvKind, EnvRoot, Heap, Lease, Roots, ValueKind, ValueRoot, ValueView,
};
use crate::value::{GlobalNode, SrcPos};
use crate::Result;

/// Executable code may capture metadata and IDs, never unregistered heap edges.
pub(crate) trait Code {
    fn eval<'r, 'h>(
        &self,
        machine: &mut Machine<'h>,
        roots: &Roots<'r, 'h>,
        env: EnvRoot<'_, 'h>,
        node: GlobalNode,
    ) -> Result<ValueRoot<'r, 'h>>;

    fn call<'r, 'h>(
        &self,
        machine: &mut Machine<'h>,
        roots: &Roots<'r, 'h>,
        fun: CellRoot<'_, 'h>,
        arg: CellRoot<'_, 'h>,
        pos: SrcPos,
    ) -> Result<ValueRoot<'r, 'h>>;

    /// A suspended application supplies its own temporary frame. Implementors
    /// may use it for one reduction: the caller stores a successful result
    /// without collecting, then drops the frame. Error temporaries may survive
    /// the single failure-memoization allocation, never a subsequent call loop.
    /// Stack and call-depth guards remain the implementation's responsibility.
    fn call_in_frame<'r, 'h>(
        &self,
        machine: &mut Machine<'h>,
        roots: &Roots<'r, 'h>,
        fun: CellRoot<'_, 'h>,
        arg: CellRoot<'_, 'h>,
        pos: SrcPos,
    ) -> Result<ValueRoot<'r, 'h>> {
        self.call(machine, roots, fun, arg, pos)
    }

    fn position(&self, pos: SrcPos) -> Option<Position>;

    fn with_stack<R>(&self, f: impl FnOnce() -> R) -> R {
        stacker::maybe_grow(crate::stack::STACK_RED_ZONE, crate::stack::STACK_SEGMENT, f)
    }
}

pub(crate) struct Machine<'h> {
    pub heap: Heap<'h>,
    pub identity: crate::runtime::rc::Rc<()>,
    /// Where `derivationStrict` offers background work, inside a batch.
    #[cfg(brix_parallel)]
    pub sparks: Option<std::sync::Arc<crate::parallel::sparks::Sparks<'h>>>,
    /// Set while this worker evaluates a background spark: the flag that
    /// cancels it. See `parallel::sparks` for what such an evaluation refuses.
    #[cfg(brix_parallel)]
    pub background: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    /// What the running spark does at a thunk another worker is evaluating.
    #[cfg(brix_parallel)]
    pub busy: crate::parallel::sparks::Busy,
    /// The running spark stopped at a busy thunk and is worth retrying.
    #[cfg(brix_parallel)]
    pub yielded: bool,
    /// The running walk left a value because of a busy thunk.
    #[cfg(brix_parallel)]
    pub skipped: bool,
    pub base_env: Option<Lease<'h, EnvKind>>,
    pub uninit: Option<Lease<'h, CellKind>>,
    pub empty_list: Option<Lease<'h, CellKind>>,
    pub literals: Vec<Option<Lease<'h, CellKind>>>,
    /// Attribute names are immutable strings, shared exactly as symbol_value
    /// shares them in the arena evaluator. Leases keep cells live across GC.
    pub symbol_values: Vec<Option<Lease<'h, CellKind>>>,
    pub internal: HashMap<&'static str, Lease<'h, ValueKind>>,
    #[cfg(brix_parallel)]
    pub shared_file_cache:
        std::sync::Arc<std::sync::Mutex<HashMap<brix_store::SourcePath, Lease<'h, CellKind>>>>,
    pub file_cache: HashMap<brix_store::SourcePath, Lease<'h, CellKind>>,
    pub imports: HashMap<(brix_store::AccessorId, Vec<u8>), brix_store::SourcePath>,
}

#[inline]
pub(crate) fn allocation<T>(result: brix_gc::Result<T>) -> Result<T> {
    result.map_err(allocation_error)
}

#[cold]
#[inline(never)]
fn allocation_error(error: brix_gc::Error) -> crate::Error {
    EvalError::new(
        match error {
            brix_gc::Error::OutOfMemory | brix_gc::Error::AllocationTooLarge => {
                ErrorKind::OutOfMemory
            }
            brix_gc::Error::InvalidRange => ErrorKind::Eval,
        },
        format!("garbage collector: {error}"),
    )
}

impl<'h> Machine<'h> {
    pub fn new(heap: Heap<'h>) -> Self {
        Self {
            heap,
            identity: crate::runtime::rc::Rc::new(()),
            #[cfg(brix_parallel)]
            sparks: None,
            #[cfg(brix_parallel)]
            background: None,
            #[cfg(brix_parallel)]
            busy: crate::parallel::sparks::Busy::Wait,
            #[cfg(brix_parallel)]
            yielded: false,
            #[cfg(brix_parallel)]
            skipped: false,
            base_env: None,
            uninit: None,
            empty_list: None,
            literals: Vec::new(),
            symbol_values: Vec::new(),
            internal: HashMap::new(),
            file_cache: HashMap::new(),
            #[cfg(brix_parallel)]
            shared_file_cache: Default::default(),
            imports: HashMap::new(),
        }
    }

    pub fn with_roots<R>(&mut self, f: impl for<'r> FnOnce(&mut Self, &Roots<'r, 'h>) -> R) -> R {
        self.heap.root_registry().with_roots(|roots| f(self, roots))
    }

    /// Nix's forceValue: snapshot before blackholing, evaluate, and memoize.
    /// Overwriting the cell never discards the suspended environment/application.
    #[inline]
    pub fn force<'r>(
        &mut self,
        code: &impl Code,
        out: &Roots<'r, 'h>,
        cell: CellRoot<'_, 'h>,
        pos: SrcPos,
    ) -> Result<ValueRoot<'r, 'h>> {
        self.force_cell(code, out, cell, pos)?;
        Ok(self.heap.value(out, cell))
    }

    /// Force and memoize in the already rooted cell without registering an
    /// additional value snapshot. Callers can borrow its value after this call.
    #[inline]
    pub fn force_cell(
        &mut self,
        code: &impl Code,
        roots: &Roots<'_, 'h>,
        cell: CellRoot<'_, 'h>,
        pos: SrcPos,
    ) -> Result<()> {
        if self.heap.is_whnf(cell) {
            Ok(())
        } else {
            self.force_suspended(code, roots, cell, pos)
        }
    }

    // Keep thunk evaluation, blackholing and unwind handling out of the tiny
    // forceValue fast path used for already evaluated arguments and variables.
    #[inline(never)]
    fn force_suspended<'r>(
        &mut self,
        code: &impl Code,
        out: &Roots<'r, 'h>,
        cell: CellRoot<'_, 'h>,
        pos: SrcPos,
    ) -> Result<()> {
        #[cfg(brix_parallel)]
        self.safepoint()?;
        #[cfg(brix_parallel)]
        loop {
            use crate::parallel::{publication::Claim, wait::WaitError};
            match self.heap.claim(cell) {
                Claim::Acquired => break,
                Claim::Ready => {
                    return match self.heap.read(cell) {
                        ValueView::Failed(error) => {
                            let error = crate::error::snapshot(error);
                            fix_position(&error, code, pos);
                            Err(error)
                        }
                        ValueView::Blackhole => Err(EvalError::new(
                            ErrorKind::InfiniteRecursion,
                            "infinite recursion encountered",
                        )),
                        _ => Ok(()),
                    };
                }
                Claim::Running(_) if self.background.is_some() && self.yields() => {
                    crate::runtime::note_deferral("a thunk another worker is evaluating");
                    return Err(crate::error::deferred());
                }
                Claim::Running(owner) => match self.heap.wait_on(cell, owner) {
                    Ok(()) => {}
                    Err(WaitError::Deferred) => {
                        crate::runtime::note_deferral("a cycle of waits through this spark");
                        self.yielded = true;
                        return Err(crate::error::deferred());
                    }
                    Err(WaitError::Recursion) => {
                        return Err(EvalError::new(
                            ErrorKind::InfiniteRecursion,
                            "infinite recursion encountered",
                        ))
                    }
                    Err(WaitError::Cancelled) => return Err(crate::error::cancelled()),
                },
            }
        }
        #[cfg(not(brix_parallel))]
        match self.heap.read(cell) {
            ValueView::Blackhole => {
                return Err(EvalError::new(
                    ErrorKind::InfiniteRecursion,
                    "infinite recursion encountered",
                ));
            }
            ValueView::Failed(error) => {
                let error = crate::error::snapshot(error);
                fix_position(&error, code, pos);
                return Err(error);
            }
            ValueView::Thunk { .. } | ValueView::App(_) => {}
            _ => return Ok(()),
        }

        out.with_roots(|temp| {
            let machine = self;
            #[derive(Clone, Copy)]
            enum Suspended<'r, 'h> {
                Thunk(EnvRoot<'r, 'h>, GlobalNode),
                App {
                    saved: ValueRoot<'r, 'h>,
                    fun: CellRoot<'r, 'h>,
                    arg: CellRoot<'r, 'h>,
                },
            }
            impl<'h> Suspended<'_, 'h> {
                #[inline]
                fn restore(self, heap: &mut Heap<'h>, cell: CellRoot<'_, 'h>) {
                    match self {
                        Self::Thunk(env, node) => heap.store_thunk(cell, env, node),
                        Self::App { saved, .. } => heap.store(cell, saved),
                    }
                }
            }
            #[cfg(not(brix_parallel))]
            let original = machine.heap.read(cell);
            #[cfg(brix_parallel)]
            let original = machine.heap.read_suspension(cell);
            let suspended = match original {
                // The environment and immutable node ID are the complete thunk
                // payload. Keep its one managed edge instead of also rooting a
                // duplicate Value snapshot through blackholing and execution.
                ValueView::Thunk { env, node } => Suspended::Thunk(env.root(temp), node),
                // Applications still need their original RawApp object alive
                // for restoration, in addition to the children passed to code.
                ValueView::App(app) => Suspended::App {
                    #[cfg(not(brix_parallel))]
                    saved: machine.heap.value(temp, cell),
                    #[cfg(brix_parallel)]
                    saved: machine.heap.suspension_value(temp, cell),
                    fun: app.fun().root(temp),
                    arg: app.arg().root(temp),
                },
                _ => unreachable!("the cell was suspended before entering its root frame"),
            };
            machine.heap.store_blackhole(cell);
            // Host callbacks can panic. Restore the original suspension before
            // propagating an unwind, instead of leaving a false blackhole.
            let result = catch_unwind(AssertUnwindSafe(|| match suspended {
                Suspended::Thunk(env, node) => {
                    code.with_stack(|| code.eval(machine, temp, env, node))
                }
                Suspended::App { fun, arg, .. } => {
                    code.with_stack(|| code.call_in_frame(machine, temp, fun, arg, pos))
                }
            }));
            match result {
                Ok(Ok(value)) => {
                    machine.heap.store(cell, value);
                    Ok(())
                }
                Ok(Err(error)) => {
                    #[cfg(brix_parallel)]
                    if machine.background.is_some() {
                        // The ordered consumer must choose the error and its
                        // position. Even caught failures must not be warmed.
                        suspended.restore(&mut machine.heap, cell);
                        return Err(if error.kind == ErrorKind::Deferred {
                            error
                        } else {
                            crate::runtime::note_deferral(format_args!(
                                "{:?}: {}",
                                error.kind,
                                bstr::BStr::new(&error.msg[..error.msg.len().min(60)])
                            ));
                            crate::error::deferred()
                        });
                    }
                    #[cfg(brix_parallel)]
                    if matches!(error.kind, ErrorKind::Interrupted | ErrorKind::Deferred) {
                        suspended.restore(&mut machine.heap, cell);
                        return Err(error);
                    }
                    // A call_in_frame override may leave one reduction's
                    // temporaries here until this one allocation completes.
                    // They disappear with temp on every return or unwind; no
                    // application loop can accumulate them in this frame.
                    fix_position(&error, code, pos);
                    #[cfg(test)]
                    let failed = if FAIL_FAILURE_MEMOIZATION.replace(false) {
                        Err(brix_gc::Error::OutOfMemory)
                    } else {
                        machine.heap.failed(temp, crate::error::snapshot(&error))
                    };
                    #[cfg(not(test))]
                    let failed = machine.heap.failed(temp, crate::error::snapshot(&error));
                    match failed {
                        Ok(failed) => machine.heap.store(cell, failed),
                        Err(allocation_error) => {
                            suspended.restore(&mut machine.heap, cell);
                            return allocation(Err(allocation_error));
                        }
                    }
                    Err(error)
                }
                Err(panic) => {
                    suspended.restore(&mut machine.heap, cell);
                    resume_unwind(panic)
                }
            }
        })
    }
}

#[cfg(brix_parallel)]
impl Machine<'_> {
    #[inline]
    #[track_caller]
    pub fn ordered_effect(&self) -> Result<()> {
        if self.background.is_some() {
            crate::runtime::note_deferral(std::panic::Location::caller());
            Err(crate::error::deferred())
        } else {
            Ok(())
        }
    }

    /// Where a worker stops for a collection, a cancellation or a cancelled
    /// background spark: before evaluating a thunk and before every call. Reading
    /// an already evaluated value is not a safepoint, so the common force of
    /// a WHNF cell stays one load and one compare.
    #[inline]
    pub fn safepoint(&mut self) -> Result<()> {
        self.poll_background()?;
        self.heap.poll()
    }

    /// A spark at a thunk another worker is evaluating: whether to stop
    /// instead of waiting for it. A blocked worker runs nothing, so a spark
    /// that has only just started gives way to the queued ones and is
    /// retried. One with work behind it waits: stopping would discard every
    /// call in progress, and most such waits are short.
    #[cold]
    fn yields(&mut self) -> bool {
        use crate::parallel::sparks::Busy;
        match self.busy {
            Busy::Skip => {
                self.skipped = true;
                true
            }
            Busy::Wait => false,
            Busy::Yield(started) => {
                self.yielded = started.elapsed() < crate::parallel::sparks::CHEAP_TO_RETRY
                    && self.sparks.as_ref().is_some_and(|sparks| sparks.queued());
                self.yielded
            }
        }
    }

    #[inline]
    pub fn poll_background(&self) -> Result<()> {

        if self
            .background
            .as_ref()
            .is_some_and(|cancelled| cancelled.load(std::sync::atomic::Ordering::Relaxed))
        {
            Err(crate::error::deferred())
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
std::thread_local! {
    static FAIL_FAILURE_MEMOIZATION: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn fix_position(error: &crate::Error, code: &impl Code, pos: SrcPos) {
    if error.kind == ErrorKind::InfiniteRecursion && error.pos().is_none() {
        error.set_pos(code.position(pos));
    }
}

#[cfg(all(test, not(brix_parallel)))]
mod tests {
    use super::*;
    use crate::managed_heap::with_owned_heap;
    use std::cell::Cell;

    struct TestCode {
        calls: Cell<usize>,
        fail: bool,
        panic: bool,
    }
    impl Code for TestCode {
        fn eval<'r, 'h>(
            &self,
            m: &mut Machine<'h>,
            r: &Roots<'r, 'h>,
            env: EnvRoot<'_, 'h>,
            _: GlobalNode,
        ) -> Result<ValueRoot<'r, 'h>> {
            self.calls.set(self.calls.get() + 1);
            for _ in 0..10 {
                allocation(m.heap.collect())?;
            }
            if self.panic {
                panic!("host callback panic");
            }
            if self.fail {
                return Err(EvalError::new(ErrorKind::Thrown, "test failure"));
            }
            let child = m.heap.env_view(env).get(0).unwrap().root(r);
            Ok(m.heap.value(r, child))
        }
        fn call<'r, 'h>(
            &self,
            m: &mut Machine<'h>,
            r: &Roots<'r, 'h>,
            fun: CellRoot<'_, 'h>,
            arg: CellRoot<'_, 'h>,
            _: SrcPos,
        ) -> Result<ValueRoot<'r, 'h>> {
            allocation(m.heap.collect())?;
            assert!(matches!(m.heap.read(fun), ValueView::Int(7)));
            Ok(m.heap.value(r, arg))
        }
        fn position(&self, _: SrcPos) -> Option<Position> {
            None
        }
    }
    fn config() -> brix_gc::Config {
        brix_gc::Config {
            workers: 0,
            chunk_bytes: 256,
            steady_threshold_bytes: None,
            trace_collections: false,
            threshold_bytes: 512,
            stress: true,
        }
    }

    #[test]
    fn force_keeps_suspended_graph_alive_and_memoizes_after_movement() {
        with_owned_heap(config(),|heap| {
            let mut m=Machine::new(heap);
            m.with_roots(|m,r| {
                let thunk=m.with_roots(|m,t| {
                    let s=m.heap.string(t,b"alive through blackholing").unwrap();
                    let child=m.heap.cell(t,s).unwrap();
                    let env=m.heap.env(t,None,&[child]).unwrap();
                    let thunk=m.heap.thunk(t,env,GlobalNode(0));
                    let cell=m.heap.cell(t,thunk).unwrap();
                    r.copy(cell)
                });
                let code=TestCode{calls:Cell::new(0),fail:false,panic:false};
                m.force_cell(&code,r,thunk,SrcPos::NONE).unwrap();
                assert!(matches!(m.heap.read(thunk),ValueView::String(s) if s.bytes()==b"alive through blackholing"));
                m.heap.collect().unwrap();
                m.force(&code,r,thunk,SrcPos::NONE).unwrap();
                assert_eq!(code.calls.get(),1);
            });
        }).unwrap();
    }

    #[test]
    fn successful_force_releases_captured_environment_but_keeps_result() {
        with_owned_heap(config(), |heap| {
            let mut machine = Machine::new(heap);
            machine.with_roots(|machine, roots| {
                let thunk = machine.with_roots(|machine, temporary| {
                    let result = machine.heap.string(temporary, b"result").unwrap();
                    let result = machine.heap.cell(temporary, result).unwrap();
                    let unused = machine.heap.string(temporary, &vec![b'x'; 128 * 1024]).unwrap();
                    let unused = machine.heap.cell(temporary, unused).unwrap();
                    let env = machine.heap.env(temporary, None, &[result, unused]).unwrap();
                    let value = machine.heap.thunk(temporary, env, GlobalNode(0));
                    let cell = machine.heap.cell(temporary, value).unwrap();
                    roots.copy(cell)
                });
                machine.heap.collect().unwrap();
                assert!(machine.heap.used_bytes() >= 128 * 1024);
                let code = TestCode { calls: Cell::new(0), fail: false, panic: false };
                machine.force_cell(&code, roots, thunk, SrcPos::NONE).unwrap();
                machine.heap.collect().unwrap();
                assert!(machine.heap.used_bytes() < 1024, "evaluated thunk retained its environment");
                assert!(matches!(machine.heap.read(thunk), ValueView::String(s) if s.bytes() == b"result"));
            });
        }).unwrap();
    }

    #[test]
    fn failed_force_is_memoized_and_panic_restores_thunk() {
        with_owned_heap(config(), |heap| {
            let mut m = Machine::new(heap);
            m.with_roots(|m, r| {
                let cell = m.with_roots(|m, temporary| {
                    let text = m.heap.string(temporary, b"capture survived panic").unwrap();
                    let child = m.heap.cell(temporary, text).unwrap();
                    let cell = m.heap.cell(temporary, temporary.null()).unwrap();
                    let env = m.heap.env(temporary, None, &[child, cell]).unwrap();
                    let value = m.heap.thunk(temporary, env, GlobalNode(37));
                    m.heap.store(cell, value);
                    r.copy(cell)
                });
                let panic = TestCode {
                    calls: Cell::new(0),
                    fail: false,
                    panic: true,
                };
                assert!(catch_unwind(AssertUnwindSafe(|| m.force_cell(
                    &panic,
                    r,
                    cell,
                    SrcPos::NONE
                )))
                .is_err());
                m.heap.collect().unwrap();
                let ValueView::Thunk { env, node } = m.heap.read(cell) else {
                    panic!("panic restored the thunk")
                };
                assert_eq!(node, GlobalNode(37));
                assert!(
                    matches!(m.heap.read_view(env.get(0).unwrap()), ValueView::String(text)
                    if text.bytes() == b"capture survived panic")
                );
                let back_edge = env.get(1).unwrap().root(r);
                assert!(m.heap.same_cell(back_edge, cell));
                let code = TestCode {
                    calls: Cell::new(0),
                    fail: true,
                    panic: false,
                };
                let error = m.force_cell(&code, r, cell, SrcPos::NONE).unwrap_err();
                m.heap.collect().unwrap();
                let again = m.force(&code, r, cell, SrcPos::NONE).unwrap_err();
                assert!(std::rc::Rc::ptr_eq(&error, &again));
                assert_eq!(code.calls.get(), 1);
            });
        })
        .unwrap();
    }

    #[test]
    fn forcing_whnf_cells_needs_no_snapshot_roots_or_scope() {
        with_owned_heap(config(), |heap| {
            let mut m = Machine::new(heap);
            m.with_roots(|m, roots| {
                let text = m.heap.string(roots, b"already evaluated").unwrap();
                let cell = m.heap.cell(roots, text).unwrap();
                let before = m.heap.root_slot_bytes();
                let collections = m.heap.stats().collections;
                let code = TestCode { calls: Cell::new(0), fail: false, panic: false };
                for _ in 0..4096 {
                    m.force_cell(&code, roots, cell, SrcPos::NONE).unwrap();
                }
                assert_eq!(m.heap.root_slot_bytes(), before);
                assert_eq!(m.heap.stats().collections, collections);
                assert_eq!(code.calls.get(), 0);
                m.heap.collect().unwrap();
                assert!(matches!(m.heap.read(cell), ValueView::String(s) if s.bytes() == b"already evaluated"));
            });
        }).unwrap();
    }

    #[test]
    fn failed_error_allocation_restores_suspension_for_retry() {
        with_owned_heap(config(), |heap| {
            let mut m = Machine::new(heap);
            m.with_roots(|m, roots| {
                let thunk = m.with_roots(|m, temporary| {
                    let text = m.heap.string(temporary, b"survived memoization OOM").unwrap();
                    let child = m.heap.cell(temporary, text).unwrap();
                    let env = m.heap.env(temporary, None, &[child]).unwrap();
                    let cell = m.heap.thunk_cell(temporary, env, GlobalNode(0)).unwrap();
                    roots.copy(cell)
                });
                let failed = TestCode { calls: Cell::new(0), fail: true, panic: false };
                FAIL_FAILURE_MEMOIZATION.set(true);
                let error = m.force_cell(&failed, roots, thunk, SrcPos::NONE).unwrap_err();
                assert_eq!(error.kind, ErrorKind::OutOfMemory);
                assert_eq!(failed.calls.get(), 1);
                m.heap.collect().unwrap();
                assert!(matches!(m.heap.read(thunk), ValueView::Thunk { .. }));
                let success = TestCode { calls: Cell::new(0), fail: false, panic: false };
                m.force_cell(&success, roots, thunk, SrcPos::NONE).unwrap();
                m.heap.collect().unwrap();
                assert!(matches!(m.heap.read(thunk), ValueView::String(s) if s.bytes() == b"survived memoization OOM"));
                assert_eq!(success.calls.get(), 1);
            });
        }).unwrap();
    }

    #[test]
    fn language_application_uses_owned_frame_and_keeps_result_through_collection() {
        let arena = crate::Arena::new();
        let host = crate::NullHost;
        let language = crate::managed_language::Language::new(crate::Evaluator::new(
            &arena,
            crate::Config::default(),
            &host,
        ));
        with_owned_heap(config(), |heap| {
            let mut machine = Machine::new(heap);
            machine.with_roots(|machine, roots| {
                let app = machine.with_roots(|machine, build| {
                    let fun = language.eval_str(machine, build, b"x: x", "/").unwrap();
                    let fun = machine.heap.cell(build, fun).unwrap();
                    let text = machine
                        .heap
                        .string(build, b"retained only by application")
                        .unwrap();
                    let arg = machine.heap.cell(build, text).unwrap();
                    let app = machine.heap.app_cell(build, fun, arg).unwrap();
                    roots.copy(app)
                });
                machine.heap.collect().unwrap();
                machine
                    .force_cell(&language, roots, app, SrcPos::NONE)
                    .unwrap();
                for _ in 0..3 {
                    machine.heap.collect().unwrap();
                    assert!(matches!(machine.heap.read(app), ValueView::String(text)
                        if text.bytes() == b"retained only by application"));
                }
            });
        })
        .unwrap();
    }

    #[test]
    fn language_application_error_memoization_oom_restores_graph_for_retry() {
        let arena = crate::Arena::new();
        let host = crate::NullHost;
        let language = crate::managed_language::Language::new(crate::Evaluator::new(
            &arena,
            crate::Config::default(),
            &host,
        ));
        with_owned_heap(config(), |heap| {
            let mut machine = Machine::new(heap);
            machine.with_roots(|machine, roots| {
                let flag = machine.heap.cell(roots, roots.bool(true)).unwrap();
                let app = machine.with_roots(|machine, build| {
                    let fun = language
                        .eval_str(
                            machine,
                            build,
                            b"x: if x then throw \"application failure\" else \"retry succeeded\"",
                            "/",
                        )
                        .unwrap();
                    let fun = machine.heap.cell(build, fun).unwrap();
                    let app = machine.heap.app_cell(build, fun, flag).unwrap();
                    roots.copy(app)
                });
                FAIL_FAILURE_MEMOIZATION.set(true);
                let error = machine
                    .force_cell(&language, roots, app, SrcPos::NONE)
                    .unwrap_err();
                assert_eq!(error.kind, ErrorKind::OutOfMemory);
                machine.heap.collect().unwrap();
                assert!(matches!(machine.heap.read(app), ValueView::App(_)));
                machine.heap.store(flag, roots.bool(false));
                machine
                    .force_cell(&language, roots, app, SrcPos::NONE)
                    .unwrap();
                machine.heap.collect().unwrap();
                assert!(matches!(machine.heap.read(app), ValueView::String(text)
                    if text.bytes() == b"retry succeeded"));

                // Normal error memoization runs its collecting allocation while
                // the step frame is live. Afterwards it must cache the same
                // error and release that step's temporary graph on scope exit.
                let fun = language
                    .eval_str(machine, roots, b"x: throw \"memoized application\"", "/")
                    .unwrap();
                let fun = machine.heap.cell(roots, fun).unwrap();
                let mut baseline = None;
                for _ in 0..8 {
                    machine.with_roots(|machine, temporary| {
                        let app = machine.heap.app_cell(temporary, fun, flag).unwrap();
                        let first = machine
                            .force_cell(&language, temporary, app, SrcPos::NONE)
                            .unwrap_err();
                        machine.heap.collect().unwrap();
                        let second = machine
                            .force_cell(&language, temporary, app, SrcPos::NONE)
                            .unwrap_err();
                        assert!(std::rc::Rc::ptr_eq(&first, &second));
                    });
                    machine.heap.collect().unwrap();
                    let used = machine.heap.used_bytes();
                    if let Some(baseline) = baseline {
                        assert_eq!(used, baseline);
                    }
                    baseline = Some(used);
                }
            });
        })
        .unwrap();
    }

    #[test]
    fn application_children_survive_collection_after_blackholing() {
        with_owned_heap(config(), |heap| {
            let mut m = Machine::new(heap);
            m.with_roots(|m, r| {
                let app = m.with_roots(|m, t| {
                    let fun = m.heap.cell(t, t.int(7)).unwrap();
                    let arg = m.heap.cell(t, t.int(42)).unwrap();
                    let app = m.heap.app(t, fun, arg, false).unwrap();
                    let app = m.heap.cell(t, app).unwrap();
                    r.copy(app)
                });
                let code = TestCode {
                    calls: Cell::new(0),
                    fail: false,
                    panic: false,
                };
                m.force_cell(&code, r, app, SrcPos::NONE).unwrap();
                assert!(matches!(m.heap.read(app), ValueView::Int(42)));
            });
        })
        .unwrap();
    }
}

#[cfg(test)]
mod timing {
    /// Manual, warmed-code comparison: run with --release --ignored --nocapture.
    #[test]
    #[ignore = "manual timing comparison; no performance assertion in CI"]
    fn no_collection_fold_timing() {
        use super::Machine;
        use crate::managed_heap::{with_owned_heap, ValueView};
        use crate::managed_language::Language;
        let cases: &[(&str, &[u8], i64)] = &[
            ("fold", b"builtins.foldl' (a: b: a + b) 0 (builtins.genList (x: x) 20000)", 199990000),
            ("attrs", br#"let xs = builtins.listToAttrs (builtins.genList (i: {name = "x${toString i}"; value = i;}) 1000); ys = builtins.mapAttrs (n: v: v + 1) xs; in builtins.foldl' (a: n: a + ys.${n}) 0 (builtins.attrNames (xs // ys))"#, 500500),
            ("strings", br#"builtins.stringLength (builtins.concatStringsSep ":" (map (i: "item-${toString i}") (builtins.genList (x: x) 5000)))"#, 48889),
        ];
        for &(name, source, expected) in cases {
            let arena = crate::Arena::new();
            let code_arena = crate::Arena::new();
            let host = crate::NullHost;
            let ev = crate::Evaluator::with_arenas(
                &arena,
                &code_arena,
                crate::Config::default(),
                &host,
                |_| {},
            );
            let expr = ev
                .parse_source(source, b"/", &crate::ir::Origin::String)
                .unwrap();
            let code = ev
                .load(
                    &expr,
                    crate::ir::Origin::String,
                    source.to_vec(),
                    None,
                    false,
                )
                .unwrap();
            assert!(
                matches!(ev.eval_node(code, code.root, ev.base_env).unwrap(), crate::Value::Int(n) if n == expected)
            );
            let language = Language::new(ev);
            with_owned_heap(brix_gc::Config { workers: 0,
                chunk_bytes: 1024 * 1024, steady_threshold_bytes: None, trace_collections: false, threshold_bytes: 1024 * 1024 * 1024, stress: false,
            }, |heap| {
                let mut machine = Machine::new(heap);
                machine.with_roots(|m, r| {
                    let env = language.base_env(m, r).unwrap();
                    let value = language.eval_code(m, r, code, env).unwrap();
                    assert!(matches!(m.heap.view(value), ValueView::Int(n) if n == expected));
                });
                let mut baseline = std::time::Duration::ZERO;
                let mut managed = std::time::Duration::ZERO;
                for round in 0..40 {
                    for half in 0..2 {
                        let start = std::time::Instant::now();
                        if (round + half) % 2 == 0 {
                            let ev = language.legacy();
                            assert!(matches!(ev.eval_node(code, code.root, ev.base_env).unwrap(), crate::Value::Int(n) if n == expected));
                            baseline += start.elapsed();
                        } else {
                            machine.with_roots(|m, r| {
                                let env = language.base_env(m, r).unwrap();
                                let value = language.eval_code(m, r, code, env).unwrap();
                                assert!(matches!(m.heap.view(value), ValueView::Int(n) if n == expected));
                            });
                            managed += start.elapsed();
                        }
                    }
                }
                eprintln!("case={name} legacy={baseline:?} managed={managed:?} ratio={:.3} collections={} used_bytes={}", managed.as_secs_f64() / baseline.as_secs_f64(), machine.heap.stats().collections, machine.heap.used_bytes());
                assert_eq!(machine.heap.stats().collections, 0);
            }).unwrap();
        }
    }
}

#[cfg(test)]
mod pressure {
    #[test]
    fn allocation_pressure_collects_inside_builtin_callbacks_and_reclaims_temporaries() {
        use crate::managed_api::{GcEvaluator, GcValueView};
        let arena = crate::Arena::new();
        let host = crate::NullHost;
        let mut eval = GcEvaluator::new(
            crate::Evaluator::new(&arena, crate::Config::default(), &host),
            brix_gc::Config {
                workers: 0,
                chunk_bytes: 1024,
                steady_threshold_bytes: None,
                trace_collections: false,
                threshold_bytes: 32 * 1024,
                stress: false,
            },
        )
        .unwrap();
        let value=eval.eval_str(b"builtins.foldl' (a: x: a + builtins.stringLength (builtins.concatStringsSep \"\" (builtins.genList (_: \"abcdefgh\") 16))) 0 (builtins.genList (x: x) 1000)",b"/").unwrap();
        assert!(matches!(
            eval.view(&value).unwrap(),
            GcValueView::Int(128000)
        ));
        assert!(
            eval.gc_stats().collections > 5,
            "collection must occur inside evaluation, not just at API exit"
        );
        assert!(eval.gc_stats().reclaimed_bytes > 1024 * 1024);
        eval.collect().unwrap();
        assert!(
            eval.heap_bytes() < 128 * 1024,
            "temporary loop values must not stay rooted: {} bytes",
            eval.heap_bytes()
        );
    }
}
