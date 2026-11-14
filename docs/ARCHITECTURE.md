# brix architecture

brix is a bug-for-bug compatible implementation of Nix, written in Rust,
with the same command-line structure as `nix`. It parses and evaluates Nix
expressions, flakes in particular, up to the point where derivations
exist, and realises them by delegating to a pre-existing Nix daemon over
the worker protocol (unix socket or `ssh-ng`). That covers `brix build`,
`brix run`, `brix develop`, `brix flake check` and import-from-derivation.
What brix does not do is own a store: no store database, no garbage
collection, no substitution logic, no `nix copy`-style store management.
The `nix` commands of that kind are handed to Nix itself (see *brix
(library) and brix-cli*).

The reference implementation is Determinate Nix 3.24.1, which is Nix
2.35.3 (the `nix-src` flake input). When the Nix manual and the C++
source disagree, the source wins: brix matches what `nix` *does*. The
places where it knowingly does not are listed in
`crates/brix-eval/KNOWN_DEVIATIONS.md`.

`flake check --eval-cores N` can use a separately compiled parallel evaluator.
Its worker, thunk publication, moving-GC, and serial-cost model are described
in [Parallel evaluation](PARALLEL_EVALUATION.md).

## Goals, in priority order

1. **Compatibility.** Same values, same store paths, same `.drv` files, same
   error messages, same evaluation order where it is observable (`trace`,
   `throw` precedence, laziness). The Nix language test corpus
   (`tests/functional/lang` in `nix-src`) and differential tests against the
   real `nix` binary are the acceptance tests.
2. **Flakes with pure evaluation only.** No `NIX_PATH`, no `--impure`, no
   channels, no `nix-env`. `--expr` and `--file` work, but evaluate in
   pure mode too (`--file` with its directory readable), where Nix's
   `--file` implies `--impure`.
3. **A virtual store.** The store directory (`/nix/store`) is a parameter.
   Nothing is ever written to disk; "adding a path to the store" means
   computing its NAR hash and store path and remembering where its content
   came from, so that whoever consumes brix's output can materialise it.
4. **A narrow library API**, usable from Rust and compilable to WebAssembly.
   The core crates do no I/O of their own: file systems, fetchers and the
   daemon connection are traits the host provides.
5. **Performance** at least matching Nix on plain sources and well ahead of
   it on pre-compiled inputs (see *Compiled inputs* below).

## Crates

```
brix-syntax    lexer + parser → AST                  (no I/O, wasm-clean)
brix-store     hashes, store paths, derivations,     (no I/O but the
               NAR, URLs, settings, accessors,        `std-fs` accessor;
               the virtual store                      wasm-clean without it)
brix-eval      compiler (AST → resolved IR),         (no I/O, wasm-clean)
               values, evaluator, builtins,
               printers, compiled-input images
brix-flake     flakerefs, flake.lock, fetchers,      (fetchers behind the
               callFlake's inputs                     `native` feature;
                                                      wasm-clean without it)
brix-daemon    Nix worker-protocol client (unix socket, ssh-ng)
brix-alloc     the allocator of the brix binary: mimalloc under a
               pre-faulting wrapper
brix           the library facade: the public API    (the native host)
brix-cli       the `brix` binary
brix-testutil  shared test helpers (a dev-dependency, not published)
```

A crate depends only on crates above it in that list: `brix-eval` on
`brix-syntax` and `brix-store`, `brix-flake` and `brix-daemon` on
`brix-store` alone, `brix` on the five before `brix-alloc`, and
`brix-cli` on `brix`, `brix-alloc`, `brix-syntax`, `brix-store` and
`brix-flake`. `brix-syntax` and `brix-store` know nothing about each
other, and neither `brix-alloc` nor `brix-testutil` depends on another
brix crate.

"wasm-clean" means that the crate, with no default features, builds for
`wasm32-unknown-unknown` and calls nothing that panics there:
`scripts/check-wasm.sh` runs Clippy for that target over brix-syntax,
brix-store, brix-eval and brix-flake with a configuration
(`scripts/wasm/clippy.toml`) that disallows the clock, threads and
environment reads, and CI runs it. The workspace takes brix-store
without `std-fs` and brix-flake without `native`; the members that read
the file system or fetch ask for them. Of the evaluator's own needs, only
two depend on the platform, both in brix-eval's `platform.rs`: a clock
for the phase timings (`BRIX_TIMINGS`), zero where there is none (the
report says so), and threads for compiling images, which run on the
calling thread where there are none. The rest of the outside world
(files, fetches, builds, the store, logging) is the `Host`'s. There is
no wasm runtime in the devshell, so nothing runs on wasm in CI: the
check is that it builds and does not reach for what is missing.

### brix-syntax

`logos` for the lexer, a hand-written recursive-descent parser (Pratt for
operators) following `parser.y`'s precedence table exactly. The parser
performs the same desugarings Nix's parser does, so the evaluator never sees
`-`, `*`, `/`, `<`, `<=`, `>`, `>=`, `<x>`, `let { body = …; }` or unary
minus: they are already calls to `__sub`, `__lessThan`, `__findFile`, etc.
Attribute-path merging (`a.b = 1; a.c = 2;`) and its duplicate-attribute
errors also happen here, as in Nix's `addAttr`.

The AST (`ast.rs`) is the contract between the parser and the evaluator and
mirrors `nixexpr.hh` node for node. Strings are byte strings (`Vec<u8>`):
Nix strings are not required to be UTF-8. So are names (`ast::Name`, a
small byte string: inline up to 23 bytes, shared by reference count
beyond) and the parser's error messages, which quote them.

Positions are byte offsets into the source; line/column are computed on
demand. Relative path literals are resolved against the file's directory at
parse time, as in Nix, so `ExprPath` always carries an absolute path. That
path is bytes (`Expr::Path { path: Vec<u8> }`), as the directory given to
`parse` is (`&(impl AsRef<[u8]> + ?Sized)`): a checkout in `d\xe9/` gives
literals `/…/d\xe9/x`, printed as they are. So is a file's origin
(`Origin::Path(BString)`), which positions and messages show by its bytes
(`Origin::as_bytes`, `SourceFile::format_pos`); its `Display` is lossy.

Oracle: `nix-instantiate --parse` prints the AST in a canonical form
(`Expr::show`). brix implements the same printer, and the parser test suite
compares the two over the whole language corpus and over every 50th
`.nix` file of nixpkgs (the whole tree is an `#[ignore]`d test); every
file of nixpkgs must parse. `--parse` also reports undefined variables,
which brix finds when it compiles the AST (`brix-eval`);
`brix-eval/tests/parse_oracle.rs` compares parser and compiler together.
The parser's warnings (the `lint-*` settings, the "cursed `or`"
deprecation) are returned to the caller, which logs them through the
host.

