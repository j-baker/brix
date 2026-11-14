# Parallel evaluation

`brix flake check` evaluates independent schema leaves on several OS workers.
How many is Nix's `eval-cores` setting, read from `nix.conf` (and
`NIX_CONFIG`) with the command line on top: `--eval-cores 4` or
`--option eval-cores 4` gives four. Unset or 0, it is the number of hardware
threads on the machine's performance cores (`brix_eval::cpus`): workers on
slower efficiency cores, or beyond that count, made the check slower in
measurement. `eval-cores = 1` uses the original serial evaluator. Other
commands always use the serial evaluator. Parallelism is explicit: ordinary
Nix function calls, attribute selection, and list construction remain lazy
and synchronous.

## Selecting a backend

The `brix-eval-parallel`, `brix-parallel`, and `brix-cli-parallel` packages
compile the corresponding original sources with `cfg(brix_parallel)`. They
are separate packages rather than a Cargo feature, so Cargo feature unification
cannot add synchronization to the serial backend. The CLI selects a backend
once, before constructing the session. Other commands continue using the serial
backend. Rust callers can select the parallel packages explicitly, aliasing
their dependencies as `brix_eval` or `brix`.

Serial cells remain 16 bytes. Serial counters, metadata cells, reference counts,
arena allocation, and root frames keep their ordinary unsynchronized types.
There is no per-force mode branch, atomic publication word, GC rendezvous poll,
or worker pool in the serial evaluator. Building both variants increases binary
size and compilation/linking work; this is the tradeoff for retaining a
statically specialized serial path.

## Jobs and thunk ownership

`GcEvaluator::force_many` forces a supplied slice of values to WHNF, preserving
input order in the results. `evaluate_many` runs synchronous callbacks over the
same heap and services. `evaluate_stream` discovers jobs on the calling mutator
while workers consume a bounded queue; its reporting callback runs serially on
completing workers, with their mutators parked. Returning false stops admission
and further reports. A session owns a lazily created fixed Rayon OS thread
pool, reused between batches. Each worker runs one callback at a time. Native
callbacks can reenter their worker's evaluator, but cannot spawn a nested batch
through `GcCallContext`. A worker blocked on a thunk does not execute another
job on its stack.

A parallel cell is 24 bytes, including a four-byte atomic publication word.
Its states are suspended, running with a worker owner, and ready. Claiming a
thunk uses an acquire CAS; its owner publishes an initialized immutable payload
with release ordering. Readers acquire the ready state before accessing the
payload. Failed evaluation is memoized too, but consumers receive separate
error snapshots before adding propagation traces. Cancellation and panic restore
the original suspension and wake waiters instead of memoizing an interruption.

Wait records are allocated only on contention. They use a mutex/condition
variable pair backed by the platform's sleeping primitives. The registry tracks
worker dependencies, not movable thunk addresses. Closing a dependency cycle
of jobs reports infinite recursion (a background spark in the cycle gives way
instead; see below). Completing an awaited thunk wakes that owner's
waiters, which recheck their own rooted cells; occasionally this wakes a waiter
for a different thunk owned by the same worker. With a small bounded pool this
avoids stable heap IDs, address-keyed wait maps, and GC rekeying.

## Moving GC and shared services

Workers have separate nursery allocation regions and temporary root frames.
Persistent handles use a synchronized registry. Allocation remains a local bump
on its fast path. A common nursery budget counts the bytes all workers have
allocated (not the chunks they hold, which every worker keeps across
collections), updated at chunk refills; a worker's allocations since its last
refill are the bounded slack.

Collection requests a rendezvous. Workers poll before evaluating a thunk and
before each call (reading an already evaluated value is not a safepoint); thunk
waiters and the coordinator joining a batch are already parked. No borrowed
view or unrooted pointer crosses a parking boundary. Once everyone is stopped,
the collector transfers nursery chunks without moving their payloads, replays
remembered writes, and traces every worker's temporary roots and the common
persistent roots. The existing collector then promotes/compacts and repairs
roots and weak identities. The nursery chunks it emptied go to a pool every
worker's nursery refills from, so they are not returned to the allocator
and faulted in again. All workers reload pointers after resuming.

Compiler/source metadata and virtual-store/host caches are synchronized in the
parallel build. Compiler writes use a reentrant metadata gate. Hot closure,
page-table and builtin-table reads acquire a pointer to an immutable arena
snapshot, avoiding locks; symbol lookups take the table's lock shared and
only a new name takes it exclusively. Import code and import thunks are shared across
workers and batches. Call-depth and stack-growth state are local to each OS
thread. Lazy tree materialization is serialized per tree, while daemon requests
retain the host's existing connection lock.

Fetches, context realization/IFD, and string materialization park the worker
while operating on owned data. Native callbacks can use `blocking` for the same
purpose; its exclusive evaluator borrow prevents carrying heap views across the
call. Synchronous operations that retain heap views, and custom callbacks that
do not use `blocking`, can still delay collection until they return or reach a
safepoint. Cooperative cancellation cannot interrupt an in-progress host call.
The library exposes `cancellation()` and `clear_cancellation()`; the latter
requires exclusive access between batches. CLI signal handling remains process
policy.

## Background evaluation of derivation dependencies

