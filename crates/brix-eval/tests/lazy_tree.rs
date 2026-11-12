//! Lazily mounted trees (`crate::lazy_tree`): string operations that keep
//! a tree's hash lazy give the bytes they would give with the hash known,
//! without hashing the tree, and everything else hashes it first.
//!
//! The trees here count the reads of one file, `/secret`, which only
//! hashing the whole tree reads: a count of zero means the tree was not
//! materialised.

mod common;

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use brix_eval::{Arena, Config, Evaluator, Host, LazyTreeId, SrcPos, Value};
use brix_store::{
    AccessorError, CanonPath, DirEntries, FileIngestionMethod, FixedOutputInfo, FsAccessor,
    HashAlgo, MemoryAccessor, SourceAccessor, Stat, StoreDir,
};
use bstr::{BString, ByteSlice};

fn p(s: &str) -> CanonPath {
    CanonPath::new(s).unwrap()
}

/// A tree that counts how often `/secret` is read.
struct Watched {
    inner: Arc<dyn SourceAccessor>,
    secret_reads: AtomicUsize,
}

impl Watched {
    fn new(inner: Arc<dyn SourceAccessor>) -> Arc<Watched> {
        Arc::new(Watched {
            inner,
            secret_reads: AtomicUsize::new(0),
        })
    }

    fn hashed(&self) -> bool {
        self.secret_reads.load(Ordering::SeqCst) > 0
    }
}

impl SourceAccessor for Watched {
    fn maybe_lstat(&self, path: &CanonPath) -> Result<Option<Stat>, AccessorError> {
        self.inner.maybe_lstat(path)
    }

    fn read_dir(&self, path: &CanonPath) -> Result<DirEntries, AccessorError> {
        self.inner.read_dir(path)
    }

    fn read_file(&self, path: &CanonPath) -> Result<Vec<u8>, AccessorError> {
        if path.abs() == "/secret" {
            self.secret_reads.fetch_add(1, Ordering::SeqCst);
        }
        self.inner.read_file(path)
    }

    fn read_link(&self, path: &CanonPath) -> Result<BString, AccessorError> {
        self.inner.read_link(path)
    }
}

/// A tree of a few files; `tag` makes trees differ.
fn memory_tree(tag: &str) -> Arc<Watched> {
    let mut mem = MemoryAccessor::new();
    mem.add_file(&p("/secret"), tag, false).unwrap();
    mem.add_file(&p("/sub/v.nix"), "42", false).unwrap();
    mem.add_file(&p("/sub/data"), "hello\n", false).unwrap();
    mem.add_file(&p("/bad.nix"), "1 + \"x\"", false).unwrap();
    Watched::new(Arc::new(mem))
}

/// An empty root file system: everything the tests read is mounted.
struct EmptyHost {
    root: Arc<dyn SourceAccessor>,
    allowed: Vec<CanonPath>,
}

impl EmptyHost {
    fn new() -> EmptyHost {
        EmptyHost {
            root: Arc::new(MemoryAccessor::new()),
            allowed: Vec::new(),
        }
    }
}

impl Host for EmptyHost {
    fn root_accessor(&self) -> Arc<dyn SourceAccessor> {
        self.root.clone()
    }

    fn allowed_paths(&self) -> Vec<CanonPath> {
        self.allowed.clone()
    }
}

/// The text of a lazy tree's root once its store path is known.
fn root_text(ev: &Evaluator<'_>, id: LazyTreeId) -> String {
    ev.config.store_dir.display(&ev.materialize(id).unwrap())
}

/// `v` forced deeply and printed as `nix-instantiate --eval` does (which
/// computes every lazy hash in it).
fn printed<'a>(ev: &Evaluator<'a>, v: Value<'a>) -> BString {
    let cell = ev.alloc_cell(v);
    ev.force_deep(cell).unwrap();
    ev.print_ambiguous(cell).unwrap()
}

/// `expr` evaluated in the tree mounted at `base`, forced deeply but
/// not printed: forcing keeps lazy hashes lazy.
fn eval_forced<'a>(ev: &Evaluator<'a>, expr: impl AsRef<[u8]>, base: &CanonPath) -> Value<'a> {
    let expr = expr.as_ref().as_bstr();
    let v = ev
        .eval_str(expr, base.abs())
        .unwrap_or_else(|e| panic!("{expr}: {}", e.render(false)));
    let cell = ev.alloc_cell(v);
    ev.force_deep(cell)
        .unwrap_or_else(|e| panic!("{expr}: {}", e.render(false)));
    v
}

