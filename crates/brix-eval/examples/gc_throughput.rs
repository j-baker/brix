//! Controlled GC graphs. Run with `cargo run --release -p brix-eval
//! --example gc_throughput -- [nodes=2000000] [rounds=5]`.
//! Allocation and graph construction are outside the measured intervals.
//! Every live node is accompanied by two unreachable nodes. Results are CSV;
//! these synthetic graphs isolate costs, not predict application runtimes.
use std::hint::black_box;
use std::mem::{align_of, size_of};
use std::ptr::NonNull;
use std::time::Instant;

use brix_gc::raw::{Descriptor, RawHeap, Tracer};
use brix_gc::Config;

brix_alloc::install!();
type Pointer = NonNull<u8>;

#[repr(C)]
struct Node {
    left: Option<Pointer>,
    right: Option<Pointer>,
    value: u64,
}

unsafe fn trace_node(pointer: Pointer, _: usize, tracer: &mut Tracer<'_>) {
    // SAFETY: NODE allocations contain an initialized Node.
    let node = unsafe { &mut *pointer.cast::<Node>().as_ptr() };
    for edge in [&mut node.left, &mut node.right].into_iter().flatten() {
        unsafe { tracer.visit(edge) };
    }
}

unsafe fn trace_edges(pointer: Pointer, bytes: usize, tracer: &mut Tracer<'_>) {
    // SAFETY: EDGES allocations contain bytes / size_of::<Pointer>() pointers.
    let edges = unsafe {
        std::slice::from_raw_parts_mut(
            pointer.cast::<Pointer>().as_ptr(),
            bytes / size_of::<Pointer>(),
        )
    };
    for edge in edges {
        unsafe { tracer.visit(edge) };
    }
}

