use std::cell::Cell;
use std::mem::{align_of, size_of};
use std::ptr::NonNull;
use std::rc::Rc;

use brix_gc::raw::{Descriptor, RawHeap, Tracer};
use brix_gc::{Config, Error};

type Pointer = NonNull<u8>;

#[test]
fn known_fixed_layout_preserves_aliases_and_values_across_both_generations() {
    fn run<T: Copy + Eq + std::fmt::Debug>(value: T) {
        let descriptors = Box::leak(Box::new([Descriptor {
            name: "known fixed layout",
            fixed_size: Some(size_of::<T>()),
            alignment: align_of::<T>(),
            trace: trace_bytes,
            drop: None,
        }]));
        let mut heap = RawHeap::new(
            Config {
                chunk_bytes: 128,
                ..Config::default()
            },
            descriptors,
        )
        .unwrap();
        let mut roots = Vec::new();
        for _ in 0..32 {
            unsafe {
                let pointer = heap.allocate_slow(0, size_of::<T>()).unwrap().cast::<T>();
                pointer.as_ptr().write(value);
                roots.push(pointer);
            }
        }
        let mut alias = roots[31].cast::<u8>();
        let mut external = value;
        let mut external_pointer = NonNull::from(&mut external);
        let original_external = external_pointer;
        unsafe {
            heap.collect_minor(
                |tracer| {
                    for root in &mut roots {
                        *root = tracer.relocate_fixed::<T, 0>(*root);
                    }
                    // A generic edge shares the forwarding record with typed edges.
                    tracer.visit(&mut alias);
                    external_pointer = tracer.relocate_fixed::<T, 0>(external_pointer);
                },
                |_| {},
            )
            .unwrap();
        }
        assert_eq!(roots[31].cast::<u8>(), alias);
        assert_eq!(external_pointer, original_external);
        let before_compaction = roots[31];
        let mut root = roots[31];
        drop(roots);
        // Retaining only the last slot forces compaction to move it into an
        // earlier hole. Both generic and typed roots must be repaired once.
        unsafe {
            heap.collect(
                |tracer| {
                    root = tracer.relocate_fixed::<T, 0>(root);
                    tracer.visit(&mut alias);
                    external_pointer = tracer.relocate_fixed::<T, 0>(external_pointer);
                },
                |_| {},
            )
            .unwrap();
            assert_eq!(*root.as_ptr(), value);
        }
        assert_ne!(root, before_compaction);
        assert_eq!(root.cast::<u8>(), alias);
        assert_eq!(external_pointer, original_external);
        // An old pointer is left unchanged by the nursery fast path.
        unsafe {
            heap.collect_minor(
                |tracer| {
                    root = tracer.relocate_fixed::<T, 0>(root);
                },
                |_| {},
            )
            .unwrap();
            assert_eq!(*root.as_ptr(), value);
        }
        assert_eq!(root.cast::<u8>(), alias);
        // Promotion into the compacted page now exercises reclaimed slots,
        // rather than the virgin bump-allocation path used above.
        unsafe {
            let mut fresh = heap.allocate_slow(0, size_of::<T>()).unwrap().cast::<T>();
            fresh.as_ptr().write(value);
            heap.collect_minor(
                |tracer| {
                    root = tracer.relocate_fixed::<T, 0>(root);
                    fresh = tracer.relocate_fixed::<T, 0>(fresh);
                },
                |_| {},
            )
            .unwrap();
            assert_eq!(*fresh.as_ptr(), value);
            assert_eq!(*root.as_ptr(), value);
            assert_ne!(fresh, root);
        }
    }

    #[repr(align(64))]
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct Aligned(u64);

    run(());
    run([3u8; 3]); // Separate forwarding storage for sub-pointer-sized objects.
    run([5u8; 9]); // Forwarding words and reclaimed-slot links can be unaligned.
    run([9u64; 3]); // Non-power-of-two fixed stride.
    run(Aligned(42));
}

#[repr(C)]
struct Node {
    integer: u64,
    edge: Option<Pointer>,
}
const _: () = assert!(size_of::<Node>() == 16);

#[test]
fn fixed_major_tracing_preserves_cycles_between_old_and_young_objects() {
    unsafe fn trace_fixed(pointer: Pointer, _: usize, tracer: &mut Tracer<'_>) {
        // SAFETY: this descriptor contains a Node with exact Node edges.
        unsafe {
            let node = &mut *pointer.cast::<Node>().as_ptr();
            if let Some(edge) = &mut node.edge {
                *edge = tracer.relocate_fixed::<Node, 0>(edge.cast()).cast();
            }
        }
    }
    static FIXED: [Descriptor; 1] = [Descriptor {
        name: "fixed cyclic node",
        fixed_size: Some(size_of::<Node>()),
        alignment: align_of::<Node>(),
        trace: trace_fixed,
        drop: None,
    }];
    let mut heap = RawHeap::new(Config::default(), &FIXED).unwrap();
    let mut root = node(&mut heap, 42, None).cast::<Node>();
    unsafe {
        heap.collect_minor(
            |tracer| root = tracer.relocate_fixed::<Node, 0>(root),
            |_| {},
        )
        .unwrap();
        let young = node(&mut heap, 99, Some(root.cast()));
        root.as_mut().edge = Some(young);
        heap.write_barrier(root.cast());
        let dead = node(&mut heap, 7, None);
        let mut generic_alias = young;
        heap.collect(
            |tracer| {
                root = tracer.relocate_fixed::<Node, 0>(root);
                // Typed and generic paths share both marks and forwarding records.
                tracer.visit(&mut generic_alias);
            },
            |relocation| assert_eq!(relocation.lookup(dead), None),
        )
        .unwrap();
        assert_eq!(root.as_ref().integer, 42);
        let child = root.as_ref().edge.unwrap().cast::<Node>();
        assert_eq!(child.as_ref().integer, 99);
        assert_eq!(child.as_ref().edge, Some(root.cast()));
        assert_eq!(generic_alias, child.cast());
        assert_eq!(heap.used_bytes(), 2 * size_of::<Node>());
    }
}

unsafe fn trace_node(pointer: Pointer, bytes: usize, tracer: &mut Tracer<'_>) {
    assert_eq!(bytes, size_of::<Node>());
    // SAFETY: the node descriptor governs initialized Node payloads.
    let node = unsafe { &mut *pointer.cast::<Node>().as_ptr() };
    if let Some(edge) = &mut node.edge {
        // SAFETY: edge is either an initialized managed payload or external.
        unsafe { tracer.visit(edge) };
    }
}

