# brix

A bug-for-bug compatible implementation of the Nix language and evaluator, written in Rust.
Brix is designed to be compatible with Nix up to being a drop-in replacement. Every line of the repository except the readme was written by LLMs, guided by humans (Claude Fable 5.1, 5.2, Claude Opus 5.5, and GPT 6 Astra).

## Motivations

### Better compatibility with enterprise anti-virus

Simple Nix expressions are extremely heavy with respect to file opening. This is because when you run
a Nix command which for example uses `stdenv` (like `stdenv.mkDerivation`, Nix will evaluate the entire
compiler toolchain and its dependencies, and the same for any other dependencies referenced. This can
involve many files. On a normal unmodified Linux system, this is typically very fast, as Linux filesystems
have heavily optimised file opening operations.

This is slightly different on MacOS. Anecdotally, APFS has a slightly different set of tradeoffs which
lead to workflows which open lots and lots of files being ever-so-slightly slower.

It's very different when enterprise antivirus gets involved. It's typical for enterprise antivirus to
intercept every file open and do some basic fingerprinting. This is unfortunate, but is mandatory in many
organisations. This interception dramatically slows down file open calls, and so Nix evaluation can slow to
a crawl.

Together these both mean that if you work on Mac, your gorgeous Apple Silicon processor is being under-utilised
and your CPU time is gone. I've experienced waiting for up to 15 minutes to simply pull in a new version of nixpkgs,
with the time going largely in copying the contents to the Nix store. All further evaluations are then made slower
due to the high cost of opening files.

### Store-free Nix evaluation

Nix and flake evaluation currently assumes the presence of a global Nix store. This is a blessing and a curse; the
blessing being well documented, the curse being that CI workflows are hard to accelerate because evaluating will
materialise all required store paths and therefore usually lead to potentially millions of file reads for even simple
derivations. It seemed useful to build a version of Nix which is capable of evaluating possibly whole operating systems
without an explicit `/nix/store` path that needs reading (aka can use a virtual filesystem).

### Better compatibility with running Nix in CI

Being able to compile Nix down into WebAssembly seemed attractive from a sandboxing perspective. It's common to end up
building build farms as a more mature user of Nix, but these end up with odd properties with regards to trust. For example,
granting access to a remote builder with `--builders` in a way which can be used usefully is roughly equivalent to granting
root on that system. Granting the ability to write into a cache grants the ability to permanently poison that cache.

It's possible to configure your CI system so this doesn't happen, but it's complicated to get right. There's some sense that
if you had the ability to run Nix in a trivial sandbox, possibly even compiled to WebAssembly, you could have a single CI
entrypoint which is properly isolated and avoids most of those issues. Here being able to run Nix as a library or with more
control isn't required, but is an advantage.

### Wanting to see if more work can be executed up-front

Nix flakes are highly repetitive; you might pull their inputs once and then execute Nix commands hundreds of times with those
inputs. Wanted to see if we could leverage that asymmetry to improve performance (for example to have a faster-to-process
intermediate representation).

### Wanting to see if LLMs are able to recreate something like Nix in a simple, effective way

Useful to benchmark every so often. Claude couldn't do it in January or May. Now it can.

## Why not just contribute to Nix/Lix/etc?

I have some time to build, but not time to coordinate. My time is spotty, I might have thirty minutes now and then none
for a week. Open source development isn't really compatible with this; a long lived project like Nix must be
consensus-driven meaning that for any significant work a lot of community engagement is necessary (and I have no track record).
Roughly, I have time for building, but not for coordination, and I think introducing the above to Nix would take months.
Do I expect to build and maintain a long term competitor to Nix, or to try to fork the community? No, of course not!
This project expects to maybe just die, to ideally influence some of the ideas in Nix, to be a high quality testbed, not a
foundation.

See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for the design and
[crates/brix-eval/KNOWN_DEVIATIONS.md](crates/brix-eval/KNOWN_DEVIATIONS.md)
for where brix knowingly differs from Nix.

## Key features

### Massive improvements over Nix on enterprise-encumbered systems

This is really a sidestep rather than a feature. As mentioned, on an appropriately 'defended' enterprise system 
it's not uncommon for pulling a new nixpkgs version to take 5-15 minutes. This time is sidestepped through the architecture
of 

### Significant performance improvements over Nix (and Determinate Nix) on practical systems

We've tried to be very careful with performance related tradeoffs and this pays off in user experience. Brix is significantly
faster than Nix across almost all codepaths. In the below table, values are expressed as (wall clock time/CPU time). What I
take away is that in most cases you should expect a significant performance improvement from using Brix.

| Benchmark                                              | Nix          | Lix          | Determinate Nix (serial) | Determinate Nix (parallel) | Brix (serial)  | Brix (parallel) |
|--------------------------------------------------------|--------------|--------------|--------------------------|----------------------------|----------------|-----------------|
| Developer's NixOS configuration                        | 9.071/15.57  | 8.219/14.44  | 10.472/9.76              | 6.802/14.08                | 4.982/*5.13*   | *4.847*/7.35    |
| `nix flake check --no-build` on Nix                    | 21.882/32.49 | 41.774/46.47 | 4.147/3.86               | *1.193*/8.32               | 1.801/*1.98*   | 1.279/3.29      |
| `nix flake show` on Nix                                | 2.602/2.43   | 2.032/6.15   | 2.493/2.41               | 1.112/5.42                 | 1.091/1.22     | *1.073/1.21*    |
| `nix flake check --no-build` on large personal project | unknown      | unknown      | 57.720/69.37             | 36.681/117.51              | 35.318/*57.64* | *17.157*/99.63  |

Nix and Lix are listed as unknown on the personal project because they error for a presently unknown reason.

As you can see, we use vastly less CPU than all alternative Nixes compared, and are competitive with Determinate Nix's parallel implementation as well. 

## Using brix as `nix`

The flake provides modules for NixOS, nix-darwin, System Manager and Home
Manager. Import the matching module into your configuration:

| Configuration | Module |
|---|---|
| NixOS | `inputs.brix.nixosModules.default` |
| nix-darwin | `inputs.brix.darwinModules.default` |
| System Manager | `inputs.brix.systemManagerModules.default` |
| Home Manager | `inputs.brix.homeManagerModules.default` |

Each also has a `brix` alias. All four expose the same options:

```nix
{ pkgs, inputs, ... }:
{
  imports = [ inputs.brix.nixosModules.default ]; # Choose from the table above.

  programs.brix = {
    enable = true;
    fallbackPackage = pkgs.nix; # Or pkgs.lix, or another Nix package.
  };
}
```

`fallbackPackage` defaults to `pkgs.nix`. The module installs that package
with only `bin/nix` replaced by a wrapper running brix. Commands brix
delegates go straight to the selected package's original `bin/nix` through
an absolute store path. The wrapper pins `BRIX_DELEGATE_NIX` to that path,
even if the variable was already set in your shell. Other commands,
including `nix-store`, `nix-build` and `nix-daemon`, remain those of the
selected Nix implementation.

To bypass brix for one invocation, or for all `nix` invocations in a shell
and its children:

```sh
BRIX_USE_NIX=1 nix eval --expr '1 + 1'
export BRIX_USE_NIX=1 # Use original Nix for the rest of this shell.
unset BRIX_USE_NIX   # Return to brix.
```

Only the value `1` activates the bypass. It runs the selected fallback
directly, before brix parses any arguments, even for commands brix normally
implements. This variable is handled by the modules' `nix` wrapper; it does
not change standalone `brix` invocations.

The combined package is available as `programs.brix.finalPackage` and is
also assigned to `nix.package`. Move an existing `nix.package` selection to
`programs.brix.fallbackPackage`; defining both will produce a module option
conflict. The modules leave `nix.enable` unchanged, so they also install the
CLI when Nix management is disabled (for example, with an externally managed
daemon on macOS or System Manager). Home Manager installs it in your user
profile; the other modules install it in the system profile. Your shell must
have that profile ahead of other Nix installations on `PATH`.

You can override `programs.brix.package` with another package providing
`bin/brix`. By default the modules use this flake's PGO/BOLT build on native
`x86_64-linux` and `aarch64-linux`, and the regular build on Darwin and when
cross compiling. Both use the host's package set; no overlay is required.

## Developing

Build the optimized CLI with `nix build .#brix-pgo-bolt`, then run
`./result/bin/brix`. This output is available for native `x86_64-linux` and
`aarch64-linux` builds, and as `pkgs.brix-pgo-bolt` through the overlay.
The ordinary `brix` and default outputs remain the faster-to-build packages.

The optimized build uses the workspace's fat LTO release profile, trains
LLVM PGO on the locked nixpkgs and Nix language corpus, then instruments
and rewrites the CLI with BOLT. Training also covers compiled inputs,
local flake commands, and garbage collection: extra CLI evaluations use a
16 MiB startup / 8 MiB steady nursery and must collect at least twice per run.
It runs offline inside the Nix sandbox, without a
Nix daemon or hardware performance counters. Instrumentation, merged PGO
data, the PGO build, and BOLT optimization are separate cacheable stages;
the final binary runs the training workloads again as a smoke check.
The wrapper preserves the ordinary package's Nix delegation behavior.

`nix build .#checks.x86_64-linux.pgo-bolt` builds and checks the optimized
output (also included in CI and `nix flake check`). BOLT statistics are in
`result/share/brix-optimization/bolt.log`. The package exposes
`instrumented`, `profiles`, `pgo`, and `unwrapped` passthru attributes for
inspecting intermediate stages. The local script below shares the same
training workload and also supports PGO-only and BOLT-only experiments.

```
nix develop             # rustc, cargo, crate2nix, the reference nix, …
cargo test --workspace  # needs the devshell: BRIX_NIX_SRC, BRIX_NIX_BIN, BRIX_NIXPKGS
scripts/test-all.sh     # also the tests that need a nix-daemon (--network: GitHub too)
nix flake check         # the test suites and the package, inside the sandbox
crate2nix generate      # after any change to Cargo.lock
scripts/pgo-build.sh    # a PGO + BOLT optimised local build (about 18 minutes)
```

`scripts/test-all.sh` runs `cargo test --workspace --no-fail-fast`, then
every `#[ignore]`d test whose ignore reason names `BRIX_TEST_REAL_DAEMON=1`
or `BRIX_TEST_REAL_STORE=1`, with those variables set. It needs the
devshell and a running `nix-daemon` with a writable store. With
`--network` it also runs the tests gated on `BRIX_TEST_NETWORK=1`. The
other ignored tests are slow ones (whole-nixpkgs runs, the fuzzer's long
run), run by hand with `cargo test -- --ignored <name>`.

## Environment variables

| Variable | Read by | Values |
|---|---|---|
| `BRIX_DELEGATE_NIX` | the `brix` CLI | The absolute path of the `nix` binary that runs the commands brix does not implement; the package's wrapper sets it. Unset or empty, those commands fail with a configuration error; a relative path is an error. |
| `BRIX_USE_NIX` | the modules' `nix` wrapper | Exactly `1` bypasses brix and runs the selected fallback Nix for every command. Other values or unset use brix. Does not affect the standalone `brix` CLI. |
| `BRIX_DELEGATED_BY` | the `brix` CLI | Internal, not for users: brix sets it to its process ID when it hands a command to Nix, to detect a `BRIX_DELEGATE_NIX` that leads back to brix. |
| `BRIX_TIMINGS` | the `brix` CLI, brix-eval's `eval_nixpkgs` and `profile` examples | `1` prints the evaluator's phase timings and counters to stderr when the evaluation ends, and notes about compiled images; `0` or unset, nothing. Any other value is an error. The per-operation work counters need brix-eval's `work-counters` cargo feature. The library does not read the variable: front ends set `brix_eval::Config::timings` from it. |
| `BRIX_GC_THRESHOLD_BYTES` | the `brix` CLI | Set a fixed nursery collection threshold in positive bytes. Without overrides, the nursery starts at 4 GiB and switches to 512 MiB after its first collection. `BRIX_TIMINGS=1` reports each collection and total GC costs. These are collection triggers, not total-memory limits. |
| `BRIX_GC_STEADY_THRESHOLD_BYTES` | the `brix` CLI | Set the nursery threshold after the first successful collection, in positive bytes. Can be combined with `BRIX_GC_THRESHOLD_BYTES` to set both phases; otherwise the initial threshold remains 4 GiB. |
| `BRIX_FETCH_CACHE_VERIFY` | the `brix` CLI | `1` rehashes cached fetched trees whenever they are used; `0` or unset, they are trusted. Any other value is an error. Library users set `FetchConfig::verify_cache`. |
| `BRIX_HAVE_INTERNET` | the `brix` CLI | Whether the machine has an Internet connection, in place of the check Nix makes before a command (an interface address other than loopback and link-local, or a lower-case proxy variable): `1` yes; `0` no, so brix warns that there is no Internet access and runs as with `--offline`. Unset, brix checks as Nix does. Any other value is an error. For tests: the package's test run sets `1`, since its build sandbox has no network. |
| `BRIX_PREFAULT` | `brix_alloc::init()`: the `brix` binary, brix-eval's examples | The pre-faulting policy: `off`, `sync` or `background`. Unset, `background` where the kernel supports it, else `off` with a warning. Any other value, or a policy the kernel cannot do, is an error. |

The tests read these; the devshell and the flake's checks set them, and
a test that needs one fails loudly without it:

| Variable | Read by | Values |
|---|---|---|
| `BRIX_NIX_SRC` | tests, `scripts/pgo-build.sh` | The reference Nix source tree: the language test corpus, Nix's unit-test data. |
| `BRIX_NIX_BIN` | tests, `scripts/update-cli-tree.sh` | The reference `nix`, the oracle; `nix-instantiate` and `nix-store` are taken from its directory. |
| `BRIX_NIXPKGS` | tests, benchmarks, brix-eval's examples, `scripts/pgo-build.sh` | A nixpkgs tree, for the corpus, benchmark and end-to-end tests and for PGO training. |
| `BRIX_TEST_REAL_DAEMON` | tests | `1` opts in to the tests that talk to the running `nix-daemon` (builds, store reads and writes). |
| `BRIX_TEST_REAL_STORE` | tests | `1` opts in to the tests that write into the user's Nix store through the oracle (`nix flake prefetch`, derivations). |
| `BRIX_TEST_NETWORK` | tests | `1` opts in to the tests that download from GitHub. |

A test gated on one of the last three is `#[ignore]`d and fails if it is
run with the variable unset; any value other than `1` is an error.

| Variable | Read by | Values |
|---|---|---|
| `BRIX_FUZZ_CASES` | `fuzz_differential`'s `long_run` | How many cases (default 5000). |
| `BRIX_FUZZ_SEED` | `fuzz_differential`'s `long_run` | The first seed (default: the current time). |
| `BRIX_FUZZ_JOBS` | `fuzz_differential` | How many cases run at once (default 4). |
| `BRIX_FUZZ_VERBOSE` | `fuzz_differential` | `1` prints each seed before it runs; `0` or unset, nothing. Any other value is an error. |
| `BRIX_FUZZ_CHILD_IN`, `BRIX_FUZZ_CHILD_OUT` | `fuzz_differential` | Internal: set by the harness for the child processes it runs brix in. |
| `BRIX_MOUNT` | brix-eval's `eval_nixpkgs` and `profile` examples | The directory to mount and evaluate against (default: `BRIX_NIXPKGS`). |
| `BRIX_PROFILE_REPEAT` | the `profile` example | Evaluate this many times, in fresh evaluators (default 1). |
| `BRIX_PROFILE_ALLOCS` | the `profile` example | Record a backtrace every this many allocations. |
| `BRIX_PROFILE_LAYOUT` | the `profile` example | `store` mounts the whole store directory, as the CLI does; unset, only the tree. Any other value is an error. |
| `BRIX_ALLOC_TEST_CHILD` | brix-alloc's tests | Internal: marks the child processes the tests run `init()` in. |

The numbers must be valid: an unparsable one is an error.

brix also reads Nix's own variables as Nix does: `NIX_CONFIG`,
`NIX_CONF_DIR`, `NIX_USER_CONF_FILES`, `NIX_CONFIG_HOME`,
`XDG_CONFIG_HOME` and `XDG_CONFIG_DIRS` (configuration files),
`NIX_REMOTE` (the `store` setting's default), `NIX_STORE_DIR` and
`NIX_STORE` (the store directory), `NIX_DAEMON_SOCKET_PATH` and
`NIX_STATE_DIR` (the daemon socket), `NIX_SSHOPTS` (`ssh-ng` stores),
`http_proxy`, `https_proxy`, `ftp_proxy` and `all_proxy` (any of them
set, even empty, counts as an Internet connection; the upper-case names
do not), and `TERM`, `NO_COLOR` and `NOCOLOR` (colours and the progress
bar). Its own caches live in `$XDG_CACHE_HOME/brix` (by default
`~/.cache/brix`), apart from Nix's.

## Why is it called brix?

Wanted something ending in 'ix', this is designed to be a building block that fits in nicely with other Nix tech ('bricks'), and also I started working on this while watching
the Brazilian Grand Prix.

## License

LGPL-2.1-or-later; see [LICENSE](LICENSE).
