//! Type-erased, scoped roots for a single-threaded moving heap.
//!
//! This is an unsafe backend. A root stores an updateable value, not a
//! stable heap address. Copying the inert raw payload out of a scoped root
//! (`read`, `snapshot`, `RootVec::get`/`pop`) is safe, but the copy is only
//! current until the next collection: every *use* of it (a dereference, or
//! storing it back into a root) is unsafe. Safe wrappers must tie every such
//! use of `Stored` to a borrow that prevents collection.
//!
//! A scoped root cannot escape its frame:
//! ```compile_fail
//! use brix_gc::roots::{with_registry, RootDescriptor, RootKind};
//! use brix_gc::raw::Tracer;
//! use std::ptr::NonNull;
//! enum Integer {}
//! unsafe impl RootKind for Integer { type Stored = usize; const KIND: usize = 0; }
//! unsafe fn trace(_: NonNull<u8>, _: &mut Tracer<'_>) {}
//! static DESCRIPTORS: [RootDescriptor; 1] = [RootDescriptor {
//!     size: size_of::<usize>(), align: align_of::<usize>(), trace,
//! }];
//! with_registry(&DESCRIPTORS, |registry| {
//!     let escaped = registry.with_roots(|roots| unsafe { roots.push::<Integer>(42) });
//!     assert_eq!(escaped.read(), 42);
//! });
//! ```
//!
//! Promotion copies a snapshot into an outer frame; copying a root itself
//! merely aliases its existing slot:
//! ```
//! use brix_gc::roots::{with_registry, RootDescriptor, RootKind};
//! use brix_gc::raw::Tracer;
//! use std::ptr::NonNull;
//! enum Integer {}
//! unsafe impl RootKind for Integer { type Stored = usize; const KIND: usize = 0; }
//! unsafe fn trace(_: NonNull<u8>, _: &mut Tracer<'_>) {}
//! static DESCRIPTORS: [RootDescriptor; 1] = [RootDescriptor {
//!     size: size_of::<usize>(), align: align_of::<usize>(), trace,
//! }];
//! with_registry(&DESCRIPTORS, |registry| registry.with_roots(|outer| {
//!     let promoted = outer.with_roots(|inner| unsafe {
//!         let temporary = inner.push::<Integer>(42);
//!         outer.copy(temporary.snapshot())
//!     });
//!     assert_eq!(promoted.read(), 42);
//! }));
//! ```
//!
//! Heap brands cannot be mixed, even when payload kinds are identical:
//! ```compile_fail
//! use brix_gc::roots::{with_registry, RootDescriptor, RootKind};
//! use brix_gc::raw::Tracer;
//! use std::ptr::NonNull;
//! enum Integer {}
//! unsafe impl RootKind for Integer { type Stored = usize; const KIND: usize = 0; }
//! unsafe fn trace(_: NonNull<u8>, _: &mut Tracer<'_>) {}
//! static DESCRIPTORS: [RootDescriptor; 1] = [RootDescriptor {
//!     size: size_of::<usize>(), align: align_of::<usize>(), trace,
//! }];
//! with_registry(&DESCRIPTORS, |first| first.with_roots(|outer| {
//!     with_registry(&DESCRIPTORS, |second| second.with_roots(|inner| unsafe {
//!         let foreign = inner.push::<Integer>(42);
//!         outer.copy(foreign.snapshot());
//!     }));
//! }));
//! ```
//!
//! Vector handles are scoped too:
//! ```compile_fail
//! use brix_gc::roots::{with_registry, RootDescriptor, RootKind};
//! use brix_gc::raw::Tracer;
//! use std::ptr::NonNull;
//! enum Integer {}
//! unsafe impl RootKind for Integer { type Stored = usize; const KIND: usize = 0; }
//! unsafe fn trace(_: NonNull<u8>, _: &mut Tracer<'_>) {}
//! static DESCRIPTORS: [RootDescriptor; 1] = [RootDescriptor {
//!     size: size_of::<usize>(), align: align_of::<usize>(), trace,
//! }];
//! with_registry(&DESCRIPTORS, |registry| {
//!     let escaped = registry.with_roots(|roots| roots.vector::<Integer>());
//!     assert_eq!(escaped.len(), 0);
//! });
//! ```
//!
//! Persistent leases retain the heap brand when promoted into a frame:
//! ```compile_fail
//! use brix_gc::roots::{with_registry, RootDescriptor, RootKind};
//! use brix_gc::raw::Tracer;
//! use std::ptr::NonNull;
//! enum Integer {}
//! unsafe impl RootKind for Integer { type Stored = usize; const KIND: usize = 0; }
//! unsafe fn trace(_: NonNull<u8>, _: &mut Tracer<'_>) {}
//! static DESCRIPTORS: [RootDescriptor; 1] = [RootDescriptor {
//!     size: size_of::<usize>(), align: align_of::<usize>(), trace,
//! }];
//! with_registry(&DESCRIPTORS, |first| first.with_roots(|outer| {
//!     with_registry(&DESCRIPTORS, |second| unsafe {
//!         let foreign = second.persist::<Integer>(42);
//!         outer.copy(foreign.snapshot());
//!     });
//! }));
//! ```

use std::alloc::{alloc, dealloc, handle_alloc_error, Layout};
use std::cell::{Cell, RefCell, UnsafeCell};
use std::marker::PhantomData;
use std::ptr::NonNull;
use std::rc::{Rc, Weak};

use crate::raw::Tracer;

/// Owning root leases whose reference counts may cross OS threads. Root
/// payload access and relocation still require the caller's mutator protocol.
pub mod shared;

type Brand<'a> = PhantomData<fn(&'a ()) -> &'a ()>;
const MIN_SLOTS_PER_CHUNK: usize = 16;

/// Exact layout and tracing operation for one root payload kind.
#[derive(Clone, Copy)]
pub struct RootDescriptor {
    pub size: usize,
    pub align: usize,
    /// Updates precisely the managed pointers in an initialized payload.
    /// Must not unwind, reenter the registry, or retain payload references.
    pub trace: unsafe fn(NonNull<u8>, &mut Tracer<'_>),
}

/// Associates a marker with a copyable raw payload and registry descriptor.
///
/// # Safety
/// `KIND` must select a descriptor with exactly `Stored`'s layout and tracing
/// semantics in every registry used with this implementation. The tracer may
/// mutate managed pointers through shared handles; `Stored` must not contain
/// borrowed references to its root storage or require a destructor.
pub unsafe trait RootKind {
    type Stored: Copy;
    const KIND: usize;
}

struct Chunk {
    data: NonNull<u8>,
    layout: Layout,
    used: usize,
}

impl Chunk {
    fn new(layout: Layout) -> Self {
        // Root layouts have positive padded size, including zero-sized payloads.
        let data =
            NonNull::new(unsafe { alloc(layout) }).unwrap_or_else(|| handle_alloc_error(layout));
        Self {
            data,
            layout,
            used: 0,
        }
    }
}

impl Drop for Chunk {
    fn drop(&mut self) {
        unsafe { dealloc(self.data.as_ptr(), self.layout) };
    }
}

struct Storage {
    chunks: RefCell<Vec<Chunk>>,
    layout: Layout,
    active_chunk: Cell<usize>,
    next: Cell<*mut u8>,
    end: Cell<*mut u8>,
    first: Cell<*mut u8>,
    first_end: Cell<*mut u8>,
}

impl Storage {
    fn new(layout: Layout) -> Self {
        Self {
            chunks: RefCell::new(Vec::new()),
            layout,
            active_chunk: Cell::new(0),
            next: Cell::new(std::ptr::null_mut()),
            end: Cell::new(std::ptr::null_mut()),
            first: Cell::new(std::ptr::null_mut()),
            first_end: Cell::new(std::ptr::null_mut()),
        }
    }

