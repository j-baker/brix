# Sandboxed evaluation: brix as a WebAssembly module

Status: plan, 2026-11-14. Nothing here is built yet.

## Goal

Run a whole `flake check` evaluation as one call into a WebAssembly
module, on a dedicated host with no enclave. The module has no ambient
authority. Everything it does to the outside world goes through a small
set of imports the embedder (trustless) implements and polices:

1. No file access except through an interface the embedder provides.
2. No network access except through an interface the embedder provides.
3. Nothing else: no environment, no processes, no threads, no clock it
   was not given.

The reason for wasm, and not only traits in a native build: the boundary
is enforced by the runtime. A memory-safety bug in brix (the collector
and the managed heap are `unsafe` code) stays inside the instance. A
trait boundary in a native process is a convention, not a boundary.

The embedder also gets, from the runtime and for free: a hard memory
cap, CPU deadlines and cancellation (epoch interruption), and one place
(its HTTP import) to apply customer networking, allowlists and metering.

## Where brix is today

Facts below were checked against the code on 2026-11-14.

Already in place:

- `brix-syntax`, `brix-store`, `brix-eval` and `brix-flake` (without
  `native`) build for `wasm32-unknown-unknown`. `scripts/check-wasm.sh`
  lints them in CI for calls that panic there (clock, threads,
  environment).
- The evaluator's I/O is one trait, `brix_eval::Host`: the root file
  system, `fetch_tree`, `fetch_file`, `build`, `is_valid_path`,
  `compiled_image`, logging.
- The collector takes memory from `std::alloc` only. No `mmap`, no
  tagged pointers; its layout asserts are pointer-width generic and
  hold on wasm32. Threads and the clock are already compiled out for
  wasm.
- Compiled images load from bytes (`Image::from_vec`), with no file
  system. Their decompression is pure Rust.
- The evaluator's dependency graph for wasm has no C code.
- The worker-protocol encoding in `brix-daemon` (`wire.rs`) is generic
  over `Read`/`Write`.
- `brix_flake::lock_flake` reaches the outside only through the
  `LockDeps` and `Fetcher` traits.

Not in place:

- **Nothing has ever run on wasm.** CI checks that the core compiles and
  avoids known-panicking calls. There is no wasm runtime in the devshell
  and no test executes there. This is the largest unknown.
- **The facade (`crates/brix`) is native-only.** `Session`, `Cursor`,
  flake locking, `callFlake`, the schemas and `check` live there. It
  names the concrete `NativeHost` in about 25 places and touches the OS
  directly: `std::fs`, environment variables, `rayon`, `std::thread`,
  `memmap2`, `Condvar` waits, `std::process` (`git add` for lock
  files), the daemon socket. `Config::default()` itself reads the
  environment and the file system.
- **`brix-daemon` is `#![cfg(unix)]`**, so on wasm it is an empty
  crate. `DaemonClient` holds a concrete `Connection` (unix socket or
  child pipes) and can only be built from a socket path or an ssh
  target.
- **The fetchers are welded to native I/O.** Everything under
  `brix-flake/src/native` assumes `ureq`, a cache directory with
  `flock`, the local file system and subprocesses. In particular:
  - Remote git fetches shell out to `git` whenever it is on `PATH`;
    the in-process `gix` path is the fallback. `gix` itself maps pack
    files and spawns `ssh`/`git-upload-pack` for non-HTTP transports.
  - Mercurial is entirely subprocess.
  - `xz` and `zstd` unpacking bind C libraries.
  - Git-over-HTTP spawns a thread per request.
- **Stack depth.** Natively the evaluator runs on a 256 MiB thread and
  grows further with `stacker`. On wasm `stacker` only moves the
  shadow stack in linear memory; the engine's call stack is whatever
  the runtime allows. Deep recursion must be configured for and
  tested.
- **Panics abort.** `wasm32-unknown-unknown` is `panic=abort`, so the
  evaluator's `catch_unwind` guards do nothing: a panic is a trap and
  the instance is dead.

## Design

### Shape

```
  embedder (trustless, native, tokio)
  ┌──────────────────────────────────────────────────────────┐
  │  brix-sandbox-host: wasmtime, limits, the import glue    │
  │    implements the imports over the embedder's own:       │
  │    HTTP client + policy, blob cache, store stream, log   │
  └───────────────▲──────────────────────────┬───────────────┘
        imports   │                          │  export: check(request)
  ┌───────────────┴──────────────────────────▼───────────────┐
  │  brix-sandbox (wasm32-unknown-unknown, one instance/call) │
  │    brix facade without `native`                           │
  │      Platform impl = the imports                          │
  │    brix-flake fetch logic, brix-daemon client, brix-eval  │
  └──────────────────────────────────────────────────────────┘
```