unsafe fn trace_bytes(_: Pointer, _: usize, _: &mut Tracer<'_>) {}

#[repr(C)]
struct Env {
    len: usize,
    up: Option<Pointer>,
}

unsafe fn trace_env(pointer: Pointer, bytes: usize, tracer: &mut Tracer<'_>) {
    // SAFETY: Env header followed by len initialized pointer slots.
    unsafe {
        let env = &mut *pointer.cast::<Env>().as_ptr();
        assert_eq!(bytes, size_of::<Env>() + env.len * size_of::<Pointer>());
        if let Some(up) = &mut env.up {
            tracer.visit(up);
        }
        let slots = pointer.as_ptr().add(size_of::<Env>()).cast::<Pointer>();
        for i in 0..env.len {
            tracer.visit(&mut *slots.add(i));
        }
    }
}

struct Resource(Rc<Cell<usize>>);
impl Drop for Resource {
    fn drop(&mut self) {
        self.0.set(self.0.get() + 1);
    }
}

unsafe fn drop_resource(pointer: Pointer, bytes: usize) {
    assert_eq!(bytes, size_of::<Rc<Resource>>());
    // SAFETY: initialized Rc holder, owned once by this dead payload.
    unsafe { pointer.cast::<Rc<Resource>>().as_ptr().drop_in_place() };
}

const NODE: usize = 0;
const BYTES: usize = 1;
const ENV: usize = 2;
const RESOURCE: usize = 3;
const ALIGNED: usize = 4;
static DESCRIPTORS: [Descriptor; 5] = [
    Descriptor {
        name: "16-byte node",
        fixed_size: Some(size_of::<Node>()),
        alignment: align_of::<Node>(),
        trace: trace_node,
        drop: None,
    },
    Descriptor {
        name: "bytes",
        fixed_size: None,
        alignment: 1,
        trace: trace_bytes,
        drop: None,
    },
    Descriptor {
        name: "variable environment",
        fixed_size: None,
        alignment: align_of::<Env>(),
        trace: trace_env,
        drop: None,
    },
    Descriptor {
        name: "owned Rc",
        fixed_size: Some(size_of::<Rc<Resource>>()),
        alignment: align_of::<Rc<Resource>>(),
        trace: trace_bytes,
        drop: Some(drop_resource),
    },
    Descriptor {
        name: "aligned variable bytes",
        fixed_size: None,
        alignment: 64,
        trace: trace_bytes,
        drop: None,
    },
];

fn heap() -> RawHeap {
    RawHeap::new(
        Config {
        workers: 1,
            chunk_bytes: 128,
            steady_threshold_bytes: None,
            trace_collections: false,
            threshold_bytes: 256,
            stress: false,
        },
        &DESCRIPTORS,
    )
    .unwrap()
}

fn node(heap: &mut RawHeap, integer: u64, edge: Option<Pointer>) -> Pointer {
    // SAFETY: initialize the payload before returning or calling the heap.
    unsafe {
        let pointer = heap
            .try_allocate(NODE, size_of::<Node>())
            .unwrap_or_else(|| heap.allocate_slow(NODE, size_of::<Node>()).unwrap());
        pointer
            .cast::<Node>()
            .as_ptr()
            .write(Node { integer, edge });
        pointer
    }
}

fn bytes(heap: &mut RawHeap, kind: usize, value: &[u8]) -> Pointer {
    // SAFETY: byte payload has no edges or destructor; initialized immediately.
    unsafe {
        let pointer = heap.allocate_slow(kind, value.len()).unwrap();
        std::ptr::copy_nonoverlapping(value.as_ptr(), pointer.as_ptr(), value.len());
        pointer
    }
}

fn env(heap: &mut RawHeap, up: Option<Pointer>, children: &[Pointer]) -> Pointer {
    // SAFETY: initialize header and all inline slots before exposing the object.
    unsafe {
        let pointer = heap
            .allocate_slow(ENV, size_of::<Env>() + std::mem::size_of_val(children))
            .unwrap();
        pointer.cast::<Env>().as_ptr().write(Env {
            len: children.len(),
            up,
        });
        std::ptr::copy_nonoverlapping(
            children.as_ptr(),
            pointer.as_ptr().add(size_of::<Env>()).cast(),
            children.len(),
        );
        pointer
    }
}

fn resource(heap: &mut RawHeap, drops: &Rc<Cell<usize>>) -> Pointer {
    // SAFETY: ownership of this Rc transfers to the initialized heap payload.
    unsafe {
        let pointer = heap
            .allocate_slow(RESOURCE, size_of::<Rc<Resource>>())
            .unwrap();
        pointer
            .cast::<Rc<Resource>>()
            .as_ptr()
            .write(Rc::new(Resource(drops.clone())));
        pointer
    }
}

#[test]
fn fixed_fast_path_exhausts_refills_and_relocates() {
    let mut heap = heap();
    let mut root = None;
    // SAFETY: NODE has nonzero, unpadded Node-sized payloads. Every successful
    // allocation is initialized before another heap operation. The sole graph
    // entry point is rewritten during every collection.
    unsafe {
        assert!(heap.try_allocate_known_fixed::<16>(NODE).is_none());
        for round in 0..3 {
            // Each refill contributes exactly eight nodes. Collection leaves
            // a full destination chunk, so the next round must refill again.
            assert!(heap.try_allocate_known_fixed::<16>(NODE).is_none());
            let first = heap.allocate_slow(NODE, 16).unwrap();
            first.cast::<Node>().as_ptr().write(Node {
                integer: round * 8,
                edge: root,
            });
            root = Some(first);
            for offset in 1..8 {
                let next = heap.try_allocate_known_fixed::<16>(NODE).unwrap();
                assert_eq!(next.as_ptr(), first.as_ptr().add(offset * 16));
                next.cast::<Node>().as_ptr().write(Node {
                    integer: round * 8 + offset as u64,
                    edge: root,
                });
                root = Some(next);
            }
            let used = heap.used_bytes();
            assert!(heap.try_allocate_known_fixed::<16>(NODE).is_none());
            assert_eq!(heap.used_bytes(), used);
            let previous = root.unwrap();
            heap.collect(|tracer| tracer.visit(root.as_mut().unwrap()), |_| {})
                .unwrap();
            assert_ne!(root.unwrap(), previous);
            let mut cursor = root;
            for expected in (0..(round + 1) * 8).rev() {
                let node = cursor.unwrap().cast::<Node>().as_ref();
                assert_eq!(node.integer, expected);
                cursor = node.edge;
            }
            assert_eq!(cursor, None);
            assert_eq!(heap.used_bytes(), used);
        }
    }
}

#[test]
fn heterogeneous_graph_cycles_sharing_and_interior_roots() {
    let mut heap = heap();
    let content = bytes(&mut heap, BYTES, b"prefix/hello/suffix");
    // SAFETY: interior pointer is within the byte payload.
    let mut slice = unsafe { NonNull::new_unchecked(content.as_ptr().add(7)) };
    let first = node(&mut heap, 41, Some(slice));
    let mut root = env(&mut heap, None, &[first, first, content]);
    let empty = env(&mut heap, Some(root), &[]);
    // SAFETY: update the exact initialized parent field; a cycle through envs.
    unsafe { (*root.cast::<Env>().as_ptr()).up = Some(empty) };
    for iteration in 0..20 {
        let old_root = root;
        let old_slice = slice;
        let mut weak_root = None;
        let mut weak_slice = None;
        // SAFETY: all live entry points are rooted and rewritten. Stale copies
        // kept for address comparison are never used to access their payloads.
        unsafe {
            heap.collect(
                |tracer| {
                    tracer.visit(&mut root);
                    tracer.visit(&mut slice);
                },
                |relocation| {
                    weak_root = relocation.lookup(old_root);
                    weak_slice = relocation.lookup(old_slice);
                },
            )
            .unwrap();
            assert_eq!(weak_root, Some(root));
            assert_eq!(weak_slice, Some(slice));
            if iteration == 0 {
                assert_ne!(root, old_root);
                assert_ne!(slice, old_slice);
            }
            let top = &*root.cast::<Env>().as_ptr();
            let child = &*top.up.unwrap().cast::<Env>().as_ptr();
            assert_eq!(child.len, 0);
            assert_eq!(child.up, Some(root));
            let slots = root.as_ptr().add(size_of::<Env>()).cast::<Pointer>();
            assert_eq!(*slots, *slots.add(1));
            let first = &*(*slots).cast::<Node>().as_ptr();
            assert_eq!(first.integer, 41);
            assert_eq!(first.edge, Some(slice));
            assert_eq!(std::slice::from_raw_parts(slice.as_ptr(), 5), b"hello");
            assert_eq!(slice.as_ptr(), (*slots.add(2)).as_ptr().add(7));
        }
        assert_eq!(heap.stats().collections, iteration + 1);
    }
}

#[test]
fn tracing_revisits_kinds_until_the_graph_is_complete() {
    let mut heap = RawHeap::new(Config::default(), &DESCRIPTORS).unwrap();
    let mut head = None;
    for integer in 0..12_000 {
        let parent = env(&mut heap, head, &[]);
        head = Some(node(&mut heap, integer, Some(parent)));
    }
    let mut root = head.unwrap();
    for _ in 0..3 {
        // The only root starts in NODE. Every node discovers an ENV whose
        // parent discovers another NODE after NODE's scan had gone idle.
        unsafe {
            heap.collect(|tracer| tracer.visit(&mut root), |_| {})
                .unwrap();
            let mut next = Some(root);
            for expected in (0..12_000).rev() {
                let value = next.unwrap().cast::<Node>().as_ref();
                assert_eq!(value.integer, expected);
                next = value.edge.unwrap().cast::<Env>().as_ref().up;
            }
            assert!(next.is_none());
        }
    }
}

#[test]
fn capacity_bound_preserves_exact_collection_triggers_after_refills_and_moves() {
    let mut heap = heap();
    let mut retained = node(&mut heap, 7, None);
    for phase in 0..3 {
        for _ in 0..40 {
            let occupied = heap.young_bytes();
            let capacity: usize = heap.space_usage().iter().map(|(_, _, bytes)| bytes).sum();
            assert_eq!(heap.capacity_bytes(), capacity);
            for pending in [0, 16, 127, 256, usize::MAX] {
                assert_eq!(
                    heap.collection_due(pending),
                    occupied != 0 && occupied.saturating_add(pending) > 256
                );
            }
            node(&mut heap, phase, None);
        }
        unsafe {
            heap.collect(|tracer| tracer.visit(&mut retained), |_| {})
                .unwrap();
        }
        assert_eq!(heap.used_bytes(), 16);
        // Old-generation storage does not consume the nursery's budget.
        assert_eq!(heap.young_bytes(), 0);
        assert_eq!(heap.old_bytes(), 16);
        assert!(!heap.collection_due(16));
    }
    unsafe {
        heap.collect(|_| {}, |_| {}).unwrap();
    }
    assert_eq!(heap.capacity_bytes(), 0);
    assert!(!heap.collection_due(usize::MAX));
}

#[test]
fn compaction_scratch_is_bounded_without_a_second_old_heap() {
    let mut heap = RawHeap::new(Config::default(), &DESCRIPTORS).unwrap();
    let count = 20_000;
    let mut head = None;
    for integer in 0..count {
        head = Some(node(&mut heap, integer as u64, head));
    }
    let source_capacity = heap.capacity_bytes();
    let allocated = heap.used_bytes();
    let mut root = head.unwrap();
    unsafe {
        heap.collect(|tracer| tracer.visit(&mut root), |_| {})
            .unwrap()
    };
    // Nursery forwarding, mark worklist and compaction forwarding each need
    // one word per object. Slab occupancy costs one byte per slot. Allow a
    // partial slab and class/page metadata, but no second full old heap.
    assert!(
        heap.stats().peak_collection_capacity
            <= source_capacity + allocated + count * (3 * size_of::<Pointer>() + 1) + 128 * 1024
    );
    assert_eq!(heap.stats().last_live_bytes, allocated);
}

#[test]
fn dropping_dead_owned_payloads_and_heap_drops_each_resource_once() {
    let drops = Rc::new(Cell::new(0));
    {
        let mut heap = heap();
        let dead = resource(&mut heap, &drops);
        let live = resource(&mut heap, &drops);
        let mut root = node(&mut heap, dead.as_ptr() as usize as u64, Some(live));
        for iteration in 0..8 {
            // SAFETY: root is the only live strong entry point. Integer that
            // looks like the dead pointer is intentionally not traced.
            unsafe {
                heap.collect(
                    |tracer| tracer.visit(&mut root),
                    |relocation| {
                        if iteration == 0 {
                            assert_eq!(relocation.lookup(dead), None);
                            assert!(relocation.lookup(live).is_some());
                            assert_eq!(drops.get(), 0, "weak repair precedes destruction");
                        }
                    },
                )
                .unwrap();
            }
            assert_eq!(drops.get(), 1, "moving does not clone or drop live Rc");
        }
        // SAFETY: no root retained after this collection.
        unsafe { heap.collect(|_| {}, |_| {}).unwrap() };
        assert_eq!(drops.get(), 2);
        assert_eq!(heap.used_bytes(), 0);
        assert_eq!(heap.capacity_bytes(), 0);
        resource(&mut heap, &drops);
    }
    assert_eq!(drops.get(), 3);
}

#[test]
fn external_pointers_are_not_traced_and_weak_edges_do_not_retain() {
    let permanent = Box::new(901u64);
    let external = NonNull::from(&*permanent).cast();
    let mut heap = heap();
    let dead = node(&mut heap, 111, None);
    let mut root = node(&mut heap, 222, Some(external));
    let mut weak = Some(dead);
    // SAFETY: external owner remains alive; roots and weak edges are exact.
    unsafe {
        heap.collect(
            |tracer| tracer.visit(&mut root),
            |relocation| {
                weak = weak.and_then(|pointer| relocation.lookup(pointer));
                assert_eq!(relocation.lookup(external), Some(external));
            },
        )
        .unwrap();
        assert_eq!((*root.cast::<Node>().as_ptr()).edge, Some(external));
    }
    assert_eq!(weak, None);
    assert_eq!(*permanent, 901);
    assert_eq!(heap.used_bytes(), size_of::<Node>());
}

#[test]
fn fixed_cells_have_no_headers_and_collection_rechecks_small_live_budget() {
    let mut heap = RawHeap::new(
        Config {
        workers: 1,
            chunk_bytes: 128,
            steady_threshold_bytes: None,
            trace_collections: false,
            threshold_bytes: 32,
            stress: false,
        },
        &DESCRIPTORS,
    )
    .unwrap();
    let first = node(&mut heap, 1, None);
    let second = node(&mut heap, 2, None);
    assert_eq!(second.as_ptr() as usize - first.as_ptr() as usize, 16);
    assert_eq!(heap.used_bytes(), 32);
    for i in 0..100 {
        node(&mut heap, i, None);
    }
    let mut root = first;
    // SAFETY: only root survives; discarded raw handles are never used again.
    unsafe {
        heap.collect(|tracer| tracer.visit(&mut root), |_| {})
            .unwrap()
    };
    assert_eq!(heap.used_bytes(), 16);
    assert!(!heap.collection_due(16));
    node(&mut heap, 3, None);
    assert!(!heap.collection_due(16));
    node(&mut heap, 4, None);
    assert!(heap.collection_due(16));
}

#[test]
fn variable_alignment_empty_payloads_and_large_buffers() {
    let mut heap = heap();
    let mut empty = bytes(&mut heap, ALIGNED, &[]);
    let mut root = bytes(&mut heap, ALIGNED, &vec![73; 100_003]);
    assert_eq!(root.as_ptr() as usize % 64, 0);
    assert_eq!(empty.as_ptr() as usize % 64, 0);
    // SAFETY: both payload addresses rooted; empty owner is still an in-bounds
    // unique address (not a one-past-end interior slice).
    unsafe {
        heap.collect(
            |tracer| {
                tracer.visit(&mut root);
                tracer.visit(&mut empty);
            },
            |_| {},
        )
        .unwrap();
        assert_eq!(
            std::slice::from_raw_parts(root.as_ptr(), 100_003),
            &vec![73; 100_003]
        );
    }
    assert_eq!(root.as_ptr() as usize % 64, 0);
    assert_eq!(empty.as_ptr() as usize % 64, 0);
    assert_ne!(empty, root);
}

#[test]
fn invalid_layouts_and_requests_leave_initialized_graph_intact() {
    static BAD: [Descriptor; 1] = [Descriptor {
        name: "bad",
        fixed_size: Some(16),
        alignment: 3,
        trace: trace_bytes,
        drop: None,
    }];
    assert!(matches!(
        RawHeap::new(Config::default(), &BAD),
        Err(Error::InvalidRange)
    ));
    let drops = Rc::new(Cell::new(0));
    let mut heap = heap();
    let mut root = resource(&mut heap, &drops);
    let before = heap.used_bytes();
    // SAFETY: every call fails before allocating or modifying the old graph.
    unsafe {
        assert_eq!(heap.allocate_slow(NODE, 17), Err(Error::InvalidRange));
        assert_eq!(
            heap.allocate_slow(DESCRIPTORS.len(), 1),
            Err(Error::InvalidRange)
        );
        assert_eq!(
            heap.allocate_slow(BYTES, usize::MAX),
            Err(Error::AllocationTooLarge)
        );
        assert!(heap.try_allocate(BYTES, usize::MAX).is_none());
        assert_eq!(heap.used_bytes(), before);
        assert_eq!(drops.get(), 0);
        heap.collect(|tracer| tracer.visit(&mut root), |_| {})
            .unwrap();
    }
    assert_eq!(drops.get(), 0);
    drop(heap);
    assert_eq!(drops.get(), 1);
}

#[test]
fn deep_graph_is_traced_iteratively() {
    let mut heap = heap();
    let mut root = node(&mut heap, 0, None);
    for i in 1..100_000 {
        root = node(&mut heap, i, Some(root));
    }
    // SAFETY: one root owns the entire chain, descriptors trace each edge.
    unsafe {
        heap.collect(|tracer| tracer.visit(&mut root), |_| {})
            .unwrap();
        for expected in (0..100_000).rev() {
            let node = &*root.cast::<Node>().as_ptr();
            assert_eq!(node.integer, expected);
            if let Some(next) = node.edge {
                root = next;
            }
        }
    }
    assert_eq!(heap.used_bytes(), 100_000 * 16);
}

#[test]
fn paired_fixed_runs_preserve_cross_edges_interior_roots_and_individual_identity() {
    static PAIR: [Descriptor; 2] = [
        Descriptor {
            name: "first node",
            fixed_size: Some(16),
            alignment: align_of::<Node>(),
            trace: trace_node,
            drop: None,
        },
        Descriptor {
            name: "second node",
            fixed_size: Some(16),
            alignment: align_of::<Node>(),
            trace: trace_node,
            drop: None,
        },
    ];
    let mut heap = RawHeap::new(
        Config {
            chunk_bytes: 128,
            ..Config::default()
        },
        &PAIR,
    )
    .unwrap();
    let mut roots = Vec::new();
    let mut dead = Vec::new();
    unsafe {
        // Includes a warm reservation, one-side refills and two-side refills.
        for (count_a, count_b) in [(3, 5), (2, 1), (9, 4), (1, 12)] {
            let (a, b) = heap
                .allocate_known_fixed_pair::<16, 16>(0, count_a, 1, count_b)
                .unwrap();
            for (pointer, count, other, number) in [(a, count_a, b, 10), (b, count_b, a, 20)] {
                for index in 0..count {
                    pointer.cast::<Node>().as_ptr().add(index).write(Node {
                        integer: number + index as u64,
                        edge: (index == 0).then_some(other),
                    });
                }
            }
            // Both runs are now initialized. Keep an interior pointer to a's
            // first edge; its cross-space cycle must preserve exactly two nodes.
            roots.push(a.add(std::mem::offset_of!(Node, edge)));
            for (pointer, count) in [(a, count_a), (b, count_b)] {
                for index in 1..count {
                    dead.push(pointer.add(index * 16));
                }
            }
        }
        let before = heap.used_bytes();
        let capacity = heap.capacity_bytes();
        for (a, b) in [(usize::MAX, 1), (1, usize::MAX)] {
            assert_eq!(
                heap.allocate_known_fixed_pair::<16, 16>(0, a, 1, b),
                Err(Error::AllocationTooLarge)
            );
            assert_eq!(heap.used_bytes(), before);
            assert_eq!(heap.capacity_bytes(), capacity);
        }
        heap.collect(
            |tracer| {
                for root in &mut roots {
                    tracer.visit(root);
                }
            },
            |relocation| {
                for pointer in dead {
                    assert_eq!(relocation.lookup(pointer), None);
                }
            },
        )
        .unwrap();
        assert_eq!(heap.used_bytes(), roots.len() * 2 * 16);
        for root in &roots {
            let a = root.sub(std::mem::offset_of!(Node, edge)).cast::<Node>();
            assert_eq!(a.as_ref().integer, 10);
            let b = a.as_ref().edge.unwrap().cast::<Node>();
            assert_eq!(b.as_ref().integer, 20);
            assert_eq!(b.as_ref().edge.unwrap(), a.cast());
        }
        heap.collect(|_| {}, |_| {}).unwrap();
        assert_eq!(heap.used_bytes(), 0);
    }
}

#[test]
fn variable_reservation_commits_exact_prefix_and_reuses_cancelled_tail() {
    let mut heap = heap();
    let first = {
        let reservation = heap.reserve_variable(BYTES, 64).unwrap();
        let pointer = reservation.payload();
        unsafe {
            pointer.as_ptr().write_bytes(0xab, 64);
        }
        drop(reservation);
        pointer
    };
    assert_eq!(heap.used_bytes(), 0);
    assert_eq!(heap.capacity_bytes(), 128);
    let reservation = heap.reserve_variable(BYTES, 64).unwrap();
    assert_eq!(reservation.payload(), first);
    let mut root = unsafe { reservation.commit(3).unwrap() };
    let first_extent = heap.allocation_extent(BYTES, 3).unwrap();
    assert_eq!(heap.used_bytes(), first_extent);
    let second = heap.reserve_variable(BYTES, 32).unwrap();
    assert_eq!(
        second.payload().as_ptr().addr(),
        first.as_ptr().addr() + first_extent
    );
    unsafe {
        second.payload().as_ptr().write_bytes(7, 4);
        second.commit(4).unwrap();
    }
    let used = heap.used_bytes();
    assert!(matches!(
        unsafe { heap.reserve_variable(BYTES, 8).unwrap().commit(9) },
        Err(Error::InvalidRange)
    ));
    assert_eq!(heap.used_bytes(), used);
    assert!(matches!(
        heap.reserve_variable(NODE, 16),
        Err(Error::InvalidRange)
    ));
    assert!(matches!(
        heap.reserve_variable(usize::MAX, 16),
        Err(Error::InvalidRange)
    ));
    assert!(matches!(
        heap.reserve_variable(BYTES, usize::MAX),
        Err(Error::AllocationTooLarge)
    ));
    assert_eq!(heap.used_bytes(), used);
    let capacity = heap.capacity_bytes();
    let empty = unsafe { heap.reserve_variable(BYTES, 0).unwrap().commit(0).unwrap() };
    assert_ne!(empty, root);
    assert_eq!(heap.capacity_bytes(), capacity);
    unsafe {
        heap.collect(|tracer| tracer.visit(&mut root), |_| {})
            .unwrap();
    }
    assert_eq!(heap.used_bytes(), first_extent);
    assert_eq!(
        unsafe { std::slice::from_raw_parts(root.as_ptr(), 3) },
        &[0xab; 3]
    );
}

#[test]
fn variable_reservation_panic_and_forget_leave_no_objects() {
    let mut heap = heap();
    let mut root = node(&mut heap, 42, None);
    let used = heap.used_bytes();
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let pending = heap.reserve_variable(ENV, 128).unwrap();
        unsafe {
            pending.payload().as_ptr().write_bytes(0xff, 3);
        }
        panic!("cancel partly initialized environment");
    }));
    assert!(panic.is_err());
    assert_eq!(heap.used_bytes(), used);
    let pending = heap.reserve_variable(ENV, 128).unwrap();
    std::mem::forget(pending);
    assert_eq!(heap.used_bytes(), used);
    unsafe {
        heap.collect(|tracer| tracer.visit(&mut root), |_| {})
            .unwrap();
    }
    assert_eq!(unsafe { root.cast::<Node>().as_ref().integer }, 42);
    assert_eq!(heap.used_bytes(), used);
}

