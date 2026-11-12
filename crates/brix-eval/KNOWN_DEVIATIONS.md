# Known deviations from Nix

brix aims to reproduce Determinate Nix 3.24.1 (Nix 2.35.3) exactly.
These are the places where it knowingly does not: for each, what Nix
does, what brix does instead, why, and where it helps, how to see the
difference. The list covers every crate, not only brix-eval: the
language and the evaluator first, then strings and paths that are not
UTF-8, fetchers and flakes, the store and the daemon, and the command
line.

## Language and evaluator

### Regular expressions

Nix uses `std::regex` with `std::regex::extended` (POSIX ERE,
leftmost-longest matching). brix uses the `regex` crate after translating
the pattern (`src/regex.rs`) the way libstdc++ reads it: on bytes (a
multi-byte character is several, in bracket expressions too), `.`
matching newlines, bracket expressions member by member (ranges compared
as C `char`s, collating symbols and character classes by libstdc++'s
names), only metacharacters escapable, `{` only opening an interval, a
repetition of a repetition repeating it again (`a*?` is a greedy
`(a*)?`, not a lazy `a*`). Differences:

- `regex` picks the leftmost-*first* alternative, POSIX the leftmost-
  *longest*. Whole-string matches (`match`) are anchored, which hides
  most of this, but `split` differs when alternatives overlap:
  `builtins.split "(a|ab)" "abc"` is `[ "" [ "ab" ] "c" ]` in Nix and
  `[ "" [ "a" ] "bc" ]` in brix.
- `split` iterates like `std::regex_iterator` (an empty match right after
  a non-empty one is reported; after an empty match the search retries at
  the same position for a non-empty one). This is implemented by hand
  over `captures_at`, so it matches Nix except where leftmost-first versus
  leftmost-longest already differs.
- Back-references are not part of ERE and are rejected by both; error
  messages for invalid patterns are brix's own ("invalid regular
  expression '…'" is the same text, the position may differ).
- Nix's "memory limit exceeded by regular expression" error does not
  exist; the `regex` crate has a compile-time size limit instead.

### Parallel evaluation

The `/etc/nix/nix.conf` Determinate Nix installs sets `eval-cores = 0`,
which runs the parallel evaluator; the compiled-in default, as in
upstream Nix, is 1, which runs sequentially. brix reads evaluation
settings from the command line only, so it behaves as with 1 unless
given `--eval-cores`, and it always evaluates on one thread. The only
effect of the parallel evaluator it reproduces is how JSON serialisation
(`--json`, `builtins.toJSON`) reports errors: with
`Config::eval_cores != 1` values are deep-forced first, the way
`forceValueDeepParallel` does it, so errors come without the "while
evaluating attribute …" serialisation traces. The emulation
follows the single-item/many-items rule of that walk; the real thing
depends on thread scheduling in the many-items case, so there error
texts can still differ from a given run of Nix.

Determinate's `derivationStrict` also hands its argument to the worker
threads for a deep force before it validates anything. Two consequences
brix does not reproduce (it forces sequentially, like upstream):

- an attribute that refers back to the derivation being built, as in
  `rec { a = (derivation { bar = !a; }).drvPath; }`, fails on a worker
  thread too, and that failure's traces ("in the argument of the not
  operator") end up on the shared error object;
- an error message that prints an attribute set, such as "cannot coerce a
  set to a string: { … }", shows members a worker is evaluating at that
  moment as `«potential infinite recursion»` instead of `«thunk»`.

### Lazy trees

With `lazy-trees = true` (which Determinate's `/etc/nix/nix.conf` also
sets; the compiled-in default is false), Nix mounts fetched inputs at
*random* store paths during evaluation and rewrites them to their
content-addressed paths wherever a string is observed. brix accepts the
setting and ignores it: it uses the content-addressed path from the
start (it mounts some trees before hashing them, `src/lazy_tree.rs`, but
every string it produces is the one hashing up front would give). This
is visible only through `builtins.getContext` of such a string, whose
key is random in Nix. Otherwise brix behaves as with lazy trees whatever
the setting says: it reads a local flake in place rather than copying it
to the store first, and `flake metadata` shows no `Path:`, since the
virtual path a flake is mounted at is never valid.

### Symbol order

Nix keeps attribute sets sorted by symbol id, that is by creation order,
and so does brix (`src/bindings.rs`), so iterating a set visits its
attributes in Nix's order wherever that is observable (`==`,
`deepSeq`/`--strict`, primop argument loops). brix creates a file's
symbols in one pass over its AST before compiling it, in the order Nix's
parser creates them, bison's reduction order for function heads
included (a formal's name after its default, the `@` or plain argument
after the body; `src/compile.rs`).

