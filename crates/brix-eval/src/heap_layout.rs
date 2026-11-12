//! Movable evaluator storage and its exact tracing descriptors.
//!
//! These types contain no references into the managed heap. Access is through
//! the rooted evaluator interface; collection only reads and rewrites raw
//! pointer fields. Permanent code/primop pointers and owned error `Rc`s are
//! independent of the moving heap.

#![allow(unsafe_code)]
#![allow(dead_code)] // The managed evaluator interface is integrated separately.

use std::alloc::{Layout, LayoutError};
use std::mem::{align_of, size_of};
use std::ptr::{addr_of_mut, NonNull};

use brix_gc::raw::{Descriptor, Tracer};
use brix_store::AccessorId;

use crate::symbol::Symbol;
use crate::value::{GlobalNode, SrcPos};

pub(crate) type CellPtr = NonNull<RawCell>;

/// A possibly interior slice. Empty slices have an aligned dangling pointer,
/// never an allocation's one-past-end address, and are not traced.
#[repr(C)]
#[derive(Debug)]
pub(crate) struct RawSlice<T> {
    pub ptr: NonNull<T>,
    pub len: usize,
}

impl<T> Copy for RawSlice<T> {}
impl<T> Clone for RawSlice<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> RawSlice<T> {
    pub const fn empty() -> Self {
        Self {
            ptr: NonNull::dangling(),
            len: 0,
        }
    }

