# brix-gc

A precise, single-threaded generational collector used by the managed brix evaluator.
The `raw` module handles descriptor-driven heterogeneous objects and interior
pointers; `roots` provides stable scoped slots, registered vectors and owning
leases. The evaluator supplies exact layouts and safe rooted views. It has no
dependencies.

## Evaluator backend

`raw` bump-allocates into a copying nursery, using headerless typed spaces
for fixed-size objects and size prefixes for variable objects. Every survivor
is promoted at its first collection. Old objects live in segregated slabs:
one size class per fixed kind, four classes per size doubling for variable
objects, limiting rounding slack to 25%. Old payloads need no nursery prefix.
Slabs are at most 64 KiB unless one object needs more. A byte per old slot
records occupancy, marking and remembered state; variable slots also record
logical length. A pre-reserved address-to-slab table maps virtual-address
ranges to owners without binary searches on each old edge; it supports
unaligned slabs, shared granules and large allocations. Size-class slack is
included in capacity statistics. Virgin old slots use a bump cursor, so
reserving promotion capacity does not write to every potential destination;
swept slots are reused through an intrusive free list.

Minor collections trace roots, modified old objects, and nursery survivors.
They do not trace or move the rest of the old graph. The evaluator records
managed-edge mutations with an allocation-free write barrier; dirty pages
identify the old slots to trace. Since every young survivor is promoted, the
remembered set can be cleared after each successful collection. Unreachable
old objects, including their weak identities, can remain until a major GC.

Compacting major collections mark the full graph, compute forwarding destinations,
rewrite roots and live edges, repair weak references, drop dead owners, then
compact survivors in place within each size class. Packing into earlier slabs
releases empty slabs without allocating a second old heap. Interior pointers
retain their offsets, sharing and cycles survive, and owned resources are
dropped exactly once. An automatic major sweeps in place when at least 75%
of occupied slot capacity survives; it compacts when more is dead. Empty
slabs are returned in either case, and later promotions reuse swept holes.
Explicit `collect()` always compacts. Old addresses are not stable API identities.

The raw root callback runs twice during a compacting collection. Each root slot
and descriptor field must be visited exactly once per pass, and callbacks
must not dereference rewritten edges: final destinations are populated after
pointer repair. All promotion capacity and scratch are reserved before roots
change. A failed reservation leaves the graph intact but can retain empty
reserved slabs. Minor scratch scales with the nursery. A bitmap records which
objects have been forwarded; their retired source payloads hold the destination
pointers. Fixed objects smaller than a pointer use a separate forwarding table.
Variable-object boundaries use a bitmap and prefix counts for constant-time
interior lookup. The segmented mark stack reserves its worst-case capacity
before tracing but initializes only pushed entries, and queued slab/slot IDs
avoid another address lookup when popped. Major scratch additionally reserves
one forwarding word per old slot, initialized only when compacting.

`Tracer::relocate_fixed<T, KIND>` specializes nursery lookup, promotion, and
old-generation marking for known fixed-layout payload starts. Major marking
uses the known slot spacing without recovering object lengths or interior
offsets. The descriptor's size and alignment must
exactly match `T`, and the pointer must denote an object of `KIND`, not an
interior address. It shares forwarding metadata with generic tracing. Evaluator
cell fields use this path; variable layouts and slices use ordinary relocation.

`roots` supplies stable scoped slots, rooted vectors, and owning leases.
Registering a persistent root reuses free entries in constant time; dropping
its final lease removes reachability. Scoped roots share a packed, tagged bump
buffer in each frame; cleanup resets one cursor, and tracing visits only its
initialized slots. The evaluator's safe API prevents borrowed heap views from
crossing collection and rejects handles from other runtimes.

`Config::threshold_bytes` is the initial nursery trigger, 4 GiB in the
facade (1 MiB in the raw default). After the first successful collection,
`Config::steady_threshold_bytes` replaces it: the facade uses 512 MiB;
the raw default is `None`, which retains the initial threshold. Old objects
do not consume the nursery allowance. A failed collection does not change
the nursery phase. The larger initial nursery avoids collection for short
evaluations; the smaller steady nursery bounds collection scratch and the
temporary overlap between young payloads and their promotion destinations.
Automatic collection runs a major cycle once old occupancy reaches its
budget. After a major cycle, its live-size multiplier grows from 2 to 4 to 8
when at least 75% of examined old occupancy survives, and resets to 2 when
more garbage is reclaimed. With a steady nursery configured, additional old
growth is capped at the initial nursery allowance between major collections.
The configured threshold remains a minimum. Explicit `collect()` always
collects both generations. These are soft triggers, not memory limits.
`Stats` separates minor/major counts and time, promoted and compacted bytes,
and reports total
bytes moved, reclamation, retained occupancy, cumulative time (zero on wasm),
and peak backing capacity including collector metadata. `trace_collections`
prints occupancy and reclamation at each collection. Retained occupancy
after a minor cycle includes old garbage awaiting major collection.

## Run

```sh
cargo test -p brix-gc --offline
cargo clippy -p brix-gc --all-targets --offline -- -D warnings
```