### brix-store

Pure data: `Hash` (md5/sha1/sha256/sha512, and blake3 with the
`blake3-hashes` feature; base16/nix-base32/base64/SRI; parsed from bytes,
the errors quoting them as they are), `StorePath`,
`StoreDir`, the store-path derivation functions (`makeStorePath`,
`makeFixedOutputPath`, `makeTextPath`, `makeOutputPath`, whose names are
bytes and are validated once the path is computed, as in Nix),
content addresses, `Derivation` with its ATerm serialisation
(`unparse`/`parse`, bytes like the output names, platform, builder,
arguments and environment they carry; the store paths in a `.drv` file
are parsed from bytes too, with `parseStorePath`'s errors quoting them as
they are, though a valid one is ASCII and kept as text),
`hashDerivationModulo`, NAR serialisation, Nix's URL
grammar (`url`: `parseURL`, `ParsedURL`, `decodeQuery`, percent
encoding), which store references, flake references and fetchers share,
`DerivedPath` (`drv^out` on the command line, `drv!out` on the wire,
both bytes as output names are), the
one parser for setting values (`setting`: `nix.conf`, the command line
and store URI parameters) and the experimental features that change what
the store code accepts (`XpSettings`, passed explicitly: there is no
global), and nlohmann::json's parser (`json`: what it accepts, its SAX
events in document order and its exact errors), with which
`builtins.fromJSON` and the fetchers read the JSON Nix reads with it.
The hash, store-path, content-address, NAR and derivation
functions are checked against the real `nix` (`tests/oracle.rs`), against
`.drv` files it wrote (`tests/fixtures.rs`) and against Nix's own
unit-test data; the URL grammar and the rest have unit tests.

`SourceAccessor` is the trait through which everything reads files: the
real file system (`FsAccessor`, behind the default `std-fs` feature), an
in-memory tree (fetched tarballs, wasm), or a mounted composite. A
`SourcePath` is `(accessor, path)`. Paths are bytes, as Nix's
`std::string`s are (see *Paths and names as bytes* below): a `CanonPath`
is canonical bytes (`Vec<u8>`, its getters `&BStr`), compared, hashed and
sorted as its bytes, so that maps keyed by paths are searched with a
`&[u8]` or a `&BStr` (`Borrow`); directory listings (`DirEntries`, keyed
by `BString`, in unsigned byte order: the order of NAR entries and of
`readDir`), symlink targets (`read_link`) and what an accessor shows a
path as (`display_prefix`, `show_path`) are bytes, and so are the paths
in `AccessorError`, `NarError`, `StorePathError` and `CanonPathError`,
whose `message_bytes` is the message Nix prints (`Display` is lossy).
`FsAccessor` takes names and targets as the operating system gives them
(on Unix any bytes, as `PosixSourceAccessor` takes `d_name`), through
`os_path` (`to_path`, `path_bytes`; elsewhere only UTF-8 converts, and
other names are `AccessorError::NonUtf8`). The NAR writer writes names and
targets as they are, and the parser takes whatever Nix's `parseDump`
takes. `StoreDir::parse`, `is_in_store`, `to_store_path`, `StorePath::new`
and the like take `impl AsRef<[u8]>` and give Nix's errors for any bytes;
a valid store path is ASCII, so `StorePath` and the store directory stay
text. `brix-store` re-exports `BStr`, `BString` and `ByteSlice`. The
`VirtualStore` maps each
`StorePath` evaluation "adds" to the store to a `StoreObject`: a source
tree (its `SourcePath`, ingestion method, hash and whether a filter
applied), a text file (`toFile`: contents and references) or a
derivation. That map is part of brix's output.

### brix-eval

Two stages:

1. **Compile.** The AST is turned into a flat, index-based IR per source
   file: a `Vec<Node>` with `u32` node ids, a constant pool, and variables
   resolved to `(level, displacement)` exactly as Nix's `bindVars` does
   (including the `with` chain, so scoping bugs are reproduced faithfully).
   The IR contains no pointers, which is what makes it serialisable as the
   compiled-input format later. Function bodies are nodes, so "jump points"
   are node ids.
2. **Evaluate.** Each IR node is compiled, the first time it runs, into a
   Rust closure that captures its operands (Feeley and Lapalme's "closures
   for code generation": no code is generated at run time, the closure
   bodies are ordinary compiled Rust). Evaluation follows Nix's own runtime
   model: environments `{ up, values[] }`, thunks `(node, env)`, blackholing
   for infinite-recursion detection, `Value` a 16-byte tagged union. Thunks and lambdas are both
   `{ env, node }` held inline in the `Value` (no allocation, as Nix's
   `tThunk` and `tLambda`): `node` is an evaluator-wide node number that
   `Evaluator::resolve_node` maps back to its file and node with one
   page-table load. The facade and CLI use `managed_api::GcEvaluator`:
   runtime objects bump-allocate until allocation pressure triggers a precise,
   single-threaded generational collection. Nursery survivors are promoted;
   major collections mark and compact the old generation in place. Scoped root slots and owning cursor
   handles are updated when objects move; borrowed views exclude collection.
   Code, compiled closures, symbols and host caches remain permanent metadata.
   `Config::gc` defaults to a 4 GiB nursery threshold, with a separate
   old-generation budget based on retained size; this is not a total-memory limit. The legacy
   reference-based `Evaluator` remains available for existing low-level users
   and supplies the managed runtime's compiler and host services.
   An environment frame is one heap allocation, a header (`up` and the
   slot count) followed by its slots, 16 + 8n bytes against Nix's 8 + 8n;
   the count keeps every slot access bounds-checked, so a mismatched
   environment and node is a panic, never an out-of-bounds read. `Env` is
   a one-pointer handle to that block (`env.rs`).
   Attribute sets are kept in symbol-id order, as Nix's `Bindings` are
   (`bindings.rs` and `managed_bindings.rs`): up to 64 attributes as one heap block, an 8-byte
   header (position, length) followed by the attributes inline as in
   Nix's `Bindings`; larger sets as persistent B+-trees under a 16-byte
   header, so that `//` on nixpkgs-sized sets copies paths instead of
   whole arrays. A `Value::Attrs` is a thin pointer to the block. Lookups,
   `//`, `removeAttrs`, `intersectAttrs` and formal matching compare two
   `u32`s; iterating a set visits attributes in the order Nix does, which
   `==`, `deepSeq` and primop argument loops make observable. Name order
   is produced only where Nix produces it (its `lexicographicOrder()`
   and the sorts in `attrNames`, `attrValues` and the printers), by
   `Bindings::lexicographic`, which sorts through the symbol table's
   8-byte name prefixes.
   An attribute name handed to Nix code as a string (`attrNames`,
   `mapAttrs`, `filterAttrs`, `zipAttrsWith`) is one shared, immutable
   string cell per symbol over the symbol table's own bytes, made on first
   use, as Nix's `SymbolValue` is; sharing it is unobservable because no
   forced cell is ever written.
   Forcing a thunk and calling a function first make sure 128 KiB of
   native stack remain, and move to a fresh 8 MiB segment (`stacker`)
   when they do not, so deep evaluations never overflow the thread stack
   and only `max-call-depth` bounds them. The check
   (`Evaluator::with_stack`, `stack.rs`) is an inline compare of a
   local's address against a window of the current segment cached on the
   evaluator; only an address outside it asks `stacker`. A node's closure
   calls its strict children's closures directly, without a force or a
   call in between, so the closures carry checks of their own: the
   compiler counts how many strict parents call a closure directly
   since the last check (a node compiled on its own, a thunk's or a
   function body, starts at zero) and wraps every eighth level in the
   same check (`STACK_CHECK_LEVELS` in `closure.rs`). Deeply nested source
   (a long `else if` chain, `a + (b + …)`) grows the stack too, while the
   shallow code most programs consist of runs no extra check. A node
   compiled on its own that is entered neither by forcing nor by calling
   (a `select … or` default, a dynamic attribute name) goes through
   `eval_checked`. The recursive walks over values (the printers for
   `--eval`, `--json`, `--xml` and error messages, `deepSeq`, `==`,
   string coercion of nested lists) check on every level.