#[test]
fn variable_reservation_moves_interior_roots_and_traces_only_committed_edges() {
    let mut heap = heap();
    let left = node(&mut heap, 10, None);
    let right = node(&mut heap, 20, None);
    let discarded = node(&mut heap, 30, None);
    let actual = size_of::<Env>() + 2 * size_of::<Pointer>();
    let pending = heap
        .reserve_variable(ENV, actual + size_of::<Pointer>())
        .unwrap();
    let payload = pending.payload();
    unsafe {
        payload
            .cast::<Env>()
            .as_ptr()
            .write(Env { len: 2, up: None });
        let slots = payload.as_ptr().add(size_of::<Env>()).cast::<Pointer>();
        slots.write(left);
        slots.add(1).write(right);
        slots.add(2).write(discarded);
        pending.commit(actual).unwrap();
    }
    let interior_offset = size_of::<Env>() + size_of::<Pointer>();
    let mut root = unsafe { payload.add(interior_offset) };
    unsafe {
        heap.collect(
            |tracer| tracer.visit(&mut root),
            |relocation| {
                assert!(relocation.lookup(discarded).is_none());
            },
        )
        .unwrap();
        let environment = root.sub(interior_offset);
        assert_eq!(environment.cast::<Env>().as_ref().len, 2);
        let slots = environment.as_ptr().add(size_of::<Env>()).cast::<Pointer>();
        assert_eq!((*slots).cast::<Node>().as_ref().integer, 10);
        assert_eq!((*slots.add(1)).cast::<Node>().as_ref().integer, 20);
    }
    assert_eq!(
        heap.used_bytes(),
        heap.allocation_extent(ENV, actual).unwrap() + 2 * size_of::<Node>()
    );
    let aligned = heap.reserve_variable(ALIGNED, 127).unwrap();
    assert_eq!(aligned.payload().as_ptr().addr() % 64, 0);
    unsafe {
        aligned.payload().as_ptr().write_bytes(1, 65);
        aligned.commit(65).unwrap();
    }
}

