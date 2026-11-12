# Precise garbage collector plan

Current design: a single-threaded, stop-the-world generational collector for
runtime objects. Bump-allocate into a copying nursery and promote survivors
into an old generation compacted in place during major collections when
fragmented; dense automatic majors sweep slots in place. Exact
write barriers track old-to-young edges. Concurrent collection is not used.

The generational collector and rooted evaluation path are implemented, including
the default facade and CLI. The original copying-only design and its historical
measurements below remain useful background; they do not measure this new policy.
The primary goals remain precise
reachability, fast allocation, and very little slowdown on evaluations that
never need a collection. Pause latency is secondary.

See [the proposed Rust API](GARBAGE_COLLECTOR_API.md) for concrete signatures,
root scopes, allocation and forcing examples, and the public session boundary.

The collector and its root registry live in
[`brix-gc`](../crates/brix-gc/README.md).

The default facade uses `managed_api::GcEvaluator`, which evaluates through
precise roots and can collect during thunk forcing, function calls, builtin
loops, and persistent attribute-set construction. Its opaque `GcValue` handles
retain values across calls; their final drop removes the root. Borrowed views
exclude mutating evaluator calls. The original reference API remains available
for low-level compatibility. Native flake callbacks reenter the managed runtime
through a scoped context with the same rooting rules. `brix::Config::gc` sets
the nursery thresholds (4 GiB initially, 512 MiB after the first successful
collection by default), chunk size, and stress mode.
The steady nursery budget stays fixed as the retained graph grows. Major collection
runs when old occupancy reaches its budget. Its retained-size multiplier
increases from 2 to 4 to 8 when at least 75% survives, and resets to 2 after
higher reclamation; the initial nursery threshold remains a minimum. With a
steady nursery configured, the initial allowance also caps additional old
growth between major cycles. Explicit collection
always runs a compacting major cycle. These triggers are not hard caps.

The implementation includes exact descriptors for all runtime object layouts,
interior string slices, scoped and persistent roots, weak identity sets that
follow relocation, and exact-once error destruction. Expression and builtin
regressions collect before each allocation boundary, including the preflight
of a bounded constructor batch. Compiler/symbol/host
metadata remains permanent and is outside the collectible heap budget.

Regression coverage includes the workspace suites, differential comparison
with the reference evaluator and Nix, and forced collection at allocation
boundaries. Batched constructors reserve their full pending extent, reload
rooted inputs after collection, and initialize without intervening collection.
The implementation reaches timing parity within run-to-run variance on the
measured CLI workload below. This does not establish zero overhead on other workloads.
Performance comparisons must use the same release workload and report
collection time separately from ordinary evaluation; avoiding collection alone
does not establish low overhead.

### Generational regression and correction (2026-11-10)

The first generational implementation changed the facade's default nursery
from 4 GiB to 64 MiB without measuring the CLI workload. That was a regression:
`brix flake show ~/git/nix` went from approximately 1.2 seconds to 20 seconds.
A diagnostic reproduction took 21.15 seconds, including 19.73 seconds in 18
collections. A CPU profile attributed 45.46% of sampled cycles to the old
heap's binary address lookup, with further time in repeated owner lookup and
pointer repair. Correctness tests alone did not establish acceptable runtime
performance.

Corrections retain the generational design and restore the 4 GiB facade
budget. An address-to-slab table replaces old-heap binary searches, major
tracing checks old ownership before searching the nursery, and compaction
repair avoids a duplicate owner lookup. Promotion fills older partial slabs
before new slabs, in allocation order. The major budget now backs off when
at least 75% of the old heap survives. Minor/major times, promotion, compaction
and old reclamation are reported separately under `BRIX_TIMINGS=1`.

Release measurements on the same host, local flake and warm caches, alternating
binary order and verifying byte-for-byte identical stdout:

| Configuration | Previous binary | Corrected binary | Measured pairs |
| --- | ---: | ---: | ---: |
| Default budget, preserved copying binary vs generational | 1.3046 s | 1.3125 s | 8 |
| Forced 64 MiB nursery, initial vs corrected generational | 24.30 s | 6.08 s | 3 |

Values are median wall time with timings disabled and a separate warmup pair.
The default difference is +0.61%; both runs perform zero collections, and
median peak RSS is approximately 1452 MiB for each. This compares end-to-end
binaries, not isolated barrier instruction cost. The 64 MiB trials had much
more wall-time variation (including one 47 s initial run); median user CPU
was 21.58 s initially and 5.04 s corrected, while median peak RSS fell from
1958 to 1716 MiB. These are measurements of this workload, not a general
speedup guarantee.