One instance per `check` call, thrown away afterwards. A trap, a
deadline or an out-of-memory ends that call and nothing else.

### Target

`wasm32-unknown-unknown`, not WASI.

- It is what the repository already lints for and what the Nix-provided
  toolchain already has. No WASI target is in the toolchain.
- With no WASI there are no imports at all beyond the ones we define,
  which is exactly requirement 3. WASI would bring a generic file
  system, clocks, environment and sockets that then have to be
  virtualised or denied one by one.

### The interface

Defined once, as a Rust trait in the facade (`brix::platform::Platform`),
so that the native build and the wasm build run the same logic above it.
The wasm ABI is a mechanical binding of that trait.

Five groups. Calls are synchronous from the module's side; the embedder
may implement them asynchronously (the runtime suspends the instance).

| Group | Calls | Used for |
|---|---|---|
| **HTTP** | `request(method, url, headers, body) → status, headers, body stream`; read the body in chunks | Forge APIs, tarballs, `fetchurl`, the flake registry. Policy, credentials, TLS, proxies, retries' transport and metering are the embedder's. |
| **Cache** | `get(key) → blob?`, `put(key, blob)`; tree store: `tree_open(nar_hash) → handle?`, `tree_create → writer`, then `lstat`/`read_dir`/`read_file`/`read_link` on a handle | Ref→rev entries with TTL; unpacked source trees kept host-side and read lazily, so a tree costs module memory only for what evaluation reads; compiled images. |
| **Store** | `open → stream`; `read`, `write`, `close_write` on the stream | The Nix worker protocol, spoken by `brix-daemon` inside the module. Uploading derivations, validity queries, import-from-derivation builds and reading their outputs all ride it. Trustless already serves this protocol to untrusted clients. |
| **Files** | The same tree-handle calls, on trees the embedder chooses to expose | Optional. Not needed when the flake is named `github:owner/repo/<sha>` and comes through HTTP. For a checkout the embedder already has. |
| **Events and log** | `event(check event)`, `log(level, bytes)`, `now()` | `CheckEvent`s as they happen; evaluator and build log lines; the clock for cache TTLs (the embedder may return a fixed time). |

The export:

```
check(request) → report
  request: flake reference, system (or all), build_all,
           evaluation settings, fetch settings (TTL, access to
           registries), GC thresholds
  report:  derivations to build with their attribute paths (each
           already valid in the store), unchecked outputs, omitted
           systems, whether any check failed to evaluate
```

This wraps `brix::check::check` and then `store_upload` for each
derivation, so a returned derivation needs nothing more from brix.

### ABI binding

Recommendation: the component model. The interface is written in WIT,
`wit-bindgen` generates the module side, `wasmtime::component::bindgen!`
the embedder side, and streams and tree handles are WIT resources.

The alternative is a hand-written ABI of about fifteen `(ptr, len)`
functions with a serialised message format. It needs no extra tooling
but leaves handle lifetimes and message framing to us on both sides.

Phase 0 settles this by building the spike both ways only if the first
attempt (WIT) hits a toolchain problem: `wasm-tools` has to wrap a
`wasm32-unknown-unknown` core module into a component, which I have not
tried with this toolchain.

### What moves where in brix

**`brix` (facade).** A `native` cargo feature, on by default.

- `Platform` trait and `NativePlatform` (today's behaviour: `std::fs`,
  `ureq`, the flock'd cache directory, the daemon socket, threads,
  `mmap`, environment-derived defaults).
- `NativeHost` becomes a host over `Arc<dyn Platform>`. It is a private
  type, and no public type has a host parameter, so the public API
  stays as it is.
- Everything that is policy stays shared: registry resolution, lazy
  trees and mounts, overrides, the image store's decisions,
  `daemon_build`, fetch orchestration.
- Without `native`: no background prefetch (fetches happen when
  needed, in order), images compiled on the calling thread, no lock
  file writing, no `git`/`hg` calls, `Config` defaults that read
  nothing.
- Public types that expose native things need a look:
  `Error::Daemon`, `store`'s re-exports, `CompiledImage::path`,
  `Config`'s paths.