## Measurements

For collection throughput independently of evaluator execution, run:

```sh
cargo run --release -p brix-eval --example gc_throughput -- 2000000 5
```

This uses the CLI's allocator and measures minor collection and a dense major
collection separately, excluding construction and teardown. It covers chains,
trees, shared graphs with randomly distributed references, mixed fixed and
variable objects, and young graphs referencing an existing old graph. Two
unreachable nodes accompany every newly allocated live node. CSV output
includes known live object/edge counts, managed bytes, nanoseconds per live
object, and effective managed MB/s. Graph shape and locality matter: bytes per
second alone do not describe the collector's work. These cases are diagnostic;
validate changes on full evaluator workloads as well.

For pointer-visit counts on evaluator workloads, build with diagnostic counters:

```sh
cargo build --release -p brix-cli -p brix-gc --features brix-gc/trace-counts
BRIX_TIMINGS=1 target/release/brix <command>
```

Each collection reports total visits, young-object first and repeated visits,
and old-object marking visits, with young visits broken down by descriptor.
These counters add work to the tracing path, so use a normal build for timing
comparisons. They are absent from normal builds. During compaction, total visits
also include the pointer-repair pass; first/repeated visit counts describe the
initial marking and promotion pass.

An experimental four-byte forwarding table roughly halved collection time on
the randomly shared synthetic graph, but slightly regressed the trustless
evaluation in alternating runs. Adding an initialization bitmap did not improve
that workload either. The collector therefore retains forwarding pointers in
retired payloads. Synthetic locality gains alone are insufficient evidence for
changing the default representation.

On the trustless flake-check workload, specializing cell-pointer lookup and
promotion reduced mean GC time from 17.24 s to 16.46 s (4.5%) in two alternating
runs per version on the Ryzen AI 9 365, restricted to CPUs 0 and 2. Collection
remained single-threaded. Minor GC fell from 12.91 s to 12.17 s; peak cgroup
memory stayed around 8.2 GiB under a 10 GiB limit with swap disabled. Collection
counts, promoted bytes, and the 181 successful output checks were unchanged.

A subsequent paired comparison of fixed-layout major marking reduced mean
major-GC time from 4.33 s to 4.04 s (6.6%), and total GC from 16.59 s to 16.34 s
(1.5%). This used two runs per version with the same CPU and memory limits;
peak memory and output checks remained unchanged. End-to-end timings were
variable and did not establish an application-level speedup.

A subsequent `perf` recording separated the 36 minor collections from the two
major collections using collection timing and major-only stack frames. Across
2,408 minor-GC samples, tracing callbacks and their inlined pointer handling
accounted for about 39% of cycles (self), promotion for 36% (inclusive), and the
collector driver/setup for 17% (self). These categories do not overlap. The
out-of-line copy routine alone accounted for only 4.5% (self), but that excludes
small inline copies and kernel work beneath copying. This is not evidence that
all copying costs only 4.5%: metadata lookup, forwarding, allocation, and graph
traversal need to be measured separately from bulk memory bandwidth.

An experimental per-size-class promotion cursor cached virgin slab runs and
batched free-space accounting. It passed correctness tests but did not reliably
improve trustless: in the reverse-order pair, minor GC took 12.49 s with the
cursor versus 12.36 s without it. The earlier pair had a much slower baseline,
so its apparent gain was not sufficient evidence to retain the extra machinery.
The experiment was reverted.

Specializing application pointers was a smaller, repeatable improvement. These
objects accounted for about 15% of promotions in the diagnostic run. Two
alternating trustless runs per version reduced mean minor GC from 12.29 s to
12.14 s (1.2%) and total GC from 16.48 s to 16.27 s (1.3%). Both pairs improved;
the 181 output checks and managed allocation statistics matched exactly. Peak
cgroup memory remained around 8.1–8.2 GiB. The evaluator's application fields now
use `relocate_fixed<RawApp, APP>`; generic pointers retain their existing path.

Remembered-set processing took about 0.08 s across all minor collections in
these runs (about 0.6% of minor-GC time). That phase includes scanning modified
old owners and immediate promotions from their edges, but not the subsequent
young-object traversal in the graph phase. Write barriers execute during
evaluation and are outside these GC phase timings. Removing old-to-young edges
would not eliminate classification of young-to-old edges.

## Validation and boundaries

Generational tests cover bounded copying as a retained graph grows, remembered
interior owners, cross-generation cycles, compaction of sparse slabs, strong
and weak pointer repair, variable lengths/alignment, randomized graph models,
and reservation failure before root rewriting. Doctests in `roots` include
compile-fail cases for its scoped API.

The local toolchain lacks Miri. An AddressSanitizer build was also attempted,
but this Rust installation lacks `librustc-stable_rt.asan.a`, so it could not
link. Neither check has passed; these remain necessary follow-ups for the
unsafe memory implementation. Ordinary tests and strict Clippy checks pass.

The evaluator integration uses the `raw` and `roots` backends, with exact layouts,
weak identity tables and resource destruction supplied by `brix-eval`.
Hard-limit recovery and a strict RSS budget are not implemented.