    #[inline]
    fn push<T: Copy>(&self, kind: usize, value: T) -> NonNull<T> {
        let mut cell = self.next.get();
        // Every slot starts on a word boundary. For ordinary pointer/value
        // roots these constants are 16/24 bytes; unusually aligned payloads
        // can need padding between the tag and payload. The registry's checked
        // maximum layout guarantees this worst-case extent fits a chunk.
        let payload_bytes = size_of::<T>().max(1).next_multiple_of(size_of::<usize>());
        let extent = align_of::<T>().max(align_of::<usize>()) + payload_bytes;
        if self.end.get().addr() - cell.addr() < extent {
            cell = self.next_chunk();
        }
        // No user callbacks or collection occur between these updates. Only the
        // initialized prefix becomes visible to tracing after the payload write.
        // SAFETY: the checked remaining extent covers the word tag, alignment
        // padding and payload. For common alignments this is just cell + 8.
        let after_tag = unsafe { cell.add(size_of::<usize>()) };
        let payload = if align_of::<T>() <= align_of::<usize>() {
            // Chunk bases are word-aligned, and every published next cursor is
            // an aligned payload plus a word-padded size. Reset/reuse preserve
            // that invariant, so ordinary roots need no alignment rounding.
            after_tag.cast::<T>()
        } else {
            after_tag
                .map_addr(|address| address.next_multiple_of(align_of::<T>()))
                .cast::<T>()
        };
        unsafe {
            cell.cast::<usize>().write(kind);
            payload.write(value);
        }
        self.next
            .set(unsafe { payload.cast::<u8>().add(payload_bytes) });
        unsafe { NonNull::new_unchecked(payload) }
    }

    fn chunk_layout(&self, index: usize) -> Layout {
        // Small scopes retain only their initial chunk; larger frames reach
        // the previous 256-slot size after four growth steps. Layout validation
        // also handles descriptors near the platform's maximum allocation.
        self.layout
            .size()
            .checked_mul(1usize << index.min(4))
            .and_then(|bytes| Layout::from_size_align(bytes, self.layout.align()).ok())
            .unwrap_or(self.layout)
    }

    #[cold]
    #[inline(never)]
    fn next_chunk(&self) -> *mut u8 {
        let mut chunks = self.chunks.borrow_mut();
        let index = if self.next.get().is_null() {
            0
        } else {
            let previous = &mut chunks[self.active_chunk.get()];
            // Unlike fixed slots, the next payload may not fit the trailing
            // space. Preserve the exact initialized prefix before refilling.
            previous.used = self.next.get().addr() - previous.data.as_ptr().addr();
            self.active_chunk.get() + 1
        };
        if index == chunks.len() {
            chunks.push(Chunk::new(self.chunk_layout(index)));
        }
        let chunk = &chunks[index];
        let cell = chunk.data.as_ptr();
        // Reused chunks retain their own layouts. Cursor bounds must describe
        // the actual allocation rather than the initial small-chunk geometry.
        let end = unsafe { cell.add(chunk.layout.size()) };
        if index == 0 {
            self.first.set(cell);
            self.first_end.set(end);
        }
        self.active_chunk.set(index);
        self.end.set(end);
        cell
    }

    #[inline]
    fn reset(&self) {
        self.active_chunk.set(0);
        self.next.set(self.first.get());
        self.end.set(self.first_end.get());
    }
}

struct ErasedVector {
    pointer: NonNull<()>,
    trace: unsafe fn(NonNull<()>, &RootDescriptor, &mut Tracer<'_>),
    drop: unsafe fn(NonNull<()>),
    kind: usize,
}

impl Drop for ErasedVector {
    fn drop(&mut self) {
        unsafe { (self.drop)(self.pointer) };
    }
}

struct Frame {
    // Next deeper reusable frame. The registry's Rc vector owns every target;
    // links are written only when a new frame is appended, never invalidated.
    next: Cell<Option<NonNull<Frame>>>,
    storage: Storage,
    vectors: RefCell<Vec<ErasedVector>>,
    has_vectors: Cell<bool>,
}

impl Frame {
    // Most scopes only contain bump-allocated slots. Keep vector destruction
    // and its unwind paths out of their scope-exit code.
    #[cold]
    #[inline(never)]
    fn clear_vectors(&self) {
        self.has_vectors.set(false);
        self.vectors.borrow_mut().clear();
    }
}

#[derive(Default)]
struct Frames {
    // Shared allocations keep frame addresses and reference provenance stable
    // when deeper scopes move or grow this vector.
    storage: Vec<Rc<Frame>>,
}

/// Reusable root frames belonging to a single generatively branded heap.
///
/// Slot chunks remain allocated until the registry is dropped. Entering a
/// previously used frame reuses its chunks, without allocating per root.
pub struct RootRegistry<'heap> {
    descriptors: &'static [RootDescriptor],
    chunk_layout: Layout,
    frames: RefCell<Frames>,
    depth: Cell<usize>,
    // Exactly frames[depth] when that frame exists, otherwise None. This cache
    // never owns a frame: frames.storage retains all Rc owners until teardown.
    next_frame: Cell<Option<NonNull<Frame>>>,
    persistent: RefCell<Vec<Weak<dyn TracePersistent + 'heap>>>,
    persistent_free: Rc<RefCell<Vec<usize>>>,
    heap: Brand<'heap>,
    single_thread: PhantomData<Rc<()>>,
}

/// Creates an independent heap brand for a raw registry.
pub fn with_registry<R>(
    descriptors: &'static [RootDescriptor],
    callback: impl for<'heap> FnOnce(&RootRegistry<'heap>) -> R,
) -> R {
    // The higher-ranked callback introduces a fresh invariant heap lifetime.
    callback(&unsafe { RootRegistry::new(descriptors) })
}

impl<'heap> RootRegistry<'heap> {
    /// Backing bytes reserved for scoped root slots, including inactive frames
    /// retained for reuse. Excludes vector roots, persistent lease allocations,
    /// registry bookkeeping and allocator overhead.
    pub fn reserved_slot_bytes(&self) -> usize {
        self.frames
            .borrow()
            .storage
            .iter()
            .map(|frame| {
                frame
                    .storage
                    .chunks
                    .borrow()
                    .iter()
                    .map(|chunk| chunk.layout.size())
                    .sum::<usize>()
            })
            .sum()
    }