In a separate corrected 64 MiB diagnostic trial, 16 minor collections took
2.06 s and two major collections took 2.02 s. The heap promoted 843 MiB and
compacted 632 MiB, but reclaimed only 0.60 MiB of old objects. Most collected
young objects survive and nearly all old objects remain live: an aggressive
small nursery is still a poor policy for this workload. Restoring the default
budget removes its regression; it does not make collection free. Keep the
threshold configurable and measure genuinely long-lived workloads separately.

Reproduce ordinary runs with `cargo build --release -p brix-cli --offline`
and `target/release/brix flake show ~/git/nix`. Use
`BRIX_GC_THRESHOLD_BYTES=67108864` to force the smaller nursery, and
`BRIX_TIMINGS=1` for collection diagnostics; use timings disabled for the
paired end-to-end comparison. Regression tests also check that promotion
leaves no avoidable gaps, fully live packed slabs require no compaction,
minor collections do not retrace unchanged old graphs, and major-budget
backoff resets after substantial reclamation.

### CLI performance investigation (2026-11-10)

The acceptance workload is `target/release/brix flake show ~/git/nix`, compared
with the release evaluator at `77ba011` on the same host and caches. Repeated
runs have identical stdout. The final three-way comparison disabled
`BRIX_TIMINGS` and measured each binary 12 times, using all six order
permutations twice. It compared the managed binary with both a rebuilt baseline
and the preserved original baseline binary.

| Comparison | Reference median | Managed median | Difference |
| --- | ---: | ---: | ---: |
| Three-way, rebuilt baseline | 1.1013 s | 1.0968 s | -0.41% |
| Three-way, preserved original | 1.1120 s | 1.0968 s | -1.37% |
| Independent 16-pair confirmation, rebuilt baseline | 1.1154 s | 1.1182 s | +0.25% |

Separate diagnostic trials recorded about 10.517 billion managed CPU
instructions versus 10.33 billion for the reference, peak RSS of about 1448 MiB
versus 1567 MiB, and zero collections. Precise collection remains enabled with
a 4 GiB initial threshold; this workload finishes below it, so these timings
measure allocation, rooting and evaluation overhead rather than copying pauses.

The repeated measurements support timing parity within run-to-run variance on
this workload, not a speedup claim. Earlier eight-pair comparisons ranged from
-0.05% against the rebuilt baseline to +1.85% against the preserved original.
Managed execution still uses more instructions. These results do not establish
zero overhead on every workload or the cost of collecting a large live graph.
Timing, instruction and memory measurements come from separate runs and should
not be combined into an exact cost decomposition.

Final validation passed: 84 workspace test suites reported 1213 passed, zero
failed and 64 ignored; strict workspace Clippy for all targets, the evaluator's
`wasm32-unknown-unknown` check, formatting and diff checks also passed. Running
the same real workload with a 256 MiB initial threshold forced two collections
and reclaimed 215,874,416 bytes (about 206 MiB), with identical stdout. This
checks collection during the workload as well as the default-budget timing
case; it does not imply the same elapsed time at the smaller budget.

Retained performance changes include:

* Packed tagged roots, smaller initial root chunks, cached scope entry and
  constant-time cleanup. Existing root capabilities open nested scopes without
  cloning the registry; redundant wrapper scopes and result snapshots are
  removed while bounded loop scopes remain.
* Fused application-cell construction, fixed-size allocation paths, direct
  environment initialization, and bounded batches for lazy `mapAttrs` and
  eligible static attribute sets. Batch inputs remain rooted across a single
  preflight collection and are reloaded before initialization.
* Whole-leaf sharing during attribute-set merges, preserving right-biased
  values and source positions. Only complete original leaves are shared;
  consumed fragments are copied. Copied leaves fill an uncommitted reservation
  directly and publish its exact initialized extent, avoiding a scratch copy.
  The merge cursor caches whole-leaf identity without changing other traversal
  cursors.
* String concatenation allocates its bytes once and shares context byte edges,
  with one preflight for bytes, contexts and the string header. Literal caches
  use dense IDs, and compiled closures use lazy per-file arrays instead of a
  global hash lookup or an eagerly allocated duplicate reference-runtime table.
* Forcing consumers that only need a cell's memoized value avoid a separate
  value snapshot. Forcing retains just a rooted environment and immutable
  node ID while a thunk is blackholed; applications retain their original
  snapshot for restoration. Panic and failed error-allocation paths restore
  the suspension from roots.
