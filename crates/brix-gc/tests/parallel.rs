#![cfg(not(target_family = "wasm"))]
use brix_gc::raw::{Descriptor, RawHeap, Tracer};
use brix_gc::Config;
use std::cell::Cell;
use std::mem::{align_of, size_of};
use std::ptr::NonNull;
use std::rc::Rc;
use std::sync::Mutex;
use std::thread::{self, ThreadId};

type Pointer = NonNull<u8>;
#[repr(C)]
struct Node {
    left: Option<Pointer>,
    right: Option<Pointer>,
    value: usize,
}
struct Owned {
    drops: Rc<Cell<usize>>,
    thread: ThreadId,
}
impl Drop for Owned {
    fn drop(&mut self) {
        assert_eq!(thread::current().id(), self.thread);
        self.drops.set(self.drops.get() + 1);
    }
}
static THREADS: Mutex<Vec<ThreadId>> = Mutex::new(Vec::new());
unsafe fn trace_node(p: Pointer, _: usize, t: &mut Tracer<'_>) {
    // SAFETY: exact initialized Node payload; each callback owns its fields.
    unsafe {
        let node = &mut *p.cast::<Node>().as_ptr();
        if let Some(left) = &mut node.left {
            *left = t.relocate_fixed::<Node, 0>(left.cast()).cast();
        }
        if let Some(right) = &mut node.right {
            t.visit(right);
        }
        if node.value % 128 == 0 {
            let id = thread::current().id();
            let mut threads = THREADS.lock().unwrap();
            if !threads.contains(&id) {
                threads.push(id);
            }
        }
    }
}
unsafe fn trace_none(_: Pointer, _: usize, _: &mut Tracer<'_>) {}
unsafe fn drop_owned(p: Pointer, _: usize) {
    // SAFETY: this dead payload still owns exactly one Rc reference.
    unsafe { p.cast::<Owned>().as_ptr().drop_in_place() };
}
static DESCRIPTORS: [Descriptor; 4] = [
    Descriptor {
        name: "node",
        fixed_size: Some(size_of::<Node>()),
        alignment: align_of::<Node>(),
        trace: trace_node,
        drop: None,
    },
    Descriptor {
        name: "owned",
        fixed_size: Some(size_of::<Owned>()),
        alignment: align_of::<Owned>(),
        trace: trace_none,
        drop: Some(drop_owned),
    },
    Descriptor {
        name: "variable bytes",
        fixed_size: None,
        alignment: 64,
        trace: trace_none,
        drop: None,
    },
    Descriptor {
        name: "tiny",
        fixed_size: Some(1),
        alignment: 1,
        trace: trace_none,
        drop: None,
    },
];
fn heap(workers: usize) -> RawHeap {
    let mut heap = RawHeap::new(
        Config {
            workers,
            threshold_bytes: 1024 * 1024,
            chunk_bytes: 4096,
            ..Config::default()
        },
        &DESCRIPTORS,
    )
    .unwrap();
    // SAFETY: callbacks touch only their payload and a synchronized test log.
    unsafe { heap.enable_parallel_tracing() };
    heap
}
unsafe fn node(
    heap: &mut RawHeap,
    value: usize,
    left: Option<Pointer>,
    right: Option<Pointer>,
) -> Pointer {
    // SAFETY: initialize every field before collection can happen.
    unsafe {
        let p = heap.allocate_slow(0, size_of::<Node>()).unwrap();
        p.cast::<Node>().as_ptr().write(Node { left, right, value });
        p
    }
}

#[test]
fn sharing_cycles_remembered_edges_and_compaction_use_workers() {
    THREADS.lock().unwrap().clear();
    let mut heap = heap(4);
    let mut roots = Vec::new();
    unsafe {
        let common = node(&mut heap, 999_999, None, None);
        for i in 0..16_384 {
            let p = node(&mut heap, i, Some(common), Some(common));
            roots.push(p);
        }
        (*common.cast::<Node>().as_ptr()).left = Some(common);
        let original_common = common;
        let mut weak = Some(common);
        heap.collect_minor(
            |t| {
                for p in &mut roots {
                    t.visit(p);
                }
            },
            |r| weak = weak.and_then(|p| r.lookup(p)),
        )
        .unwrap();
        assert_eq!(heap.stats().last_collection_workers, 4);
        assert!(
            THREADS.lock().unwrap().len() > 1,
            "large graph must execute callbacks on multiple threads"
        );
        let common = (*roots[0].cast::<Node>().as_ptr()).left.unwrap();
        assert_ne!(common, original_common);
        assert_eq!(weak, Some(common));
        for (i, &p) in roots.iter().enumerate() {
            let n = p.cast::<Node>().as_ref();
            assert_eq!(n.value, i);
            assert_eq!(n.left, Some(common));
            assert_eq!(n.right, Some(common));
        }
        assert_eq!(common.cast::<Node>().as_ref().left, Some(common));
        // Every old root points back into a new batch, with shared young nodes.
        for (i, &root) in roots.iter().take(2048).enumerate() {
            let young = node(&mut heap, 50_000 + i, Some(root), Some(common));
            (*root.cast::<Node>().as_ptr()).right = Some(young);
            heap.write_barrier(root);
        }
        heap.collect_minor(
            |t| {
                for p in &mut roots {
                    t.visit(p);
                }
            },
            |_| {},
        )
        .unwrap();
        for (i, &root) in roots.iter().take(2048).enumerate() {
            let young = root.cast::<Node>().as_ref().right.unwrap();
            assert_eq!(young.cast::<Node>().as_ref().value, 50_000 + i);
            assert_eq!(young.cast::<Node>().as_ref().left, Some(root));
        }
        roots.truncate(512);
        heap.collect(
            |t| {
                for p in &mut roots {
                    t.visit(p);
                }
            },
            |_| {},
        )
        .unwrap();
        for (i, &p) in roots.iter().enumerate() {
            assert_eq!(p.cast::<Node>().as_ref().value, i);
            let young = p.cast::<Node>().as_ref().right.unwrap();
            assert_eq!(young.cast::<Node>().as_ref().left, Some(p));
        }
    }
}

#[test]
fn interiors_tiny_forwarding_and_thread_affine_drops() {
    for workers in [1, 2, 4, 8] {
        let drops = Rc::new(Cell::new(0));
        let mut heap = heap(workers);
        let mut owned = Vec::new();
        let mut bytes = Vec::new();
        let mut tiny = Vec::new();
        unsafe {
            for i in 0..513 {
                let p = heap.allocate_slow(1, size_of::<Owned>()).unwrap();
                p.cast::<Owned>().as_ptr().write(Owned {
                    drops: drops.clone(),
                    thread: thread::current().id(),
                });
                if i % 3 != 0 {
                    owned.push(p);
                }
                let len = i * 17 + 1;
                let p = heap.allocate_slow(2, len).unwrap();
                p.as_ptr().write_bytes((i % 251) as u8, len);
                bytes.push((NonNull::new_unchecked(p.as_ptr().add(len - 1)), i, len));
                let p = heap.allocate_slow(3, 1).unwrap();
                p.as_ptr().write((i % 251) as u8);
                tiny.push((p, p));
            }
            let caller = thread::current().id();
            heap.collect_minor(
                |t| {
                    assert_eq!(thread::current().id(), caller);
                    for p in &mut owned {
                        t.visit(p);
                    }
                    for (p, _, _) in &mut bytes {
                        t.visit(p);
                    }
                    for (a, b) in &mut tiny {
                        t.visit(a);
                        t.visit(b);
                    }
                },
                |_| assert_eq!(thread::current().id(), caller),
            )
            .unwrap();
            assert_eq!(drops.get(), 171);
            heap.collect(
                |t| {
                    for p in &mut owned {
                        t.visit(p);
                    }
                    for (p, _, _) in &mut bytes {
                        t.visit(p);
                    }
                    for (a, b) in &mut tiny {
                        t.visit(a);
                        t.visit(b);
                    }
                },
                |_| {},
            )
            .unwrap();
            for (p, i, len) in bytes {
                assert_eq!(p.as_ptr().read(), (i % 251) as u8);
                let start = p.as_ptr().sub(len - 1);
                assert_eq!(start as usize % 64, 0);
                assert_eq!(start.read(), (i % 251) as u8);
            }
            for (i, (a, b)) in tiny.into_iter().enumerate() {
                assert_eq!(a, b);
                assert_eq!(a.as_ptr().read(), (i % 251) as u8);
            }
        }
        drop(heap);
        assert_eq!(drops.get(), 513);
        assert_eq!(Rc::strong_count(&drops), 1);
    }
}