What remains: the AST does not record where dynamic attributes
(`${e} = …;`, other than a plain string, which is static in both) stand
among a set's static ones, so brix creates the symbols inside them after
those of all the set's static bindings, where Nix creates them in source
order. A name that first occurs in a file inside such an attribute then
gets a later id than in Nix, which reorders every set that has it and a
name created in between:

```nix
let s = { ${toString 1} = { freshOne = 1; }; freshTwo = 2; };
in builtins.deepSeq { freshTwo = throw "two"; freshOne = throw "one"; } null
```

throws "one" in Nix and "two" in brix. Names that existed before (Nix's
own, those of files loaded earlier, earlier ones in the same file) are
not affected.

### TOML

`builtins.fromTOML`'s parse errors come from the `toml` crate ("while
parsing TOML: …"), not toml11.

### Read-only mode

`nix-instantiate --eval` cannot read back a `builtins.toFile` it just
"wrote" (its store is read-only: "path … is not valid"). brix serves the
file from the virtual store, as `nix eval` does.

### Relative and URL search-path entries

`builtins.findFile` in brix only resolves search-path entries beginning
with `/`; other entries are skipped. Nix resolves relative strings
against the process's current directory and can fetch URL entries
(including `channel:` pseudo-URLs), with download warnings and lookup
hooks. brix's evaluator host does not expose those lookup operations or
a process working directory. Consequently, a relative entry in pure
evaluation can produce a forbidden-path error in Nix but a missing
lookup error in brix, and URL entries omit Nix's download warnings.

### fetchClosure

With the `fetch-closure` experimental feature, `builtins.fetchClosure`
(`__fetchClosure` in the global scope) exists as in Nix but always
fails ("brix does not implement 'builtins.fetchClosure'"): it copies
store paths from a binary cache into the store, and brix has no store of
its own to copy them into.

## Strings and paths that are not UTF-8

Nix strings, paths and names are bytes, and so are brix's. A path keeps
its bytes as they are (`/a + "\xff"`, with a raw 0xff byte in the
source, is the path `/a\xff`); a file name or symlink target read from a
tree is its bytes; and messages, traces and positions show such names
as they are, as Nix does. Some things are text in brix: URLs, flake
references and the arguments fetchers take. Where turning bytes into
such text would lose some, brix refuses; the exceptions are noted.

### Fetcher arguments

Nix passes the attributes of `fetchTree`, `fetchGit`, `fetchurl`,
`fetchTarball` and `fetchMercurial` to its fetchers as bytes. brix's
fetchers take text (URLs, refs, revisions, names, paths), so an
argument or argument name that is not UTF-8 fails with "argument 'url'
to 'fetchTree' is not valid UTF-8; brix does not support non-UTF-8
fetcher arguments" rather than being altered. In Nix most such
arguments fail too (no URL, revision or store path name can hold those
bytes), with the fetcher's own message; a path, a Git ref or a flake's
`dir` can hold them. An argument `fetchurl` does not know is named as
it is, as in Nix. Hashes are bytes, as in Nix: `fetchurl`'s and
`fetchTarball`'s `sha256` and a `narHash` that is not UTF-8 fail with
Nix's hash error.