* The hot lazy-cell constructor is inlined, compiled nodes use the existing
  root capability to enter their temporary frames, and root allocation skips
  redundant alignment rounding for word-aligned root payloads. Over-aligned
  roots retain their alignment checks. Batch collection decisions use the raw
  heap's stress check directly instead of repeating the same immutable flag.

The facade uses 2 MiB chunks; the raw collector's default remains 64 KiB.
Large chunks use at least 4 KiB alignment to avoid allocator size-class inflation
on the measured host, and use the CLI allocator's existing pre-faulting policy.
These choices trade refill frequency against unused capacity and are not a
portable performance guarantee. Experiments widening or tagging value-root
handles to inline scalars were reverted after measurement.

Earlier memory growth included about 560 MB of allocations from missing
attribute-name and empty-value reuse. Restoring that reuse removed those
allocations. Shared context storage and whole-leaf merging further reduce
allocation; round 23's occupied runtime heap was about 1.247 GB, versus about
1.515 GB before leaf sharing. Occupied arena/heap bytes, reserved capacity, and
peak RSS are distinct metrics; do not treat capacity or the last collection's
live count as current live data.

## Why copying

A non-moving collector can bump-allocate through reclaimed free spans, but
fragmentation can shorten those spans and increase refill costs. Copying
compacts survivors and gives allocation large contiguous runs again. It also
fits this evaluator's many short-lived cells, environments and intermediate
attribute sets, although actual survival rates need measurement.

Stable addresses are useful in today's implementation, but its arena-long
Rust references already need redesign for *any* reclaiming collector.
Making explicit roots updateable now is preferable to choosing non-moving
collection solely to preserve those references.

The costs are relocation work and evacuation headroom. A collection visits
and copies the live graph; an evaluation retaining most of its heap may
benefit little. Reserving destination address space need not commit all of
it, but both old objects and copied survivors occupy memory during a cycle.
This is not a strict low-memory design without explicit headroom accounting.

`~/git/rsgc` is useful for region management, allocation buffers, object
descriptors and collector accounting. Its README describes semi-conservative
stack discovery, and its collector includes concurrent machinery. Neither
belongs here. In particular, no guessing pointers from Rust stacks or registers.

## Existing code that shapes the design

* `brix-eval/src/lib.rs` exports `Arena = bumpalo::Bump`.
  `Evaluator` and most runtime types borrow it for the entire session.
* `Value` and `ValueCell` are 16 bytes; `ValueRef` is a Rust reference.
  `Env` and `Bindings` are pointer-sized handles with custom variable-sized
  layouts. Preserve those hot payload sizes where possible.
* `bindings.rs` shares B+-tree nodes and slices of attribute arrays.
  `NixStr` shares byte and context slices, including interior substrings.
* `closure.rs` allocates opaque Rust closures in the same arena as values.
  Those closures can capture `Lazy::Const` cells and linked code.
* `force` replaces a thunk or application with `Blackhole` before evaluation.
  Its previous payload then survives only through local variables.
* Facade cursors now use owning managed leases and release their roots on
  last drop. The facade and CLI use the managed runtime by default.
* `deep_seen`, printers and deep forcing use address-based visited sets.

The main implementation effort is therefore the root and borrowing model,
not the copying loop.

## Heap organization and allocation

Separate two allocation domains:

1. **Session metadata:** linked code, compiled Rust closures, symbols,
   source metadata and literal descriptions. These remain permanent; managed
   literal values instead live in the runtime heap, retained by per-machine
   root leases. Permanent memory remains until session teardown and must be
   reported separately from collectible memory.
2. **Runtime heap:** cells, environments, lists, attribute blocks and tree
   nodes, applications, runtime strings, paths and their backing storage.
   These participate in copying collection.

Use a segmented semispace design: the active space owns chunks, each with a
bump frontier; collection builds a new space from spare or newly obtained
chunks. This avoids requiring one enormous contiguous reservation. At the
end of collection, old chunks become reusable spare chunks; release surplus
committed pages rather than retaining every historical peak indefinitely.

The ordinary allocation path is alignment, bounds check, pointer advance,
initialization, and the minimum metadata stores needed to identify objects.
Budget checks and chunk acquisition belong on the refill path. Large requests
must also check the budget before acquiring a dedicated chunk. No heap walk,
clock read, OS memory query, free-list search or root-table allocation on
every object allocation.

