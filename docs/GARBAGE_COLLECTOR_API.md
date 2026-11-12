# Garbage collector API and design sketch

Companion to [the collector plan](GARBAGE_COLLECTOR_PLAN.md). The implemented
public API is described first, followed by the original internal design sketch.
The central distinction is:
**a root survives collection; a borrowed view prevents collection while used.**

The implemented entry point is `brix_eval::managed_api::GcEvaluator`, built
from an existing evaluator and a `GcConfig`. `eval_str`, `apply`, and value
constructors return opaque cloneable `GcValue` handles. `view(&value)` borrows
the evaluator; forcing, evaluation and explicit collection require `&mut self`.
Every handle carries a runtime identity, and foreign handles are rejected.

```rust
use brix_eval::{Arena, Config, Evaluator, NullHost};
use brix_eval::managed_api::{GcEvaluator, GcValueView};
let arena = Arena::new();
let host = NullHost;
let mut ev = GcEvaluator::new(
    Evaluator::new(&arena, Config::default(), &host),
    Default::default(),
)?;
let value = ev.eval_str(b"let x = 20; in x + 22", b"/")?;
ev.collect()?;
assert!(matches!(ev.view(&value)?, GcValueView::Int(42)));
```

The ordinary facade session and CLI now use this entry point. Internally,
`managed_heap` implements generative scopes,
rooted snapshots and vectors with stable direct slots. The original design
sketch below records the intended borrowing rules; its names are not a promise
of the current public API. The facade configures a 4 GiB initial nursery,
512 MiB nursery after the first successful collection, and 2 MiB chunks;
constructing `GcEvaluator` directly with `GcConfig::default()`
uses the smaller raw collector defaults. These thresholds trigger collection
and do not impose a process memory limit. Nursery survivors are promoted;
major collections mark and reclaim the old generation, compacting fragmented
heaps and sweeping dense heaps in place. Explicit `collect()` compacts and
reclaims both generations. Raw integrations must record managed-edge writes
with `RawHeap::write_barrier` and support two root passes when compacting;
the safe evaluator handles this internally. Neither generation guarantees
stable object addresses.

## Types and lifetimes

```rust
// Private fields; no public constructors or Deref implementations.
pub struct Evaluator<'h> { /* heap, evaluator state, root registry */ }
pub struct Roots<'r, 'h> { /* scoped root storage */ }
pub struct Root<'r, 'h, T> { /* index into root storage, brands */ }
pub struct RootVec<'r, 'h, T> { /* registered, traced vector */ }
pub struct Persistent<'h, T> { /* owning lease on a persistent root slot */ }

// Public opaque marker types for root contents; these are not raw layouts.
pub enum Value {}
pub enum ValueCell {}
pub enum Env {}
pub enum NixString {}
pub enum List {}
pub enum Bindings {}
```

`'h` identifies one heap; `'r` identifies a root scope. Both brands are
invariant. The constructor for a standalone evaluator introduces a fresh
heap brand through a higher-ranked callback. The session facade can hide
this branding behind its existing owner/dependent arrangement.

`Root<ValueCell>` holds a relocated pointer to a mutable cell. `Root<Value>`
holds a tagged value snapshot, including its traced payload. They are
different: rooting the snapshot preserves a thunk's environment after its
cell has been overwritten with `Blackhole`.

A `Root` is a copyable slot identifier. Copying it aliases the same root;
it does not allocate, increment a reference count, or copy the current heap
address. The scope owns and eventually releases the slots. Root storage
uses reusable chunks and type descriptors; it is valid independently of
where a Rust stack frame or `Vec` buffer moves.

Heap fields use private direct-pointer types such as `Gc<'h, HeapCell<'h>>`.
Those types never escape through a safe public API. `Root` does not become
an extra indirection on every heap edge: it is used at the execution stack
and external-owner boundaries. The collector rewrites root slots and direct
heap edges when objects move.

## Scopes and allocation

The essential signatures are:

```rust
impl<'h> Evaluator<'h> {
    pub fn with_roots<R>(
        &mut self,
        f: impl for<'r> FnOnce(&mut Self, &Roots<'r, 'h>) -> R,
    ) -> R;

    pub fn alloc_cell<'r>(
        &mut self,
        roots: &Roots<'r, 'h>,
        value: Root<'_, 'h, Value>,
    ) -> Result<Root<'r, 'h, ValueCell>>;

    pub fn mk_string<'r>(
        &mut self,
        roots: &Roots<'r, 'h>,
        bytes: &[u8], // Non-GC input, or a borrow that cannot alias this mutator.
    ) -> Result<Root<'r, 'h, NixString>>;

    pub fn force<'r>(
        &mut self,
        roots: &Roots<'r, 'h>,
        cell: Root<'_, 'h, ValueCell>,
        pos: SrcPos,
    ) -> Result<Root<'r, 'h, Value>>;

    pub fn collect(&mut self) -> Result<()>; // Explicit collection/test hook.
}

impl<'r, 'h> Roots<'r, 'h> {
    pub fn int(&self, n: i64) -> Root<'r, 'h, Value>;

    pub fn copy<T>(
        &self,
        ev: &Evaluator<'h>,
        value: Root<'_, 'h, T>,
    ) -> Root<'r, 'h, T>;

    pub fn vec<T>(&self) -> RootVec<'r, 'h, T>;
}
```

Only the sealed set of supported root kinds can instantiate allocation and
vector operations; generic bounds are omitted here for readability. Root
storage exhaustion has the same fail-stop policy as other collector metadata
allocation. Managed allocation-limit errors propagate through `Result`.

An ordinary caller looks like this:

```rust
ev.with_roots(|ev, roots| -> Result<()> {
    let value = roots.int(42);             // Rooted immediate; no heap object.
    let cell = ev.alloc_cell(roots, value)?; // May collect before allocating.
    let value = ev.force(roots, cell, SrcPos::NONE)?;
    assert_eq!(ev.as_int(value)?, 42);      // Returns an owned scalar.
    Ok(())
})?;
```

Inputs to collecting constructors are already rooted. A constructor may
collect, reload its inputs, allocate and initialize the object, and publish
the result in a reserved output root slot. No collection can occur between
reading an input address and publishing the fully initialized output.

The output lifetime comes from `roots`, not the mutable borrow of `ev`.
Consequently a returned root can be held through subsequent calls. The
higher-ranked scope callback prevents returning one of its local roots:

```rust,compile_fail
let escaped = ev.with_roots(|_, roots| roots.int(42));
```

To return a result from a helper, allocate its result slot in the caller's
scope. Local intermediates can then be released at the helper's exit:

```rust
fn example<'r, 'h>(
    ev: &mut Evaluator<'h>,
    out: &Roots<'r, 'h>,
) -> Result<Root<'r, 'h, Value>> {
    ev.with_roots(|ev, temp| {
        let cell = ev.alloc_cell(temp, temp.int(42))?;
        let value = ev.force(temp, cell, SrcPos::NONE)?;
        Ok(out.copy(ev, value))
    })
}
```

`out.copy` copies the current root contents into a new slot in the outer
scope. It does not copy the heap object and does not collect. Just returning
the inner `Root` would leave a handle to a released slot.

## Reading and mutation

Reading borrows the evaluator for as long as a heap view is used:

```rust
impl<'h> Evaluator<'h> {
    pub fn string_bytes<'v, 'r: 'v>(
        &'v self,
        string: Root<'r, 'h, NixString>,
    ) -> &'v [u8];

    pub fn view<'v, 'r: 'v>(
        &'v self,
        value: Root<'r, 'h, Value>,
    ) -> ValueView<'v>;

    pub fn snapshot<'r>(
        &self,
        roots: &Roots<'r, 'h>,
        cell: Root<'_, 'h, ValueCell>,
    ) -> Root<'r, 'h, Value>;

    pub fn store(
        &mut self,
        cell: Root<'_, 'h, ValueCell>,
        value: Root<'_, 'h, Value>,
    ); // Does not collect or allocate; mutation still needs exclusive access.
}
```