/// Names in a lazy tree are bytes, as Nix's are: a path into the tree
/// that a string with the tree's lazy hash names (`"${toString
/// ./.}/caf\xe9"`, with the raw byte) is read at the placeholder without
/// hashing, and a position's file name keeps the name's bytes after the
/// lazy hash.
#[test]
fn names_that_are_not_utf8_stay_in_the_lazy_tree() {
    let host = EmptyHost::new();
    let arena = Arena::new();
    let ev = Evaluator::new(&arena, Config::default(), &host);
    let mut mem = MemoryAccessor::new();
    mem.add_file(&p("/secret"), "s", false).unwrap();
    mem.add_file(&CanonPath::new(b"/caf\xe9").unwrap(), "cafe", false)
        .unwrap();
    mem.add_file(
        &CanonPath::new(b"/d\xe9/pos.nix").unwrap(),
        "__curPos",
        false,
    )
    .unwrap();
    let tree = Watched::new(Arc::new(mem));
    let (id, root) = ev.mount_lazy_tree(tree.clone(), "source", "«a»");
    let v = eval_forced(
        &ev,
        b"[ (builtins.readFile (/. + \"${toString ./.}/caf\xe9\"))
            (builtins.readFile \"${toString ./.}/caf\xe9\")
            (builtins.attrNames (builtins.readDir ./.))
            (builtins.baseNameOf (toString ./. + \"/caf\xe9\")) ]",
        &root,
    );
    let file = eval_forced(&ev, b"(import (./. + \"/d\xe9/pos.nix\")).file", &root);
    assert!(
        !tree.hashed(),
        "reading, listing and positions by name must not hash"
    );
    assert_eq!(
        printed(&ev, v),
        &b"[ \"cafe\" \"cafe\" [ \"caf\xe9\" \"d\xe9\" \"secret\" ] \"caf\xe9\" ]"[..]
    );
    let mut expected = format!("\"{}", root_text(&ev, id)).into_bytes();
    expected.extend_from_slice(b"/d\xe9/pos.nix\"");
    assert_eq!(printed(&ev, file), expected);
}

#[test]
fn a_substring_keeps_the_hash_lazy_at_its_new_offset() {
    let host = EmptyHost::new();
    let arena = Arena::new();
    let ev = Evaluator::new(&arena, Config::default(), &host);
    let tree = memory_tree("a");
    let (id, root) = ev.mount_lazy_tree(tree.clone(), "source", "«a»");
    let v = eval_forced(
        &ev,
        r#"let r = toString ./.; s = builtins.substring 1 1000 r; in {
          inherit s;
          len = builtins.stringLength s;
          same = s == builtins.substring 1 1000 (toString ./.);
          prefix = builtins.substring 0 10 s;
          ordered = builtins.lessThan s (builtins.substring 1 1000 (toString ./sub));
          empty = builtins.substring 5 0 r;
          emptyHasContext = builtins.hasContext (builtins.substring 5 0 r) == builtins.hasContext r;
          asPath = builtins.readFile "/${builtins.substring 1 1000 (toString ./sub)}/data";
        }"#,
        &root,
    );
    assert!(
        !tree.hashed(),
        "slicing, comparing and reading must not hash"
    );
    let text = root_text(&ev, id);
    assert_eq!(
        printed(&ev, v),
        format!(
            r#"{{ asPath = "hello\n"; empty = ""; emptyHasContext = true; len = {}; ordered = true; prefix = "{}"; s = "{}"; same = true; }}"#,
            text.len() - 1,
            &text[1..11],
            &text[1..],
        )
    );

    // A range that cuts through the hash needs it.
    let tree = memory_tree("b");
    let (id, root) = ev.mount_lazy_tree(tree.clone(), "source", "«b»");
    let v = eval_forced(
        &ev,
        "builtins.substring 1 1000 (builtins.baseNameOf (toString ./.))",
        &root,
    );
    assert!(tree.hashed());
    let text = root_text(&ev, id);
    let base = text.rsplit_once('/').unwrap().1;
    assert_eq!(printed(&ev, v), format!("\"{}\"", &base[1..]));
}

#[test]
fn strings_from_two_trees_compare_by_their_hashes() {
    let host = EmptyHost::new();
    let arena = Arena::new();
    let ev = Evaluator::new(&arena, Config::default(), &host);
    let a = memory_tree("a");
    let b = memory_tree("b");
    let same_as_a = memory_tree("a");
    let (ia, ra) = ev.mount_lazy_tree(a.clone(), "source", "«a»");
    let (ib, rb) = ev.mount_lazy_tree(b.clone(), "source", "«b»");
    let (_, rc) = ev.mount_lazy_tree(same_as_a.clone(), "source", "«c»");
    // Same length, hashes in the same place: only the hashes decide.
    let v = eval_forced(
        &ev,
        format!(
            "let a = toString {}; b = toString {}; c = toString {}; in \
             [ (a == b) (a == c) (a < b) (b < a) (builtins.sort builtins.lessThan [ b a ]) ]",
            ra.abs(),
            rb.abs(),
            rc.abs()
        ),
        &ra,
    );
    assert!(a.hashed() && b.hashed() && same_as_a.hashed());
    let (ta, tb) = (root_text(&ev, ia), root_text(&ev, ib));
    let (first, second) = if ta < tb { (&ta, &tb) } else { (&tb, &ta) };
    assert_eq!(
        printed(&ev, v),
        format!(
            r#"[ false true {} {} [ "{first}" "{second}" ] ]"#,
            ta < tb,
            tb < ta
        )
    );
}

#[test]
fn a_string_becomes_a_lazy_path_only_with_one_hash_where_a_store_path_has_it() {
    let host = EmptyHost::new();
    let arena = Arena::new();
    let ev = Evaluator::new(&arena, Config::default(), &host);
    let tree = memory_tree("a");
    let (_, root) = ev.mount_lazy_tree(tree.clone(), "source", "«a»");
    // The hash where a store path's is: read without hashing, `..`
    // included (the placeholder sits in the store directory too).
    let lazy = eval_forced(
        &ev,
        r#"let r = toString ./.; in [
          (builtins.readFile "${r}/sub/data")
          (builtins.readFile "${r}/sub/../sub/data")
          (builtins.pathExists "${r}/missing")
          (builtins.attrNames (builtins.readDir "${r}/sub/.."))
          (builtins.readFile ("/nix/store/" + builtins.baseNameOf r + "/sub/data"))
        ]"#,
        &root,
    );
    assert!(!tree.hashed());
    assert_eq!(
        printed(&ev, lazy),
        r#"[ "hello\n" "hello\n" false [ "bad.nix" "secret" "sub" ] "hello\n" ]"#
    );

    // Anything else is resolved first: two hashes, a hash elsewhere, text
    // after the name that does not continue a path below the tree.
    for (expr, expected) in [
        (
            r#"builtins.readFile "${r}/sub/${builtins.baseNameOf r}/../data""#,
            r#""hello\n""#,
        ),
        (
            r#"builtins.readFile "${dirOf r}/x/../${builtins.baseNameOf r}/sub/data""#,
            r#""hello\n""#,
        ),
        (r#"builtins.pathExists "${r}x""#, "false"),
        (
            r#"builtins.pathExists "${r}x/../${builtins.baseNameOf r}/sub""#,
            "true",
        ),
    ] {
        let tree = memory_tree("b");
        let (_, root) = ev.mount_lazy_tree(tree.clone(), "source", "«b»");
        let v = eval_forced(&ev, format!("let r = toString ./.; in {expr}"), &root);
        assert!(tree.hashed(), "{expr}");
        assert_eq!(printed(&ev, v), expected, "{expr}");
    }
}

/// `/. + "${r}/…"` is a path into the lazy tree, as a string turned into
/// a path is (`coerceToPath`): no hash is needed. Anywhere else the store
/// path's text is, the hash is computed.
#[test]
fn a_path_plus_a_lazy_string_keeps_the_hash_lazy() {
    let host = EmptyHost::new();
    let arena = Arena::new();
    let ev = Evaluator::new(&arena, Config::default(), &host);
    let tree = memory_tree("a");
    let (_, root) = ev.mount_lazy_tree(tree.clone(), "source", "«a»");
    let lazy = eval_forced(
        &ev,
        r#"let r = toString ./.; in [
          (builtins.readFile (/. + "${r}/sub/data"))
          (builtins.readFile (/. + "${r}" + "/sub/data"))
          (builtins.readFile (/. + "${r}/sub/../sub/data"))
          (builtins.pathExists (/. + "${r}/missing"))
        ]"#,
        &root,
    );
    assert!(!tree.hashed());
    assert_eq!(
        printed(&ev, lazy),
        r#"[ "hello\n" "hello\n" "hello\n" false ]"#
    );

    for (expr, expected) in [
        (r#"builtins.pathExists (/nix + "${r}")"#, "false"),
        (
            r#"builtins.readFile (/. + "${dirOf r}/x/../${baseNameOf r}/sub/data")"#,
            r#""hello\n""#,
        ),
    ] {
        let tree = memory_tree("b");
        let (_, root) = ev.mount_lazy_tree(tree.clone(), "source", "«b»");
        let v = eval_forced(&ev, format!("let r = toString ./.; in {expr}"), &root);
        assert!(tree.hashed(), "{expr}");
        assert_eq!(printed(&ev, v), expected, "{expr}");
    }
}

/// `builtins.path` names a copy after the path's last component, which
/// is known without hashing the tree; only the tree's root needs it.
#[test]
fn builtins_path_names_a_subdirectory_without_hashing_the_tree() {
    let host = EmptyHost::new();
    let arena = Arena::new();
    let ev = Evaluator::new(&arena, Config::default(), &host);
    let tree = memory_tree("a");
    let (id, root) = ev.mount_lazy_tree(tree.clone(), "source", "«a»");
    let v = eval_forced(&ev, "builtins.path { path = ./sub; }", &root);
    assert!(!tree.hashed());
    let (sub_hash, _) = brix_store::nar::hash(&*tree.inner, &p("/sub"), HashAlgo::Sha256).unwrap();
    let sub = StoreDir::default()
        .make_fixed_output_path(
            "sub",
            &FixedOutputInfo {
                method: FileIngestionMethod::NixArchive,
                hash: sub_hash,
                references: Default::default(),
            },
        )
        .unwrap();
    assert_eq!(
        printed(&ev, v),
        format!("\"{}\"", StoreDir::default().display(&sub))
    );

    // The root's copy is named after the tree's store path.
    let v = eval_forced(&ev, "builtins.path { path = ./.; }", &root);
    let tree_path = ev.materialize(id).unwrap();
    let (tree_hash, _) = brix_store::nar::hash(&*tree.inner, &p("/"), HashAlgo::Sha256).unwrap();
    let copy = StoreDir::default()
        .make_fixed_output_path(
            tree_path.as_str(),
            &FixedOutputInfo {
                method: FileIngestionMethod::NixArchive,
                hash: tree_hash,
                references: Default::default(),
            },
        )
        .unwrap();
    assert_eq!(
        printed(&ev, v),
        format!("\"{}\"", StoreDir::default().display(&copy))
    );
}

/// `SourcePath::baseName` of a root is `source`.
#[test]
fn builtins_path_names_a_root_source() {
    let mut mem = MemoryAccessor::new();
    mem.add_file(&p("/f"), "x", false).unwrap();
    let host = EmptyHost {
        root: Arc::new(mem),
        allowed: vec![CanonPath::root()],
    };
    let arena = Arena::new();
    let ev = Evaluator::new(&arena, Config::default(), &host);
    let v = ev
        .eval_str(
            br#"[ (builtins.path { path = /.; }) (builtins.path { path = /.; name = "source"; }) ]"#,
            "/",
        )
        .unwrap();
    let out = printed(&ev, v);
    let paths: Vec<&[u8]> = out
        .split_str("\"")
        .filter(|s| s.starts_with(b"/"))
        .collect();
    assert_eq!(paths.len(), 2, "{out}");
    assert_eq!(paths[0], paths[1]);
    assert!(paths[0].ends_with(b"-source"), "{out}");
}

/// What a flake's `self` is in these tests: its `outPath` is the tree's
/// store path, computed when used.
fn self_value<'a>(ev: &Evaluator<'a>, id: LazyTreeId) -> brix_eval::ValueRef<'a> {
    let attrs = vec![ev.attr_cell(ev.sym("outPath"), ev.lazy_tree_out_path_thunk(id))];
    ev.alloc_cell(ev.mk_attrs(attrs, SrcPos::NONE))
}