unsafe fn trace_bytes(_: Pointer, _: usize, _: &mut Tracer<'_>) {}
const NODE: usize = 0;
const EDGES: usize = 1;
const BYTES: usize = 2;
static DESCRIPTORS: [Descriptor; 3] = [
    Descriptor {
        name: "node",
        fixed_size: Some(size_of::<Node>()),
        alignment: align_of::<Node>(),
        trace: trace_node,
        drop: None,
    },
    Descriptor {
        name: "edges",
        fixed_size: None,
        alignment: align_of::<Pointer>(),
        trace: trace_edges,
        drop: None,
    },
    Descriptor {
        name: "bytes",
        fixed_size: None,
        alignment: 1,
        trace: trace_bytes,
        drop: None,
    },
];

#[derive(Clone, Copy, PartialEq, Debug)]
enum Shape {
    Chain,
    Dag,
    Tree,
    Mixed,
    OldEdges,
}

fn node(heap: &mut RawHeap, left: Option<Pointer>, right: Option<Pointer>, value: u64) -> Pointer {
    // SAFETY: initialize before another heap allocation; these calls cannot collect.
    unsafe {
        let p = heap.allocate_slow(NODE, size_of::<Node>()).unwrap();
        p.cast::<Node>().as_ptr().write(Node { left, right, value });
        p
    }
}

fn run(shape: Shape, nodes: usize, round: usize) {
    let mut heap = RawHeap::new(
        Config {
            chunk_bytes: 2 * 1024 * 1024,
            // The first explicit collection is minor (old heap empty), the next
            // is an automatic major. Raw allocation never checks this trigger.
            threshold_bytes: 1,
            ..Config::default()
        },
        &DESCRIPTORS,
    )
    .unwrap();
    let mut old_root = None;
    let mut old_history = Vec::new();
    if shape == Shape::OldEdges {
        for i in 0..nodes / 4 {
            old_root = Some(node(&mut heap, old_root, None, i as u64));
        }
        // Seed and mark the old generation outside the measured interval.
        // Its major trigger then allows one minor before the next major.
        for _ in 0..2 {
            unsafe {
                heap.collect_minor(|t| t.visit(old_root.as_mut().unwrap()), |_| {})
                    .unwrap();
            }
        }
        let mut current = old_root;
        while let Some(pointer) = current {
            old_history.push(pointer);
            current = unsafe { pointer.cast::<Node>().as_ref().left };
        }
    }
    let old_before = heap.old_bytes();
    let mut history = Vec::with_capacity(nodes);
    let mut seed = 0x123456789abcdef0u64;
    let mut objects = nodes;
    let mut edges = 0;
    let mut expected_live =
        old_before + nodes * heap.allocation_extent(NODE, size_of::<Node>()).unwrap();
    for index in 0..nodes {
        node(&mut heap, None, None, 0);
        node(&mut heap, None, None, 0);
        let logical = if shape == Shape::Tree {
            nodes - index - 1
        } else {
            index
        };
        let (left, mut right) = if shape == Shape::Tree {
            let child = |i: usize| (i < nodes).then(|| history[nodes - i - 1]);
            (child(logical * 2 + 1), child(logical * 2 + 2))
        } else {
            (history.last().copied(), None)
        };
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        if index != 0 && matches!(shape, Shape::Dag | Shape::Mixed) {
            right = Some(history[seed as usize % index]);
        }
        if shape == Shape::OldEdges {
            right = Some(old_history[seed as usize % old_history.len()]);
        }
        if shape == Shape::Mixed && index != 0 && index % 4 == 0 {
            let mut references = vec![left.unwrap(), right.unwrap()];
            if index % 16 == 0 {
                let bytes = 64 + index % 193;
                // SAFETY: the buffer owns initialized pointer-free bytes.
                let buffer = unsafe {
                    let p = heap.allocate_slow(BYTES, bytes).unwrap();
                    p.as_ptr().write_bytes(0x5a, bytes);
                    p
                };
                references.push(buffer);
                objects += 1;
                expected_live += heap.allocation_extent(BYTES, bytes).unwrap();
            }
            edges += references.len();
            // SAFETY: all references are initialized live heap pointers.
            let array = unsafe {
                let p = heap
                    .allocate_slow(EDGES, references.len() * size_of::<Pointer>())
                    .unwrap();
                std::ptr::copy_nonoverlapping(
                    references.as_ptr(),
                    p.cast::<Pointer>().as_ptr(),
                    references.len(),
                );
                p
            };
            right = Some(array);
            objects += 1;
            expected_live += heap
                .allocation_extent(EDGES, references.len() * size_of::<Pointer>())
                .unwrap();
        }
        edges += usize::from(left.is_some()) + usize::from(right.is_some());
        history.push(node(&mut heap, left, right, logical as u64));
    }
    let mut root = *history.last().unwrap();
    drop(history); // Only root is registered; construction handles aren't roots.
    let young = heap.young_bytes();
    for generation in ["minor", "major"] {
        let start = Instant::now();
        unsafe {
            heap.collect_minor(
                |tracer| {
                    tracer.visit(&mut root);
                    if let Some(old) = &mut old_root {
                        tracer.visit(old);
                    }
                },
                |_| {},
            )
            .unwrap();
        }
        let seconds = start.elapsed().as_secs_f64();
        let live = heap.used_bytes();
        assert_eq!(live, expected_live, "collection changed graph reachability");
        let root_value = unsafe { root.cast::<Node>().as_ref().value };
        assert_eq!(
            root_value,
            if shape == Shape::Tree {
                0
            } else {
                nodes as u64 - 1
            }
        );
        black_box(root);
        let (objects, edges, live) = if generation == "major" {
            (
                objects + old_history.len(),
                edges + old_history.len().saturating_sub(1),
                live,
            )
        } else {
            (objects, edges, live - old_before)
        };
        println!("{shape:?},{round},{generation},{objects},{edges},{young},{live},{seconds:.6},{:.3},{:.3}",
            seconds * 1e9 / objects as f64, live as f64 / seconds / 1e6);
    }
    let expected = if shape == Shape::OldEdges { 2 } else { 1 };
    assert_eq!(heap.stats().minor_collections, expected);
    assert_eq!(heap.stats().major_collections, expected);
}

fn main() {
    brix_alloc::init().unwrap();
    let mut args = std::env::args().skip(1);
    let nodes = args.next().map(|s| s.parse().unwrap()).unwrap_or(2_000_000);
    let rounds = args.next().map(|s| s.parse().unwrap()).unwrap_or(5);
    assert!((16..=4_000_000).contains(&nodes));
    assert!((1..=100).contains(&rounds));
    println!("shape,round,generation,live_objects,edges,nursery_bytes,live_bytes,seconds,ns_per_live_object,live_MB_per_second");
    let shapes = [
        Shape::Chain,
        Shape::Dag,
        Shape::Tree,
        Shape::Mixed,
        Shape::OldEdges,
    ];
    for round in 0..rounds {
        for offset in 0..shapes.len() {
            run(shapes[(round + offset) % shapes.len()], nodes, round);
        }
    }
}