Builtins live in `builtins/`, one module per group, each function
documented with the Nix behaviour it reproduces, including error messages.
Printers (`nix-instantiate --eval` form, `--json`, `--xml`) match Nix byte
for byte: they are what the language corpus compares against.

Everything that touches the outside world goes through one trait, the
evaluator's `Host` (`host.rs`): the root `SourceAccessor` every path
value lives on and the paths pure mode may read, `fetch_tree` and
`fetch_file` for the fetch builtins (a failed `fetch_tree` is a
`FetchError`: Nix's message, the fetchers' traces such as "while
fetching the input '…'", which the builtin puts on its error, and the
exit status), `build` for import-from-derivation
(given an `EvalStoreView` of the virtual store, so the host can upload
what the daemon has never seen; it returns the closures of what it built,
the only store paths the import may read afterwards, as Nix's
`allowClosure` allows), `is_valid_path` for the store's
validity checks, `compiled_image` for compiled inputs, and `log`
(`trace`, `warn`) for messages. There is no separate fetcher or builder
trait in brix-eval. Every method but `root_accessor` has a default: the
defaults refuse fetches and builds ("… is not supported by this host (no
fetcher configured)", "… this host has no builder"), know no store, no
readable paths and no images, and log to stderr. `NullHost`, an empty
root and nothing else, is a hermetic evaluator. The facade's
`NativeHost` (`brix/src/host.rs`) implements all of them.

### brix-flake

Flakeref parsing and printing (`github:`, `gitlab:`, `sourcehut:`,
`git+https:`, `path:`, `tarball+`, `file+`, `hg+`, indirect), registries,
`flake.lock` (reads versions 5 to 7, writes version 7), locking with
`follows` and overrides, and the data `callFlake` needs to build the
`inputs`/`self` attribute sets the way Nix's `call-flake.nix` does
(`call_flake.rs`; the facade evaluates them). Nothing in this crate
evaluates Nix code: `readFlake`'s checks run on the `FlakeValue` the
facade makes of an evaluated `flake.nix`, each attribute with its
position, and the facade shows their errors as evaluation errors, with
the code at the positions their traces name. A scheme's fetch is the
`Fetcher` trait (`brix_flake::Fetcher`, `fetch.rs`), and `fetch_input`
wraps it with the scheme-independent rules (lock entries are final, the
`narHash` must match the tree, the store path follows from it), placed
as Nix places them: the fetch and `checkLocks` under the trace "while
fetching the input '…'" (`Input::getAccessor`), the NAR-hash check after
it (`mountInput`, exit status 102).
`NativeFetcher`, which
dispatches to the HTTP (tarballs, files, GitHub/GitLab/SourceHut
archives), Git, Mercurial and path fetchers and keeps the persistent
fetch cache, is behind the `native` cargo feature, so that without it
the crate still compiles for wasm. Mercurial inputs are fetched by
running the `hg` program on `PATH`, as Nix does (`native/mercurial.rs`:
a clone per URL under `<cache dir>/hg`, `hg archive` of the revision,
and Nix's two cache entries, the revision a branch named for
`tarball-ttl` and a revision's `revCount` and tree).

Locked inputs produce source trees whose store path is
`makeFixedOutputPath(recursive, narHash, name)`, the name being `source`
unless the input sets one; brix verifies the `narHash` from the lock
file against the fetched content.

All HTTP goes through one client (`native/http.rs`, on `ureq`): tarball
and file downloads, the Git LFS batch API (a POST) and its downloads,
and, when there is no `git` to run (see below), Git's smart HTTP
protocol, for which gix is given a transport backend on the same client
(`native/git_http.rs`; gix's own backends, curl and reqwest, are not
built). It trusts the CA certificates Nix's `ssl-cert-file` names, and
only those, as curl does for Nix. Downloads (not Git fetches, which Nix
leaves to `git`) give up connecting after `connect-timeout` seconds and
are tried `filetransfer-retry-attempts` times.

Git itself is gix, with two exceptions, both Nix's. A repository to
fetch from (any URL but a local checkout's) is asked with `git` when it
is on `PATH`, as Nix asks it: `git ls-remote --symref` for its `HEAD`,
`git fetch --progress --force [--depth 1] -- <url> <refspec>` into the
bare clone, `git symbolic-ref` for the `HEAD` the clone remembers, with
brix's environment, stdin and stderr, so that what git prints (a server's
failure, its progress) and how it connects, authenticates and trusts
certificates (its own configuration, not `ssl-cert-file`) are Nix's.
The refspecs are Nix's too (a revision goes to
`refs/__nix_internal_fetchers_48ae34d6b380c5ec__/tip-<xx>`), and so are
the rules around the fetch: a ref whose file in the clone is younger
than `tarball-ttl` is not fetched again, whichever input names it; when
fetching a ref the clone has fails, the error is logged (the fetch
settings' `error` sink, Nix's `logError`) and the clone's version taken
with a warning. Without `git`, Nix fetches with libgit2 and brix with
gix, whose messages are their own. And a commit whose signature an input asks to
verify (`verifyCommit`, `publicKey`, `publicKeys`) is checked by running
`git verify-commit` against an allowed-signers file, as Nix does
(`native/git_verify.rs`), so `git` and `ssh-keygen` must be on `PATH`
for such inputs.