The same goes for flake references and flakes: the argument of
`builtins.getFlake`, `parseFlakeRef` and `flakeRefToString` ("'…' given
to 'builtins.parseFlakeRef' is not valid UTF-8; brix does not support
non-UTF-8 flake references"), and the attribute names and strings of a
`flake.nix` ("attribute name '…' in a flake is not valid UTF-8, which
brix does not support"; Nix fails too when it writes such a name into
the lock file, which is JSON, but shows a description as it is). The
facade's APIs that hand out text (`Cursor::get_attrs`, `get_string`, …)
refuse such names and strings the same way; their byte forms keep them.

### Percent-escapes in URLs

Nix decodes `%XX` escapes in a URL (user, host, path segments, query,
fragment) to bytes, whatever they are. brix's URL parts are strings, so
an escape that decodes to bytes that are not UTF-8 is an error
("invalid URI parameter '…': not valid UTF-8", `brix-store/src/url.rs`)
in flake references, fetcher URLs and store URIs.

### Fetcher error messages

The fetchers read names as bytes, but their own error messages are text
in brix: a name that is not UTF-8 in one of them shows U+FFFD in place
of those bytes, where Nix writes them as they are. A tarball member of a
type Nix does not unpack is an example ("file '…' in tarball has
unsupported file type …").

## Fetchers and flakes

### Mercurial inputs

brix fetches `hg` inputs and evaluates `builtins.fetchMercurial` as Nix
does, by running the `hg` program with the same commands, environment,
cache entries and rules (`brix-flake/src/native/mercurial.rs`), down to
the branch a revision reports depending on whether the fetch cache
answered. What remains different:

- A revision on a branch whose name has a space: `hg log` then prints
  more than the three words Nix asserts it prints (the revision, its
  number and its branch), and Nix crashes ("terminating due to
  unexpected unrecoverable internal error: Assertion 'tokens.size() ==
  3' failed in … at …/mercurial.cc:355", a stack trace, exit status
  134). brix fails with an error of its own, "'hg log -r <rev>' printed
  '<output>', which is not a revision, its number and its branch", under
  the input's "while fetching the input '…'".
- What `hg` prints (file names, branch names) must be UTF-8.

### Git inputs with `lfs`

Git LFS files are smudged as Nix smudges them, when they are read (the
same objects, cache, endpoints, requests, warnings and errors), with
these differences:

- A value of the wrong type in an LFS server's answer, or output of
  `git-lfs-authenticate` that is not JSON, is reported with nlohmann's
  message under the trace "while smudging git-lfs file '…'", which Nix
  does not add to an exception of nlohmann's own. A batch answer that
  lists no objects, which Nix reads past the end of, and a remote URL
  without an authority, on which Nix's assertion aborts, are errors of
  brix's own.

### Git submodules, `exportIgnore` and Nix 2.19 exports

Submodules, `exportIgnore` and `nix-219-compat` exports are Nix's, the
NAR hash a tree's fingerprint remembers included (a clean working
tree's, `git:<rev>`, says nothing of its submodules), with these
differences:

- Nix copies a tree to the store (its `outPath` used, `"${src}"`) under
  the hash remembered for its fingerprint only when the store has that
  path; otherwise under the tree's own hash, which it then remembers
  instead. brix always takes the remembered hash, and the store path it
  names.
- brix remembers the hash of a whole tree only, where Nix also
  remembers that of a path inside it it copies (`"${src + "/sub"}"`).
- A Git commit that is a flake's top-level tree is shown in messages
  as its checkout's directory when the repository is local, where Nix
  shows it as its input (`«git+file:///…?rev=…»/flake.nix`).

### Remote Git repositories without `git`

Nix asks a repository to fetch from with `git`, and so does brix. With
no `git` on `PATH`, Nix fetches with libgit2, and its `git ls-remote`
for the remote's `HEAD` fails (`error: executing "git": No such file or
directory`, then the warning that it uses `master`); brix falls back to
gix, which reads the remote's `HEAD` (and when it cannot, brix warns
with gix's error), and a failed fetch is reported with gix's message
where Nix gives libgit2's (`fetching '<refspec>' from '<url>': …`).

### Fetching ahead

Nix fetches a flake's input when evaluation first needs it. brix starts
the other inputs of the same flakes then, in the background
(`prefetch_plan`, see docs/ARCHITECTURE.md), so a server sees requests
for inputs Nix never fetches, all of a failing download's attempts
included (until brix exits). What those fetches write to stderr (their
messages, and what the programs they run print: `git fetch`'s progress
and `From …` lines, git's errors) is held back and shown only if
evaluation needs their input, where Nix's own fetch would show it, so
the output is Nix's.

## Store and daemon

### Which store

Nix's `store` setting defaults to `$NIX_REMOTE`, else `auto`, which is
the local store for a user who can write it and the daemon otherwise.
brix reads `NIX_REMOTE` the same way, but by `auto` and `local` it
always means the daemon: it never opens a store itself (`local` with
settings is refused: "brix cannot open a local store; use 'daemon'"). It
talks only to `daemon`, `unix://…` and `ssh-ng://…` stores (and, for
evaluation alone, `dummy://`). The legacy `ssh://` store is refused ("the
legacy 'ssh://' store speaks the serve protocol, which brix does not
implement; use 'ssh-ng://'"), and so are the other store types Nix
knows ("invalid store URI '…': unsupported store scheme '…'"). A store
name that cannot be parsed at all gives Nix's "Cannot parse Nix store
'…'", but as a plain error, without Nix's "Try '… --help'" line.

### Connections

- Nix keeps a pool of connections and replaces one that failed in a way
  that may have left it in an unknown state. brix opens one connection
  per session and, after such a failure (anything but an error the
  daemon reported), refuses every further operation on it ("the
  connection to the Nix daemon is unusable after an earlier error")
  rather than reconnect: its temporary roots live on that connection.
- For `ssh-ng://` Nix starts an `ssh -M` master connection and runs each
  command through it. brix runs one plain `ssh` per session, as Nix does
  with `max-connections = 1`, a setting brix only validates.
- At the `chatty` verbosity and above Nix passes `-v` to `ssh`; brix
  does not.
- For `ssh-ng://localhost` both run the remote program directly. When it
  cannot be executed, brix reports "unable to execute '…': …" and stops;
  Nix's child prints that message and the parent then fails the
  handshake: "cannot open connection to remote store
  'ssh-ng://localhost': Nix daemon disconnected unexpectedly (maybe it
  crashed?)".

### Evaluating with an unreachable store

brix reads a local flake in place and asks the store only when
something needs it (a locked input already under the store directory, a
flake that lives in the store, `flake metadata`'s question about the
flake's store path), so `nix eval path:<dir>#x` fails in Nix when the
store cannot be reached ("cannot connect to socket at '…': …") and can
succeed in brix. When brix does fail, the messages are Nix's.

### Store directories without a store

A locked input whose tree is already under the store directory (found by
its `narHash`) is read from there when the store says the path is valid,
as Nix's `fetchToStore` shortcut does. Without a store (`dummy://`, or
the library's default configuration) there is nobody to ask, and brix
trusts the directory: Nix's dummy store has no paths, so Nix fetches the
input instead. A directory that was never registered, or was left behind
half-written, is then read as if it were the input's tree.

### Outputs brix cannot name, and Git hashing

- brix knows a derivation's output paths only when they are fixed in the
  derivation. Commands that need the path of a floating
  content-addressed output (`ca-derivations`) without building it
  (`path-info` and `build --dry-run --json` of an installable, `develop`)
  fail: "output '…' of '…' has no output path known in advance
  (content-addressed outputs are not supported by brix)".
- Dynamic derivations are not supported: `derivation add` refuses JSON
  with dynamic outputs ("dynamic derivation outputs are not supported by
  brix"), and a derived path names a derivation by its store path only.
- The `git` hashing mode (`git-hashing`) is not implemented for hashing
  files: `brix hash path --mode git` fails with "the 'git' hashing mode
  is not supported by brix".

## Command line

### Pure evaluation only

brix evaluates in pure mode only: `--impure`, `--no-pure-eval` and
`pure-eval = false` fail with "brix only evaluates in pure mode;
'--impure' is not supported". Nix's `--file` implies `--impure`; brix
evaluates the file in pure mode too, with the file's directory (and each
`-I` directory) readable. So `nix eval --file f.nix` with
`builtins.getEnv "HOME"` in it prints the home directory, and brix
prints `""`; `builtins.currentTime`, `builtins.currentSystem` and lookup
paths behave as in pure mode. Nix also rejects `--file` together with an
explicit `--pure-eval` ("'--file' is not compatible with '--pure-eval'");
brix accepts it.

### Flags brix does not implement

- `--profile` on `build` and `develop`: "'--profile' is not supported:
  brix does not manage profiles".
- `--commit-lock-file`, `--inputs-from` and `--override-flake`: "'…' is
  not supported by brix yet".

The commands brix does not implement at all are run by Nix (see
`docs/ARCHITECTURE.md`).

### Command-line values still read as text

Nix takes every command-line argument as bytes. brix accepts arbitrary
bytes in `--expr`, `--apply`, `--file`, `-I`, the current directory, and
installables that resolve to store paths (including symlinks). Flake
references and attribute selection paths remain UTF-8, as do other
flags and positional arguments; brix refuses non-UTF-8 values by name.
Flake references, lock-file paths and development-environment scripts
still require a UTF-8 current directory when they use it as text.
Commands delegated to Nix receive every argument unchanged.

### The environment of the programs brix starts

`run`, `shell` and `develop` pass the environment in the order Nix's
`setenv` calls leave it, and `fmt` in name order, as Nix's does. What
remains:

- `nix develop` changes its own environment (the options' variables,
  `NIX_GCROOT`) before it builds the interactive bash; brix fetches and
  builds it with its original environment and applies the changes only
  to the shell it starts.
- Programs brix starts (`run`, `shell`, `develop`, `fmt`) begin with
  `SIGPIPE` ignored: the Rust runtime ignores it in brix, and brix calls
  `execve` itself, which keeps an ignored disposition. Nix only blocks
  the signal and restores its mask before it runs a program, so its
  programs start with the default disposition.
- A command brix hands to Nix runs with `BRIX_DELEGATED_BY` added to its
  environment (see the README).