thread_local! {
    static TRACED: Cell<usize> = const { Cell::new(0) };
}

unsafe fn counted_trace(pointer: Pointer, bytes: usize, tracer: &mut Tracer<'_>) {
    TRACED.set(TRACED.get() + 1);
    unsafe { trace_node(pointer, bytes, tracer) };
}

#[test]
fn growing_old_graph_is_not_retraced_or_recopied_by_minor_collections() {
    static COUNTED: [Descriptor; 1] = [Descriptor {
        name: "counted node",
        fixed_size: Some(size_of::<Node>()),
        alignment: align_of::<Node>(),
        trace: counted_trace,
        drop: None,
    }];
    let mut heap = RawHeap::new(
        Config {
            steady_threshold_bytes: None,
            trace_collections: false,
            threshold_bytes: usize::MAX,
            ..Config::default()
        },
        &COUNTED,
    )
    .unwrap();
    let mut root = node(&mut heap, 0, None);
    for i in 1..10_000 {
        root = node(&mut heap, i, Some(root));
    }
    unsafe {
        heap.collect_minor(|t| t.visit(&mut root), |_| {}).unwrap();
    }
    let copied = heap.stats().copied_bytes;
    for i in 0..100 {
        TRACED.set(0);
        let old_root = root;
        root = node(&mut heap, 10_000 + i, Some(root));
        unsafe {
            heap.collect_minor(|t| t.visit(&mut root), |_| {}).unwrap();
            assert_eq!(root.cast::<Node>().as_ref().edge, Some(old_root));
        }
        assert_eq!(TRACED.get(), 1, "minor collection traced an old object");
        assert_eq!(heap.stats().copied_bytes - copied, (i as usize + 1) * 16);
        assert_eq!(heap.young_bytes(), 0);
    }
    assert_eq!(heap.stats().minor_collections, 101);
    assert_eq!(heap.stats().major_collections, 0);
    unsafe {
        heap.collect(|_| {}, |_| {}).unwrap();
    }
    assert_eq!(heap.used_bytes(), 0);
    assert_eq!(heap.capacity_bytes(), 0);
}