Prototype compact descriptors before fixing a layout:

* Fixed-size, common types such as `ValueCell` can use typed chunks with an
  implicit size/type, avoiding a header on every 16-byte cell.
* Variable-sized objects need a kind and allocation extent, including
  alignment padding. A compact prefix is the initial implementation.
* Forwarding information can overwrite dead from-space payload where it
  fits; use side metadata for smaller allocations. Never require adding a
  second pointer to every live handle just for forwarding.
* Chunks need enough allocation-boundary metadata to resolve known interior
  pointers. Include its initialization and memory cost in benchmarks.

Allocation metadata is necessary even before the first collection. Do not
promise today's exact allocation cost or defer descriptors until pressure
appears: the collector cannot reconstruct unrecorded object layouts later.

Initially copy large objects too, using dedicated chunks where necessary.
A separately traced, non-moving large-byte-buffer space is a later option
if large strings dominate copying cost; it is not required for correctness
or part of the initial collector.

## Roots, handles and Rust borrowing

Replace arena-long runtime references with branded heap handles. A handle
inside a heap object is a direct pointer that the collector can rewrite.
Avoid a permanent indirection table for every runtime object.

References held across a possible collection live in explicit, updateable
root slots. Use scoped root frames backed by reusable storage, with typed
slots for values, cells, environments, bindings and slice descriptors.
Frame entry and exit should be a few stores, not a malloc or reference-count
operation per temporary. Frames must be unwound on errors and panics and
remain valid across `stacker` stack-segment changes. Never register pointers
to movable Rust locals or a `Vec` buffer that can reallocate.

Rooting must be in place from the start of execution, even when the budget
is large. It cannot be switched on only when a collection becomes necessary.

The safe interface must enforce these rules:

* Heap identity and access lifetime are different. A session-lifetime brand
  identifies the heap; a short access borrow permits dereferencing it.
* Ordinary Rust references into moving storage cannot cross a collecting
  call. An access/no-collection guard prevents collection while such a
  reference exists. APIs that may collect require exclusive access to the
  corresponding mutator capability.
* Save a value in a root before a collecting call and reload it afterwards.
  Updating a root slot does **not** update a previously copied pointer.
* Root every live payload, not just its mutable owner. If a cell is replaced
  with `Blackhole`, the previous environment/application must have its own
  root. Similarly, an earlier copied `Value` can outlive a cell overwrite.
* Temporary `Vec`, `SmallVec`, maps, work queues, iterators and accumulators
  must use rooted storage or retain and reload their backing owners.
* Public callers receive owning root handles or owned snapshots, never an
  unrestricted session-long `&ValueCell` into collectible space. Host
  callbacks follow the same contract.

Build a small API prototype and compile-fail tests before migrating the
evaluator. It must demonstrate nested evaluation, returning a rooted value,
mutable containers, and rejection of a borrowed string held across a force.
The exact signatures should follow that prototype rather than preserving
`&self` everywhere through unchecked interior mutation. Keep unsafe memory
operations confined to the heap/root implementation and audited layouts.

Collection occurs on allocation slow paths only when the caller has exposed
all its live roots. Construction is a bounded no-collection operation:
reserve capacity first with its inputs rooted, initialize the entire object,
then publish it. Builders that evaluate callbacks must instead use rooted
intermediate storage between calls; never trace partly initialized slots.
Do not defer collection for an entire recursive evaluation or unbounded
builtin loop. Those paths need safe allocation boundaries too.

## Exact tracing and relocation

Each allocation kind supplies an exact visitor over pointer-bearing slots.
Visitors both discover and rewrite edges. Scalars and arbitrary byte data
are never scanned for possible addresses.

| Allocation | Edges visited |
| --- | --- |
| Value cell | Only the active `Value` variant's payload |
| Environment | Parent and all initialized value slots |
| Flat bindings | Attribute value cells |
| Tree bindings/nodes | Root, child arrays, leaves and attribute values |
| List | Element storage and element cells |
| App / partial primop application | Function and argument |
| String | Byte storage, context array, context path/output storage |
| Path | Path byte storage |
| Failed value | Owned error wrapper, with appropriate destruction |
| Bytes, symbols, numeric arrays | No GC edges |