    /// Creates a registry under a brand supplied by an owning heap wrapper.
    ///
    /// # Safety
    /// The owner must ensure `'heap` uniquely brands this heap. Descriptors
    /// must satisfy their documented tracing contracts for every stored value.
    pub unsafe fn new(descriptors: &'static [RootDescriptor]) -> Self {
        let mut size = 1;
        let mut align = 1;
        for descriptor in descriptors {
            Layout::from_size_align(descriptor.size.max(1), descriptor.align)
                .expect("valid root descriptor layout");
            size = size.max(descriptor.size);
            align = align.max(descriptor.align);
        }
        let payload = Layout::from_size_align(size, align).expect("valid maximum root layout");
        let (slot_layout, _) = Layout::new::<usize>()
            .extend(payload)
            .expect("root slot layout is representable");
        let slot_layout = slot_layout.pad_to_align();
        let chunk_layout = Layout::from_size_align(
            slot_layout
                .size()
                .checked_mul(MIN_SLOTS_PER_CHUNK)
                .expect("root chunk layout is representable"),
            slot_layout.align(),
        )
        .expect("root chunk layout is representable");
        Self {
            descriptors,
            chunk_layout,
            frames: RefCell::new(Frames::default()),
            depth: Cell::new(0),
            next_frame: Cell::new(None),
            persistent: RefCell::new(Vec::new()),
            persistent_free: Rc::new(RefCell::new(Vec::new())),
            heap: PhantomData,
            single_thread: PhantomData,
        }
    }

    /// Runs a scope, restoring its initialized extents even when it unwinds.
    pub fn with_roots<R>(&self, callback: impl for<'scope> FnOnce(Roots<'scope, 'heap>) -> R) -> R {
        let index = self.depth.get();
        let pointer = match self.next_frame.get() {
            Some(frame) => frame,
            None => self.grow_frames(index),
        };
        // SAFETY: Cached links point into Rc allocations owned by frames.storage,
        // which never removes or replaces entries. Vec growth cannot move them.
        // The generative callback cannot retain this frame beyond its guard.
        let frame = unsafe { pointer.as_ref() };
        self.next_frame.set(frame.next.get());
        self.depth.set(index + 1);
        let _guard = FrameGuard {
            registry: self,
            index,
            frame,
        };
        callback(Roots {
            registry: self,
            frame,
            scope: PhantomData,
        })
    }

    // Only a new maximum nesting depth borrows/grows the owning vector. Publish
    // links after push succeeds, so allocation failure leaves the cache intact.
    #[cold]
    #[inline(never)]
    fn grow_frames(&self, index: usize) -> NonNull<Frame> {
        let mut frames = self.frames.borrow_mut();
        assert_eq!(
            index,
            frames.storage.len(),
            "root frame cache follows depth"
        );
        let frame = Rc::new(Frame {
            next: Cell::new(None),
            storage: Storage::new(self.chunk_layout),
            vectors: RefCell::new(Vec::new()),
            has_vectors: Cell::new(false),
        });
        let pointer = NonNull::from(frame.as_ref());
        frames.storage.push(frame);
        if index != 0 {
            frames.storage[index - 1].next.set(Some(pointer));
        }
        pointer
    }

    #[inline]
    fn validate<K: RootKind>(&self) {
        // The unsafe RootKind contract guarantees this correspondence. These
        // are diagnostics for unsafe implementations, not a runtime type check.
        #[cfg(debug_assertions)]
        {
            let descriptor = &self.descriptors[K::KIND];
            assert_eq!(descriptor.size, size_of::<K::Stored>(), "root payload size");
            assert_eq!(
                descriptor.align,
                align_of::<K::Stored>(),
                "root payload alignment"
            );
        }
    }

    /// Registers a root that lives until its last owning lease is dropped.
    /// Cloning the lease aliases the same cell; it does not copy its value.
    /// Expired registration entries are reused by later calls.
    ///
    /// # Safety
    /// `value` must be current, belong to this heap, and match its descriptor.
    /// The owning heap must outlive every use of the returned lease.
    pub unsafe fn persist<K: RootKind>(&self, value: K::Stored) -> Persistent<'heap, K>
    where
        K::Stored: 'heap,
    {
        self.validate::<K>();
        let mut entries = self.persistent.borrow_mut();
        let index = self
            .persistent_free
            .borrow_mut()
            .pop()
            .unwrap_or(entries.len());
        let cell = Rc::new(PersistentCell {
            value: UnsafeCell::new(value),
            trace: self.descriptors[K::KIND].trace,
            index,
            free: self.persistent_free.clone(),
        });
        let erased: Rc<dyn TracePersistent + 'heap> = cell.clone();
        let weak = Rc::downgrade(&erased);
        if index < entries.len() {
            entries[index] = weak;
        } else {
            entries.push(weak);
        }
        Persistent {
            cell,
            heap: PhantomData,
            marker: PhantomData,
        }
    }

    /// Visits initialized active-frame cells, vector elements, and live
    /// persistent leases. Expired persistent registrations are ignored.
    ///
    /// # Safety
    /// The tracer must belong to this registry's heap. No payload references
    /// or snapshots may be used across tracing. Descriptor callbacks must not
    /// reenter this registry, grow roots, or retain references to root storage.
    pub unsafe fn trace_roots(&self, tracer: &mut Tracer<'_>) {
        let frames = self.frames.borrow();
        for frame in &frames.storage[..self.depth.get()] {
            let storage = &frame.storage;
            let chunks = storage.chunks.borrow();
            if !chunks.is_empty() {
                let active = storage.active_chunk.get();
                for (index, chunk) in chunks[..=active].iter().enumerate() {
                    let mut slot = chunk.data.as_ptr();
                    let end = if index == active {
                        storage.next.get()
                    } else {
                        unsafe { slot.add(chunk.used) }
                    };
                    while slot != end {
                        // Published slots have both an exact descriptor tag and
                        // initialized payload. Cursor and end share this chunk.
                        unsafe {
                            let kind = slot.cast::<usize>().read();
                            let descriptor = &self.descriptors[kind];
                            let payload = slot
                                .add(size_of::<usize>())
                                .map_addr(|address| address.next_multiple_of(descriptor.align));
                            (descriptor.trace)(NonNull::new_unchecked(payload), tracer);
                            slot = payload
                                .add(descriptor.size.max(1).next_multiple_of(size_of::<usize>()));
                        }
                    }
                }
            }
            for vector in frame.vectors.borrow().iter() {
                unsafe { (vector.trace)(vector.pointer, &self.descriptors[vector.kind], tracer) };
            }
        }
        for entry in self.persistent.borrow().iter() {
            if let Some(root) = entry.upgrade() {
                unsafe { root.trace(tracer) };
            }
        }
    }
}

trait TracePersistent {
    unsafe fn trace(&self, tracer: &mut Tracer<'_>);
}

struct PersistentCell<T: Copy> {
    value: UnsafeCell<T>,
    trace: unsafe fn(NonNull<u8>, &mut Tracer<'_>),
    index: usize,
    free: Rc<RefCell<Vec<usize>>>,
}

impl<T: Copy> Drop for PersistentCell<T> {
    fn drop(&mut self) {
        // Independent of the registration table: tracing may hold its borrow
        // while dropping a temporary strong reference. Last-owner release and
        // registering another root both take constant time.
        self.free.borrow_mut().push(self.index);
    }
}

impl<T: Copy> TracePersistent for PersistentCell<T> {
    unsafe fn trace(&self, tracer: &mut Tracer<'_>) {
        let pointer = NonNull::from(&self.value).cast();
        unsafe { (self.trace)(pointer, tracer) };
    }
}

/// An owning persistent root lease, independent of temporary frames.
/// Only live leases keep their payload reachable. `Clone` aliases this cell.
pub struct Persistent<'heap, K: RootKind> {
    cell: Rc<PersistentCell<K::Stored>>,
    heap: Brand<'heap>,
    marker: PhantomData<K>,
}

impl<K: RootKind> Clone for Persistent<'_, K> {
    fn clone(&self) -> Self {
        Self {
            cell: self.cell.clone(),
            heap: PhantomData,
            marker: PhantomData,
        }
    }
}