/// Every place `bad.nix` is shown in a rendered error, its directory
/// replaced by `<dir>`.
fn bad_nix_mentions(stderr: &str, dir: &str) -> Vec<String> {
    let mut found: Vec<String> = stderr
        .replace(dir, "<dir>")
        .split(|c: char| c.is_whitespace() || c == '\'')
        // Paths, not the source excerpt's `"${self}/bad.nix"`.
        .filter(|w| w.contains("bad.nix") && !w.contains('"'))
        .map(|w| w.trim_end_matches(':').to_owned())
        .collect();
    found.sort();
    found.dedup();
    found
}

/// The files of a lazy tree are shown as the tree (its directory, for a
/// local flake) under the tree's store path however that path became
/// known, and a copy's files under the copy's path, as in Nix.
#[test]
fn files_under_a_lazy_trees_store_path_are_shown_as_in_nix() {
    let dir = tempfile::tempdir().unwrap();
    let flake = dir.path().join("flake");
    std::fs::create_dir_all(&flake).unwrap();
    let cases = [
        ("viaSelf", r#"import "${self}/bad.nix""#),
        (
            "viaSelfAfterNamedCopy",
            r#"builtins.seq (builtins.path { path = ./.; name = "source"; }) (import "${self}/bad.nix")"#,
        ),
        ("viaCopy", r#"import "${./.}/bad.nix""#),
        (
            "viaCopyAfterSelf",
            r#"builtins.seq self.outPath (import "${./.}/bad.nix")"#,
        ),
    ];
    let outputs: String = cases
        .iter()
        .map(|(name, expr)| format!("{name} = {expr};\n"))
        .collect();
    std::fs::write(
        flake.join("flake.nix"),
        format!("{{ outputs = {{ self }}: {{\n{outputs}}}; }}\n"),
    )
    .unwrap();
    std::fs::write(flake.join("bad.nix"), "1 + \"x\"\n").unwrap();
    let flake_dir = flake.to_str().unwrap();

    for (name, expr) in cases {
        let theirs = nix_flake_eval(&flake, name);
        assert!(!theirs.status.success(), "{name} must fail in Nix");
        let stderr = String::from_utf8_lossy(&theirs.stderr);
        let theirs = bad_nix_mentions(&stderr, flake_dir);
        assert!(
            !theirs.is_empty(),
            "{name}: Nix did not reach bad.nix:\n{stderr}"
        );

        // Each case in a fresh evaluator: the order of operations is the
        // point.
        let host = EmptyHost::new();
        let arena = Arena::new();
        let ev = Evaluator::new(&arena, Config::default(), &host);
        let (id, root) = ev.mount_lazy_tree(Arc::new(FsAccessor::new(&flake)), "source", flake_dir);
        let f = ev
            .eval_str(format!("self: {expr}").as_bytes(), root.abs())
            .unwrap();
        let err = ev
            .call(f, &[self_value(&ev, id)], SrcPos::NONE)
            .and_then(|v| ev.force(ev.alloc_cell(v), SrcPos::NONE))
            .expect_err("bad.nix fails");
        let ours = bad_nix_mentions(&err.render(true).to_str_lossy(), flake_dir);
        assert_eq!(ours, theirs, "{name}");
    }
}

/// `nix eval path:<flake>#<attr>`, with lazy trees as Determinate Nix
/// runs it, into a writable `dummy://` store: brix's store is virtual,
/// and Nix shows a copy's files at the copy's store path in that store
/// as in the daemon's, where a local store under another root (Nix's
/// choice in the build sandbox, which has no daemon) shows them at its
/// real location.
fn nix_flake_eval(flake: &Path, attr: &str) -> brix_testutil::OracleOutput {
    brix_testutil::Oracle::new()
        .store("dummy://?read-only=false")
        .args(["eval", "--show-trace"])
        .arg(format!("path:{}#{attr}", flake.display()))
        .output()
}