    pub const fn new(ptr: NonNull<T>, len: usize) -> Self {
        if len == 0 {
            Self::empty()
        } else {
            Self { ptr, len }
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum RawValue {
    Int(i64),
    Float(f64),
    Bool(bool),
    Null,
    String(NonNull<RawStr>),
    Path(NonNull<RawPath>),
    Attrs(NonNull<RawBindings>),
    List(NonNull<RawList>),
    Lambda {
        env: NonNull<RawEnv>,
        node: GlobalNode,
    },
    /// Permanent, evaluator-owned primop metadata. Never traced.
    PrimOp(usize),
    PrimOpApp(NonNull<RawApp>),
    Thunk {
        env: NonNull<RawEnv>,
        node: GlobalNode,
    },
    App(NonNull<RawApp>),
    Blackhole,
    Failed(NonNull<RawError>),
}

#[repr(C)]
#[cfg(not(brix_parallel))]
#[derive(Clone, Copy, Debug)]
pub(crate) struct RawCell {
    pub value: RawValue,
}

#[cfg(not(brix_parallel))]
impl RawCell {
    pub const fn new(value: RawValue) -> Self {
        Self { value }
    }
    #[inline]
    pub fn load(&self) -> RawValue {
        self.value
    }
    #[inline]
    pub fn load_for_whnf(&self) -> RawValue {
        self.value
    }
}

/// The value remains at offset zero for value-snapshot tracing. Only the
/// parallel build pays for the publication word (24 bytes on 64-bit targets).
#[cfg(brix_parallel)]
#[repr(C)]
pub(crate) struct RawCell {
    pub value: std::cell::UnsafeCell<RawValue>,
    pub publication: crate::parallel::publication::Publication,
}

#[cfg(brix_parallel)]
impl RawCell {
    pub const fn new(value: RawValue) -> Self {
        use crate::parallel::publication::Publication;
        Self {
            value: std::cell::UnsafeCell::new(value),
            publication: match value {
                RawValue::Thunk { .. } | RawValue::App(_) => Publication::suspended(),
                _ => Publication::ready(),
            },
        }
    }

    /// A running cell is observed as a blackhole without reading its changing
    /// payload. Only the force owner may read the original suspension.
    #[inline(always)]
    pub fn load(&self) -> RawValue {
        if self.publication.is_published() {
            return unsafe { self.value.get().read() };
        }
        self.load_unpublished()
    }

    #[cold]
    #[inline(never)]
    fn load_unpublished(&self) -> RawValue {
        use crate::parallel::publication::State;
        loop {
            match self.publication.state() {
                State::Ready => return unsafe { self.value.get().read() },
                State::Running(_) => return RawValue::Blackhole,
                State::Suspended => {
                    if let Ok(value) = self
                        .publication
                        .inspect(|| unsafe { self.value.get().read() })
                    {
                        return value;
                    }
                }
            }
        }
    }

    #[inline(always)]
    pub fn load_for_whnf(&self) -> RawValue {
        if self.publication.is_published() {
            return unsafe { self.value.get().read() };
        }
        self.load_unpublished_for_whnf()
    }

    #[cold]
    #[inline(never)]
    fn load_unpublished_for_whnf(&self) -> RawValue {
        // Published between the two reads, or still suspended/running.
        if self.publication.state() == crate::parallel::publication::State::Ready {
            unsafe { self.value.get().read() }
        } else {
            RawValue::Blackhole
        }
    }

    #[inline]
    pub fn load_owned(&self, owner: crate::parallel::WorkerId) -> RawValue {
        use crate::parallel::publication::State;
        assert_eq!(self.publication.state(), State::Running(owner));
        unsafe { self.value.get().read() }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub(crate) struct RawStr {
    pub bytes: RawSlice<u8>,
    pub context: RawSlice<RawContext>,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum RawContext {
    LazyHash {
        offset: u32,
        tree: u32,
    },
    LazyPath(u32),
    Opaque(RawSlice<u8>),
    DrvDeep(RawSlice<u8>),
    Built {
        drv: RawSlice<u8>,
        output: RawSlice<u8>,
    },
    Path(RawSlice<u8>),
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub(crate) struct RawPath {
    pub accessor: AccessorId,
    pub path: RawSlice<u8>,
    pub literal: Option<GlobalNode>,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub(crate) struct RawList {
    pub elems: RawSlice<CellPtr>,
}

/// Both suspended applications and partial primop applications.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub(crate) struct RawApp {
    pub fun: CellPtr,
    pub arg: CellPtr,
}

/// A frame's `len` cell pointers follow this header in the same allocation.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub(crate) struct RawEnv {
    pub up: Option<NonNull<RawEnv>>,
    pub len: u32,
}

/// Fixed allocation layout for the common single-argument lambda frame.
/// Its prefix is exactly RawEnv, so the ordinary environment views and tracer
/// also work for this descriptor without a second runtime representation.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub(crate) struct RawEnvOne {
    pub header: RawEnv,
    pub value: CellPtr,
}

pub(crate) const ENV_SLOTS_OFFSET: usize = size_of::<RawEnv>();

impl RawEnv {
    pub fn layout(len: usize) -> Result<Layout, LayoutError> {
        let (layout, _) = Layout::new::<Self>().extend(Layout::array::<CellPtr>(len)?)?;
        Ok(layout.pad_to_align())
    }
}

/// Shared header. Flat bindings have inline attributes; a tree has a root.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub(crate) struct RawBindings {
    pub pos: SrcPos,
    pub len: u32,
}

pub(crate) const FLAT_ATTRS_OFFSET: usize =
    size_of::<RawBindings>().next_multiple_of(align_of::<RawAttr>());

impl RawBindings {
    pub fn flat_layout(len: usize) -> Result<Layout, LayoutError> {
        let (layout, _) = Layout::new::<Self>().extend(Layout::array::<RawAttr>(len)?)?;
        Ok(layout.pad_to_align())
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub(crate) struct RawTreeBindings {
    pub header: RawBindings,
    pub root: NonNull<RawNode>,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub(crate) struct RawAttr {
    pub name: Symbol,
    pub pos: SrcPos,
    pub value: CellPtr,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum RawNode {
    Leaf(RawSlice<RawAttr>),
    Inner {
        children: RawSlice<NonNull<RawNode>>,
        firsts: RawSlice<Symbol>,
    },
}

/// `crate::Error` is `Rc<EvalError>`: moving this wrapper transfers its one
/// strong reference; only an unreachable/final wrapper runs its destructor.
#[repr(C)]
#[derive(Debug)]
pub(crate) struct RawError {
    pub error: crate::Error,
}

const _: () = {
    assert!(size_of::<RawValue>() == 16);
    #[cfg(not(brix_parallel))]
    assert!(size_of::<RawCell>() == 16);
    #[cfg(brix_parallel)]
    assert!(size_of::<RawCell>() == 24);
    assert!(size_of::<RawAttr>() == 8 + size_of::<usize>());
    assert!(size_of::<RawTreeBindings>() == 8 + size_of::<usize>());
    assert!(size_of::<Option<NonNull<RawEnv>>>() == size_of::<usize>());
    assert!(ENV_SLOTS_OFFSET % align_of::<CellPtr>() == 0);
    assert!(std::mem::offset_of!(RawEnvOne, header) == 0);
    assert!(std::mem::offset_of!(RawEnvOne, value) == ENV_SLOTS_OFFSET);
    assert!(size_of::<RawEnvOne>() == ENV_SLOTS_OFFSET + size_of::<CellPtr>());
    assert!(align_of::<RawEnvOne>() == align_of::<RawEnv>());
    assert!(FLAT_ATTRS_OFFSET == 8);
    assert!(size_of::<RawError>() == size_of::<usize>());
};

pub(crate) const CELL: usize = 0;
pub(crate) const STRING: usize = 1;
pub(crate) const PATH: usize = 2;
pub(crate) const LIST: usize = 3;
pub(crate) const APP: usize = 4;
pub(crate) const ENV: usize = 5;
pub(crate) const FLAT_BINDINGS: usize = 6;
pub(crate) const TREE_BINDINGS: usize = 7;
pub(crate) const NODE: usize = 8;
pub(crate) const ERROR: usize = 9;
pub(crate) const BYTES: usize = 10;
pub(crate) const CONTEXTS: usize = 11;
pub(crate) const ATTRS: usize = 12;
pub(crate) const CELLS: usize = 13;
pub(crate) const NODES: usize = 14;
pub(crate) const SYMBOLS: usize = 15;
pub(crate) const ENV_ONE: usize = 16;

const fn fixed<T>(
    name: &'static str,
    trace: unsafe fn(NonNull<u8>, usize, &mut Tracer<'_>),
) -> Descriptor {
    Descriptor {
        name,
        fixed_size: Some(size_of::<T>()),
        alignment: align_of::<T>(),
        trace,
        drop: None,
    }
}

const fn variable<T>(
    name: &'static str,
    trace: unsafe fn(NonNull<u8>, usize, &mut Tracer<'_>),
) -> Descriptor {
    Descriptor {
        name,
        fixed_size: None,
        alignment: align_of::<T>(),
        trace,
        drop: None,
    }
}

pub(crate) static DESCRIPTORS: [Descriptor; 17] = [
    fixed::<RawCell>("cell", trace_cell),
    fixed::<RawStr>("string", trace_string),
    fixed::<RawPath>("path", trace_path),
    fixed::<RawList>("list", trace_list),
    fixed::<RawApp>("application", trace_app),
    variable::<RawEnv>("environment", trace_env),
    // Inline attributes require their alignment even though the header has 4.
    variable::<RawAttr>("flat bindings", trace_flat_bindings),
    fixed::<RawTreeBindings>("tree bindings", trace_tree_bindings),
    fixed::<RawNode>("bindings node", trace_node),
    Descriptor {
        drop: Some(drop_error),
        ..fixed::<RawError>("error", trace_none)
    },
    variable::<u8>("bytes", trace_none),
    variable::<RawContext>("contexts", trace_contexts),
    variable::<RawAttr>("attributes", trace_attrs),
    variable::<CellPtr>("cell edges", trace_cell_edges),
    variable::<NonNull<RawNode>>("node edges", trace_node_edges),
    variable::<Symbol>("symbols", trace_none),
    fixed::<RawEnvOne>("single-slot environment", trace_env),
];

// All trace callbacks receive initialized payloads of their declared kind.
// Reads copy pointer/scalar values into locals; only raw writes modify the
// payload. No reference into an allocation survives a relocation call.

#[inline(always)]
unsafe fn trace_pointer<T>(slot: *mut NonNull<T>, tracer: &mut Tracer<'_>) {
    // SAFETY: the descriptor identifies this initialized managed pointer field.
    unsafe { slot.write(tracer.relocate(slot.read().cast()).cast()) };
}

#[inline(always)]
unsafe fn trace_cell_pointer(slot: *mut CellPtr, tracer: &mut Tracer<'_>) {
    // SAFETY: these fields always point to the start of a CELL allocation.
    unsafe { slot.write(tracer.relocate_fixed::<RawCell, CELL>(slot.read())) };
}

#[inline(always)]
unsafe fn trace_slice<T>(slot: *mut RawSlice<T>, tracer: &mut Tracer<'_>) {
    // SAFETY: the descriptor identifies an initialized slice, whose nonempty
    // data pointer is in bounds of a live owner; relocation preserves offset.
    unsafe {
        let slice = slot.read();
        let ptr = if slice.len == 0 {
            NonNull::dangling()
        } else {
            tracer.relocate(slice.ptr.cast()).cast()
        };
        slot.write(RawSlice {
            ptr,
            len: slice.len,
        });
    }
}

unsafe fn trace_cell(object: NonNull<u8>, _: usize, tracer: &mut Tracer<'_>) {
    // SAFETY: RawValue is Copy; matching a local enum examines only its active
    // variant, including when an Int's bits resemble an allocated address.
    unsafe {
        let slot = object.cast::<RawValue>().as_ptr();
        let mut value = slot.read();
        match &mut value {
            RawValue::String(p) => trace_pointer(p, tracer),
            RawValue::Path(p) => trace_pointer(p, tracer),
            RawValue::Attrs(p) => trace_pointer(p, tracer),
            RawValue::List(p) => trace_pointer(p, tracer),
            RawValue::Lambda { env, .. } | RawValue::Thunk { env, .. } => {
                trace_pointer(env, tracer)
            }
            RawValue::PrimOpApp(p) | RawValue::App(p) => {
                // Both variants point to the exact start of an APP allocation.
                *p = tracer.relocate_fixed::<RawApp, APP>(*p);
            }
            RawValue::Failed(p) => trace_pointer(p, tracer),
            RawValue::Int(_)
            | RawValue::Float(_)
            | RawValue::Bool(_)
            | RawValue::Null
            | RawValue::PrimOp(_)
            | RawValue::Blackhole => {}
        }
        slot.write(value);
    }
}

unsafe fn trace_string(object: NonNull<u8>, _: usize, tracer: &mut Tracer<'_>) {
    // SAFETY: a string consists of these two initialized raw slices.
    unsafe {
        let p = object.cast::<RawStr>().as_ptr();
        trace_slice(addr_of_mut!((*p).bytes), tracer);
        trace_slice(addr_of_mut!((*p).context), tracer);
    }
}

unsafe fn trace_path(object: NonNull<u8>, _: usize, tracer: &mut Tracer<'_>) {
    // SAFETY: the accessor id is scalar; path is the only pointer field.
    unsafe {
        trace_slice(
            addr_of_mut!((*object.cast::<RawPath>().as_ptr()).path),
            tracer,
        )
    };
}

unsafe fn trace_list(object: NonNull<u8>, _: usize, tracer: &mut Tracer<'_>) {
    // SAFETY: list points to an initialized cell-edge buffer.
    unsafe {
        trace_slice(
            addr_of_mut!((*object.cast::<RawList>().as_ptr()).elems),
            tracer,
        )
    };
}

unsafe fn trace_app(object: NonNull<u8>, _: usize, tracer: &mut Tracer<'_>) {
    // SAFETY: both application edges are initialized cell pointers.
    unsafe {
        let p = object.cast::<RawApp>().as_ptr();
        trace_cell_pointer(addr_of_mut!((*p).fun), tracer);
        trace_cell_pointer(addr_of_mut!((*p).arg), tracer);
    }
}

unsafe fn trace_env(object: NonNull<u8>, _: usize, tracer: &mut Tracer<'_>) {
    // SAFETY: the initialized header is followed by len initialized cell edges.
    unsafe {
        let p = object.cast::<RawEnv>().as_ptr();
        let up = addr_of_mut!((*p).up);
        if let Some(mut parent) = up.read() {
            trace_pointer(&mut parent, tracer);
            up.write(Some(parent));
        }
        let len = addr_of_mut!((*p).len).read() as usize;
        let edges = object.as_ptr().add(ENV_SLOTS_OFFSET).cast::<CellPtr>();
        for i in 0..len {
            trace_cell_pointer(edges.add(i), tracer);
        }
    }
}

unsafe fn trace_flat_bindings(object: NonNull<u8>, _: usize, tracer: &mut Tracer<'_>) {
    // SAFETY: the header is followed by len initialized RawAttrs.
    unsafe {
        let len = addr_of_mut!((*object.cast::<RawBindings>().as_ptr()).len).read() as usize;
        let attrs = object.as_ptr().add(FLAT_ATTRS_OFFSET).cast::<RawAttr>();
        for i in 0..len {
            trace_cell_pointer(addr_of_mut!((*attrs.add(i)).value), tracer);
        }
    }
}

unsafe fn trace_tree_bindings(object: NonNull<u8>, _: usize, tracer: &mut Tracer<'_>) {
    // SAFETY: a tree's root is its only managed pointer field.
    unsafe {
        trace_pointer(
            addr_of_mut!((*object.cast::<RawTreeBindings>().as_ptr()).root),
            tracer,
        )
    };
}

unsafe fn trace_node(object: NonNull<u8>, _: usize, tracer: &mut Tracer<'_>) {
    // SAFETY: copy the discriminated node to a local before relocating slices.
    unsafe {
        let p = object.cast::<RawNode>().as_ptr();
        let mut node = p.read();
        match &mut node {
            RawNode::Leaf(attrs) => trace_slice(attrs, tracer),
            RawNode::Inner { children, firsts } => {
                trace_slice(children, tracer);
                trace_slice(firsts, tracer);
            }
        }
        p.write(node);
    }
}

unsafe fn trace_contexts(object: NonNull<u8>, bytes: usize, tracer: &mut Tracer<'_>) {
    // SAFETY: payload is a whole number of initialized context elements; only
    // active variants contain edges. The local copies have no destructors.
    unsafe {
        for i in 0..bytes / size_of::<RawContext>() {
            let p = object.cast::<RawContext>().as_ptr().add(i);
            let mut context = p.read();
            match &mut context {
                RawContext::Opaque(path) | RawContext::DrvDeep(path) | RawContext::Path(path) => {
                    trace_slice(path, tracer)
                }
                RawContext::Built { drv, output } => {
                    trace_slice(drv, tracer);
                    trace_slice(output, tracer);
                }
                RawContext::LazyHash { .. } | RawContext::LazyPath(_) => {}
            }
            p.write(context);
        }
    }
}

unsafe fn trace_attrs(object: NonNull<u8>, bytes: usize, tracer: &mut Tracer<'_>) {
    // SAFETY: every whole RawAttr in the payload is initialized.
    unsafe {
        for i in 0..bytes / size_of::<RawAttr>() {
            trace_cell_pointer(
                addr_of_mut!((*object.cast::<RawAttr>().as_ptr().add(i)).value),
                tracer,
            );
        }
    }
}

unsafe fn trace_cell_edges(object: NonNull<u8>, bytes: usize, tracer: &mut Tracer<'_>) {
    // SAFETY: every whole cell pointer in the payload is initialized.
    unsafe {
        for i in 0..bytes / size_of::<CellPtr>() {
            trace_cell_pointer(object.cast::<CellPtr>().as_ptr().add(i), tracer);
        }
    }
}

unsafe fn trace_node_edges(object: NonNull<u8>, bytes: usize, tracer: &mut Tracer<'_>) {
    // SAFETY: every whole node pointer in the payload is initialized.
    unsafe {
        for i in 0..bytes / size_of::<NonNull<RawNode>>() {
            trace_pointer(object.cast::<NonNull<RawNode>>().as_ptr().add(i), tracer);
        }
    }
}

fn trace_none(_: NonNull<u8>, _: usize, _: &mut Tracer<'_>) {}

unsafe fn drop_error(object: NonNull<u8>, _: usize) {
    // SAFETY: the collector calls this once for a dead owning wrapper, never
    // for its forwarded source copy. Rc targets are outside the moving heap.
    // An error's source-loader captures can have user-defined destructors.
    // Collection has already changed roots, so such a panic must not unwind
    // back into the mutator with a partially evacuated heap.
    let dropped = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        object.cast::<RawError>().as_ptr().drop_in_place();
    }));
    if dropped.is_err() {
        std::process::abort();
    }
}

#[cfg(all(test, not(brix_parallel)))]
mod tests {
    use super::*;
    use std::rc::Rc;

    use crate::error::{ErrorKind, EvalError};
    use brix_gc::raw::RawHeap;

    fn heap() -> RawHeap {
        RawHeap::new(
            brix_gc::Config {
                chunk_bytes: 256,
                ..Default::default()
            },
            &DESCRIPTORS,
        )
        .unwrap()
    }

    // The test builders initialize each allocation completely before the next
    // heap call. They never collect; each caller supplies the matching kind.
    unsafe fn put<T>(heap: &mut RawHeap, kind: usize, value: T) -> NonNull<T> {
        unsafe {
            let p = heap
                .allocate_slow(kind, size_of::<T>())
                .unwrap()
                .cast::<T>();
            p.as_ptr().write(value);
            p
        }
    }

    unsafe fn array<T: Copy>(heap: &mut RawHeap, kind: usize, values: &[T]) -> RawSlice<T> {
        if values.is_empty() {
            return RawSlice::empty();
        }
        unsafe {
            let p = heap
                .allocate_slow(kind, size_of_val(values))
                .unwrap()
                .cast::<T>();
            std::ptr::copy_nonoverlapping(values.as_ptr(), p.as_ptr(), values.len());
            RawSlice::new(p, values.len())
        }
    }

    unsafe fn cell(heap: &mut RawHeap, value: RawValue) -> CellPtr {
        unsafe { put(heap, CELL, RawCell::new(value)) }
    }

    unsafe fn env(
        heap: &mut RawHeap,
        up: Option<NonNull<RawEnv>>,
        values: &[CellPtr],
    ) -> NonNull<RawEnv> {
        unsafe {
            let p = heap
                .allocate_slow(ENV, RawEnv::layout(values.len()).unwrap().size())
                .unwrap();
            p.cast::<RawEnv>().as_ptr().write(RawEnv {
                up,
                len: values.len() as u32,
            });
            std::ptr::copy_nonoverlapping(
                values.as_ptr(),
                p.as_ptr().add(ENV_SLOTS_OFFSET).cast(),
                values.len(),
            );
            p.cast()
        }
    }

    unsafe fn flat(heap: &mut RawHeap, attrs: &[RawAttr]) -> NonNull<RawBindings> {
        unsafe {
            let p = heap
                .allocate_slow(
                    FLAT_BINDINGS,
                    RawBindings::flat_layout(attrs.len()).unwrap().size(),
                )
                .unwrap();
            p.cast::<RawBindings>().as_ptr().write(RawBindings {
                pos: SrcPos(7),
                len: attrs.len() as u32,
            });
            std::ptr::copy_nonoverlapping(
                attrs.as_ptr(),
                p.as_ptr().add(FLAT_ATTRS_OFFSET).cast(),
                attrs.len(),
            );
            p.cast()
        }
    }

    unsafe fn list(heap: &mut RawHeap, values: &[CellPtr]) -> CellPtr {
        unsafe {
            let elems = array(heap, CELLS, values);
            let list = put(heap, LIST, RawList { elems });
            cell(heap, RawValue::List(list))
        }
    }

    unsafe fn elements(root: CellPtr) -> RawSlice<CellPtr> {
        unsafe {
            let RawValue::List(list) = root.as_ptr().read().value else {
                panic!("expected list")
            };
            list.as_ptr().read().elems
        }
    }

    unsafe fn collect(heap: &mut RawHeap, root: &mut CellPtr) {
        // No payload references survive the call, and root is the only strong
        // root. The graph's descriptors account for every managed child.
        unsafe {
            let mut raw = root.cast::<u8>();
            heap.collect(|tracer| tracer.visit(&mut raw), |_| {})
                .unwrap();
            *root = raw.cast();
        }
    }

    #[test]
    fn all_layouts_and_active_variants_survive_a_cyclic_collection() {
        // Every heap use below is through initialized raw pointers, and only
        // root is accessed across a collection. Local scalar metadata survives.
        unsafe {
            let mut heap = heap();
            let mut permanent_primop = 0_u8;
            let permanent = NonNull::from(&mut permanent_primop).cast::<()>();
            let error = EvalError::new(ErrorKind::Eval, "remembered failure");
            let mut root = cell(&mut heap, RawValue::Null);
            let bytes = array(&mut heap, BYTES, b"/shared/path");
            let contexts = array(
                &mut heap,
                CONTEXTS,
                &[
                    RawContext::LazyHash { offset: 1, tree: 2 },
                    RawContext::LazyPath(3),
                    RawContext::Opaque(bytes),
                    RawContext::DrvDeep(bytes),
                    RawContext::Built {
                        drv: bytes,
                        output: bytes,
                    },
                    RawContext::Path(bytes),
                ],
            );
            let string = put(
                &mut heap,
                STRING,
                RawStr {
                    bytes,
                    context: contexts,
                },
            );
            let string_cell = cell(&mut heap, RawValue::String(string));
            let path = put(
                &mut heap,
                PATH,
                RawPath {
                    accessor: AccessorId(7),
                    path: bytes,
                    literal: Some(GlobalNode(23)),
                },
            );
            let path_cell = cell(&mut heap, RawValue::Path(path));
            let failed = put(
                &mut heap,
                ERROR,
                RawError {
                    error: error.clone(),
                },
            );
            let failed_cell = cell(&mut heap, RawValue::Failed(failed));
            let application = put(
                &mut heap,
                APP,
                RawApp {
                    fun: root,
                    arg: failed_cell,
                },
            );
            let app_cell = cell(&mut heap, RawValue::App(application));
            let partial_cell = cell(&mut heap, RawValue::PrimOpApp(application));
            let parent = env(&mut heap, None, &[root]);
            let frame = env(&mut heap, Some(parent), &[string_cell, app_cell]);
            let lambda = cell(
                &mut heap,
                RawValue::Lambda {
                    env: frame,
                    node: GlobalNode(11),
                },
            );
            let thunk = cell(
                &mut heap,
                RawValue::Thunk {
                    env: frame,
                    node: GlobalNode(12),
                },
            );
            let attrs: Vec<_> = (0..65)
                .map(|i| RawAttr {
                    name: Symbol(i),
                    pos: SrcPos(i + 20),
                    value: if i % 2 == 0 { lambda } else { path_cell },
                })
                .collect();
            let flat = flat(&mut heap, &attrs[..2]);
            let flat_cell = cell(&mut heap, RawValue::Attrs(flat));
            let attrs = array(&mut heap, ATTRS, &attrs);
            let left = put(&mut heap, NODE, RawNode::Leaf(RawSlice::new(attrs.ptr, 32)));
            let right = put(
                &mut heap,
                NODE,
                RawNode::Leaf(RawSlice::new(
                    NonNull::new_unchecked(attrs.ptr.as_ptr().add(32)),
                    33,
                )),
            );
            let children = array(&mut heap, NODES, &[left, right]);
            let firsts = array(&mut heap, SYMBOLS, &[Symbol(0), Symbol(32)]);
            let node = put(&mut heap, NODE, RawNode::Inner { children, firsts });
            let tree = put(
                &mut heap,
                TREE_BINDINGS,
                RawTreeBindings {
                    header: RawBindings {
                        pos: SrcPos(9),
                        len: 65,
                    },
                    root: node,
                },
            );
            let tree_cell = cell(&mut heap, RawValue::Attrs(tree.cast()));
            let mut values = vec![
                string_cell,
                path_cell,
                failed_cell,
                app_cell,
                partial_cell,
                lambda,
                thunk,
                flat_cell,
                tree_cell,
                root,
            ];
            for value in [
                RawValue::Int(42),
                RawValue::Float(1.5),
                RawValue::Bool(true),
                RawValue::Null,
                RawValue::Blackhole,
                RawValue::PrimOp(permanent.as_ptr() as usize),
            ] {
                values.push(cell(&mut heap, value));
            }
            let elems = array(&mut heap, CELLS, &values);
            let root_list = put(&mut heap, LIST, RawList { elems });
            root.as_ptr().write(RawCell::new(RawValue::List(root_list)));
            for iteration in 0..3 {
                let old_address = root.as_ptr() as usize;
                collect(&mut heap, &mut root);
                if iteration == 0 {
                    assert_ne!(root.as_ptr() as usize, old_address);
                }
                assert_eq!(Rc::strong_count(&error), 2);
                let elems = elements(root);
                let at = |i| elems.ptr.as_ptr().add(i).read().as_ptr().read().value;
                assert_eq!(elems.ptr.as_ptr().add(9).read(), root);
                let RawValue::String(string) = at(0) else {
                    panic!("string")
                };
                let string = string.as_ptr().read();
                assert_eq!(
                    std::slice::from_raw_parts(string.bytes.ptr.as_ptr(), string.bytes.len),
                    b"/shared/path"
                );
                assert_eq!(string.context.len, 6);
                for i in 2..6 {
                    let context = string.context.ptr.as_ptr().add(i).read();
                    match context {
                        RawContext::Opaque(p) | RawContext::DrvDeep(p) | RawContext::Path(p) => {
                            assert_eq!(p.ptr, string.bytes.ptr)
                        }
                        RawContext::Built { drv, output } => {
                            assert_eq!(drv.ptr, string.bytes.ptr);
                            assert_eq!(output.ptr, drv.ptr);
                        }
                        _ => panic!("context variant"),
                    }
                }
                let RawValue::Path(path) = at(1) else {
                    panic!("path")
                };
                assert_eq!(path.as_ptr().read().path.ptr, string.bytes.ptr);
                let RawValue::Failed(failed) = at(2) else {
                    panic!("failed")
                };
                assert!(Rc::ptr_eq(&(*failed.as_ptr()).error, &error));
                let RawValue::App(app) = at(3) else {
                    panic!("app")
                };
                assert_eq!(app.as_ptr().read().fun, root);
                let RawValue::PrimOpApp(partial) = at(4) else {
                    panic!("partial")
                };
                assert_eq!(partial, app);
                let RawValue::Lambda { env, node } = at(5) else {
                    panic!("lambda")
                };
                assert_eq!(node, GlobalNode(11));
                let RawValue::Thunk {
                    env: thunk_env,
                    node,
                } = at(6)
                else {
                    panic!("thunk")
                };
                assert_eq!(thunk_env, env);
                assert_eq!(node, GlobalNode(12));
                let slots = env.as_ptr().byte_add(ENV_SLOTS_OFFSET).cast::<CellPtr>();
                assert_eq!(slots.read(), elems.ptr.as_ptr().read());
                assert_eq!(slots.add(1).read(), elems.ptr.as_ptr().add(3).read());
                let parent = env.as_ptr().read().up.unwrap();
                assert_eq!(
                    parent
                        .as_ptr()
                        .byte_add(ENV_SLOTS_OFFSET)
                        .cast::<CellPtr>()
                        .read(),
                    root
                );
                let RawValue::Attrs(flat) = at(7) else {
                    panic!("flat")
                };
                assert_eq!(flat.as_ptr().read().len, 2);
                assert_eq!(
                    flat.as_ptr()
                        .byte_add(FLAT_ATTRS_OFFSET)
                        .cast::<RawAttr>()
                        .read()
                        .value,
                    elems.ptr.as_ptr().add(5).read()
                );
                let RawValue::Attrs(tree) = at(8) else {
                    panic!("tree")
                };
                let tree = tree.cast::<RawTreeBindings>().as_ptr().read();
                let RawNode::Inner { children, firsts } = tree.root.as_ptr().read() else {
                    panic!("inner")
                };
                assert_eq!(firsts.ptr.as_ptr().add(1).read(), Symbol(32));
                let RawNode::Leaf(left) = children.ptr.as_ptr().read().as_ptr().read() else {
                    panic!("left")
                };
                let RawNode::Leaf(right) = children.ptr.as_ptr().add(1).read().as_ptr().read()
                else {
                    panic!("right")
                };
                assert_eq!(right.ptr.as_ptr(), left.ptr.as_ptr().add(32));
                assert_eq!(
                    left.ptr.as_ptr().read().value,
                    elems.ptr.as_ptr().add(5).read()
                );
                assert_eq!(
                    right.ptr.as_ptr().add(1).read().value,
                    elems.ptr.as_ptr().add(1).read()
                );
                assert!(matches!(at(10), RawValue::Int(42)));
                assert!(matches!(at(11), RawValue::Float(f) if f == 1.5));
                assert!(matches!(at(12), RawValue::Bool(true)));
                assert!(matches!(at(13), RawValue::Null));
                assert!(matches!(at(14), RawValue::Blackhole));
                assert!(matches!(at(15), RawValue::PrimOp(p) if p == permanent.as_ptr() as usize));
            }
            drop(heap);
            assert_eq!(Rc::strong_count(&error), 1);
        }
    }

    #[test]
    fn interior_and_empty_slices_preserve_sharing_without_false_roots() {
        // Empty slices deliberately name a dead owner's start here; tracing
        // must canonicalize them without looking up or retaining that owner.
        unsafe {
            let mut heap = heap();
            let error = EvalError::new(ErrorKind::Eval, "not rooted by an empty slice");
            let dead = put(
                &mut heap,
                ERROR,
                RawError {
                    error: error.clone(),
                },
            );
            let bytes = array(&mut heap, BYTES, b"abcdef");
            let part = RawSlice::new(NonNull::new_unchecked(bytes.ptr.as_ptr().add(2)), 3);
            let s1 = put(
                &mut heap,
                STRING,
                RawStr {
                    bytes,
                    context: RawSlice::empty(),
                },
            );
            let s2 = put(
                &mut heap,
                STRING,
                RawStr {
                    bytes: part,
                    context: RawSlice::empty(),
                },
            );
            let empty = put(
                &mut heap,
                STRING,
                RawStr {
                    bytes: RawSlice {
                        ptr: dead.cast(),
                        len: 0,
                    },
                    context: RawSlice::empty(),
                },
            );
            let a = cell(&mut heap, RawValue::String(s1));
            let b = cell(&mut heap, RawValue::String(s2));
            let c = cell(&mut heap, RawValue::String(empty));
            // An integer with the dead object's address is not a root either.
            let bits = cell(&mut heap, RawValue::Int(dead.as_ptr() as usize as i64));
            let mut root = list(&mut heap, &[a, b, c, bits]);
            collect(&mut heap, &mut root);
            assert_eq!(Rc::strong_count(&error), 1);
            let elems = elements(root);
            let string = |i| {
                let RawValue::String(p) = elems.ptr.as_ptr().add(i).read().as_ptr().read().value
                else {
                    panic!("string")
                };
                p.as_ptr().read()
            };
            let a = string(0).bytes;
            let b = string(1).bytes;
            assert_eq!(b.ptr.as_ptr(), a.ptr.as_ptr().add(2));
            assert_eq!(std::slice::from_raw_parts(b.ptr.as_ptr(), b.len), b"cde");
            assert_eq!(string(2).bytes.ptr, NonNull::dangling());
            assert_eq!(string(2).bytes.len, 0);
            assert_eq!(RawSlice::new(a.ptr, 0).ptr, NonNull::dangling());
        }
    }

    #[test]
    fn failed_errors_drop_once_when_dead_or_when_the_heap_is_dropped() {
        unsafe {
            let mut heap = heap();
            let error = EvalError::new(ErrorKind::Eval, "shared error");
            let _dead = put(
                &mut heap,
                ERROR,
                RawError {
                    error: error.clone(),
                },
            );
            let live = put(
                &mut heap,
                ERROR,
                RawError {
                    error: error.clone(),
                },
            );
            let mut root = cell(&mut heap, RawValue::Failed(live));
            assert_eq!(Rc::strong_count(&error), 3);
            collect(&mut heap, &mut root);
            assert_eq!(Rc::strong_count(&error), 2);
            collect(&mut heap, &mut root);
            assert_eq!(Rc::strong_count(&error), 2);
            heap.collect(|_| {}, |_| {}).unwrap();
            assert_eq!(Rc::strong_count(&error), 1);
            let _remaining = put(
                &mut heap,
                ERROR,
                RawError {
                    error: error.clone(),
                },
            );
            drop(heap);
            assert_eq!(Rc::strong_count(&error), 1);
        }
    }
}