impl<'heap, K: RootKind> Persistent<'heap, K> {
    /// Borrows this lease as a scoped alias; it cannot outlive this borrow.
    #[inline]
    pub fn as_root(&self) -> RawRoot<'_, 'heap, K> {
        RawSlot {
            cell: NonNull::from(&self.cell.value).cast(),
            scope: PhantomData,
            heap: PhantomData,
            marker: PhantomData,
        }
        .root()
    }

    /// Deliberately still `unsafe`, unlike [`RawRoot::read`]: callers select
    /// between this lease and [`shared::Persistent`] by configuration through
    /// the same call sites, and the shared variant's read can race with a
    /// write from another thread.
    ///
    /// # Safety
    /// The returned raw payload must not be used across collection.
    pub unsafe fn read(&self) -> K::Stored {
        unsafe { self.cell.value.get().read() }
    }

    /// # Safety
    /// `value` must be current and belong to this heap. References to this
    /// cell's previous contents must not exist during this operation.
    pub unsafe fn write(&self, value: K::Stored) {
        unsafe { self.cell.value.get().write(value) };
    }

    /// Deliberately still `unsafe`; see [`Persistent::read`].
    ///
    /// # Safety
    /// The snapshot must not be used across collection.
    pub unsafe fn snapshot(&self) -> RawSnapshot<'heap, K> {
        RawSnapshot {
            value: unsafe { self.read() },
            heap: PhantomData,
            marker: PhantomData,
        }
    }
}

struct FrameGuard<'registry, 'heap> {
    registry: &'registry RootRegistry<'heap>,
    index: usize,
    frame: &'registry Frame,
}

impl Drop for FrameGuard<'_, '_> {
    #[inline]
    fn drop(&mut self) {
        assert_eq!(
            self.registry.depth.get(),
            self.index + 1,
            "root scopes unwind in stack order"
        );
        self.frame.storage.reset();
        if self.frame.has_vectors.get() {
            self.frame.clear_vectors();
        }
        // Restore the cache only after cleanup, including vector destruction.
        // The frame remains Rc-owned even while inactive and across Vec growth.
        self.registry
            .next_frame
            .set(Some(NonNull::from(self.frame)));
        self.registry.depth.set(self.index);
    }
}

/// Allocation capability for one active root frame.
#[derive(Clone, Copy)]
pub struct Roots<'scope, 'heap> {
    registry: &'scope RootRegistry<'heap>,
    frame: &'scope Frame,
    scope: Brand<'scope>,
}

impl<'scope, 'heap> Roots<'scope, 'heap> {
    /// Creates a rooted, initialized payload.
    ///
    /// # Safety
    /// `value` must contain valid raw values of this heap, matching its
    /// descriptor. No collection may have invalidated a copied input value.
    #[inline]
    pub unsafe fn push<K: RootKind>(self, value: K::Stored) -> RawRoot<'scope, 'heap, K> {
        unsafe { self.slot::<K>(value) }.root()
    }

    /// Creates an initialized slot that can be overwritten and reused.
    ///
    /// # Safety
    /// The input must meet [`Roots::push`]'s requirements.
    #[inline]
    pub unsafe fn slot<K: RootKind>(self, value: K::Stored) -> RawSlot<'scope, 'heap, K> {
        self.registry.validate::<K>();
        let cell = self.frame.storage.push(K::KIND, value);
        RawSlot {
            cell,
            scope: PhantomData,
            heap: PhantomData,
            marker: PhantomData,
        }
    }

    /// Copies a raw snapshot into this frame, permitting promotion outward.
    ///
    /// # Safety
    /// No collection may have occurred since the snapshot was taken.
    #[inline]
    pub unsafe fn copy<K: RootKind>(
        self,
        snapshot: RawSnapshot<'heap, K>,
    ) -> RawRoot<'scope, 'heap, K> {
        unsafe { self.push::<K>(snapshot.value) }
    }

    /// Enters a child frame while this frame remains available for promotion.
    pub fn with_roots<R>(self, callback: impl for<'inner> FnOnce(Roots<'inner, 'heap>) -> R) -> R {
        self.registry.with_roots(callback)
    }

    /// Registers an empty vector whose control block stays fixed as it grows.
    pub fn vector<K: RootKind>(self) -> RootVec<'scope, 'heap, K> {
        self.registry.validate::<K>();
        let pointer = NonNull::from(Box::leak(Box::new(
            UnsafeCell::new(Vec::<K::Stored>::new()),
        )));
        let erased = ErasedVector {
            pointer: pointer.cast(),
            trace: trace_vector::<K::Stored>,
            drop: drop_vector::<K::Stored>,
            kind: K::KIND,
        };
        self.frame.vectors.borrow_mut().push(erased);
        self.frame.has_vectors.set(true);
        RootVec {
            pointer,
            scope: PhantomData,
            heap: PhantomData,
            marker: PhantomData,
        }
    }
}

/// A copyable alias of an initialized root cell.
pub struct RawRoot<'scope, 'heap, K: RootKind> {
    slot: RawSlot<'scope, 'heap, K>,
}

/// A stable root cell. Copies alias the same updateable value.
pub struct RawSlot<'scope, 'heap, K: RootKind> {
    cell: NonNull<K::Stored>,
    scope: Brand<'scope>,
    heap: Brand<'heap>,
    marker: PhantomData<(K, Rc<()>)>,
}

/// A raw payload copied from a root, invalidated by the next collection.
pub struct RawSnapshot<'heap, K: RootKind> {
    value: K::Stored,
    heap: Brand<'heap>,
    marker: PhantomData<(K, Rc<()>)>,
}

macro_rules! copy_handle {
    ($name:ident <$($lifetime:lifetime),+>) => {
        impl<K: RootKind> Copy for $name<$($lifetime),+, K> {}
        impl<K: RootKind> Clone for $name<$($lifetime),+, K> {
            fn clone(&self) -> Self { *self }
        }
    };
}
copy_handle!(RawRoot<'_, '_>);
copy_handle!(RawSlot<'_, '_>);
copy_handle!(RawSnapshot<'_>);

impl<'scope, 'heap, K: RootKind> RawSlot<'scope, 'heap, K> {
    #[inline]
    pub fn root(self) -> RawRoot<'scope, 'heap, K> {
        RawRoot { slot: self }
    }

    /// Copies the raw payload out of this slot.
    ///
    /// The returned raw value is only current until the next collection.
    /// Reading it is safe; using it (dereferencing it, or storing it back
    /// into a root) is where the unsafe obligation lies. Dereferences require
    /// an owning heap borrow that excludes collection and mutation.
    #[inline]
    pub fn read(self) -> K::Stored {
        // SAFETY: the scope and heap brands keep this handle from outliving
        // its initialized cell. Handles and their registry are `!Send`, and no
        // reference into the cell is ever handed out, so safe code cannot
        // access it concurrently; every writer (`write`, tracing, and a
        // shared lease's `write` on another thread) is an unsafe operation
        // whose contract excludes this read. The payload is `Copy`, so this
        // is a plain bitwise copy.
        unsafe { self.cell.as_ptr().read() }
    }

    /// # Safety
    /// The value must be current, belong to this heap, and match its descriptor.
    /// There must be no outstanding references to this cell's previous contents.
    pub unsafe fn write(self, value: K::Stored) {
        unsafe { self.cell.as_ptr().write(value) };
    }

    /// Copies the raw payload into a heap-branded snapshot.
    ///
    /// Like [`RawSlot::read`], the snapshot is only current until the next
    /// collection. Taking it is safe; rooting it again with [`Roots::copy`]
    /// is where the unsafe obligation lies.
    #[inline]
    pub fn snapshot(self) -> RawSnapshot<'heap, K> {
        RawSnapshot {
            value: self.read(),
            heap: PhantomData,
            marker: PhantomData,
        }
    }
}