A commit is exported as Nix ≥ 2.20 does, walking its tree
(`native/git.rs`). With `exportIgnore` (`fetchGit`'s default) the paths
the `export-ignore` attribute names are hidden by a filtering accessor;
the attribute files are read with gix's attribute matcher in libgit2's
order, which takes in a checkout's working tree and index for any of
its commits (`native/git_attrs.rs`). With `lfs` the files whose `filter`
attribute (looked up the same way) is `lfs` are Git LFS pointers, and
the objects they name replace them: asked of the batch API of the server
the remote's URL leads to (through `git-lfs-authenticate` over `ssh` for
an SSH remote), downloaded, checked against the pointer and kept in
`<cache>/git-lfs` (`native/git_lfs.rs`), each when its file is first
read, as in Nix: the fetch cache keeps no copy of such a tree, only its
NAR hash once `fetch_input` has computed it (`Fetcher::nar_hash_computed`);
a working tree is read as it is on disk. With `submodules` each submodule of
`.gitmodules` that is a gitlink is fetched as a `git` input of its own
(its URL resolved against the parent's as libgit2 resolves it,
`native/git_submodules.rs`) and mounted at its path; a working tree
mounts its submodules' checkouts, read in place. Under `nix-219-compat`
a commit is exported as Nix 2.19 did, by running `git archive` (or `git
checkout` into a repository of its own when submodules are wanted,
`native/git_legacy.rs`); either export is tried when a locked `narHash`
was made by the other. An exported tree carries Nix's fingerprint for
it (`git:<rev>` with `;e`, `;l`, `;legacy` and, with submodules
mounted, `;s`; a clean working tree `git:<rev>` too), which a flake's
fingerprint starts from; brix's fetch cache does not answer for such a
commit by its attributes alone. As Nix's `fetchToStore` does, brix
remembers a fingerprinted tree's NAR hash under the fingerprint
(`Fetcher::remembered_nar_hash`, in the fetch cache) and gives that
hash to every tree with the same fingerprint, the one quirk included
(a working tree's says nothing of its submodules). A Git fetch runs
twice where Nix's does: `Input::getAccessorUnchecked` first asks for
what needs no fetching (`fastOnly`) and drops its error, so what that
try prints is printed again (git's messages about a remote's `HEAD`
among it).

Fetchers report what they do through the `Progress` trait
(`progress.rs`): one activity per input fetched from its source and one
per HTTP transfer, with its bytes. Cache and store hits report nothing.

**Parallel fetching.** Evaluation stays single-threaded and asks for one
tree at a time, as Nix does, but the fetches brix can foresee start early
on a pool of `http-connections` threads (`brix/src/fetching.rs`):

- the lock algorithm announces every fetch a flake's inputs need before
  making the first (`LockDeps::prefetch`, computed by the same steps as
  `computeInput`);
- when evaluation first needs a lock node's tree, the other inputs of the
  flakes that declare it and the node's own inputs start in the
  background (`prefetch_plan`); a flake evaluation never reaches is not
  fetched, as in Nix;
- `flake prefetch-inputs` starts every input at once, as Nix's does.

A background result is handed to the first foreground request with the
same attributes, so evaluation sees exactly what a foreground fetch would
have produced, errors included; a failed fetch of an input evaluation
never needs is never reported. What a background fetch logs (its
"unpacking …" line, a retried download's warnings) is held back
(`brix::log::hold`) and written when the foreground takes the result, so
stderr is what Nix's fetch, made only then, would write; a fetch nobody
takes says nothing, and only the server sees it. A request for a fetch still queued takes
it over rather than waiting behind others; fetches in the foreground
count against `http-connections` too, which in Nix limits all transfers
at once. Git fetches into one bare clone are serialised, within a
process and across the brix processes sharing a cache, by a lock on
`<clone>.lock` (`flock`, as Nix's `PathLocks` takes it); a writer of the
fetch cache holds one on its temporary directory, so that others can
tell an abandoned one from one in progress.

### brix-daemon

A client for the Nix worker protocol: unix socket, or `ssh-ng://` by
spawning `ssh` (for `ssh-ng://localhost`, `nix-daemon --stdio` itself, as
Nix does) and speaking the protocol over its stdio. brix's store host
(`brix/src/store.rs`) uses `SetOptions` (after the handshake; never over
`ssh-ng`, as in Nix), `IsValidPath` and `QueryPathInfo` (validity, NAR
hashes, `path-info`, and the closure of what an import-from-derivation
built, which is all of the store the import may then read), `AddMultipleToStore`
(to upload the sources, `.drv` files and `toFile` texts a build needs: every
missing object in one operation, as content-addressed paths, each NAR
streamed as it is serialised), `AddToStore` (for `derivation add` and
single texts),
`BuildPathsWithResults` (builds, import-from-derivation included),
`QueryMissing` (dry runs), `QueryValidDerivers` (`path-info
--derivation`), `EnsurePath` (substitution), `AddIndirectRoot` (`result`
links) and `NarFromPath` (to read a built output back into an in-memory
accessor for `import`). A few more operations (`QueryValidPaths`,
`BuildPaths`, `BuildDerivation`, `AddTempRoot(s)`, …) are implemented
and tested against a real daemon but have no caller.

A session opens one connection, on first use, and keeps it: there is no
connection pool and no ssh master connection (`max-connections` is only
validated), and a connection on which an operation failed other than by
an error the daemon reported is refused from then on rather than
replaced.

### brix-alloc

The allocator the `brix` binary and the brix-eval measurement examples
install (`brix_alloc::install!()`, then `brix_alloc::init()` at the top
of `main`); library users keep their own. It is mimalloc, built without
transparent huge pages and with eager arena commit turned off by a C
constructor (`tune.c`), under `Prefaulting`, a wrapper that has the
kernel populate the top of each large allocation (the evaluator's arena
chunks) with `MADV_POPULATE_WRITE`, on a helper thread by default. About
9% of a `firefox.drvPath` evaluation was first-touch page faults;
pre-faulting took it from 789 to 680 ms. `BRIX_PREFAULT` (`off`, `sync`
or `background`) selects the policy; unset, it is `background` where
the kernel supports it.

### brix (library) and brix-cli

The facade (`crates/brix`) is the evaluation and store backend, for any
program; what a command makes of the results is `brix-cli`'s. It is the
native host, with no cargo features: it reads the real file system,
memory-maps images, keeps caches under `$XDG_CACHE_HOME`, fetches on
threads and talks to the daemon. Evaluating without the network or a
store is a matter of configuration (`FetchConfig::enabled`, no
`daemon`), which keeps Nix's semantics for what is already on the
machine (trees in the store directory, local paths); a host with no file
system at all, such as wasm, builds on brix-eval and brix-flake (without
`native`) and implements `brix_eval::Host` itself. It exports:

- `Session`: one evaluator, its host (`host.rs`, with the root file
  system of pure evaluation in `root_fs.rs`) and everything evaluated so
  far. It evaluates expressions and files, locks and calls flakes
  (`Flake`) or only locks them (`lock_flake`, for the `flake` commands
  that Nix does not call the flake for), and hands back the derivations
  and store objects evaluation produced. Its store operations (`store_realise`, `store_query_missing`,
  `store_path_info`, …, uploading what evaluation produced first) are in
  `store.rs`, its image compilation (`compile_input`,
  `compile_flake_inputs`) in `compiled.rs`.
- `Cursor` (lazy access to a value, with Nix's attribute-path errors;
  its text getters refuse what is not UTF-8, its byte getters keep it),
  `Value` (an owned snapshot, strings, paths and attribute names as
  bytes),
  `Format`, `AutoArg`, `ContextElem`.
- `Config` and its parts, `Error`, `Result`, and the brix-store,
  brix-flake and brix-eval types its signatures use.
- The modules `config`, `cursor`, `error`, `compiled`, `store` (the
  daemon's build and query types, and the build logger),
  `derivation_json` (`nix derivation show`'s JSON read back, for
  `derivation add`), `nixconf` (`nix.conf` and Nix's setting parsers),
  `schemas` (flake schemas evaluated and their inventory read, for
  whoever resolves installables or walks outputs) and `log` (stderr as
  Nix's logger writes it, with the `Logger` a program installs in place
  of plain stderr, Nix's global `logger`).

`brix-cli` is the `brix` binary: a command-line model built from the
inventory of the real `nix` CLI (`tree.rs`, `args.rs`) and a module per
command group, plus the presentation the facade leaves to it:
installables and their resolution through the schemas for a command's
roles (`installable.rs`), the JSON documents of `derivation show` and
`flake metadata --json` (`output.rs`), `flake show`'s inventory and
`flake check`'s walk (`schemas.rs`) and the progress bar (`progress.rs`).

`brix-cli` uses no argument-parsing library. Its command tree is data:
`data/nix-cli-tree.json` is the reference `nix __dump-cli` with the
descriptions stripped (`scripts/update-cli-tree.sh` regenerates it),
`tree.rs` loads it and adds brix's extensions (`flake compile`,
`--compiled-input`, `--no-compiled-inputs`, each marked as such in its
help) and the hidden `__dump-cli` and `__parse`, `descriptions.rs` holds
the help text, and `args.rs` parses `argv` the way Nix's
`Args`/`MultiCommand` do and renders `--help`. A test compares brix's
`__dump-cli` with the reference's. brix implements `eval`, `build`,
`run`, `shell`, `develop`, `print-dev-env`, `fmt`, `formatter run` and
`build`, `bundle`, `path-info`, `search`, `derivation show` and `add`,
`hash`, `help`, `help-stores` and the `flake` subcommands `metadata`,
`info`, `show`, `lock`, `update`, `check`, `prefetch`,
`prefetch-inputs`, `init`, `new` and `compile`.

`develop` and `print-dev-env` work as in Nix: brix writes the
`<name>-env` variant of the derivation (its builder runs Nix's own
`get-env.sh`, byte for byte, so the derivation and its output paths are
Nix's), builds it through the daemon, and turns the JSON environment it
produces into the bash script `print-dev-env` prints and `develop` starts
bash with. `run`, `shell` and `develop` start their program with
`execve`, passing the environment in the order Nix's `setenv` calls
leave it.

The other `nix` commands (`repl`, `log`, `why-depends`, `profile`,
`store`, `registry`, …) are run by Nix itself: brix parses the
command line (so `--help` and usage errors are its own), refuses the brix
extension flags Nix would not know, and replaces itself with the `nix`
binary named by `BRIX_DELEGATE_NIX`, given the original arguments. The
package's wrapper sets that variable to nixpkgs' Nix (the module's to
the Nix it wraps); without it these commands fail and say so. To agree
with that `nix`, brix reads `nix.conf` as Nix does (system file, user
files, `NIX_CONFIG`, then the command line, over `NIX_REMOTE` as the
store's default; a setting under any of its aliases) for the settings
that choose and configure the store, those a client forwards to its
daemon and those that affect fetching; evaluation settings stay
command-line only. Like Nix's `main`, before running one of its own
commands brix looks for an Internet connection (`internet.rs`: an
interface address other than loopback and link-local, or a lower-case
proxy variable; `BRIX_HAVE_INTERNET=1|0` decides instead, for tests).
Without one it warns and goes on as with `--offline`, which, as in Nix,
does not stop fetching: the network settings the user has not set
change (no substitution by the daemon, every earlier download current,
one attempt, a one-second connection timeout), and what was never
fetched is still downloaded.

On a terminal (Nix's `isTTY()`: not `TERM=dumb`, no `NO_COLOR`) and
unless `--log-format` asks otherwise, the CLI draws Nix's one-line
progress bar on stderr (`progress.rs`, installed as the facade's
`brix::log::Logger`): `[2/1/3 fetched, 12.3/45.0 MiB DL]
downloading '…'`. It is drawn only while the evaluator waits for a fetch
or an image compilation, and erased when the wait ends, so nothing the
single evaluator thread prints can meet a half-drawn line; messages
logged through `brix::log::stderr` during a wait are written above it.

The binary installs `brix-alloc` and ends its session with
`Teardown::Leak`: the store connection is closed and the timings
`BRIX_TIMINGS=1` asks for are printed, but the remaining managed heap, permanent arena and evaluator's
tables are left for the kernel to take back at exit instead of being
freed piece by piece (and handed back by mimalloc a few pages at a time
at exit).

## Paths and names as bytes

Nix's strings, paths and file names are `std::string`s: bytes, with no
encoding. A file may be named `caf\xe9` (Latin-1), `/a + "\xff"` is the
path `/a\377`, `readDir` lists such names as attribute names, paths sort
as unsigned bytes (`strcmp`), and Nix prints all of them as they are,
unquoted and unescaped. brix keeps them as bytes throughout, the same
way:

- **Core types.** `CanonPath` and everything a `SourceAccessor` returns
  are bytes (see *brix-store*); so are path literals and file origins in
  brix-syntax, path values in brix-eval (`PathVal { accessor, path:
  &[u8] }`, made with `Evaluator::mk_path(id, impl AsRef<[u8]>)`), the
  map keys the evaluator keeps per path (copies to the store, imports:
  `(AccessorId, &[u8])`), source origins (`ir::Origin::File { display,
  path }`, `BString`) and positions (`Position::display`, `BString`), the
  owned values the facade hands out (`OwnedValue::Path`,
  `brix::Value::Path`, `Cursor::coerce_to_source_path`, all `BString`),
  and compiled-input images, whose directory entry names and symlink
  targets are `Vec<u8>` (format version 11).
- **Messages.** A message Nix shows is built as bytes: brix-eval's `msg!`
  and the errors' `message_bytes()`, never `Display`, `to_string()` or
  `from_utf8_lossy` of a path; `Display` of `BStr`, `BString`,
  `CanonPath` and the errors is lossy and only for Rust-side text (logs,
  brix-flake's errors). The tests compare such messages with Nix's byte
  for byte, never through a lossy conversion; they build trees with
  names like `z`, `caf\xe9`, `\xff`, `a\x80b`, a directory `d\xe9/` and a
  symlink `l\xff` to `t\xe9` (`brix_testutil::write_bytes`, `os`), which
  pin both the bytes and the unsigned order.
- **The operating system.** On Unix a file name is any bytes, and
  `brix_store::os_path` converts both ways (`OsStr::from_bytes`), without
  touching the file system, so the module is wasm-clean; elsewhere only
  UTF-8 converts. The file-system accessor, the fetch cache's trees, the
  trees `flake init` and `flake new` write, the command line (`--expr`,
  `--apply`, `--file`, `-I`, store-path installables and symlinks to them,
  the working directory) and the daemon's logs, activity text and build
  errors take bytes. CLI operations that use the current directory as
  text, such as resolving flake references, check its UTF-8 encoding.
- **Fetched trees.** Tarballs (a tar member's raw path and link name, a
  zip entry's raw name), Git trees (gix's `BStr` paths go straight into
  `CanonPath`) and their fingerprints (Git tree hashes over byte names;
  Nix's `CanonPath` order, which compares C `char`s, through
  `std::ffi::c_char`) keep names as bytes. Working trees also match
  `CanonPath::isAllowed`'s directory visibility: on signed-char
  platforms, a tracked `a\xe9` can hide the parent of tracked `a/b`.
  This visibility is computed when the accessor is created, so reads
  keep using hash lookups.
- **What stays text, and why.** URLs (Nix decodes percent-escapes to
  bytes; brix refuses what is not UTF-8 there), flake references, lock
  files and registries (JSON: Nix's own writer fails on such a name with
  nlohmann's error 316), fetcher arguments and the display prefixes of
  fetched trees (`«github:…»`), `SourceAccessor::fingerprint`, the store
  directory and store paths (a valid store path name is ASCII), settings,
  Mercurial's output, Git remote URLs and LFS, and brix-flake's error
  messages (lossy for a byte name, where Nix prints it as it is).
  `CanonPath::to_str` is the one way from a path to text, for these
  consumers, which say what they do with a path that is not UTF-8.
  `KNOWN_DEVIATIONS.md` lists what differs from Nix as a result.

The hot paths cost the same or less: `CanonPath` has the layout of the
`String` it was, built in one pass with one allocation fewer; a path
value is the same fat pointer; appending to a path and linking path
literals no longer check UTF-8, an image's entry names are no longer
UTF-8-checked when it is opened (`rkyv`'s `ArchivedString` was), and
`FsAccessor::read_dir` no longer checks each name for UTF-8.

## Compiled inputs

A flake input such as nixpkgs is parsed thousands of times a day by every
Nix user, and Nix spends a large share of a cold `nix build` lexing and
parsing. brix can compile an input once into an image containing:

- every `.nix` file as IR (already resolved, already desugared), with a
  module table so `import ./foo.nix` is an index lookup, and each
  module's line table so positions resolve without the text;
- the file tree's shape (directory listings, sizes, symlinks) and the
  NAR hash of the tree and of every subtree, so `readDir`, `pathExists`
  and `src = ./.`-style path additions never touch the source;
- optionally (`brix flake compile --with-tree`) every file's contents,
  deflate-compressed, for an image that works without the checkout; by
  default none are stored and `readFile`, `hashFile` and error excerpts
  read the source tree, which the cache always has at hand.

The file is a 64-byte preamble (magic, format version, fingerprint,
the archive's length, a checksum of the preamble), an `rkyv` archive,
and a table of checksums of the archive's parts (see below). The
archive is validated once when it is opened;
its big tables (names, module records, module code) are flat vectors of
integers and bytes, so that costs a few range checks rather than a walk
over every module. A module's code is fixed-width little-endian 32-bit
words in one code area: a record per node (three or four words for
variables, selections and calls; a tag byte plus a 24-bit field for
naturally small values such as an environment level), fixed-size
entries for attributes, formals, list elements and string parts.
Importing a module
reads its words in place from the mapping and writes the evaluator's
tables straight into the arena with loads, range checks and stores;
string literals are copied with one `memcpy`, and the line table is
decoded only when a position in the module is shown. There is no
varint, delta or decompression step and no intermediate `ir::Code`.
nixpkgs compiles to 151 MiB this way, in the same two seconds as
before: 50 MiB of nodes (3.2 words each on average), 33 MiB of string
literals, 17 MiB of attribute definitions. The previous encoding,
LZ4-compressed varint streams decoded into `ir::Code` on every import,
took 72 MiB but spent 7.5% of a firefox evaluation's
instructions and 16% of a NixOS toplevel's decoding and linking, against
2.1% and 3.9% now; a cache file is memory-mapped (`memmap2`), so only
the imported modules' pages are read. (Source: 193 MiB; the first,
rkyv-only layout: 453 MiB.)
Evaluation over an image behaves exactly as over the source tree: it is
a cache keyed by the input's `narHash`, never a semantic change.

Implementation (`brix_eval::image`, `brix::compiled`):

- One archive per input, named `<nix32 narHash>-<first 16 hex digits of
  the fingerprint>.brix` under `$XDG_CACHE_HOME/brix/compiled/` (by
  default `~/.cache/brix/compiled/`). The fingerprint hashes the format
  version, the compiler version, the parse options, the evaluator's
  builtin symbols and its global scope, so an image compiled by a
  different brix (or for a different set of host primops) is rejected,
  never misread. Cache files are written to a temporary name and renamed
  into place, and memory-mapped when used; an image the user names with
  `--compiled-input` is read whole instead, since brix does not own the
  file, and must open or the evaluation fails.
- An input that is downloaded as an archive (a forge's `github:`,
  `gitlab:` or `sourcehut:` archive, a `tarball` input) is compiled
  while it downloads. The response body is decompressed and unpacked as
  it arrives (`unpack_stream` in `brix-flake`'s `native/tarball.rs`;
  zip archives, whose directory is at their end, and anything that goes
  wrong are left to the unpacking of the whole download, which gives
  Nix's errors), and the fetch shows each regular file to the session's
  `brix_flake::ArchiveObserver`. That is `brix::compiled`'s
  `StreamCompiler`: one thread per archive compiles the `.nix` files in
  the archive's order, each as it arrives, and once the archive has
  unpacked compiles the tree into the cache with those modules taken as
  compiled (`Precompiled` in `brix_eval::image::writer`). A module
  compiled ahead is used only where the tree has that source at that
  path, so the image is byte for byte the one the tree compiles to. The
  evaluator that asks for the tree's image waits for that thread and
  opens the cache file; a session that ends waits for the images still
  being written. A cold `github:NixOS/nixpkgs` evaluation has all 41,308
  modules compiled by the time the download ends.
- A bad cache file is replaced, at open or in use. One that does not
  open (another format version or fingerprint, a damaged preamble,
  header, archive or tree, the NAR hash of another tree) is compiled
  afresh on the spot, and the evaluation goes on. Damage elsewhere is
  found when the damaged part is first used (a module when it is
  linked, a directory when a lookup reaches it): that operation fails
  with an error naming the image, and after the failed evaluation the
  cache file is deleted, so the next one compiles the input again.
  Images the user named are never deleted.
- Images are untrusted input. Decoding them uses no `unsafe`; the one
  `unsafe` block is the memory map in `brix::compiled`, which relies on
  nothing outside brix rewriting a cache file in place, the usual caveat
  of a mapping. Opening checks the preamble, validates the archive and
  checks that the root directory carries the header's NAR hash. Every
  other directory is checked when a lookup first reaches it (hello over
  nixpkgs reaches 890 of 37,000): its names are valid and strictly
  sorted (bytes, as Nix's; sorted as unsigned bytes, the order of a
  NAR; format version 11), its blob indices in range, and the
  directories it lists come after it (so the tree has no cycles), in
  increasing order, and are
  listed by no other directory: it claims each as its parent, so a
  shared subtree fails when its second parent is reached, before it is
  expanded twice. `Image::verify` also checks that the tree reaches
  every directory. Lookups follow
  symlinks inside the tree as the file system would (at most 40); a path
  that leads out of the tree through one is answered by the source tree.
  Linking a module validates everything it reads: every table index and
  range, node reference, position, tag and symbol, that a `with` variable
  names an enclosing `with` (so lookups follow a finite chain). Counts
  that size allocations are bounded by the words present. Damage in
  module code therefore surfaces as a clean evaluation error when the
  module is imported. Static scoping (environment levels and
  displacements) is not re-derived; a damaged one hits the evaluator's
  bounds checks.
- Damage that leaves a valid image (a flipped bit in an integer
  literal, a string, a name, a directory's NAR hash, a file's length)
  would evaluate to a different value or store path, so every part of
  the archive has an XXH3-64 checksum (`xxhash-rust`), computed by the
  writer from the archive it serialised with the loader's own functions,
  and compared before the part is first used, once per image (a bitset
  of the parts that matched): the header (with every table's length)
  when the image is opened; a directory (its NAR hash, and the span of
  the archive its entries and their names occupy, in one pass) when a
  lookup first reaches it; a chunk of 32 names when one of them is first
  interned; a module (its path, record, error, words and bytes) when it
  is linked, which also vouches for the module lookup that found it (the
  path table is checked only when a lookup misses); a blob (its length,
  encoding and bytes) when it is read or `stat` reports its length.
  Opening costs two checksums of a few dozen bytes; the rest is a bulk
  XXH3 pass over each part an evaluation uses (for a module, its words
  and bytes, which linking reads anyway), then a bit test: hello over
  nixpkgs hashes 2.5 MB of modules, 0.6 MB of directories and 0.9 MB of
  names. `Image::verify` compares everything at once. A test flips every
  bit of a small image: each flip is refused, or hits only alignment
  padding and changes nothing the loader reads.
- Symbols: builtin symbols are stored by id (every evaluator with the same
  fingerprint creates them identically); each module lists the other names
  it uses in the order the compiler first met them, and the loader interns
  that list in order. This reproduces the symbol ids compiling the source
  would have produced, so even symbol-id-ordered behaviour (`rec`
  evaluation order) is identical. A per-evaluator cache of the symbol for
  each of the image's names turns a name an earlier module already
  interned into one array load. Because a module's ids differ from the
  loading evaluator's, linked attribute sets carry an `id_order`
  permutation, the order their bindings are evaluated in and the order
  of the set they build (sorted at link time, skipped when already in
  order); displacements are unaffected.
- Path literals are compiled against a marker root, stored without it and
  given the mount point when linked, so one image serves any store
  directory; a module whose path literal escapes the tree (or whose values
  outgrow the encoding) is served from source.
- `ImageAccessor` answers `precomputed_nar_hash` for every directory, which
  `VirtualStore::add_path` consults for unfiltered recursive SHA-256
  ingestion (`"${./dir}"`, `builtins.path`, `src = ./.`).
- The facade compiles an input on first use when it has at least 50 `.nix`
  files (nixpkgs takes about two seconds warm), `brix flake compile` does
  it ahead of time (`--out DIR` for shipping images), and
  `--compiled-input NAME=PATH` / `--compiled-input NARHASH=PATH` and
  `--no-compiled-inputs` control use. These are brix extensions to the nix
  command tree and say so in their help.

## Testing

`cargo test --workspace` runs every crate's tests. It needs the devshell
(`nix develop`), which sets the three variables the tests read:
`BRIX_NIX_SRC` (the reference Nix source tree: the language corpus and
Nix's unit-test data), `BRIX_NIX_BIN` (the reference `nix`, the oracle,
with `nix-instantiate` and `nix-store` beside it) and `BRIX_NIXPKGS` (a
nixpkgs tree). A test that needs one fails when it is missing rather
than passing silently. The flake check `checks.<system>.tests` runs the
same suites in the sandbox, with the same variables, one derivation per
workspace member (brix-eval's with its `work-counters` feature, which
the image tests compare); CI builds it together with the package. There
the oracle has no daemon and uses a local store under `$HOME`, created
before the tests start; what only the daemon's store does alike in brix
and Nix is tested with the opt-ins below.

- **Language corpus** (`$BRIX_NIX_SRC/tests/functional/lang`): parse-okay,
  parse-fail, eval-okay, eval-fail, each compared to its `.exp`/`.err.exp`.
- **Differential tests** against `$BRIX_NIX_BIN`, compared byte for byte:
  expression tables in the plain, `--json` and `--xml` formats
  (`brix-eval/tests/differential.rs`, `builtins_oracle.rs`); random
  expressions from a generator, shrunk to a minimal case on a mismatch
  (`fuzz_differential.rs`, 500 fixed seeds by default); `nix-instantiate
  --parse` over every 50th file of nixpkgs; `hello.drvPath` and
  `lib.version` of nixpkgs through the facade; and the `brix` binary
  against `nix` (`brix-cli/tests`: the command tree, outputs and errors,
  lock files, fetched-tree attributes, `readFlake`'s errors).
- **Store vectors**: the hash, store-path, content-address, NAR and
  derivation functions checked against what the real `nix` computes,
  against `.drv` files it wrote and against Nix's unit-test data.
- **Compiled images** (`brix-eval/tests/image.rs`): an image must look
  and evaluate exactly like its source tree, work counters included, and
  every kind of damage must be refused or fail the import cleanly.

`brix-testutil` holds what these suites share: the environment helpers
(`nix_bin`, `nix_src`, `nixpkgs`, `nix_instantiate`, each failing loudly
when its variable is missing), one `Oracle` builder for commands of the
reference `nix`, with a pinned `NIX_CONFIG` and a filter for what its
stderr says about the machine rather than the command (the crash
reporter, no Internet access, a chroot store), a deterministic
`GitRepo`, scratch stores, `copy_dir` and `require_env` for the opt-in
tests. It depends on no brix crate and is not published.

Some tests are `#[ignore]`d. Those that need more than the devshell are
opt-in through a variable, `1` to run them; run without it, such a test
fails rather than pass:

- `BRIX_TEST_REAL_DAEMON`: tests that talk to the running `nix-daemon`
  (builds, import-from-derivation, store reads and writes);
- `BRIX_TEST_REAL_STORE`: tests that write into the user's Nix store
  through the oracle (`nix flake prefetch`, derivations);
- `BRIX_TEST_NETWORK`: tests that download from GitHub.

`scripts/test-all.sh` runs `cargo test --workspace --no-fail-fast` and
then every ignored test gated on `BRIX_TEST_REAL_DAEMON` or
`BRIX_TEST_REAL_STORE`, with those set (it needs the devshell and a
running `nix-daemon` with a writable store); `--network` adds the
`BRIX_TEST_NETWORK` ones. The flake check `checks.<system>.gated-tests`
(nix/tests/gated.nix) runs the daemon- and store-gated tests in the
sandbox too, through `scripts/test-all.sh --gated-only`: the reference
`nix-daemon` serves a store under a root of its own, seeded with the
closures the tests name, on a socket of its own
(`NIX_DAEMON_SOCKET_PATH`, `NIX_REMOTE=daemon`); the diverted root stays
invisible because brix and the oracle read it only through the daemon.
Only the network-gated tests have no check: the sandbox has no network.
The other ignored tests are slow (the whole of
nixpkgs through the parser, the evaluator and the image compiler, a
NixOS system, the fuzzer's long run) and are run by hand with
`cargo test -- --ignored <name>`.

**Benchmarks** are not automated: no CI job runs them. The criterion
benches (`cargo bench -p brix-syntax`: parse all of nixpkgs;
`cargo bench -p brix-eval`: a few synthetic expressions, and
`hello.drvPath` and `lib.version` of `$BRIX_NIXPKGS`), the measurement
examples of brix-eval (`eval_nixpkgs`, `profile`) and the PGO and BOLT
pipeline `scripts/pgo-build.sh` are run by hand.

## Performance: measured and rejected

Optimisations that were implemented, benchmarked against a common
baseline (best of 5 or 10, interleaved, on lib.version, hello, numpy,
firefox and a NixOS toplevel; run-to-run noise is about ±2-3% in process)
and not kept. They are recorded so they are not re-argued without new
evidence; the branches are kept in git.

- **Per-site inline caches for attribute lookups** (`perf/icache`). A
  monomorphic cache per static selection, `?` and `with`-variable site,
  keyed by the identity of the immutable `Bindings`. Only 59% (firefox) and
  27% (NixOS toplevel) of static selections see the same set again at the
  same site, there are 2.36 M distinct (site, set) pairs in the NixOS
  evaluation, and misses on the large `pkgs` set number about 1,400, so
  the cache's bookkeeping cost more than it saved (0 to +2%).
- **Fused node shapes** (`perf/fused`). Specialised nodes for a call of a
  variable, `lib.f x`, a one-name selection of a variable and `x ? a`
  removed 24% of node dispatches with identical results and no measurable
  change in time. Dispatch is not where evaluation time goes; closure
  compilation (above) captures what there was to gain.
- **Collapsing the call path** (`perf/calls`). Applying arguments in a loop
  was already done; iterating `App` chains or `__functor` would change the
  depth at which `max-call-depth` fires, which Nix's tests and ours pin.
- **An inlined "already evaluated" fast path in `force`**: 0.4-2.4% slower
  than one out-of-line function.
- Earlier rounds: inline 8-byte name prefixes in attribute entries (net
  zero), SIMD count-less-than search in bindings (slower than binary
  search at n ≤ 64), eliding thunks for lambdas in lazy positions (no
  measurable gain; other elisions are inadmissible because Nix prints an
  unforced cell as «thunk»), transparent huge pages (pathological under
  memory pressure).