#[test]
fn remembered_interior_owner_keeps_a_young_cycle_and_weak_edges_alive() {
    for chunk_bytes in [64 * 1024, 2 * 1024 * 1024] {
        let mut heap = RawHeap::new(
            Config {
                chunk_bytes,
                steady_threshold_bytes: None,
                trace_collections: false,
                threshold_bytes: usize::MAX,
                ..Config::default()
            },
            &DESCRIPTORS,
        )
        .unwrap();
        let drops = Rc::new(Cell::new(0));
        let initial = node(&mut heap, 1, None);
        let mut root = env(&mut heap, None, &[initial]);
        unsafe {
            heap.collect_minor(|t| t.visit(&mut root), |_| {}).unwrap();
        }
        let stable = root;
        let child = node(&mut heap, 42, Some(root));
        let dead = resource(&mut heap, &drops);
        let mut weak_child = Some(child);
        unsafe {
            let slot = root.add(size_of::<Env>());
            heap.write_barrier(slot);
            slot.cast::<Pointer>().as_ptr().write(child);
            heap.collect_minor(
                |t| t.visit(&mut root),
                |r| {
                    weak_child = weak_child.and_then(|p| r.lookup(p));
                    assert_eq!(r.lookup(dead), None);
                    assert_eq!(drops.get(), 0);
                },
            )
            .unwrap();
            assert_eq!(root, stable);
            let child = root.add(size_of::<Env>()).cast::<Pointer>().as_ptr().read();
            assert_eq!(weak_child, Some(child));
            assert_eq!(child.cast::<Node>().as_ref().edge, Some(root));
            assert_eq!(child.cast::<Node>().as_ref().integer, 42);
        }
        assert_eq!(drops.get(), 1);
        let copied = heap.stats().copied_bytes;
        unsafe {
            heap.collect_minor(|t| t.visit(&mut root), |_| {}).unwrap();
        }
        assert_eq!(heap.stats().copied_bytes, copied);
        // The obsolete initial child is floating old garbage until a major GC.
        assert_eq!(
            heap.old_bytes(),
            heap.allocation_extent(ENV, size_of::<Env>() + size_of::<Pointer>())
                .unwrap()
                + 32
        );
        unsafe {
            heap.collect(|t| t.visit(&mut root), |_| {}).unwrap();
        }
        assert_eq!(
            heap.old_bytes(),
            heap.allocation_extent(ENV, size_of::<Env>() + size_of::<Pointer>())
                .unwrap()
                + 16
        );
    }
}