Known interior slices are still precise references. Preserve the allocation
owner and slice offset/length when relocating them. For the initial layout,
resolve interior addresses through chunk allocation-boundary metadata during
collection and rebuild the pointer using the destination allocation's
provenance. Treat zero-length slices canonically; do not attempt owner lookup
on dangling empty-slice pointers or ambiguous one-past-end addresses.
Permanent and static storage must be explicitly distinguishable from moving
storage. This applies to B+-tree leaf slices as well as string substrings.

Collector cycle:

1. Stop at a safe allocation boundary and retire the allocation buffer.
2. Enumerate evaluator roots, active root frames, public handles and explicit
   permanent-to-runtime edges.
3. Evacuate each reachable object once, install its forwarding address, and
   rewrite the referring slot. An iterative scan queue processes copied
   objects and rewrites their outgoing edges; cycles and sharing survive.
4. Update weak identity tables using forwarding information, removing entries
   for dead objects. Rebuild hash tables whose keys changed.
5. Destroy owned resources in dead objects, retire from-space, and resume
   bump allocation after the copied survivors in the new space.

Tracing and destruction cannot invoke evaluation or allocate GC objects.
Rust-owned payloads such as the `Rc<Error>` stored for `Value::Failed` need
explicit relocation/drop rules: relocation transfers ownership, so do not
drop a forwarded source copy. Drop dead wrappers exactly once. User finalizers,
resurrection and arbitrary `Drop` callbacks are outside the initial scope.

## Permanent code and opaque closures

Keeping a Rust closure alive does not make its captured pointers traceable.
Enforce that compiled closures capture only permanent code/literal data,
IDs, other permanent closures, or explicit rooted handles. The managed
runtime captures dense literal IDs and imports literal values into each
machine's heap on demand. A machine retains those cells through persistent
root leases; compiled closures never capture their moving addresses. Lazy
per-file closure caches remain permanent and separate from these runtime
values. Any mutable permanent cell that can gain a runtime edge must be in
an explicit root registry.

Audit every closure constructor and primop registration. Extension callbacks
must register heap captures explicitly; the existing arbitrary `dyn Fn`
interface cannot silently accept hidden raw runtime references. Never trace
the bytes of a Rust closure as a substitute. Code unloading can be a later
project; this first collector does not reclaim linked modules or symbols.

## Evaluator state and identity

Root constants, base environment, builtins, internal functions,
`derivation_fn`, symbol-value caches, file/import caches and every other
runtime edge in evaluator state. Audit string-bearing cache keys and values,
including `path_copies` and the store-path interner. Rebuild maps if key hashes
depend on relocated addresses; content-hashed keys still need their pointers
updated through an appropriate API. Release `RefCell` borrows before collection
so root enumeration cannot panic or alias a mutable borrow.

Keep import/file memoization strong initially: dropping it could repeat
evaluation and observable effects. Report its retained memory so a large
live cache is not mistaken for a collector failure. Cache eviction is a
separate semantic decision.

Replace the session's append-only cursor table with reusable root slots.
`Cursor`, `Flake` and cached schema results own slot leases; cloning retains
a lease and dropping the last owner releases it. Generation-tagged slot IDs
prevent stale reuse. The public lease can use single-threaded reference
counting without putting reference counting on internal heap edges.

Address-based visited sets need explicit GC treatment:

* `deep_seen` becomes a weak identity table. At collection, relocate keys for
  surviving cells and discard dead keys before addresses can be reused.
* Printer/deep-walk visited tables register for relocation while active.
  Keep an identity alive when the operation's semantics require remembering
  an already visited object even after other edges disappear.
* Equality and placeholder checks compare current handles reloaded after
  safepoints. Numeric addresses must not escape as persistent identity.

Do not merely clear every visited set on collection: that can change repeated
value printing or cause cycles to be traversed again. Stable object IDs are
an alternative if table relocation proves expensive, but should not add an
unmeasured header/counter cost to every allocation.

## Trigger policy and memory limits

Use an explicit managed-heap budget, not continuous RSS polling. The current
generational policy is:

* Nursery threshold: 4 GiB initially, then 512 MiB after the first successful
  collection in the facade, configurable. Old objects do not consume this
  budget; survivors are promoted after one collection. A failed collection
  leaves the nursery phase unchanged.
* Major threshold: initially the configured threshold, then the larger of
  that threshold and retained old occupancy times a multiplier. The multiplier
  starts at 2, doubles up to 8 when at least 75% of old occupancy survives a
  major cycle, and resets to 2 when reclamation exceeds 25%. This avoids
  repeatedly marking and compacting a growing, almost entirely live graph.
  With a steady nursery configured, additional old growth between major
  collections is capped at the initial nursery allowance, preventing this
  multiplier from postponing reclamation by many gigabytes.