impl<'scope, 'heap, K: RootKind> RawRoot<'scope, 'heap, K> {
    #[inline]
    pub fn slot(self) -> RawSlot<'scope, 'heap, K> {
        self.slot
    }

    /// Copies the raw payload out of this root; see [`RawSlot::read`]. The
    /// value is only current until the next collection, and using it is where
    /// the unsafe obligation lies.
    #[inline]
    pub fn read(self) -> K::Stored {
        self.slot.read()
    }

    /// Copies the raw payload into a snapshot; see [`RawSlot::snapshot`]. It
    /// is only current until the next collection, and using it is where the
    /// unsafe obligation lies.
    #[inline]
    pub fn snapshot(self) -> RawSnapshot<'heap, K> {
        self.slot.snapshot()
    }
}

/// A registered vector. Its control block, rather than pointers into its
/// reallocating backing buffer, is registered with the owning frame.
pub struct RootVec<'scope, 'heap, K: RootKind> {
    pointer: NonNull<UnsafeCell<Vec<K::Stored>>>,
    scope: Brand<'scope>,
    heap: Brand<'heap>,
    marker: PhantomData<(K, Rc<()>)>,
}
copy_handle!(RootVec<'_, '_>);

impl<K: RootKind> RootVec<'_, '_, K> {
    pub fn len(self) -> usize {
        unsafe { (&*self.pointer.as_ref().get()).len() }
    }
    pub fn is_empty(self) -> bool {
        self.len() == 0
    }

    /// # Safety
    /// The value must be current and belong to this heap. No reference into
    /// the vector may exist during this operation or during tracing.
    pub unsafe fn push(self, value: K::Stored) {
        unsafe { (&mut *self.pointer.as_ref().get()).push(value) };
    }

    /// Copies the raw element at `index`, if any.
    ///
    /// A returned raw value is only current until the next collection.
    /// Reading it is safe; using it (dereferencing it, or storing it back
    /// into a root) is where the unsafe obligation lies.
    pub fn get(self, index: usize) -> Option<K::Stored> {
        // SAFETY: the scope brand keeps this handle from outliving the frame
        // that owns the vector's control block. Handles are `!Send`, and no
        // reference into the vector outlives any single method call, so this
        // shared borrow is unaliased. The element is `Copy`.
        unsafe { (&*self.pointer.as_ref().get()).get(index).copied() }
    }

    /// # Safety
    /// The input must meet [`RootVec::push`]'s requirements.
    pub unsafe fn set(self, index: usize, value: K::Stored) {
        unsafe { (&mut *self.pointer.as_ref().get())[index] = value };
    }

    /// Removes and returns the last raw element, if any.
    ///
    /// A returned raw value is only current until the next collection and is
    /// no longer protected by this vector after removal. Popping it is safe;
    /// using it (dereferencing it, or storing it back into a root) is where
    /// the unsafe obligation lies.
    pub fn pop(self) -> Option<K::Stored> {
        // SAFETY: the scope brand keeps this handle from outliving the frame
        // that owns the vector's control block. Handles are `!Send`, and no
        // reference into the vector outlives any single method call, so this
        // exclusive borrow is unaliased. The element is `Copy`.
        unsafe { (&mut *self.pointer.as_ref().get()).pop() }
    }

    pub fn truncate(self, len: usize) {
        unsafe { (&mut *self.pointer.as_ref().get()).truncate(len) };
    }

    /// Removes every element while retaining the vector's backing capacity.
    pub fn clear(self) {
        self.truncate(0);
    }
}

unsafe fn trace_vector<T: Copy>(
    pointer: NonNull<()>,
    descriptor: &RootDescriptor,
    tracer: &mut Tracer<'_>,
) {
    let vector = unsafe { &mut *pointer.cast::<UnsafeCell<Vec<T>>>().as_ref().get() };
    for element in vector {
        unsafe { (descriptor.trace)(NonNull::from(element).cast(), tracer) };
    }
}