`ValueView` exposes scalars or short-lived opaque views of strings, lists,
bindings and functions. It must not expose a `Gc<'h, T>` that can be copied
out and later dereferenced. APIs for turning a child into a root copy its
current pointer into registered storage while access is still valid.
Mutable heap fields are not exposed as unrestricted public `Cell`s.

This must fail to compile:

```rust,compile_fail
let string = ev.mk_string(roots, b"hello")?;
let bytes = ev.string_bytes(string);
ev.collect()?;                    // Requires &mut ev while bytes borrows ev.
println!("{}", bytes.len());
```

This works:

```rust
let string = ev.mk_string(roots, b"hello")?;
ev.collect()?;
let bytes = ev.string_bytes(string); // Reloads the relocated root.
println!("{}", bytes.len());
```

No runtime no-GC counter is needed for ordinary reads: the shared borrow is
the guard. This relies on forbidding collection through `&Evaluator`, including
through hidden interior mutability or a reentrant callback. Mutating methods
take `&mut Evaluator` even when they cannot collect, so moving storage and
mutable heap slots cannot be rewritten behind live Rust references.

For sharing a substring, provide a collecting constructor taking the root
and a range, rather than accepting an untracked interior slice:

```rust
let part = ev.substring(roots, string, 1..4)?;
```

It validates the range and reloads the backing allocation after any
collection. It preserves the byte-sharing and context behavior of the current
string helpers. Direct byte copying is for owned/external inputs.

## Forcing a thunk

The interesting case is retaining the suspended payload while its cell is
blackholed. The current implementation roots a thunk's environment and saves
its immutable node ID before overwriting the cell; it does not also allocate a
duplicate value snapshot. Applications retain their original snapshot and
rooted function/argument cells. `force_cell` memoizes without returning a new
value root when its caller only needs to read or project the rooted cell.

The original sketch below illustrates the more general snapshot rule (ordinary
value dispatch and trace decoration abbreviated):

```rust
fn force<'r, 'h>(
    ev: &mut Evaluator<'h>,
    out: &Roots<'r, 'h>,
    cell: Root<'_, 'h, ValueCell>,
    pos: SrcPos,
) -> Result<Root<'r, 'h, Value>> {
    ev.with_roots(|ev, temp| {
        let saved = ev.snapshot(temp, cell);
        if !ev.is_suspended(saved) {
            return ev.finish_force(out, saved, pos); // Includes Failed/Blackhole.
        }

        ev.blackhole(cell); // Does not collect. `saved` keeps the old edges live.
        match ev.eval_suspended(temp, saved, pos) {
            Ok(value) => {
                ev.store(cell, value);
                Ok(out.copy(ev, value))
            }
            Err(error) => {
                ev.memoize_failure(cell, &error, pos)?;
                Err(error)
            }
        }
    })
}
```

Both `cell` and `saved` remain roots during recursive evaluation. Their
contents can move repeatedly. `eval_suspended` obtains rooted environment
or application inputs before it calls further evaluation; it cannot hold
a borrowed `ValueView` across those calls. Error memoization roots the cell
while allocating its error wrapper. The implemented
forcing path restores the original suspension if error-wrapper allocation
fails, and restores it before propagating a panic. Both restorations reload
the saved roots after any intervening collection; neither allocates.

## Loops, vectors and builders

An ordinary `Vec<Root<...>>` retains slot handles but can keep thousands of
individual temporary slots alive. `RootVec` instead stores the traced
contents in one registered vector. Its allocation and mutation API updates
the registry when the backing buffer moves; collection visits only its
initialized length.

```rust
let list = ev.with_roots(|ev, roots| {
    let mut items = roots.vec::<ValueCell>();
    for i in 0..1000 {
        ev.with_roots(|ev, temp| -> Result<()> {
            let value = temp.int(i);
            let cell = ev.alloc_cell(temp, value)?;
            items.push(ev, cell); // Copies the traced contents, not the slot ID.
            Ok(())
        })?; // This iteration's temporary slots are released.
    }
    let list = ev.alloc_list(roots, &items)?;
    Ok(ev.persist(list)) // A persistent handle may escape this scope.
})?;
```