#[test]
fn major_compacts_fragmented_pages_repairs_cycles_and_reuses_dead_slots() {
    let mut heap = RawHeap::new(
        Config {
        workers: 1,
            chunk_bytes: 128,
            steady_threshold_bytes: None,
            trace_collections: false,
            threshold_bytes: usize::MAX,
            stress: false,
        },
        &DESCRIPTORS,
    )
    .unwrap();
    let mut roots: Vec<_> = (0..128).map(|i| node(&mut heap, i, None)).collect();
    unsafe {
        heap.collect_minor(
            |t| {
                for p in &mut roots {
                    t.visit(p);
                }
            },
            |_| {},
        )
        .unwrap();
    }
    let full_capacity = heap.capacity_bytes();
    let dead = roots[1];
    // Keep a sparse cycle spanning many pages, with an interior root as well.
    roots.retain(|p| unsafe { p.cast::<Node>().as_ref().integer % 17 == 0 });
    for i in 0..roots.len() {
        let owner = roots[i];
        let next = roots[(i + 1) % roots.len()];
        heap.write_barrier(owner);
        unsafe {
            (*owner.cast::<Node>().as_ptr()).edge = Some(next);
        }
    }
    let old_roots = roots.clone();
    let mut interior = unsafe { roots[3].add(8) };
    let mut weak = old_roots.clone();
    unsafe {
        heap.collect(
            |t| {
                for p in &mut roots {
                    t.visit(p);
                }
                t.visit(&mut interior);
            },
            |r| {
                assert_eq!(r.lookup(dead), None);
                for p in &mut weak {
                    *p = r.lookup(*p).unwrap();
                }
            },
        )
        .unwrap();
        assert_eq!(weak, roots);
        assert_eq!(interior, roots[3].add(8));
        assert!(roots.iter().zip(&old_roots).any(|(a, b)| a != b));
        for i in 0..roots.len() {
            let n = roots[i].cast::<Node>().as_ref();
            assert_eq!(n.integer, (i * 17) as u64);
            assert_eq!(n.edge, Some(roots[(i + 1) % roots.len()]));
        }
    }
    assert!(heap.capacity_bytes() <= 256);
    assert!(heap.capacity_bytes() < full_capacity / 4);
    assert!(heap.stats().compacted_bytes > 0);
    // Reuse compacted tail capacity without changing the retained cycle.
    let extra = node(&mut heap, 999, Some(roots[0]));
    roots.push(extra);
    unsafe {
        heap.collect_minor(
            |t| {
                for p in &mut roots {
                    t.visit(p);
                }
            },
            |_| {},
        )
        .unwrap();
    }
    assert_eq!(heap.used_bytes(), roots.len() * 16);
}