unsafe fn drop_vector<T: Copy>(pointer: NonNull<()>) {
    drop(unsafe { Box::from_raw(pointer.cast::<UnsafeCell<Vec<T>>>().as_ptr()) });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::raw::{Descriptor, RawHeap};
    use crate::Config;
    use std::panic::{catch_unwind, AssertUnwindSafe};

    enum PointerKind {}
    unsafe impl RootKind for PointerKind {
        type Stored = NonNull<u8>;
        const KIND: usize = 0;
    }

    #[derive(Clone, Copy)]
    struct Pair {
        pointer: NonNull<u8>,
        scalar: usize,
    }
    enum PairKind {}
    unsafe impl RootKind for PairKind {
        type Stored = Pair;
        const KIND: usize = 1;
    }

    #[repr(align(64))]
    #[derive(Clone, Copy)]
    struct Aligned(usize);
    enum AlignedKind {}
    unsafe impl RootKind for AlignedKind {
        type Stored = Aligned;
        const KIND: usize = 2;
    }

    unsafe fn pointer_trace(pointer: NonNull<u8>, tracer: &mut Tracer<'_>) {
        unsafe { tracer.visit(&mut *pointer.cast::<NonNull<u8>>().as_ptr()) };
    }
    unsafe fn pair_trace(pointer: NonNull<u8>, tracer: &mut Tracer<'_>) {
        unsafe { tracer.visit(&mut (*pointer.cast::<Pair>().as_ptr()).pointer) };
    }
    unsafe fn no_roots(_: NonNull<u8>, _: &mut Tracer<'_>) {}
    static ROOT_DESCRIPTORS: [RootDescriptor; 3] = [
        RootDescriptor {
            size: size_of::<NonNull<u8>>(),
            align: align_of::<NonNull<u8>>(),
            trace: pointer_trace,
        },
        RootDescriptor {
            size: size_of::<Pair>(),
            align: align_of::<Pair>(),
            trace: pair_trace,
        },
        RootDescriptor {
            size: size_of::<Aligned>(),
            align: align_of::<Aligned>(),
            trace: no_roots,
        },
    ];
    unsafe fn leaf_trace(_: NonNull<u8>, _: usize, _: &mut Tracer<'_>) {}
    static HEAP_DESCRIPTORS: [Descriptor; 1] = [Descriptor {
        name: "integer",
        fixed_size: Some(size_of::<usize>()),
        alignment: align_of::<usize>(),
        trace: leaf_trace,
        drop: None,
    }];

    fn heap() -> RawHeap {
        RawHeap::new(
            Config {
                workers: 1,
                chunk_bytes: 128,
                steady_threshold_bytes: None,
                trace_collections: false,
                threshold_bytes: 128,
                stress: false,
            },
            &HEAP_DESCRIPTORS,
        )
        .unwrap()
    }

    fn allocate(heap: &mut RawHeap, value: usize) -> NonNull<u8> {
        let pointer = unsafe { heap.allocate_slow(0, size_of::<usize>()).unwrap() };
        unsafe { pointer.cast::<usize>().as_ptr().write(value) };
        pointer
    }

    unsafe fn value(pointer: NonNull<u8>) -> usize {
        unsafe { pointer.cast::<usize>().as_ptr().read() }
    }

    #[test]
    fn frame_reuse_resets_root_kinds_beyond_a_machine_word() {
        struct HighKind;
        // SAFETY: every entry in HIGH_DESCRIPTORS describes this exact pointer.
        unsafe impl RootKind for HighKind {
            type Stored = NonNull<u8>;
            const KIND: usize = usize::BITS as usize;
        }
        static HIGH_DESCRIPTORS: [RootDescriptor; usize::BITS as usize + 1] = [RootDescriptor {
            size: size_of::<NonNull<u8>>(),
            align: align_of::<NonNull<u8>>(),
            trace: pointer_trace,
        };
            usize::BITS as usize + 1];
        with_registry(&HIGH_DESCRIPTORS, |registry| {
            let mut heap = heap();
            registry.with_roots(|roots| {
                let root = unsafe { roots.push::<HighKind>(allocate(&mut heap, 42)) };
                unsafe {
                    heap.collect(|tracer| registry.trace_roots(tracer), |_| {})
                        .unwrap();
                }
                assert_eq!(unsafe { value(root.read()) }, 42);
            });
            registry.with_roots(|_| unsafe {
                heap.collect(|tracer| registry.trace_roots(tracer), |_| {})
                    .unwrap();
            });
            assert_eq!(
                heap.used_bytes(),
                0,
                "reusing the frame must not resurrect old roots"
            );
        });
    }

    #[test]
    fn mixed_slots_preserve_zero_sized_alignment_and_ignore_retired_chunks() {
        #[repr(align(64))]
        #[derive(Clone, Copy)]
        struct Marker;
        enum MarkerKind {}
        unsafe impl RootKind for MarkerKind {
            type Stored = Marker;
            const KIND: usize = 1;
        }
        static DESCRIPTORS: [RootDescriptor; 2] = [
            RootDescriptor {
                size: size_of::<NonNull<u8>>(),
                align: align_of::<NonNull<u8>>(),
                trace: pointer_trace,
            },
            RootDescriptor {
                size: 0,
                align: 64,
                trace: no_roots,
            },
        ];
        with_registry(&DESCRIPTORS, |registry| {
            let mut heap = heap();
            registry.with_roots(|roots| {
                let pointer = unsafe { roots.push::<PointerKind>(allocate(&mut heap, 42)) };
                for _ in 0..(MIN_SLOTS_PER_CHUNK * 3) {
                    let marker = unsafe { roots.push::<MarkerKind>(Marker) };
                    assert_eq!(marker.slot.cell.as_ptr() as usize % 64, 0);
                }
                unsafe {
                    heap.collect(|tracer| registry.trace_roots(tracer), |_| {})
                        .unwrap();
                    assert_eq!(value(pointer.read()), 42);
                }
            });
            registry.with_roots(|roots| unsafe {
                roots.push::<MarkerKind>(Marker);
                heap.collect(|tracer| registry.trace_roots(tracer), |_| {})
                    .unwrap();
            });
            assert_eq!(
                heap.used_bytes(),
                0,
                "cached old slots must not retain roots"
            );
        });
    }

    #[test]
    fn nested_frames_outer_growth_promotion_and_aliases_survive_moves() {
        with_registry(&ROOT_DESCRIPTORS, |registry| {
            let mut heap = heap();
            registry.with_roots(|outer| {
                let first = unsafe { outer.push::<PointerKind>(allocate(&mut heap, 11)) };
                let alias = first;
                let promoted = outer.with_roots(|inner| {
                    let target = unsafe { inner.push::<PointerKind>(allocate(&mut heap, 22)) };
                    // Both frames grow beyond several chunks while old aliases exist.
                    let count = registry.chunk_layout.size() / (2 * size_of::<usize>()) * 15 + 1;
                    for number in 0..count {
                        unsafe {
                            inner.push::<PointerKind>(allocate(&mut heap, number));
                        }
                        unsafe {
                            outer.push::<AlignedKind>(Aligned(number));
                        }
                    }
                    assert!(inner.frame.storage.chunks.borrow().len() > 4);
                    assert!(outer.frame.storage.chunks.borrow().len() > 4);
                    unsafe {
                        heap.collect(|tracer| registry.trace_roots(tracer), |_| {})
                            .unwrap()
                    };
                    assert_eq!(unsafe { value(alias.read()) }, 11);
                    unsafe { outer.copy(target.snapshot()) }
                });
                unsafe {
                    heap.collect(|tracer| registry.trace_roots(tracer), |_| {})
                        .unwrap()
                };
                assert_eq!(heap.used_bytes(), 2 * size_of::<usize>());
                assert_eq!(unsafe { value(promoted.read()) }, 22);
                unsafe { first.slot().write(promoted.read()) };
                assert_eq!(alias.read(), promoted.read());
                unsafe {
                    heap.collect(|tracer| registry.trace_roots(tracer), |_| {})
                        .unwrap()
                };
                assert_eq!(heap.used_bytes(), size_of::<usize>());
            });
            unsafe {
                heap.collect(|tracer| registry.trace_roots(tracer), |_| {})
                    .unwrap()
            };
            assert_eq!(heap.used_bytes(), 0);
        });
    }

    #[test]
    fn unwinding_pops_only_inner_frame_and_reuses_chunks_without_stale_roots() {
        with_registry(&ROOT_DESCRIPTORS, |registry| {
            let mut heap = heap();
            registry.with_roots(|outer| {
                let live = unsafe { outer.push::<PointerKind>(allocate(&mut heap, 1)) };
                let mut old_cell = None;
                let failure = catch_unwind(AssertUnwindSafe(|| {
                    outer.with_roots(|inner| {
                        let doomed = unsafe { inner.push::<PointerKind>(allocate(&mut heap, 2)) };
                        old_cell = Some(doomed.slot.cell);
                        let vector = inner.vector::<PointerKind>();
                        unsafe { vector.push(allocate(&mut heap, 3)) };
                        panic!("test unwind");
                    })
                }));
                assert!(failure.is_err());
                assert_eq!(registry.depth.get(), 1);
                let promoted = outer.with_roots(|inner| {
                    let replacement = unsafe { inner.push::<PointerKind>(allocate(&mut heap, 4)) };
                    assert_eq!(Some(replacement.slot.cell), old_cell);
                    unsafe {
                        heap.collect(|tracer| registry.trace_roots(tracer), |_| {})
                            .unwrap()
                    };
                    assert_eq!(heap.used_bytes(), 2 * size_of::<usize>());
                    unsafe { outer.copy(replacement.snapshot()) }
                });
                assert_eq!(unsafe { value(live.read()) }, 1);
                assert_eq!(unsafe { value(promoted.read()) }, 4);
            });
        });
    }

    #[test]
    fn vectors_follow_reallocated_backing_and_trace_only_initialized_elements() {
        with_registry(&ROOT_DESCRIPTORS, |registry| {
            let mut heap = heap();
            registry.with_roots(|roots| {
                let vector = roots.vector::<PointerKind>();
                let alias = vector;
                for number in 0..1025 {
                    unsafe { vector.push(allocate(&mut heap, number)) };
                    if number % 31 == 0 {
                        unsafe {
                            heap.collect(|tracer| registry.trace_roots(tracer), |_| {})
                                .unwrap()
                        };
                    }
                }
                assert_eq!(alias.len(), 1025);
                for number in 0..alias.len() {
                    assert_eq!(unsafe { value(alias.get(number).unwrap()) }, number);
                }
                vector.truncate(2);
                unsafe {
                    heap.collect(|tracer| registry.trace_roots(tracer), |_| {})
                        .unwrap()
                };
                assert_eq!(heap.used_bytes(), 2 * size_of::<usize>());
                unsafe { vector.set(0, vector.get(1).unwrap()) };
                vector.pop();
                unsafe {
                    heap.collect(|tracer| registry.trace_roots(tracer), |_| {})
                        .unwrap()
                };
                assert_eq!(heap.used_bytes(), size_of::<usize>());
                assert_eq!(unsafe { value(alias.get(0).unwrap()) }, 1);
                vector.truncate(0);
                unsafe {
                    heap.collect(|tracer| registry.trace_roots(tracer), |_| {})
                        .unwrap()
                };
                assert_eq!(heap.used_bytes(), 0);
            });
        });
    }

    #[test]
    fn descriptors_are_precise_and_cells_obey_payload_alignment() {
        with_registry(&ROOT_DESCRIPTORS, |registry| {
            let mut heap = heap();
            registry.with_roots(|roots| {
                let unrooted = allocate(&mut heap, 9);
                let pair = unsafe {
                    roots.push::<PairKind>(Pair {
                        pointer: allocate(&mut heap, 7),
                        scalar: unrooted.as_ptr() as usize,
                    })
                };
                for number in 0..600 {
                    let aligned = unsafe { roots.push::<AlignedKind>(Aligned(number)) };
                    assert_eq!(aligned.slot.cell.as_ptr() as usize % 64, 0);
                    assert_eq!(aligned.read().0, number);
                }
                unsafe {
                    heap.collect(
                        |tracer| registry.trace_roots(tracer),
                        |relocation| {
                            assert!(relocation.lookup(unrooted).is_none());
                        },
                    )
                    .unwrap();
                }
                let snapshot = pair.read();
                assert_eq!(snapshot.scalar, unrooted.as_ptr() as usize);
                assert_eq!(unsafe { value(snapshot.pointer) }, 7);
                assert_eq!(heap.used_bytes(), size_of::<usize>());
            });
        });
    }

    #[test]
    fn outer_slots_stay_stable_when_their_kind_grows_inside_an_inner_frame() {
        with_registry(&ROOT_DESCRIPTORS, |registry| {
            registry.with_roots(|outer| {
                let first = unsafe { outer.push::<AlignedKind>(Aligned(17)) };
                let address = first.slot.cell;
                outer.with_roots(|inner| {
                    let inner_first = unsafe { inner.push::<AlignedKind>(Aligned(29)) };
                    for number in 0..1500 {
                        unsafe {
                            outer.push::<AlignedKind>(Aligned(number));
                        }
                        unsafe {
                            inner.push::<AlignedKind>(Aligned(number));
                        }
                    }
                    assert_eq!(first.slot.cell, address);
                    assert_eq!(first.read().0, 17);
                    assert_eq!(inner_first.read().0, 29);
                });
                assert_eq!(first.read().0, 17);
            })
        });
    }

    #[test]
    fn cached_next_frame_tracks_active_depth_when_entering_through_outer_roots() {
        fn check(registry: &RootRegistry<'_>) {
            let frames = registry.frames.borrow();
            let expected = frames
                .storage
                .get(registry.depth.get())
                .map(|frame| NonNull::from(frame.as_ref()));
            assert_eq!(registry.next_frame.get(), expected);
            for (index, frame) in frames.storage.iter().enumerate() {
                let expected = frames
                    .storage
                    .get(index + 1)
                    .map(|frame| NonNull::from(frame.as_ref()));
                assert_eq!(frame.next.get(), expected);
            }
        }
        with_registry(&ROOT_DESCRIPTORS, |registry| {
            let mut heap = heap();
            check(registry);
            for _ in 0..3 {
                registry.with_roots(|outer| {
                    let first = unsafe { outer.push::<PointerKind>(allocate(&mut heap, 1)) };
                    check(registry);
                    outer.with_roots(|middle| {
                        let second = unsafe { middle.push::<PointerKind>(allocate(&mut heap, 2)) };
                        check(registry);
                        let mut third_frame = None;
                        let failure = catch_unwind(AssertUnwindSafe(|| {
                            // An outer capability still enters after the currently
                            // deepest frame, not after its own original frame.
                            outer.with_roots(|inner| {
                                third_frame = Some(NonNull::from(inner.frame));
                                assert_ne!(inner.frame as *const _, middle.frame as *const _);
                                assert_eq!(registry.depth.get(), 3);
                                check(registry);
                                let vector = inner.vector::<PointerKind>();
                                unsafe { vector.push(allocate(&mut heap, 3)) };
                                panic!("unwind nested frame entered through outer roots");
                            });
                        }));
                        assert!(failure.is_err());
                        assert_eq!(registry.depth.get(), 2);
                        check(registry);
                        middle.with_roots(|inner| {
                            assert_eq!(Some(NonNull::from(inner.frame)), third_frame);
                            assert!(!inner.frame.has_vectors.get());
                            let third =
                                unsafe { inner.push::<PointerKind>(allocate(&mut heap, 4)) };
                            check(registry);
                            unsafe {
                                heap.collect(|tracer| registry.trace_roots(tracer), |_| {})
                                    .unwrap();
                            }
                            assert_eq!(heap.used_bytes(), 3 * size_of::<usize>());
                            assert_eq!(unsafe { value(first.read()) }, 1);
                            assert_eq!(unsafe { value(second.read()) }, 2);
                            assert_eq!(unsafe { value(third.read()) }, 4);
                        });
                        check(registry);
                    });
                    check(registry);
                });
                assert_eq!(registry.depth.get(), 0);
                assert_eq!(registry.frames.borrow().storage.len(), 3);
                check(registry);
                unsafe {
                    heap.collect(|tracer| registry.trace_roots(tracer), |_| {})
                        .unwrap();
                }
                assert_eq!(heap.used_bytes(), 0);
            }
        });
    }

    #[test]
    fn cached_frames_remain_valid_across_deep_registry_growth() {
        fn descend<'h>(registry: &RootRegistry<'h>, heap: &mut RawHeap, depth: usize) {
            registry.with_roots(|roots| {
                let root = unsafe { roots.push::<PointerKind>(allocate(heap, depth)) };
                if depth == 128 {
                    unsafe {
                        heap.collect(|tracer| registry.trace_roots(tracer), |_| {})
                            .unwrap()
                    };
                    assert_eq!(heap.used_bytes(), 129 * size_of::<usize>());
                } else {
                    descend(registry, heap, depth + 1);
                }
                assert_eq!(unsafe { value(root.read()) }, depth);
                // Promote into a previously cached frame after vector growth.
                let copy = unsafe { roots.copy(root.snapshot()) };
                assert_eq!(unsafe { value(copy.read()) }, depth);
            });
        }
        with_registry(&ROOT_DESCRIPTORS, |registry| {
            let mut heap = heap();
            descend(registry, &mut heap, 0);
            descend(registry, &mut heap, 0);
            unsafe {
                heap.collect(|tracer| registry.trace_roots(tracer), |_| {})
                    .unwrap()
            };
            assert_eq!(heap.used_bytes(), 0);
        });
    }

    #[test]
    fn root_chunks_grow_to_cap_and_reuse_their_actual_layouts() {
        let layout = Layout::from_size_align(384, align_of::<usize>()).unwrap();
        let storage = Storage::new(layout);
        let slots_per_initial_chunk = layout.size() / (2 * size_of::<usize>());
        // Fill chunks sized 1+2+4+8+16 initial chunks, then enter the second
        // maximum-sized chunk. No slot handle may change when Vec grows.
        let count = slots_per_initial_chunk * 31 + 1;
        let original: Vec<_> = (0..count).map(|i| storage.push(0, i)).collect();
        {
            let chunks = storage.chunks.borrow();
            assert_eq!(chunks.len(), 6);
            for (chunk, factor) in chunks.iter().zip([1, 2, 4, 8, 16, 16]) {
                assert_eq!(chunk.layout.size(), layout.size() * factor);
                assert_eq!(chunk.layout.align(), layout.align());
            }
        }
        for (i, pointer) in original.iter().enumerate() {
            assert_eq!(unsafe { pointer.as_ref() }, &i);
        }
        storage.reset();
        assert_eq!(
            storage.end.get().addr() - storage.next.get().addr(),
            layout.size()
        );
        for (i, original) in original.iter().enumerate() {
            let reused = storage.push(0, i + 100);
            assert_eq!(reused, *original);
            assert_eq!(unsafe { reused.as_ref() }, &(i + 100));
        }
        assert_eq!(storage.chunks.borrow().len(), 6);
    }

    #[test]
    fn root_chunk_growth_falls_back_when_layout_cannot_grow() {
        let align = align_of::<usize>();
        let maximum = (isize::MAX as usize) & !(align - 1);
        for bytes in [maximum, maximum / 2 + 1] {
            let layout = Layout::from_size_align(bytes, align).unwrap();
            let storage = Storage::new(layout);
            for index in [1, 4, usize::MAX] {
                assert_eq!(storage.chunk_layout(index), layout);
            }
        }
    }

    #[test]
    fn reused_multichunk_frame_traces_only_its_new_initialized_prefix() {
        with_registry(&ROOT_DESCRIPTORS, |registry| {
            let mut heap = heap();
            let mut first = None;
            registry.with_roots(|roots| {
                let count = registry.chunk_layout.size() / (2 * size_of::<usize>()) * 7 + 1;
                for index in 0..count {
                    let root = unsafe { roots.push::<PointerKind>(allocate(&mut heap, index)) };
                    if index == 0 {
                        first = Some(root.slot.cell);
                    }
                }
                assert!(roots.frame.storage.chunks.borrow().len() > 3);
            });
            registry.with_roots(|roots| {
                let root = unsafe { roots.push::<PointerKind>(allocate(&mut heap, 42)) };
                assert_eq!(Some(root.slot.cell), first);
                unsafe {
                    heap.collect(|tracer| registry.trace_roots(tracer), |_| {})
                        .unwrap()
                };
                assert_eq!(heap.used_bytes(), size_of::<usize>());
                assert_eq!(unsafe { value(root.read()) }, 42);
            });
        });
    }

    #[test]
    fn persistent_registration_reuses_dropped_slots_among_live_leases() {
        with_registry(&ROOT_DESCRIPTORS, |registry| {
            let mut heap = heap();
            let mut leases = (0..128)
                .map(|number| {
                    Some(unsafe { registry.persist::<PointerKind>(allocate(&mut heap, number)) })
                })
                .collect::<Vec<_>>();
            let alias = leases[0].as_ref().unwrap().clone();
            for index in (0..128).step_by(2) {
                leases[index] = None;
            }
            // The alias still owns the first registration.
            assert_eq!(registry.persistent_free.borrow().len(), 63);
            let replacements = (0..63)
                .map(|number| unsafe {
                    registry.persist::<PointerKind>(allocate(&mut heap, 1000 + number))
                })
                .collect::<Vec<_>>();
            assert_eq!(registry.persistent.borrow().len(), 128);
            unsafe {
                heap.collect(|tracer| registry.trace_roots(tracer), |_| {})
                    .unwrap();
            }
            assert_eq!(unsafe { value(alias.read()) }, 0);
            for index in (1..128).step_by(2) {
                assert_eq!(
                    unsafe { value(leases[index].as_ref().unwrap().read()) },
                    index
                );
            }
            for (index, lease) in replacements.iter().enumerate() {
                assert_eq!(unsafe { value(lease.read()) }, 1000 + index);
            }
        });
    }

    #[test]
    fn persistent_clones_alias_and_last_drop_removes_reachability() {
        with_registry(&ROOT_DESCRIPTORS, |registry| {
            let mut heap = heap();
            let persistent = registry.with_roots(|roots| {
                let temporary = unsafe { roots.push::<PointerKind>(allocate(&mut heap, 17)) };
                unsafe { registry.persist::<PointerKind>(temporary.read()) }
            });
            let alias = persistent.clone();
            assert_eq!(registry.persistent.borrow().len(), 1);
            unsafe {
                heap.collect(|tracer| registry.trace_roots(tracer), |_| {})
                    .unwrap()
            };
            assert_eq!(unsafe { value(alias.read()) }, 17);
            assert_eq!(heap.used_bytes(), size_of::<usize>());
            unsafe { alias.write(allocate(&mut heap, 29)) };
            assert_eq!(unsafe { value(persistent.read()) }, 29);
            drop(persistent);
            unsafe {
                heap.collect(|tracer| registry.trace_roots(tracer), |_| {})
                    .unwrap()
            };
            assert_eq!(heap.used_bytes(), size_of::<usize>());
            assert_eq!(unsafe { value(alias.as_root().read()) }, 29);

            registry.with_roots(|roots| {
                let independent = unsafe { roots.copy(alias.snapshot()) };
                drop(alias);
                unsafe {
                    heap.collect(|tracer| registry.trace_roots(tracer), |_| {})
                        .unwrap()
                };
                assert_eq!(unsafe { value(independent.read()) }, 29);
            });
            unsafe {
                heap.collect(|tracer| registry.trace_roots(tracer), |_| {})
                    .unwrap()
            };
            assert_eq!(heap.used_bytes(), 0);

            // Repeated persistent leases reuse registration entries instead
            // of retaining an ever-growing list of expired weak handles.
            for number in 0..100 {
                let lease = unsafe { registry.persist::<PointerKind>(allocate(&mut heap, number)) };
                assert_eq!(registry.persistent.borrow().len(), 1);
                unsafe {
                    heap.collect(|tracer| registry.trace_roots(tracer), |_| {})
                        .unwrap()
                };
                assert_eq!(unsafe { value(lease.read()) }, number);
                drop(lease);
                unsafe {
                    heap.collect(|tracer| registry.trace_roots(tracer), |_| {})
                        .unwrap()
                };
                assert_eq!(heap.used_bytes(), 0);
            }
        });
    }
}