**`brix-daemon`.** `DaemonClient` over any `Read + Write` pair plus a
`close_write`; a public constructor that takes one; `#![cfg(unix)]`
narrowed to the socket and ssh transports and `add_indirect_root`.

**`brix-flake`.** Split `native` in two:

- Fetch logic that only needs HTTP, a cache and a clock, written
  against a small `FetchIo` trait: tarball and file downloads, the
  GitHub/GitLab/SourceHut forges, unpacking, the cache-key and TTL
  rules now inside `NativeFetcher`. Builds for wasm.
- What stays native-only: git by any transport, working trees, `path:`,
  Mercurial, LFS, submodules, signature verification, legacy export.

Unpacking in the module needs pure-Rust decoders. gzip, bzip2 and zip
already are. `xz` and `zstd` are C today and need replacements (or stay
unsupported in the module until then). Zip timestamps use the system
time zone, which the module does not have; the request must carry one
or the module uses UTC.

**New crates.**

- `brix-sandbox`: the module. `Platform` over the imports, the `check`
  export, GC and stack configuration for wasm.
- `brix-sandbox-host`: the reference embedder. `wasmtime`, limits, and
  the imports expressed as Rust traits the real embedder implements. It
  is what brix's own tests run against. Whether trustless depends on it
  or copies its shape is trustless's call.

### Scope of the first version

Supported inputs: `github:`, `gitlab:`, `sourcehut:`, tarball and file
URLs over HTTP(S), and indirect references through the global registry.
That covers nixpkgs and most public flakes.

Not supported in the module, failing with an error that says so:
`git+ssh:`, `git:`, `git+file:`, `path:`, Mercurial, LFS over ssh,
commit-signature verification, `nix-219-compat`.

`git+https:` is the gap that matters (private repositories often use
it, and `git+ssh:` more so). Two ways to close it later:

- In the module: `gix` over the HTTP import with an object store that
  does not need `mmap` or a real file system. Real work; `gix`'s
  assumptions run deep.
- Outside it: an optional import where the embedder fetches a whole
  tree itself for schemes the module cannot. Simple, but that fetch is
  then the embedder's code running on a customer-chosen URL, outside
  the sandbox.

Lock files: the module never writes one. A flake whose lock file is
missing or out of date for its inputs fails.

Evaluation is serial. There is no parallel evaluator, no parallel GC
tracing and no background prefetch in the module.

### Trust rules for the embedder

These are properties the design depends on; they belong in the
embedder's implementation of the imports.

- **A cache entry written by an instance is only as trustworthy as that
  instance.** An instance that has evaluated customer code may be
  compromised. A source tree it stores under a NAR hash may only go
  into a cache shared between tenants if the embedder has recomputed
  the hash itself. Otherwise caches are per tenant.
- **Compiled images are unverifiable bytecode.** Share one only if it
  was compiled by an instance whose sole input was the tree it
  describes (a compile-only call that evaluates nothing).
- **The store stream is untrusted input**, as an ssh client's is today.
- **The HTTP import is the whole network policy.** The module can ask
  for any URL.
- **Limits are the embedder's**: memory cap, a deadline per call, stack
  size, and a bound on bytes through each import.

## Risks

| Risk | What is known | How it is retired |
|---|---|---|
| The evaluator misbehaves on wasm | Never executed there | Phase 0: the Nix language corpus under wasmtime |
| 4 GiB linear memory | A wasm32 hard limit. Host-side trees and images keep source text out of it, but the evaluated heap is in it. Large evaluations (NixOS configurations) may not fit. 64-bit wasm memory needs a Rust target the stable toolchain here does not ship. | Phase 0 measures peak heap for representative flakes natively and in wasm. This is a go/no-go number. |
| Speed | Compiled, not interpreted, but no parallel evaluation or tracing; unmeasured | Phase 0 measures the same flakes; compare wall time against native serial |
| Stack depth | Runtime call-stack limits default small; brix allows 10 000 nested calls | Phase 0: configure the runtime's stack, run the corpus's deep-recursion tests and a deliberately deep case |
| Import-from-derivation outputs | Read whole into module memory today | Measure; if it matters, mount outputs through tree handles instead |
| Image portability | Images compiled natively probably load in wasm (32-bit offsets, little-endian, fingerprint includes node size) but this is inferred | Phase 5 test: load a native image in the module, else compile in the module |
| GC defaults | The facade's default first-cycle threshold is 4 GiB, which on wasm32 saturates and never triggers | The module sets its own thresholds; tune in phase 0 |