#[test]
fn variable_compaction_preserves_lengths_alignment_interior_edges_and_drops() {
    let mut heap = RawHeap::new(
        Config {
        workers: 1,
            chunk_bytes: 128,
            steady_threshold_bytes: None,
            trace_collections: false,
            threshold_bytes: usize::MAX,
            stress: false,
        },
        &DESCRIPTORS,
    )
    .unwrap();
    let drops = Rc::new(Cell::new(0));
    let mut owners = Vec::new();
    let mut resources = Vec::new();
    for i in 0..40 {
        // Different logical lengths in the same power-of-two class.
        owners.push(bytes(&mut heap, ALIGNED, &vec![i as u8; 65 + i]));
        resources.push(resource(&mut heap, &drops));
    }
    unsafe {
        heap.collect_minor(
            |t| {
                for p in owners.iter_mut().chain(&mut resources) {
                    t.visit(p);
                }
            },
            |_| {},
        )
        .unwrap();
    }
    let mut root = unsafe { owners[37].add(7) };
    let mut owned = resources[37];
    let dead = resources[0];
    unsafe {
        heap.collect(
            |t| {
                t.visit(&mut root);
                t.visit(&mut owned);
            },
            |r| {
                assert!(r.lookup(dead).is_none());
                assert_eq!(drops.get(), 0);
            },
        )
        .unwrap();
        let owner = root.sub(7);
        assert_eq!(owner.as_ptr() as usize % 64, 0);
        assert_eq!(std::slice::from_raw_parts(root.as_ptr(), 95), &[37; 95]);
    }
    assert_eq!(drops.get(), 39);
    drop(heap);
    assert_eq!(drops.get(), 40);
}

#[test]
fn nursery_budget_stays_fixed_and_major_budget_grows_with_retained_objects() {
    let mut heap = heap();
    let mut roots = Vec::new();
    for i in 0..1000 {
        roots.push(node(&mut heap, i, None));
        if heap.collection_due(16) {
            unsafe {
                heap.collect_minor(
                    |t| {
                        for p in &mut roots {
                            t.visit(p);
                        }
                    },
                    |_| {},
                )
                .unwrap();
            }
        }
        assert!(heap.young_bytes() <= 256);
    }
    assert!(heap.stats().major_collections > 0);
    assert!(heap.stats().minor_collections > heap.stats().major_collections);
    assert!(heap.stats().copied_bytes < 2 * 1000 * 16);
    unsafe {
        heap.collect(|_| {}, |_| {}).unwrap();
    }
    assert_eq!(heap.capacity_bytes(), 0);
}

#[test]
fn mixed_minor_and_compacting_collections_match_an_owned_graph_model() {
    let mut heap = RawHeap::new(
        Config {
        workers: 1,
            chunk_bytes: 128,
            steady_threshold_bytes: None,
            trace_collections: false,
            threshold_bytes: usize::MAX,
            stress: false,
        },
        &DESCRIPTORS,
    )
    .unwrap();
    let mut roots = [None; 32];
    let mut model_roots = [None; 32];
    let mut edges: Vec<Option<usize>> = Vec::new();
    let mut weak: Vec<Option<Pointer>> = Vec::new();
    let mut seed = 0x123456789abcdefu64;
    for round in 0..1000 {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        let a = seed as usize % roots.len();
        let b = (seed >> 8) as usize % roots.len();
        match (seed >> 16) % 4 {
            0 | 1 => {
                let id = edges.len();
                let pointer = node(&mut heap, id as u64, roots[b]);
                edges.push(model_roots[b]);
                weak.push(Some(pointer));
                roots[a] = Some(pointer);
                model_roots[a] = Some(id);
            }
            2 => {
                if let Some(owner) = roots[a] {
                    heap.write_barrier(owner);
                    unsafe {
                        (*owner.cast::<Node>().as_ptr()).edge = roots[b];
                    }
                    edges[model_roots[a].unwrap()] = model_roots[b];
                }
            }
            _ => {
                roots[a] = None;
                model_roots[a] = None;
            }
        }
        let mut live = vec![false; edges.len()];
        for root in model_roots.iter().flatten() {
            let mut next = Some(*root);
            while let Some(id) = next {
                if live[id] {
                    break;
                }
                live[id] = true;
                next = edges[id];
            }
        }
        let major = round % 7 == 0;
        unsafe {
            let trace = |t: &mut Tracer<'_>| {
                for root in roots.iter_mut().flatten() {
                    t.visit(root);
                }
            };
            let update = |r: &brix_gc::raw::Relocation<'_>| {
                for p in &mut weak {
                    *p = p.and_then(|p| r.lookup(p));
                }
            };
            if major {
                heap.collect(trace, update).unwrap();
            } else {
                heap.collect_minor(trace, update).unwrap();
            }
            for (id, &is_live) in live.iter().enumerate() {
                if major {
                    assert_eq!(weak[id].is_some(), is_live, "round {round}, object {id}");
                }
                if !is_live {
                    continue;
                }
                let object = weak[id].unwrap().cast::<Node>().as_ref();
                assert_eq!(object.integer, id as u64);
                assert_eq!(object.edge, edges[id].map(|child| weak[child].unwrap()));
            }
            for (root, model) in roots.iter().zip(&model_roots) {
                assert_eq!(*root, model.map(|id| weak[id].unwrap()));
            }
        }
    }
}

#[test]
fn dead_nursery_buffers_release_unused_promotion_reservations() {
    let mut heap = heap();
    bytes(&mut heap, BYTES, &vec![123; 100_003]);
    unsafe {
        heap.collect_minor(|_| {}, |_| {}).unwrap();
    }
    assert_eq!(heap.used_bytes(), 0);
    assert_eq!(heap.capacity_bytes(), 0);
    assert_eq!(heap.stats().copied_bytes, 0);
}

#[test]
fn promotion_fills_partial_slabs_without_unnecessary_compaction() {
    let mut heap = RawHeap::new(
        Config {
        workers: 1,
            chunk_bytes: 128,
            steady_threshold_bytes: None,
            trace_collections: false,
            threshold_bytes: usize::MAX,
            stress: false,
        },
        &DESCRIPTORS,
    )
    .unwrap();
    let mut roots = Vec::new();
    for batch in [13, 3, 11, 5] {
        for _ in 0..batch {
            roots.push(node(&mut heap, roots.len() as u64, None));
        }
        unsafe {
            heap.collect_minor(
                |t| {
                    for p in &mut roots {
                        t.visit(p);
                    }
                },
                |_| {},
            )
            .unwrap();
        }
    }
    let addresses = roots.clone();
    unsafe {
        heap.collect(
            |t| {
                for p in &mut roots {
                    t.visit(p);
                }
            },
            |_| {},
        )
        .unwrap();
    }
    assert_eq!(
        heap.stats().compacted_bytes,
        0,
        "promotion left gaps before full slabs"
    );
    assert_eq!(roots, addresses);
    // Real fragmentation must still trigger compaction and release slabs.
    roots = roots.iter().step_by(4).copied().collect();
    unsafe {
        heap.collect(
            |t| {
                for p in &mut roots {
                    t.visit(p);
                }
            },
            |_| {},
        )
        .unwrap();
    }
    assert!(heap.stats().compacted_bytes > 0);
    assert_eq!(heap.capacity_bytes(), 128);
}