`RootVec::push` does not collect. Its data is owned by the scope's registered
storage; dropping the scope removes registration safely. Accessors return
owned scalars, borrowed views, or a fresh root in a supplied output scope,
never a raw pointer valid only until the next vector growth or collection.

Attribute/environment builders use the same pattern. Generic iterators or
initializer callbacks must not run inside an unpublished heap allocation.
Evaluate into rooted scratch storage, then reserve space and copy the fully
initialized fields into the final allocation. Optimize simple no-callback
constructors separately after measuring them.

## Compiled closures and primops

Change compiled evaluation closures from `Fn(&Evaluator, Env) -> Value` to:

```rust
type Eval<'h> = dyn for<'r, 'e> Fn(
    &mut Evaluator<'h>,
    &Roots<'r, 'h>,
    Root<'e, 'h, Env>,
) -> Result<Root<'r, 'h, Value>> + 'h;
```

Primops similarly receive `&mut Evaluator`, an output root scope, position,
and a rooted argument array. Their temporary roots are scoped locally. Keep
host service access separate enough that borrowing a service does not hold
a borrow of the evaluator across recursive evaluation; clone an `Rc` service
handle or pass the external host reference independently where needed.

Opaque closure captures are permanent metadata/literals, owned external Rust
data, or persistent roots. Registration cannot admit arbitrary captured raw
heap handles. A captured persistent root is explicitly traced through its
registry slot; no scanner attempts to infer the layout of a Rust closure.

## The public session API

The branded low-level API is mainly for evaluator implementation and custom
embedders. Most callers keep using the facade:

```rust
let cursor = session.eval_expr("let x = 40; in x + 2")?;
let second = cursor.clone();
drop(cursor);
assert_eq!(second.to_owned()?, OwnedValue::Int(42));
```

The low-level bridge for such a facade is:

```rust
impl<'h> Evaluator<'h> {
    pub fn persist<T>(&self, root: Root<'_, 'h, T>) -> Persistent<'h, T>;

    pub fn root<'r, T>(
        &self,
        roots: &Roots<'r, 'h>,
        persistent: &Persistent<'h, T>,
    ) -> Root<'r, 'h, T>;
}
```

Both operations copy traced contents between registered slots and cannot
collect. `Persistent::clone` shares a lease; `Roots::copy` creates a separate
scoped slot. The distinction keeps root ownership explicit.

These facade method names are illustrative. `Cursor` owns a persistent root
lease internally; cloning retains the lease and dropping the final owner
releases it. Its slot contains the current relocated pointer, protected
against slot reuse by a generation. Persistent roots are single-threaded,
heap-bound, and cannot outlive their heap. Importing a persistent root into
an operation copies its contents into that operation's root scope.

The facade can preserve `&Session` methods with one exclusive mutator borrow
per entry. If an API lends heap bytes to a closure, hold a shared borrow for
the entire callback and reject reentrant evaluation while it is active.
Owned snapshots need no such restriction. Do not let a public accessor
silently bypass the mutator borrow through `RefCell`.

## What this costs and what has been checked

The core evaluator will change substantially: methods that can allocate or
evaluate become mutable, closures receive roots, and helpers return rooted
results. Short-lived scalar calculations and non-collecting traversals can
still use ordinary locals and borrowed views. No need to root every integer
or every traversed edge individually.

Root creation stores a descriptor/payload and advances a root frontier;
reading a root loads its current contents. Scope teardown releases a range
of slots. These operations happen even without collection. The implementation
must measure them, inline common cases, and reuse output slots or batch
roots where necessary. Persistent reference counting stays at the facade
boundary. A root stack holding every historical temporary until the end of
evaluation would be both slow and excessively retaining; nested scopes and
rooted containers are part of the design, not optional cleanup.

A standalone signature-only Rust experiment was type-checked during this
design: nested scopes, collecting calls, a rooted vector and promotion to an
outer root scope compile. Holding string bytes across a mutable collection
call is rejected with E0502; returning an inner scope's root is rejected by
the lifetime checker. The experiment used placeholder bodies and does not
establish the safety of root storage, relocation, brand creation or the
eventual collector. Those require the implementation tests in the main plan.