* Minor collections trace exact roots, modified old slots, and promoted young
  objects. Major collections mark all reachable old objects. Automatic majors
  sweep when at least 75% of occupied slot capacity survives, allowing later
  promotions to reuse holes; otherwise they compact. Explicit majors compact.
* Include pending allocation in nursery checks on refill and at batch safepoints.
  Large requests can overshoot the soft budget; an empty nursery is not
  repeatedly collected to try to make an oversized allocation fit.
* Compaction packs each fixed-kind/variable-size class into earlier slabs and
  releases empty slabs. It reserves forwarding metadata, not a second old heap.

The threshold controls when to collect; it is not a total-process memory cap.
Account separately for active space, destination/spare space, descriptors,
root storage, permanent metadata and external Rust allocations. A configured
heap cap must include evacuation headroom, not just the allocation budget.

Reserve destination capacity before changing roots. Under a tight cap, use
a precise mark/count prepass to establish the survivor size and destination
requirement before evacuation. If survivors plus the pending request cannot
fit, return a defined allocation-limit error before rewriting the graph.
Never resume evaluation after a partially failed evacuation. OS allocation
failure and collector bookkeeping failure need a documented fail-stop path;
a heap cap does not guarantee protection from system OOM.

Expose collection count, allocated/copied/reclaimed/live bytes, permanent
bytes, peak collector footprint and pause time through existing statistics.
Production timing work happens only during collection; stress scheduling
and detailed allocation diagnostics are opt-in.

## Implementation sequence and acceptance

1. **Baseline and API experiment.** Record object-size/allocation profiles,
   current timings and peak memory. Prototype roots, access lifetimes,
   interior slices and a tiny relocating heap; prove the public interface
   cannot retain a dereferenceable stale reference across collection.
2. **Allocation split and descriptors.** Separate permanent metadata from
   runtime allocations. Implement chunk allocation and exact descriptors
   with collection disabled. Preserve hot value layouts; measure metadata
   cost before committing to prefixes versus typed chunks.
3. **Root migration.** Convert evaluation, compiled closures, builtins,
   coercion, imports, serialization and facade callbacks. Implement cursor
   leases and relocatable identity tables. Keep production collection off
   until every allocation kind and collecting call path is covered.
4. **Copying collector.** Add forwarding, iterative traversal, resource
   destruction, space reuse and destination/headroom checks. Enable tiny-heap
   stress runs before realistic workloads.
5. **Policy and tuning.** Add adaptive budgets and statistics, run the full
   correctness suites, then tune allocation metadata, root frames and chunk
   sizes against the same baseline.

Correctness tests must force collections at every eligible slow path and at
deterministically randomized boundaries, including while forcing blackholed
thunks, applying primops, growing temporary containers, entering another
stack segment, printing cycles, handling errors, importing files, processing
compiled images, and returning through host callbacks. Explicitly test shared
substrings/context arrays, shared tree leaves, dead cycles, cursor clone/drop,
weak visited entries, address reuse and dropping moved/dead error wrappers.
Poison or protect retired storage in debug stress mode. Use Miri on small
heap/root/layout tests and sanitizers on evaluator stress workloads.

Run existing language/differential/image suites in the repository's devshell,
with stress GC as well as normal budgets. Check byte-for-byte output, error
behavior, evaluation order and repeated-value markers, not just final values.

Measure the existing synthetic benchmarks plus lib.version, hello, numpy,
firefox and a NixOS toplevel, with interleaved repeated release runs:

* **No collections:** full descriptors and rooting enabled, large budget.
  Target no statistically meaningful slowdown, with a provisional 2–3%
  regression ceiling on representative workloads. This is an acceptance
  target, not a promise; current benchmark noise is around that range.
* **Collections required:** compare elapsed time, peak committed memory/RSS,
  copied/live bytes and pause distribution at several budgets. Repeatedly
  allocate and drop values in one session to demonstrate bounded runtime
  memory rather than simply surviving one evaluation.
* **High survival:** retain most objects and verify sensible growth or a
  limit error instead of a collection storm. Attribute remaining memory to
  roots, permanent code and external caches.

If the overhead target fails, profile root traffic and layout costs first.
The generational implementation addresses repeated high-survival copying.
Further work includes measuring barrier costs and tuning promotion age and
large-object storage. Keep non-moving collection
as a fallback justified by workload measurements, not the starting default.