#[test]
fn startup_nursery_switches_to_steady_budget_after_first_collection() {
    let mut heap = RawHeap::new(
        Config {
            chunk_bytes: 128,
            threshold_bytes: 512,
            steady_threshold_bytes: Some(128),
            ..Config::default()
        },
        &DESCRIPTORS,
    )
    .unwrap();
    let mut root = node(&mut heap, 0, None);
    for i in 1..32 {
        root = node(&mut heap, i, Some(root));
    }
    assert!(!heap.collection_due(0));
    assert!(heap.collection_due(16));
    unsafe {
        heap.collect_minor(|t| t.visit(&mut root), |_| {}).unwrap();
    }
    assert_eq!(heap.young_bytes(), 0);
    assert_eq!(heap.old_bytes(), 512);
    for cycle in 0..3 {
        for i in 0..8 {
            assert!(!heap.collection_due(16));
            node(&mut heap, i, None);
        }
        assert!(heap.collection_due(16));
        unsafe {
            if cycle == 1 {
                heap.collect(|t| t.visit(&mut root), |_| {}).unwrap();
            } else {
                heap.collect_minor(|t| t.visit(&mut root), |_| {}).unwrap();
            }
            assert_eq!(root.cast::<Node>().as_ref().integer, 31);
        }
        assert_eq!(heap.used_bytes(), 512);
    }
}

#[test]
fn promoted_power_of_two_buffers_do_not_reserve_a_nursery_prefix() {
    let mut heap = RawHeap::new(Config::default(), &DESCRIPTORS).unwrap();
    let mut roots = Vec::new();
    unsafe {
        // Exactly one slab's worth of payload. Including the nursery header
        // in the old size class would double its backing allocation.
        for byte in 0..64u8 {
            let pointer = heap.allocate_slow(BYTES, 1024).unwrap();
            pointer.as_ptr().write_bytes(byte, 1024);
            roots.push(pointer);
        }
        heap.collect_minor(
            |tracer| {
                for root in &mut roots {
                    tracer.visit(root);
                }
            },
            |_| {},
        )
        .unwrap();
        assert_eq!(heap.capacity_bytes(), 64 * 1024);
        for (byte, root) in roots.iter().enumerate() {
            assert!(std::slice::from_raw_parts(root.as_ptr(), 1024)
                .iter()
                .all(|&value| value == byte as u8));
        }
        // Major relocation must still preserve lengths and contents.
        roots.drain(..32);
        heap.collect(
            |tracer| {
                for root in &mut roots {
                    tracer.visit(root);
                }
            },
            |_| {},
        )
        .unwrap();
        for (byte, root) in roots.iter().enumerate() {
            assert_eq!(*root.as_ptr().add(1023), (byte + 32) as u8);
        }
    }
}

#[test]
fn automatic_major_sweeps_dense_slabs_and_compacts_sparse_ones() {
    let drops = Rc::new(Cell::new(0));
    let mut heap = RawHeap::new(
        Config {
            threshold_bytes: 64,
            steady_threshold_bytes: Some(32),
            ..Config::default()
        },
        &DESCRIPTORS,
    )
    .unwrap();
    let mut roots = Vec::new();
    unsafe {
        for _ in 0..128 {
            let p = heap
                .allocate_slow(RESOURCE, size_of::<Rc<Resource>>())
                .unwrap();
            p.cast::<Rc<Resource>>()
                .as_ptr()
                .write(Rc::new(Resource(drops.clone())));
            roots.push(p);
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
        let dead = roots[0];
        roots.drain(..16);
        let retained = roots.clone();
        heap.collect_minor(
            |t| {
                for p in &mut roots {
                    t.visit(p);
                }
            },
            |r| {
                assert_eq!(r.lookup(dead), None);
            },
        )
        .unwrap();
        assert_eq!(heap.stats().major_collections, 1);
        assert_eq!(heap.stats().compacted_bytes, 0);
        assert_eq!(roots, retained);
        assert_eq!(drops.get(), 16);
        let capacity = heap.capacity_bytes();
        // Promotions reuse swept slots, then reach the next major trigger.
        for _ in 0..9 {
            let p = heap
                .allocate_slow(RESOURCE, size_of::<Rc<Resource>>())
                .unwrap();
            p.cast::<Rc<Resource>>()
                .as_ptr()
                .write(Rc::new(Resource(drops.clone())));
            roots.push(p);
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
        assert_eq!(heap.capacity_bytes(), capacity);
        let dead = roots[8];
        roots.truncate(8);
        let before = roots.clone();
        heap.collect_minor(
            |t| {
                for p in &mut roots {
                    t.visit(p);
                }
            },
            |r| {
                assert_eq!(r.lookup(dead), None);
            },
        )
        .unwrap();
        assert_eq!(heap.stats().major_collections, 2);
        assert!(heap.stats().compacted_bytes > 0);
        assert_ne!(roots, before);
        assert_eq!(drops.get(), 129);
    }
    drop(heap);
    assert_eq!(drops.get(), 137);
}

#[test]
fn forwarding_handles_tiny_padded_and_unaligned_payloads() {
    static KINDS: [Descriptor; 4] = [
        Descriptor {
            name: "tiny",
            fixed_size: Some(1),
            alignment: 1,
            trace: trace_bytes,
            drop: None,
        },
        Descriptor {
            name: "padded tiny",
            fixed_size: Some(1),
            alignment: 8,
            trace: trace_bytes,
            drop: None,
        },
        Descriptor {
            name: "empty",
            fixed_size: Some(0),
            alignment: 1,
            trace: trace_bytes,
            drop: None,
        },
        Descriptor {
            name: "unaligned",
            fixed_size: Some(9),
            alignment: 1,
            trace: trace_bytes,
            drop: None,
        },
    ];
    let mut heap = RawHeap::new(Config::default(), &KINDS).unwrap();
    let mut roots = Vec::new();
    unsafe {
        for (kind, descriptor) in KINDS.iter().enumerate() {
            for byte in 0..128u8 {
                let bytes = descriptor.fixed_size.unwrap();
                let pointer = heap.allocate_slow(kind, bytes).unwrap();
                pointer.as_ptr().write_bytes(byte, bytes);
                // Distinct root slots naming the same object exercise repeated
                // forwarding without rereading the overwritten source payload.
                roots.push((pointer, pointer, byte, bytes));
            }
        }
        heap.collect_minor(
            |t| {
                for (first, second, _, _) in &mut roots {
                    t.visit(first);
                    t.visit(second);
                }
            },
            |_| {},
        )
        .unwrap();
        roots.drain(..32);
        heap.collect(
            |t| {
                for (first, second, _, _) in &mut roots {
                    t.visit(first);
                    t.visit(second);
                }
            },
            |_| {},
        )
        .unwrap();
        for (first, second, byte, bytes) in roots {
            assert_eq!(first, second);
            assert!(std::slice::from_raw_parts(first.as_ptr(), bytes)
                .iter()
                .all(|&b| b == byte));
        }
    }
}