If phase 0 says the memory cap or the speed is not acceptable, the
refactor in phases 1 to 4 is still the right shape: the same `Platform`
boundary can be enforced by running the native static binary under
seccomp with the imports over an inherited socket. That is a weaker
boundary than wasm, and a decision to take with numbers in hand.

## Phases

Each phase leaves the tree green and is mergeable alone. Sizes are
relative: S (days), M (a week or two), L (several weeks).

**Phase 0. Run it and measure (S). Gate for everything else.**

- A throwaway module exporting "evaluate this expression against these
  in-memory files", and a wasmtime runner as a test.
- Add wasmtime (and `wasm-tools` if WIT) to the devshell.
- Run the Nix language corpus in it. Fix what breaks.
- Measure, native serial against wasm, for nixpkgs `hello`, a
  `flake check` of a mid-sized flake, and a NixOS configuration: wall
  time, peak memory. Follow the repository's measurement rules (fixed
  metric, A/A noise floor first, all runs reported).
- Settle stack configuration and the ABI binding.
- Output: numbers, and a go/no-go on wasm32.

**Phase 1. `Platform` in the facade (L).**

No behaviour change; every existing test passes at each step. In order,
smallest blast radius first:

1. Log, clock and environment: `Config` defaults that read nothing
   unless `native`.
2. Files: allowed paths, the store directory accessor, registry files.
3. Store: `DaemonState` behind the trait (depends on phase 2).
4. Images: bytes from the platform; `mmap` and the cache directory in
   `NativePlatform`.
5. Fetch: `NativeFetcher` and `HttpClient` constructed by
   `NativePlatform` (the split itself is phase 3).
6. Threads: prefetch and stream compilation behind the platform's "can
   run in the background", with the serial path tested natively.

**Phase 2. `brix-daemon` over a byte stream (S).** Independent of
phase 1; can go first.

**Phase 3. Fetch logic against `FetchIo` (M).** Tarball, file and forge
fetchers plus the cache rules; native implementation unchanged in
behaviour. The oracle tests for those fetchers run against both the
native I/O and an in-memory one.

**Phase 4. The facade builds for wasm (M).** `brix` without `native`
joins `scripts/check-wasm.sh`. Pure-Rust `xz`/`zstd` decoding, or an
explicit unsupported error.

**Phase 5. The module and the reference embedder (M).**

- `brix-sandbox` and `brix-sandbox-host`, the `check` export end to
  end.
- Tests, all run under wasmtime in CI:
  - The language corpus (from phase 0, now through the real module).
  - Differential: the same flake checked natively and in the module
    gives identical events, derivation paths and `.drv` bytes.
  - Fetch: recorded HTTP responses served by the test embedder.
  - Store: the embedder pipes the store stream to a real daemon's
    socket, reusing the daemon-gated tests' setup (import-from-
    derivation included).
  - Limits: a deadline, a memory cap and a deep recursion each end the
    call with a distinct, reported error and leave the embedder
    healthy.
  - nixpkgs: derivation paths equal the oracle's (gated, as today).

**Phase 6. Hardening and reach (M, open-ended).**

- Compiled images in the module, including the compile-only call.
- Import-from-derivation outputs through tree handles if measured to
  matter.
- `git+https:` by one of the two routes above.
- Fuzzing the import boundary from the module side.

**Trustless side (not in this repository).** Implement the imports over
its own HTTP stack and network policy, cache, and the existing
worker-protocol handler; launch from the GitHub webhook; post check
runs from the events and the report.

## Decisions needed

1. **Private inputs.** Is forge-and-tarball-only acceptable for a first
   version, or is `git+https:`/`git+ssh:` needed from the start? This
   moves a large piece of work between phase 6 and phase 3.
2. **Lock policy.** Fail on a missing or stale lock file (proposed), or
   evaluate with an in-memory lock as `nix flake check` does.
3. **Cache sharing.** Per-tenant caches only (simplest), or shared with
   embedder-side verification (faster for nixpkgs, more embedder work).
4. **Where the reference embedder lives.** In this repository as
   `brix-sandbox-host` (proposed, so brix can test itself), with
   trustless depending on it or not.
5. **ABI binding.** WIT (proposed) or hand-written; phase 0 confirms.
