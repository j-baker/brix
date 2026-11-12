//! Shared persistent roots for a stopped-world, multi-mutator heap.
//!
//! Only registration, cloning, and dropping are safe concurrently. Payload
//! access through a lease (`read`, `write`) is unsafe and requires the owning
//! runtime's mutator/collection token. A thread-local alias from `as_root`
//! can copy the raw payload out safely; every write and tracing remain unsafe
//! and must exclude such reads.
//! Temporary root frames remain thread-local in the ordinary RootRegistry.

use std::cell::UnsafeCell;
use std::marker::PhantomData;
use std::ptr::NonNull;
use std::sync::{Arc, Mutex, MutexGuard, Weak};

use super::{Brand, RawRoot, RawSlot, RootDescriptor, RootKind};
use crate::raw::Tracer;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poison| poison.into_inner())
}

trait Trace: Send + Sync {
    unsafe fn trace(&self, tracer: &mut Tracer<'_>);
}

struct Cell<T: Copy> {
    value: UnsafeCell<T>,
    trace: unsafe fn(NonNull<u8>, &mut Tracer<'_>),
    index: usize,
    free: Arc<Mutex<Vec<usize>>>,
}

// SAFETY: safe operations never mutate value, and T is Copy, hence has no
// destructor to run on another thread. The only safe payload operation is a
// bitwise copy through an `as_root` alias; every mutation (write, tracing) is
// an unsafe API requiring external exclusion of all other accesses.
unsafe impl<T: Copy> Send for Cell<T> {}
unsafe impl<T: Copy> Sync for Cell<T> {}

impl<T: Copy> Trace for Cell<T> {
    unsafe fn trace(&self, tracer: &mut Tracer<'_>) {
        unsafe { (self.trace)(NonNull::from(&self.value).cast(), tracer) };
    }
}

impl<T: Copy> Drop for Cell<T> {
    fn drop(&mut self) {
        lock(&self.free).push(self.index);
    }
}

pub struct Registry<'heap> {
    descriptors: &'static [RootDescriptor],
    entries: Mutex<Vec<Weak<dyn Trace + 'heap>>>,
    free: Arc<Mutex<Vec<usize>>>,
    heap: Brand<'heap>,
}

impl<'heap> Registry<'heap> {
    /// # Safety
    /// The descriptors must match all registered kinds; 'heap must uniquely
    /// brand the graph. The owning runtime must enforce the mutator protocol.
    pub unsafe fn new(descriptors: &'static [RootDescriptor]) -> Self {
        Self {
            descriptors,
            entries: Mutex::new(Vec::new()),
            free: Arc::new(Mutex::new(Vec::new())),
            heap: PhantomData,
        }
    }

    /// # Safety
    /// value must be initialized, current, and from this heap. Collection
    /// must be excluded until registration has finished.
    pub unsafe fn persist<K: RootKind>(&self, value: K::Stored) -> Persistent<'heap, K>
    where
        K::Stored: 'heap,
    {
        let descriptor = &self.descriptors[K::KIND];
        assert_eq!(descriptor.size, std::mem::size_of::<K::Stored>());
        assert_eq!(descriptor.align, std::mem::align_of::<K::Stored>());
        let mut entries = lock(&self.entries);
        let index = lock(&self.free).pop().unwrap_or(entries.len());
        let cell = Arc::new(Cell {
            value: UnsafeCell::new(value),
            trace: descriptor.trace,
            index,
            free: self.free.clone(),
        });
        let erased: Arc<dyn Trace + 'heap> = cell.clone();
        let weak = Arc::downgrade(&erased);
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

    /// # Safety
    /// All mutators must be stopped, with no payload views/unrooted pointers.
    /// The tracer must belong to this heap. Descriptors must not unwind or
    /// reenter registration. Root cloning/dropping may proceed concurrently.
    pub unsafe fn trace_roots(&self, tracer: &mut Tracer<'_>) {
        for entry in lock(&self.entries).iter() {
            if let Some(cell) = entry.upgrade() {
                unsafe { cell.trace(tracer) };
            }
        }
    }
}

pub struct Persistent<'heap, K: RootKind> {
    cell: Arc<Cell<K::Stored>>,
    heap: Brand<'heap>,
    marker: PhantomData<fn() -> K>,
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
    /// Construct a thread-local borrowed root. Copying its raw payload out
    /// is safe; [`Persistent::write`] and tracing must exclude such reads,
    /// and the copy is only current until the next collection.
    pub fn as_root(&self) -> RawRoot<'_, 'heap, K> {
        RawSlot {
            cell: NonNull::from(&self.cell.value).cast(),
            scope: PhantomData,
            heap: PhantomData,
            marker: PhantomData,
        }
        .root()
    }

    /// Deliberately `unsafe`, unlike `RawRoot::read`: this lease is shared
    /// between threads, so the read can race with a write from another one.
    ///
    /// # Safety
    /// Exclude concurrent writes/collection; do not retain the value across a
    /// safepoint. Any pointer in the value belongs to this registry's heap.
    pub unsafe fn read(&self) -> K::Stored {
        unsafe { self.cell.value.get().read() }
    }

    /// # Safety
    /// Exclude all other accesses to this slot and collection. value must be
    /// current and belong to this heap. No view of the old contents may live.
    pub unsafe fn write(&self, value: K::Stored) {
        unsafe { self.cell.value.get().write(value) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    enum Number {}
    unsafe impl RootKind for Number {
        type Stored = usize;
        const KIND: usize = 0;
    }
    unsafe fn trace(_: NonNull<u8>, _: &mut Tracer<'_>) {}
    static DESCRIPTORS: [RootDescriptor; 1] = [RootDescriptor {
        size: std::mem::size_of::<usize>(),
        align: std::mem::align_of::<usize>(),
        trace,
    }];

    #[test]
    fn leases_cross_threads_and_recycle_registration_slots() {
        let registry = unsafe { Registry::new(&DESCRIPTORS) };
        let first = unsafe { registry.persist::<Number>(42) };
        std::thread::scope(|scope| {
            for _ in 0..4 {
                let root = first.clone();
                scope.spawn(move || {
                    for _ in 0..1000 {
                        let copy = root.clone();
                        // No writes or collection run in this test.
                        assert_eq!(unsafe { copy.read() }, 42);
                    }
                });
            }
        });
        assert!(lock(&registry.free).is_empty());
        drop(first);
        assert_eq!(lock(&registry.free).len(), 1);
        let replacement = unsafe { registry.persist::<Number>(99) };
        assert_eq!(lock(&registry.entries).len(), 1);
        assert_eq!(unsafe { replacement.read() }, 99);
    }
}