Within a streaming job, `derivationStrict` offers its arguments to idle
workers, as Nix's parallel evaluator does. Most of the time in a derivation's
evaluation goes to instantiating the derivations its attributes refer to, and
`derivationStrict` coerces those attributes strictly, in order.

An idle worker *walks* the arguments of the oldest `derivationStrict` call
still in progress that nobody has walked: it forces the attribute thunks and
offers the `outPath` of every derivation it finds as a *spark*. Forcing an
`outPath` instantiates that dependency, whose own `derivationStrict` call is
then open to a walk in turn. A list whose elements are expensive (measured:
250 µs for one element, discarding samples that span a collection) is offered
one spark per remaining element, so `symlinkJoin.paths` over a workspace's test
derivations evaluates its elements side by side. A walk never waits at a thunk
another worker is evaluating; it leaves that value as a spark of its own.

Idle workers take queued sparks before walks, newest first. A walk mostly
shadows the worker that is already coercing the same attributes, so it only
runs when there is no evaluation to take; and the consumer reaches the oldest
sparks soonest, so the newest is the least likely to be in hand. An open call
that returns withdraws its offer.

A spark is an ordinary evaluation: it claims thunks and publishes values, and
the ordered consumer finds them evaluated, or waits for the worker evaluating
them. It differs from a job in what it refuses:

- No ordered effect: no `trace`, `warn` or `break`, no fetch, no build
  (import from derivation), no embedding callback, no warning from inside
  another builtin. The spark stops there. Reading source files and adding
  paths or derivations to the store are neither ordered nor visible, and a
  spark does them.
- No memoized failure. An error restores the thunk, and the consumer
  evaluates it at its own position and reports it with its own trace.
- It yields to jobs. A spark may wait for a thunk another worker is
  evaluating. If a job then closes a cycle of waits through a spark, the
  spark is evicted: it stops, restores its thunks, and the job takes over.
  A cycle of jobs alone is the program's and is reported as infinite
  recursion. A spark that has only just started gives way at a busy thunk
  while other sparks are queued, and is retried later (at most three times).

Whatever a spark abandons is restored to its suspension, so sparks change
neither the values a job computes nor the order of what it prints. Two things
do differ from ordered evaluation, both as in Nix's parallel evaluator:

- A value printed by `trace` or in an error message before the consumer
  reached it may already be evaluated rather than `«thunk»`.
- A derivation a later attribute refers to may have been written to the store
  although an earlier attribute failed.

Sparks run until the batch has no job left that could need them; the last job
to finish cancels them, and they stop at their next safepoint. The serial
package compiles all of this out. `BRIX_TIMINGS=1` reports sparks started,
completed, stopped early, retried and stale (already evaluated or in hand when
taken), and the most frequent reasons sparks stopped: a guard that stops most
of them is parallelism lost.

## Flake checks and observable behavior

The coordinator discovers eligible leaves while the configured number of check
workers evaluate them. A FIFO queue holds at most four times that worker count;
workers take the next job as soon as they finish, and discovery replenishes the
queue continuously. A slow check does not hold up refill. Discovery parks for
backpressure; idle workers park while awaiting jobs. Output groups still finish
in the schema walk's `allowIFD` order. Each worker performs a whole leaf's
evaluation checks, derivation validation, and required `drvPath` forcing.
Serial and parallel workers use the same leaf-validation function.

Results print immediately in completion order, even while discovery is blocked.
Without `--keep-going`, the first reported failure stops admission, discards
queued jobs, and suppresses further reports. Already running jobs finish before
the command returns. Which failure appears first can therefore vary between
runs. Trace messages and host activity from those jobs may interleave. Building
the resulting derivations remains the existing separate build phase.

## Verification

The parallel integration suite exercises simultaneous forcing, exactly-once
publication, GC while another worker waits on a thunk or host operation,
parallel collection, cross-worker cycles, shared imports,
independent failure traces, panic recovery, cancellation, and pool reuse. A
second test runs the existing expression corpus concurrently and compares values
and error kinds/messages with the serial backend using the same settings.
CLI tests compare serial and parallel reports across filtering, failures,
derivations, queue refills, and `--keep-going`. Streaming tests block the first
check and discovery independently, verifying that later completions still print
and refill continues; panic and early-stop tests verify cleanup and pool reuse.
The nursery-transfer test
covers cross-worker edges, interior pointers, donor reuse, old-to-young barriers,
and both collection generations.

The shared `scripts/pgo-train.sh` trainer also runs 32 derivation checks on two
and four workers, with shared nixpkgs dependencies and independent per-check
work. Small nursery budgets require repeated GC, and training verifies that all
32 checks report completion. This covers the parallel backend in both local and
Nix PGO builds, BOLT instrumentation, and the rewritten binary's smoke tests.
It also runs one check with a list of independent expensive computations,
requiring both repeated GC and at least one completed background spark. The
parallel unit suite checks that idle workers instantiate dependencies before
the consumer reaches them, that expensive lists fan out, that effects, errors
and `tryEval` match serial evaluation, that a spark in a cycle of waits gives
way to a job, and that a diverging spark neither holds up an error nor
outlives its batch.

Useful commands:

```sh
cargo test -p brix-eval-parallel --tests
cargo test -p brix-cli --test cli flake_check_parallel_matches_serial_reports
cargo test -p brix-eval --lib
cargo test -p brix-eval --test managed_differential
cargo test -p brix-gc --lib
scripts/check-wasm.sh
```
