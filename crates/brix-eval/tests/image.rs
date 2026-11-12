//! Compiled-input images: a tree compiled to an image must look exactly
//! like the tree (files, hashes) and evaluate exactly like it (values,
//! errors, positions, work counters), with no parsing; and an image that
//! is damaged must be refused, or fail the import that meets the damage,
//! with an error that says so.
//!
//! The work counters are compared only with the `work-counters` feature:
//! `cargo test -p brix-eval --features work-counters --test image`.

mod common;

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use brix_eval::image::format::{self as fmt, IR_FINGERPRINT_FIXTURE_HASH};
use brix_eval::image::writer::{
    build_image, compile_tree_with, to_bytes, CompileOptions, Precompiled,
};
use brix_eval::image::{
    compile_tree, format, Image, ImageAccessor, ImageCompileContext, ImageError,
};
use brix_eval::DisplayPrefixAccessor;
use brix_eval::{Arena, Config, Evaluator, Host};
use brix_store::{
    AccessorError, CanonPath, DirEntries, FileType, FsAccessor, Hash, HashAlgo, HashFormat,
    MemoryAccessor, SourceAccessor, Stat,
};
use brix_testutil::Oracle;
use bstr::{BStr, BString, ByteSlice};
use common::TestHost;

const MOUNT: &str = "/nix/store/0000000000000000000000000000000z-fixture";

/// A host with `tree` mounted at [`MOUNT`], which may be read.
fn host(tree: Arc<dyn SourceAccessor>) -> TestHost {
    TestHost::with_accessors(&[(MOUNT, tree)], &[MOUNT])
}

/// The fixture as the facade mounts a fetched input: shown under the
/// mount point.
fn source_tree(dir: &Path) -> Arc<dyn SourceAccessor> {
    Arc::new(DisplayPrefixAccessor::new(
        Arc::new(FsAccessor::new(dir)),
        MOUNT.to_owned(),
    ))
}

/// The fixture served from `image` as the facade mounts a compiled input:
/// what the image does not store comes from the source tree.
fn image_tree(image: &Arc<Image>, dir: &Path) -> Arc<dyn SourceAccessor> {
    Arc::new(DisplayPrefixAccessor::new(
        Arc::new(ImageAccessor::new(image.clone()).with_fallback(Arc::new(FsAccessor::new(dir)))),
        MOUNT.to_owned(),
    ))
}

/// What one evaluation produced. The output is kept as bytes, so two
/// runs compare byte for byte (paths and names need not be UTF-8).
#[derive(Debug, PartialEq, Eq)]
struct Run {
    stdout: BString,
    stderr: BString,
    ok: bool,
    store_paths: Vec<String>,
    /// Thunks created and forced, environments, lambda calls and
    /// attribute sets: the work counters, which only the `work-counters`
    /// feature collects.
    #[cfg(feature = "work-counters")]
    work: [u64; 5],
}

/// Evaluates `expr` (bytes; relative paths against [`MOUNT`]) with
/// `image` registered at [`MOUNT`]; also returns how many files were
/// parsed and how many modules were linked from the image.
fn evaluate(host: &TestHost, image: Option<Arc<Image>>, expr: impl AsRef<[u8]>) -> (Run, u64, u64) {
    let arena = Arena::new();
    let ev = Evaluator::new(&arena, Config::default(), host);
    if let Some(img) = image {
        ev.register_image(CanonPath::new(MOUNT).unwrap(), img)
            .expect("image fits the evaluator");
    }
    let r = ev.eval_str(expr.as_ref(), MOUNT).and_then(|v| {
        let cell = ev.alloc_cell(v);
        ev.force_deep(cell)?;
        ev.print_ambiguous(cell)
    });
    let (stdout, ok) = match r {
        Ok(s) => (s, true),
        Err(e) => {
            host.log(&e.render(true));
            (BString::default(), false)
        }
    };
    let store_paths = ev
        .virtual_store()
        .iter()
        .map(|(p, _)| p.to_string())
        .collect();
    let st = ev.stats();
    (
        Run {
            stdout,
            stderr: host.take_stderr(),
            ok,
            store_paths,
            #[cfg(feature = "work-counters")]
            work: [
                st.thunks.get(),
                st.thunk_forces.get(),
                st.envs.get(),
                st.lambda_calls.get(),
                st.attr_sets.get(),
            ],
        },
        st.files_parsed.get(),
        st.image_modules.get(),
    )
}

fn context() -> ImageCompileContext {
    let arena = Arena::new();
    let host = host(Arc::new(MemoryAccessor::new()));
    let ev = Evaluator::new(&arena, Config::default(), &host);
    ev.image_compile_context().clone()
}

/// A small tree exercising imports, relative paths, errors, data files,
/// symlinks (to a file, to a directory in the tree, out of the tree) and
/// executables, and every kind of expression the compiler handles.
fn fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let w = |rel: &str, contents: &str| {
        let p = dir.path().join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, contents).unwrap();
    };
    w(
        "default.nix",
        r#"{ x ? 1 }:
let
  a = import ./lib/a.nix;
  b = import ./sub/b.nix { inherit a; };
  here = ./.;
in rec {
  inherit a b here;
  sum = a.n + b.m + x;
  data = builtins.readFile ./sub/data.txt;
  listing = builtins.readDir ./sub;
  exists = [ (builtins.pathExists ./sub/data.txt) (builtins.pathExists ./nope) ];
  pos = __curPos;
  tree = "${./sub}";
  treePath = builtins.path { path = ./sub; name = "sub-tree"; };
  str = toString ./sub/data.txt;
  recOrder = let r = rec { zeta = alpha; alpha = beta; beta = 1; q = r; }; in r.zeta;
  f = { c, a, b ? c, ... } @ args: [ a b c (builtins.attrNames args) ];
  called = f { c = 3; a = 1; extra = true; };
  link = builtins.readFile ./sub/link;
  throws = builtins.tryEval (import ./lib/throws.nix);
}
"#,
    );
    w("lib/a.nix", "{ n = 40; file = ./a.nix; parent = ./..; }\n");
    w(
        "lib/throws.nix",
        "let x = { y = throw \"boom at ${toString __curPos.line}\"; }; in x.y\n",
    );
    w(
        "sub/b.nix",
        "{ a }: { m = a.n / 20; nested = import ./deeper/c.nix; }\n",
    );
    w(
        "sub/deeper/c.nix",
        "rec { up = ./../..; up2 = ../.; v = builtins.baseNameOf (toString up2); }\n",
    );
    w("sub/data.txt", "hello data\n");
    w("bad.nix", "{ a = 1;\n");
    w("undef.nix", "let a = 1; in a + nosuchvar\n");
    w("escape.nix", "../../outside\n");
    w(
        "shapes.nix",
        r#"{ n ? 3, ... }@args:
let
  inherit (builtins) length;
  set = rec { a = 1; b = a + 1; "quoted name" = b * 2; ${"dyn" + "amic"} = 3; nested.deeper.x = 4; };
  list = [ 1 2.5 "s" null true false ./shapes.nix (x: x) [ ] { } ] ++ [ n ];
  str = "interp ${toString n} \${not} \n\t";
  ind = ''
    indented ${toString set.a}
      more ''${x}
  '';
  f = { a, b ? a * 2, ... }@all: a + b + length (builtins.attrNames all);
  g = x: y: if x < y then x - y else x / y;
in
with set;
assert a == 1;
{
  inherit a b list str ind;
  sel = set.nested.deeper.x or 0;
  missing = set.nope or "default";
  has = [ (set ? a) (set ? nested.deeper.x) (set ? nope) ];
  call = f { a = 1; };
  ops = [ (g 1 2) (g 4 2) (!true) (-n) (1 <= 2) (2 >= 3) (1 != 2) (true && false) (true || false) (false -> true) ({ x = 1; } // { y = 2; }) ];
  curried = g 3;
  dynamic = set.dynamic;
  letin = let x = 1; y = x + 1; in y;
  quoted = set."quoted name";
  args = builtins.attrNames args;
}
"#,
    );
    let symlink = |target: &str, at: &str| {
        std::os::unix::fs::symlink(target, dir.path().join(at)).unwrap();
    };
    symlink("data.txt", "sub/link");
    symlink("a.nix", "lib/alias.nix");
    symlink("../lib", "sub/libdir");
    symlink("../..", "sub/up");
    symlink("/", "sub/rootlink");
    std::fs::write(dir.path().join("sub/run.sh"), "#!/bin/sh\n").unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(
        dir.path().join("sub/run.sh"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    dir
}

fn compile(dir: &Path, ctx: &ImageCompileContext) -> Arc<Image> {
    let fs = FsAccessor::new(dir);
    let bytes =
        compile_tree(&fs, &CanonPath::root(), ctx, &CompileOptions::default()).expect("compiles");
    Arc::new(Image::open(bytes, Some(&ctx.fingerprint)).expect("opens"))
}

/// Every path in the tree, without following symlinks.
fn walk(acc: &dyn SourceAccessor, path: &CanonPath, out: &mut Vec<CanonPath>) {
    out.push(path.clone());
    if acc.lstat(path).unwrap().file_type == FileType::Directory {
        for (name, _) in acc.read_dir(path).unwrap() {
            walk(acc, &path.join_component(&name).unwrap(), out);
        }
    }
}

/// The canonical path of `path` (bytes, which need not be UTF-8).
fn p(path: impl AsRef<[u8]>) -> CanonPath {
    CanonPath::new(path).unwrap()
}

#[test]
fn image_tree_matches_source_tree() {
    let dir = fixture();
    let ctx = context();
    let image = compile(dir.path(), &ctx);
    let fs = FsAccessor::new(dir.path());
    // Only the `.nix` sources are stored by default; other files come
    // from the source tree through the fallback.
    let img =
        ImageAccessor::new(image.clone()).with_fallback(Arc::new(FsAccessor::new(dir.path())));
    let mut paths = Vec::new();
    walk(&fs, &CanonPath::root(), &mut paths);
    assert!(paths.len() > 10);
    for p in &paths {
        let a = fs.maybe_lstat(p).unwrap().expect("exists");
        let b = img.maybe_lstat(p).unwrap().expect("exists in image");
        assert_eq!(a.file_type, b.file_type, "{p:?}");
        assert_eq!(a.executable, b.executable, "{p:?}");
        match a.file_type {
            FileType::Directory => {
                assert_eq!(fs.read_dir(p).unwrap(), img.read_dir(p).unwrap(), "{p:?}");
                let (h, size) = brix_store::nar::hash(&fs, p, HashAlgo::Sha256).unwrap();
                assert_eq!(img.precomputed_nar_hash(p), Some((h, size)), "{p:?}");
            }
            FileType::Regular => {
                assert_eq!(fs.read_file(p).unwrap(), img.read_file(p).unwrap(), "{p:?}");
                // A file's NAR is hashed by the caller.
                assert_eq!(img.precomputed_nar_hash(p), None, "{p:?}");
            }
            FileType::Symlink => {
                assert_eq!(fs.read_link(p).unwrap(), img.read_link(p).unwrap(), "{p:?}");
                assert_eq!(img.precomputed_nar_hash(p), None, "{p:?}");
            }
            other => panic!("unexpected {other:?}"),
        }
    }
    assert_eq!(img.maybe_lstat(&p("/nope")).unwrap(), None);
    // A missing path is shown as the source tree shows it.
    assert_eq!(
        img.read_file(&p("/nope")).unwrap_err().to_string(),
        fs.read_file(&p("/nope")).unwrap_err().to_string()
    );
    let (root_hash, root_size) =
        brix_store::nar::hash(&fs, &CanonPath::root(), HashAlgo::Sha256).unwrap();
    assert_eq!(image.nar_hash(), root_hash);
    assert_eq!(image.nar_size(), root_size);
    // Every .nix file is a module; the one that escapes the tree is
    // recorded as unsupported, the broken ones as their errors.
    assert_eq!(image.module_count(), 9);
}

/// Symlinks among a path's ancestors are followed as the file system
/// follows them for the source tree, through a link that leaves the tree
/// by asking the source tree; a final symlink is refused when reading, as
/// the `SourceAccessor` contract requires.
#[test]
fn symlinks_are_followed_like_the_source_tree() {
    let dir = fixture();
    let ctx = context();
    let image = compile(dir.path(), &ctx);
    let fs = FsAccessor::new(dir.path());
    let img =
        ImageAccessor::new(image.clone()).with_fallback(Arc::new(FsAccessor::new(dir.path())));
    let fixture_name = dir.path().file_name().unwrap().to_str().unwrap();
    let up = format!("/sub/up/{fixture_name}/default.nix");
    for path in [
        "/sub/libdir",
        "/sub/libdir/a.nix",
        "/sub/libdir/nope",
        "/sub/libdir/a.nix/below",
        "/lib/alias.nix",
        "/sub/up",
        &up,
        "/sub/rootlink/nix",
    ] {
        let path = p(path);
        assert_eq!(
            img.maybe_lstat(&path).unwrap(),
            fs.maybe_lstat(&path).unwrap(),
            "{path:?}"
        );
    }
    // Symlinks among the ancestors are followed by both.
    assert_eq!(
        img.read_dir(&p("/sub/rootlink/nix")).unwrap(),
        fs.read_dir(&p("/sub/rootlink/nix")).unwrap()
    );
    for path in ["/sub/libdir/a.nix", &up] {
        assert_eq!(
            img.read_file(&p(path)).unwrap(),
            fs.read_file(&p(path)).unwrap(),
            "{path}"
        );
    }
    // A final symlink is refused by both, as every `SourceAccessor` must
    // (callers resolve symlinks first).
    let refused = |r: Result<_, AccessorError>| matches!(r, Err(AccessorError::PathIsASymlink(_)));
    assert!(refused(img.read_dir(&p("/sub/libdir")).map(|_| ())));
    assert!(refused(fs.read_dir(&p("/sub/libdir")).map(|_| ())));
    for path in ["/lib/alias.nix", "/sub/link"] {
        assert!(refused(img.read_file(&p(path)).map(|_| ())), "{path}");
        assert!(refused(fs.read_file(&p(path)).map(|_| ())), "{path}");
    }
    // Without the source tree, a path out of the tree cannot be answered.
    let bare = ImageAccessor::new(image);
    assert_eq!(
        bare.maybe_lstat(&p(&up)).unwrap_err().to_string(),
        format!(
            "'{up}' leads out of the compiled image through a symlink, and the source tree is \
             not available"
        )
    );
    assert_eq!(
        bare.read_file(&p("/sub/libdir/throws.nix"))
            .unwrap_err()
            .to_string(),
        "'/sub/libdir/throws.nix' is not stored in the compiled image (compiled without the \
         tree) and the source tree is not available"
    );
}

#[test]
fn image_is_deterministic() {
    let dir = fixture();
    let ctx = context();
    let fs = FsAccessor::new(dir.path());
    let a = compile_tree(
        &fs,
        &CanonPath::root(),
        &ctx,
        &CompileOptions {
            threads: 1,
            ..Default::default()
        },
    )
    .unwrap();
    let b = compile_tree(
        &fs,
        &CanonPath::root(),
        &ctx,
        &CompileOptions {
            threads: 4,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(&a[..], &b[..]);
}

/// Modules compiled before the tree is at hand (as an archive's are while
/// it downloads) are taken where the tree has that source at that path
/// and nowhere else: the image is the one the tree compiles to.
#[test]
fn precompiled_modules_give_the_same_image() {
    let dir = fixture();
    let ctx = context();
    let fs = FsAccessor::new(dir.path());
    let options = CompileOptions::default();
    let plain = compile_tree(&fs, &CanonPath::root(), &ctx, &options).unwrap();
    let mut paths = Vec::new();
    walk(&fs, &CanonPath::root(), &mut paths);
    let modules: Vec<(Vec<u8>, Vec<u8>)> = paths
        .iter()
        .filter(|path| path.rel().ends_with(b".nix"))
        .filter(|path| fs.lstat(path).unwrap().file_type == FileType::Regular)
        .map(|path| (path.rel().to_vec(), fs.read_file(path).unwrap()))
        .collect();
    assert!(modules.len() > 3, "the fixture has modules");

    // Every module, as the tree has it.
    let mut all = Precompiled::new();
    for (path, source) in &modules {
        all.compile(path, source, &ctx);
    }
    assert_eq!(all.len(), modules.len());
    let ahead = compile_tree_with(&fs, &CanonPath::root(), &ctx, &options, all).unwrap();
    assert_eq!(&plain[..], &ahead[..]);

    // Some of them, one from another source, one the tree does not have
    // and one compiled for another path than the tree has it at.
    let mut some = Precompiled::new();
    for (i, (path, source)) in modules.iter().enumerate() {
        match i % 3 {
            0 => some.compile(path, source, &ctx),
            1 => some.compile(path, b"{ stale = true; }", &ctx),
            _ => {}
        }
    }
    some.compile(b"not/in/the/tree.nix", b"1", &ctx);
    let (first, second) = (&modules[0], &modules[1]);
    some.compile(&second.0, &first.1, &ctx);
    let mixed = compile_tree_with(&fs, &CanonPath::root(), &ctx, &options, some).unwrap();
    assert_eq!(&plain[..], &mixed[..]);
}

/// The globals the fixture is compiled against for
/// [`compiler_output_is_pinned`]: the names it uses, in fixed slots.
const PINNED_GLOBALS: &[&str] = &[
    "builtins",
    "import",
    "throw",
    "toString",
    "baseNameOf",
    "true",
    "false",
    "null",
    "__sub",
    "__mul",
    "__div",
    "__lessThan",
];

/// What the compiler makes of the fixture is pinned: a change to the
/// compiler (`ir.rs`, `compile.rs`, the desugarings in `brix-syntax`) or
/// to the module encoding that changes what images hold must come with a
/// new `IR_FINGERPRINT`, or images compiled before it would be misread.
/// The fixture is compiled against a fixed context of its own, so that
/// adding a builtin (which changes every evaluator's fingerprint anyway)
/// does not change the code.
#[test]
fn compiler_output_is_pinned() {
    let dir = fixture();
    let globals = PINNED_GLOBALS
        .iter()
        .zip(0u32..)
        .map(|(name, slot)| (name.as_bytes().to_vec(), slot))
        .collect();
    // The base symbols are the ones every symbol table starts with.
    let base = brix_eval::STATIC_SYMBOLS
        .iter()
        .map(|s| s.as_bytes().to_vec())
        .collect();
    let ctx = ImageCompileContext::new(base, globals, brix_syntax::ParseOptions::default());
    let image = build_image(
        &FsAccessor::new(dir.path()),
        &CanonPath::root(),
        &ctx,
        &CompileOptions {
            threads: 1,
            include_tree: false,
            notes: false,
        },
    )
    .expect("compiles");
    // The fixture compiles as intended: everything but the broken files
    // and the one that escapes the tree is code.
    let kinds: Vec<(String, u32)> = (0..image.modules.entries.len())
        .map(|i| {
            let path = String::from_utf8(image.modules.paths.get(i).to_vec()).unwrap();
            (path, image.modules.entries[i].kind)
        })
        .collect();
    for (path, kind) in &kinds {
        let expected = match path.as_str() {
            "bad.nix" | "undef.nix" => fmt::MODULE_ERROR,
            "escape.nix" => fmt::MODULE_UNSUPPORTED,
            _ => fmt::MODULE_CODE,
        };
        assert_eq!(*kind, expected, "{path}");
    }
    let mut data = Vec::new();
    let mut section = |bytes: &[u8]| {
        data.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
        data.extend_from_slice(bytes);
    };
    let words: Vec<u8> = image
        .code
        .words
        .iter()
        .flat_map(|w| w.to_le_bytes())
        .collect();
    section(&words);
    section(&image.code.bytes);
    section(&image.names.bytes);
    let ends: Vec<u8> = image
        .names
        .ends
        .iter()
        .flat_map(|e| e.to_le_bytes())
        .collect();
    section(&ends);
    let hash = Hash::of(HashAlgo::Sha256, &data).to_string(HashFormat::Base16, false);
    assert_eq!(
        hash, IR_FINGERPRINT_FIXTURE_HASH,
        "compiler output changed: bump IR_FINGERPRINT and update this hash \
         (IR_FINGERPRINT_FIXTURE_HASH in image/format.rs) to {hash}"
    );
}

/// How an expression's files are served when it is evaluated over the
/// image.
#[derive(Clone, Copy, Debug)]
enum Served {
    /// It imports modules, linked from the image.
    Linked,
    /// It imports a file the image holds only as the error importing it
    /// raises: nothing is linked or parsed.
    Error,
    /// It imports a file the image cannot serve as code (a path literal
    /// escapes the tree, it is not a `.nix` file, or it is reached through
    /// a symlinked directory), which is parsed from source.
    Parsed,
    /// It imports nothing.
    Nothing,
}

/// An expression over the fixture: whether it evaluates, and how its
/// files are served from the image.
struct Case {
    expr: &'static str,
    ok: bool,
    served: Served,
}

const fn case(expr: &'static str, ok: bool, served: Served) -> Case {
    Case { expr, ok, served }
}

const EXPRESSIONS: &[Case] = &[
    case("(import ./. {}).sum", true, Served::Linked),
    case("(import ./. { x = 10; }).b", true, Served::Linked),
    case("(import ./. {}).a", true, Served::Linked),
    case("(import ./. {}).here", true, Served::Linked),
    case("(import ./. {}).data", true, Served::Linked),
    case("(import ./. {}).listing", true, Served::Linked),
    case("(import ./. {}).exists", true, Served::Linked),
    case("(import ./. {}).pos", true, Served::Linked),
    case("(import ./. {}).tree", true, Served::Linked),
    case("(import ./. {}).treePath", true, Served::Linked),
    case("(import ./. {}).str", true, Served::Linked),
    case("(import ./. {}).recOrder", true, Served::Linked),
    case("(import ./. {}).called", true, Served::Linked),
    case("(import ./. {}).link", true, Served::Linked),
    case("(import ./. {}).throws", true, Served::Linked),
    case("(import ./. {}).b.nested", true, Served::Linked),
    case(
        "builtins.trace (import ./. {}).a.parent 1",
        true,
        Served::Linked,
    ),
    case("import ./bad.nix", false, Served::Error),
    case("import ./undef.nix", false, Served::Error),
    case("import ./lib/throws.nix", false, Served::Linked),
    case("(import ./. {}).f { c = 1; }", false, Served::Linked),
    case(
        "(import ./. {}).f { a = 1; c = 2; d = 3; }",
        true,
        Served::Linked,
    ),
    case("import ./escape.nix", true, Served::Parsed),
    case(
        "builtins.hashFile \"sha256\" ./sub/data.txt",
        true,
        Served::Nothing,
    ),
    case(
        "let s = import ./. {}; in builtins.unsafeGetAttrPos \"sum\" s",
        true,
        Served::Linked,
    ),
    case("import ./sub/run.sh", false, Served::Parsed),
    case("import ./lib/alias.nix", true, Served::Linked),
    case("(import ./sub/libdir/a.nix).file", true, Served::Parsed),
    case("builtins.readDir ./sub/libdir", true, Served::Nothing),
    case("import ./shapes.nix {}", true, Served::Linked),
    case(
        "(import ./shapes.nix { n = 5; extra = 1; }).args",
        true,
        Served::Linked,
    ),
    case("builtins.readFile ./nope", false, Served::Nothing),
    case("import ./nope.nix", false, Served::Nothing),
];

#[test]
fn evaluation_matches_source() {
    let dir = fixture();
    let ctx = context();
    let image = compile(dir.path(), &ctx);
    let source_host = host(source_tree(dir.path()));
    // Data files come from the source tree, as in production.
    let image_host = host(image_tree(&image, dir.path()));
    let differential = Differential::new(&source_host, &image_host, &image);
    for Case { expr, ok, served } in EXPRESSIONS {
        differential.check(expr.as_bytes(), *ok, *served);
    }
}

/// Evaluations of one tree from source and over its image, compared.
struct Differential<'h> {
    source_host: &'h TestHost,
    /// Data files come from the source tree, as in production.
    image_host: &'h TestHost,
    image: &'h Arc<Image>,
    /// What an evaluation parses before touching any file: the
    /// evaluator's own helper expressions plus the expression itself.
    baseline: u64,
}

impl<'h> Differential<'h> {
    fn new(
        source_host: &'h TestHost,
        image_host: &'h TestHost,
        image: &'h Arc<Image>,
    ) -> Differential<'h> {
        let (_, baseline, _) = evaluate(image_host, Some(image.clone()), "1");
        Differential {
            source_host,
            image_host,
            image,
            baseline,
        }
    }

    /// Evaluates `expr` (bytes) from source and over the image: both
    /// must print the same, byte for byte, succeed or fail as `ok` says,
    /// and serve the expression's files as `served` says. Returns the
    /// run over the image.
    fn check(&self, expr: &[u8], ok: bool, served: Served) -> Run {
        let expr_shown = expr.as_bstr();
        let (from_source, parsed_src, _) = evaluate(self.source_host, None, expr);
        let (from_image, parsed_img, linked) =
            evaluate(self.image_host, Some(self.image.clone()), expr);
        assert_eq!(from_source, from_image, "{expr_shown}");
        assert_eq!(from_source.ok, ok, "{expr_shown}:\n{}", from_source.stderr);
        assert!(parsed_src >= 1, "{expr_shown}");
        #[cfg(feature = "work-counters")]
        if from_source.ok {
            assert!(
                from_source.work.iter().any(|&n| n > 0),
                "{expr_shown}: no work counted"
            );
        }
        // The expression itself is parsed; files come from the image,
        // except those it cannot serve.
        let expected_parsed = match served {
            Served::Parsed => self.baseline + 1,
            Served::Linked | Served::Error | Served::Nothing => self.baseline,
        };
        assert_eq!(
            parsed_img, expected_parsed,
            "{expr_shown}: files parsed via image"
        );
        match served {
            Served::Linked => assert!(linked >= 1, "{expr_shown}: no module linked"),
            Served::Error | Served::Parsed | Served::Nothing => {
                assert_eq!(linked, 0, "{expr_shown}: modules linked")
            }
        }
        from_image
    }
}

/// The source and image hosts for a tree, and its image.
fn hosts(dir: &Path) -> (TestHost, TestHost, Arc<Image>) {
    let image = compile(dir, &context());
    let source_host = host(source_tree(dir));
    let image_host = host(image_tree(&image, dir));
    (source_host, image_host, image)
}

/// "already defined at …" names the earlier definition's file the way it
/// is loaded, not the way the image compiler saw it (a path relative to
/// the tree).
#[test]
fn duplicate_attribute_errors_name_the_loaded_file() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("sub")).unwrap();
    std::fs::write(dir.path().join("sub/dup.nix"), "{\n  a = 1;\n  a = 2;\n}\n").unwrap();
    let (source_host, image_host, image) = hosts(dir.path());
    let expr = "import ./sub/dup.nix";
    let (from_source, _, _) = evaluate(&source_host, None, expr);
    let (from_image, _, linked) = evaluate(&image_host, Some(image), expr);
    assert_eq!(from_source, from_image);
    assert_eq!(
        linked, 0,
        "the error is the image's, not a parse of the source"
    );
    assert!(
        from_image.stderr.contains_str(format!(
            "error: attribute 'a' already defined at {MOUNT}/sub/dup.nix:2:3\n"
        )),
        "{}",
        from_image.stderr
    );
}

/// A file the parser warns about is served from source, so the warning
/// is logged whenever the file is parsed, as Nix does.
#[test]
fn files_with_parse_warnings_are_parsed_from_source() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("cursed.nix"),
        "let or = 1; in builtins.length [ (x: x) or ]\n",
    )
    .unwrap();
    let (source_host, image_host, image) = hosts(dir.path());
    let (_, baseline, _) = evaluate(&image_host, Some(image.clone()), "1");
    let expr = "import ./cursed.nix";
    let (from_source, _, _) = evaluate(&source_host, None, expr);
    let (from_image, parsed, _) = evaluate(&image_host, Some(image), expr);
    assert_eq!(from_source, from_image);
    assert_eq!(parsed, baseline + 1, "cursed.nix is parsed");
    assert_eq!(from_image.stdout, "1");
    assert!(
        from_image.stderr.starts_with_str(format!(
            "warning: at {MOUNT}/cursed.nix:1:34: This expression uses `or` as an identifier"
        )),
        "{}",
        from_image.stderr
    );
}

#[test]
fn rejects_wrong_images() {
    let dir = fixture();
    let ctx = context();
    let fs = FsAccessor::new(dir.path());
    let bytes = compile_tree(&fs, &CanonPath::root(), &ctx, &CompileOptions::default()).unwrap();
    // Another fingerprint.
    let mut other = ctx.clone();
    other.fingerprint[0] ^= 0xff;
    assert!(matches!(
        Image::open(bytes.clone(), Some(&other.fingerprint)),
        Err(ImageError::Fingerprint)
    ));
    // Another format version: the preamble says so before anything else
    // is read.
    let mut old = bytes.to_vec();
    old[8..12].copy_from_slice(&(format::FORMAT_VERSION - 1).to_le_bytes());
    assert!(matches!(
        Image::from_vec(old, None),
        Err(ImageError::FormatVersion { found, .. }) if found == format::FORMAT_VERSION - 1
    ));
    // Truncated anywhere: in the preamble, in the archive.
    for cut in [0, 7, 47, 48, 49, bytes.len() / 2, bytes.len() - 1] {
        let truncated = bytes[..cut].to_vec();
        assert!(Image::from_vec(truncated, None).is_err(), "cut at {cut}");
    }
    // Not an image at all, or an image from before the preamble (an
    // archive from the first byte).
    assert!(matches!(
        Image::from_vec(b"hello".to_vec(), None),
        Err(ImageError::NotAnImage)
    ));
    assert!(matches!(
        Image::from_vec(bytes[format::PREAMBLE_LEN..].to_vec(), None),
        Err(ImageError::NotAnImage)
    ));
    // Flipped bytes somewhere in the middle: refused when the image is
    // opened, or else when the part they hit is first used (here: all of
    // them, by `verify`); never a panic.
    let mut corrupt = bytes.to_vec();
    let mid = corrupt.len() / 3;
    for b in &mut corrupt[mid..mid + 64] {
        *b ^= 0x5a;
    }
    match Image::from_vec(corrupt, Some(&ctx.fingerprint)) {
        Err(e) => assert!(matches!(e, ImageError::Corrupt(_)), "{e}"),
        Ok(image) => assert!(image.verify().is_err(), "damage in the middle not found"),
    }
    // The real thing loads, and an evaluator with the matching context
    // accepts it while one with another does not.
    let image = Arc::new(Image::open(bytes, Some(&ctx.fingerprint)).unwrap());
    let arena = Arena::new();
    let host = host(Arc::new(MemoryAccessor::new()));
    let ev = Evaluator::with_builtins(&arena, Config::default(), &host, |ev| {
        ev.add_global(
            "extraGlobalForThisTest",
            ev.alloc_cell(brix_eval::Value::Null),
        );
    });
    assert!(matches!(
        ev.register_image(CanonPath::new(MOUNT).unwrap(), image.clone()),
        Err(ImageError::Fingerprint)
    ));
    let ev2 = Evaluator::new(&arena, Config::default(), &host);
    ev2.register_image(CanonPath::new(MOUNT).unwrap(), image)
        .unwrap();
}

/// The error line of a failed run (the one that starts with `error: `
/// and is not a trace).
fn error_line(run: &Run) -> &BStr {
    assert!(!run.ok, "evaluated to {}", run.stdout);
    run.stderr
        .lines()
        .rfind(|l| l.trim_start().starts_with(b"error: "))
        .unwrap_or_else(|| panic!("no error line in:\n{}", run.stderr))
        .trim_start()
        .as_bstr()
}

/// Module code is validated when a module is linked, not when the image
/// is opened. Damage to the file is caught by the module's checksum
/// first (see `bit_flips_are_caught_where_they_are_used`); code that
/// matches its checksum but is malformed, as a faulty writer could
/// produce, opens fine and then fails the import that links the module
/// with a clean error that names the image, never a crash or a misread;
/// the image keeps the damage for its owner.
#[test]
fn malformed_module_code_fails_cleanly() {
    let dir = fixture();
    let ctx = context();
    let clean = fixture_image(dir.path(), &ctx, false);
    let module = clean.modules.entries[module_index(&clean, "lib/a.nix")];
    let words = module.words_start as usize..(module.words_start + module.words_len) as usize;
    // The module's header is 15 counts (the node count third); its first
    // node follows its name list and its string table.
    let first_node =
        15 + (clean.code.words[words.start] + clean.code.words[words.start + 1]) as usize;
    type Damage = Box<dyn Fn(&mut [u32])>;
    let damages: Vec<(&str, Damage, &str)> = vec![
        (
            "a node count far beyond the words",
            Box::new(|w: &mut [u32]| w[2] = u32::MAX),
            "module code: bad node count or root",
        ),
        (
            "an unknown node tag",
            Box::new(move |w: &mut [u32]| w[first_node] = w[first_node] & !0xff | 0xee),
            "module code: ",
        ),
        (
            "every word flipped",
            Box::new(|w: &mut [u32]| {
                for x in w {
                    *x ^= 0x5a5a_5a5a;
                }
            }),
            "module code: ",
        ),
        (
            "a section count one too high",
            // The sections no longer add up to the module's words.
            Box::new(|w: &mut [u32]| w[13] += 1),
            "module code: sections exceed the module",
        ),
    ];
    let image_host = host(image_tree(&compile(dir.path(), &ctx), dir.path()));
    for (what, damage, why) in &damages {
        let mut damaged = fixture_image(dir.path(), &ctx, false);
        damage(&mut damaged.code.words[words.clone()]);
        let image = Arc::new(
            open(&damaged, &ctx)
                .unwrap_or_else(|e| panic!("{what}: does not open: {e}"))
                .with_label("/cache/fixture.brix"),
        );
        let (run, _, linked) = evaluate(&image_host, Some(image.clone()), "(import ./lib/a.nix).n");
        let line = error_line(&run);
        let prefix = format!(
            "error: compiled image '/cache/fixture.brix': corrupt brix image: module \
             'lib/a.nix': {why}"
        );
        assert!(line.starts_with_str(&prefix), "{what}: {line}");
        assert!(
            line.ends_with_str("; delete the image or run with --no-compiled-inputs"),
            "{what}: {line}"
        );
        assert_eq!(linked, 0, "{what}");
        match image.damage() {
            Some(ImageError::CorruptModule { module, why: found }) => {
                assert_eq!(module, "lib/a.nix", "{what}");
                assert!(found.starts_with(why), "{what}: {found}");
            }
            other => panic!("{what}: damage {other:?}"),
        }
    }
    // Undamaged, the same import works and nothing is recorded.
    let image = Arc::new(open(&clean, &ctx).unwrap());
    let (run, _, linked) = evaluate(&image_host, Some(image.clone()), "(import ./lib/a.nix).n");
    assert_eq!(run.stdout, "40");
    assert_eq!(linked, 1);
    assert!(image.damage().is_none());
}

/// The fixture compiled for `ctx` as a value, to be damaged.
fn fixture_image(dir: &Path, ctx: &ImageCompileContext, include_tree: bool) -> fmt::Image {
    build_image(
        &FsAccessor::new(dir),
        &CanonPath::root(),
        ctx,
        &CompileOptions {
            threads: 1,
            include_tree,
            notes: false,
        },
    )
    .expect("compiles")
}

fn open(image: &fmt::Image, ctx: &ImageCompileContext) -> Result<Image, ImageError> {
    Image::open(
        to_bytes(image, &ctx.fingerprint).expect("serialises"),
        Some(&ctx.fingerprint),
    )
}

/// The index of module `path` in the image's module table.
fn module_index(image: &fmt::Image, path: impl AsRef<[u8]>) -> usize {
    let path = path.as_ref();
    (0..image.modules.paths.len())
        .find(|&i| image.modules.paths.get(i) == path)
        .unwrap_or_else(|| panic!("no module {:?}", path.as_bstr()))
}

/// The index of the directory that entry `name` of directory `parent`
/// is.
fn dir_index(image: &fmt::Image, parent: usize, name: impl AsRef<[u8]>) -> usize {
    let name = name.as_ref().as_bstr();
    let entry = image.dirs[parent]
        .entries
        .iter()
        .find(|e| e.name == name.as_bytes())
        .unwrap_or_else(|| panic!("no entry {name:?} in directory {parent}"));
    match entry.kind {
        fmt::EntryKind::Dir(d) => d as usize,
        _ => panic!("{name:?} is not a directory"),
    }
}

/// Damage to the module table and the name table opens fine (they are
/// read when a module is linked) and fails the import that meets it,
/// with the loader's message.
#[test]
fn damaged_tables_fail_the_import() {
    let dir = fixture();
    let ctx = context();
    let clean = fixture_image(dir.path(), &ctx, false);
    let a = module_index(&clean, "lib/a.nix");
    let first_name = {
        let m = &clean.modules.entries[a];
        // The module's new names follow its 15-word header.
        clean.code.words[m.words_start as usize + 15]
    };
    type Damage = Box<dyn Fn(&mut fmt::Image)>;
    let damages: Vec<(Damage, String)> = vec![
        (
            Box::new(move |i| i.modules.entries[a].bytes_start = u32::MAX),
            "module bytes out of range".into(),
        ),
        (
            Box::new(move |i| i.modules.entries[a].words_len = u32::MAX / 4),
            "module words out of range".into(),
        ),
        (
            Box::new(move |i| i.modules.entries[a].kind = 7),
            "unknown module kind 7".into(),
        ),
        (
            Box::new(move |i| {
                i.modules.entries[a].kind = fmt::MODULE_ERROR;
                i.modules.entries[a].error = 99;
            }),
            "module error out of range".into(),
        ),
        (
            Box::new(|i| i.names = fmt::StringTable::default()),
            format!("name index {first_name} out of range"),
        ),
        (
            Box::new(|i| i.names.bytes.clear()),
            format!("name {first_name} out of range"),
        ),
    ];
    let image_host = host(image_tree(&compile(dir.path(), &ctx), dir.path()));
    for (damage, why) in &damages {
        let mut image = fixture_image(dir.path(), &ctx, false);
        damage(&mut image);
        let image = Arc::new(open(&image, &ctx).unwrap_or_else(|e| panic!("{why}: {e}")));
        let (run, _, _) = evaluate(&image_host, Some(image), "(import ./lib/a.nix).n");
        assert_eq!(
            error_line(&run),
            format!(
                "error: compiled image: corrupt brix image: module 'lib/a.nix': {why}; run with \
                 --no-compiled-inputs to evaluate from source"
            )
        );
    }
}

/// Damage to the image's other tables is found when it is opened, or
/// registered, or when the blob is read.
#[test]
fn damaged_images_are_refused() {
    let dir = fixture();
    let ctx = context();
    let corrupt = |r: Result<Image, ImageError>| match r {
        Ok(_) => panic!("damaged image opened"),
        Err(e) => e.to_string(),
    };
    // A module path without a module.
    let mut image = fixture_image(dir.path(), &ctx, false);
    image.modules.paths.push(b"extra.nix");
    assert_eq!(
        corrupt(open(&image, &ctx)),
        "corrupt brix image: module table and module paths differ in length"
    );
    // Bytes that do not start on the archive's alignment.
    let image = fixture_image(dir.path(), &ctx, false);
    let bytes = to_bytes(&image, &ctx.fingerprint).unwrap();
    struct Shifted(rkyv::util::AlignedVec, usize);
    impl AsRef<[u8]> for Shifted {
        fn as_ref(&self) -> &[u8] {
            &self.0[self.1..]
        }
    }
    let mut shifted = rkyv::util::AlignedVec::<16>::new();
    shifted.extend_from_slice(&[0]);
    shifted.extend_from_slice(&bytes);
    assert_eq!(
        corrupt(Image::open(Shifted(shifted, 1), Some(&ctx.fingerprint))),
        "corrupt brix image: image bytes are not 16-byte aligned; use Image::from_vec"
    );
    // A base symbol count that disagrees with the fingerprint.
    let mut image = fixture_image(dir.path(), &ctx, false);
    image.header.base_symbols += 1;
    let image = Arc::new(open(&image, &ctx).unwrap());
    let arena = Arena::new();
    let host = host(Arc::new(MemoryAccessor::new()));
    let ev = Evaluator::new(&arena, Config::default(), &host);
    assert_eq!(
        ev.register_image(CanonPath::new(MOUNT).unwrap(), image)
            .unwrap_err()
            .to_string(),
        "corrupt brix image: base symbol count does not match the fingerprint"
    );
}

/// A stored file whose blob is damaged reads as an error naming the
/// blob, through [`Image::blob`] and through the accessor; the length a
/// blob declares is never trusted for more than the stream can hold.
#[test]
fn damaged_blobs_fail_cleanly() {
    let dir = fixture();
    let ctx = context();
    let clean = fixture_image(dir.path(), &ctx, true);
    // `default.nix` is long enough to be stored compressed.
    let blob = clean.modules.entries[module_index(&clean, "default.nix")].blob;
    let i = blob as usize;
    assert_eq!(clean.blobs[i].encoding, fmt::BLOB_DEFLATE);
    let raw_len = clean.blobs[i].raw_len;
    type Damage = Box<dyn Fn(&mut fmt::Blob)>;
    let damages: Vec<(Damage, String)> = vec![
        (
            Box::new(|b| b.encoding = 9),
            format!("corrupt brix image: blob {blob} has unknown encoding 9"),
        ),
        (
            Box::new(|b| b.raw_len += 1),
            format!(
                "corrupt brix image: blob {blob}: decompressed to {raw_len} bytes, expected {}",
                raw_len + 1
            ),
        ),
        (
            Box::new(|b| b.raw_len = u32::MAX),
            format!(
                "corrupt brix image: blob {blob}: decompressed to {raw_len} bytes, expected {}",
                u32::MAX
            ),
        ),
        (
            Box::new(|b| b.data.truncate(b.data.len() / 2)),
            format!("corrupt brix image: blob {blob}: decompression failed: "),
        ),
        (
            Box::new(|b| {
                for x in &mut b.data {
                    *x = 0xff;
                }
            }),
            format!("corrupt brix image: blob {blob}: decompression failed: "),
        ),
    ];
    for (damage, expected) in &damages {
        let mut image = fixture_image(dir.path(), &ctx, true);
        damage(&mut image.blobs[i]);
        let image = Arc::new(open(&image, &ctx).unwrap());
        let err = image.blob(blob).unwrap_err().to_string();
        assert!(err.starts_with(expected.as_str()), "{err}");
        assert!(image.damage().is_some());
        let acc = ImageAccessor::new(image);
        let err = acc.read_file(&p("/default.nix")).unwrap_err().to_string();
        assert!(
            err.starts_with(&format!("compiled image: {expected}")),
            "{err}"
        );
    }
}

/// Walks the tree below `path`, without following symlinks, and fails
/// with the first error.
fn try_walk(acc: &dyn SourceAccessor, path: &CanonPath) -> Result<(), AccessorError> {
    if acc.lstat(path)?.file_type == FileType::Directory {
        for (name, _) in acc.read_dir(path)? {
            try_walk(acc, &path.join_component(&name).unwrap())?;
        }
    }
    Ok(())
}

/// The directory tree is checked a directory at a time, when a lookup
/// first reaches it (only the root when the image is opened): it is a
/// tree (no cycles, no shared directories), rooted at a directory with
/// the header's hash, with valid, sorted names and blobs in range. That
/// the tree reaches every directory is a property of the whole table,
/// which `verify` checks.
#[test]
fn damaged_trees_are_refused() {
    let dir = fixture();
    let ctx = context();
    let clean = fixture_image(dir.path(), &ctx, false);
    let sub = dir_index(&clean, 0, "sub");
    let deeper = dir_index(&clean, sub, "deeper");
    let lib = dir_index(&clean, 0, "lib");
    let dirs = clean.dirs.len();
    let blobs = clean.blobs.len();
    fn entry_named(image: &fmt::Image, parent: usize, name: &str) -> usize {
        image.dirs[parent]
            .entries
            .iter()
            .position(|e| e.name == name.as_bytes())
            .unwrap_or_else(|| panic!("no entry {name} in directory {parent}"))
    }
    // Names are shown escaped, as bytes.
    let first_name = format!("{:?}", clean.dirs[0].entries[0].name.as_bstr());
    type Damage = Box<dyn Fn(&mut fmt::Image)>;
    let mut damages: Vec<(Damage, String)> = vec![
        (Box::new(|i| i.dirs.clear()), "no root directory".into()),
        (
            Box::new(|i| i.dirs[0].nar_hash[0] ^= 1),
            "the root's NAR hash or size differs from the header's".into(),
        ),
        (
            Box::new(|i| i.dirs[0].nar_size += 1),
            "the root's NAR hash or size differs from the header's".into(),
        ),
        (
            // A cycle: `sub/deeper` is `sub` again.
            Box::new(move |i| {
                let e = entry_named(i, sub, "deeper");
                i.dirs[sub].entries[e].kind = fmt::EntryKind::Dir(sub as u32);
            }),
            format!("directory {sub} lists directory {sub}, which does not follow it"),
        ),
        (
            // Back to the root.
            Box::new(move |i| {
                let e = entry_named(i, deeper, "c.nix");
                i.dirs[deeper].entries[e].kind = fmt::EntryKind::Dir(0);
            }),
            format!("directory {deeper} lists directory 0, which does not follow it"),
        ),
        (
            Box::new(move |i| {
                let e = entry_named(i, sub, "deeper");
                i.dirs[sub].entries[e].kind = fmt::EntryKind::Dir(dirs as u32);
            }),
            format!("directory {sub} lists directory {dirs}, which does not follow it"),
        ),
        (
            // A shared subtree: `lib` is `sub` too.
            Box::new(move |i| {
                let e = entry_named(i, 0, "lib");
                i.dirs[0].entries[e].kind = fmt::EntryKind::Dir(sub as u32);
            }),
            format!("directory {sub} is listed twice"),
        ),
        (
            // `lib` and `sub` swapped: the writer lists directories in
            // pre-order.
            Box::new(move |i| {
                let (l, s) = (entry_named(i, 0, "lib"), entry_named(i, 0, "sub"));
                i.dirs[0].entries[l].kind = fmt::EntryKind::Dir(sub as u32);
                i.dirs[0].entries[s].kind = fmt::EntryKind::Dir(lib as u32);
            }),
            format!("directory 0 lists directory {lib} out of order"),
        ),
        (
            // `lib` no longer listed anywhere.
            Box::new(move |i| {
                let e = entry_named(i, 0, "lib");
                i.dirs[0].entries[e].kind = fmt::EntryKind::File {
                    blob: 0,
                    executable: false,
                };
            }),
            "1 directories are not in the tree".into(),
        ),
        (
            Box::new(move |i| {
                let e = entry_named(i, lib, "a.nix");
                i.dirs[lib].entries[e].kind = fmt::EntryKind::File {
                    blob: blobs as u32,
                    executable: false,
                };
            }),
            format!("directory {lib} lists blob {blobs}, which is out of range"),
        ),
        (
            Box::new(move |i| i.dirs[0].entries.swap(0, 1)),
            format!("directory 0 is not sorted at {first_name}"),
        ),
        (
            Box::new(move |i| i.dirs[0].entries[1].name = i.dirs[0].entries[0].name.clone()),
            format!("directory 0 is not sorted at {first_name}"),
        ),
    ];
    let bad_names: [&'static [u8]; 5] = [b"", b".", b"..", b"a/b", b"a\0b"];
    for bad in bad_names {
        damages.push((
            Box::new(move |i| i.dirs[0].entries[0].name = bad.to_vec()),
            format!(
                "directory 0 has an entry with the invalid name {:?}",
                bad.as_bstr()
            ),
        ));
    }
    for (damage, why) in &damages {
        let mut image = fixture_image(dir.path(), &ctx, false);
        damage(&mut image);
        let expected = format!("corrupt brix image: directory tree: {why}");
        // The root is checked when the image is opened.
        let at_open = why == "no root directory" || why.starts_with("the root's");
        match open(&image, &ctx) {
            Err(e) => {
                assert!(at_open, "{why}: refused when opened: {e}");
                assert_eq!(e.to_string(), expected);
                continue;
            }
            Ok(_) => assert!(!at_open, "{why}: the damaged root was accepted"),
        }
        // Any other directory when a walk reaches it, and by `verify`.
        let opened = Arc::new(open(&image, &ctx).unwrap());
        let walked = try_walk(&ImageAccessor::new(opened.clone()), &CanonPath::root());
        if why.ends_with("not in the tree") {
            walked.expect("a walk does not reach an unlisted directory");
        } else {
            assert_eq!(
                walked.unwrap_err().to_string(),
                format!(
                    "compiled image: {expected}; run with --no-compiled-inputs to evaluate from \
                     source"
                ),
                "{why}"
            );
            assert!(opened.damage().is_some(), "{why}: the damage is not kept");
        }
        let verified = open(&image, &ctx).unwrap().verify();
        assert_eq!(verified.unwrap_err().to_string(), expected, "{why}");
    }
    // Undamaged, it opens, walks and verifies.
    let image = Arc::new(open(&clean, &ctx).unwrap());
    try_walk(&ImageAccessor::new(image.clone()), &CanonPath::root()).unwrap();
    image.verify().unwrap();
}

/// A subtree shared by two directories is found when its second parent
/// is first reached, before the subtree is expanded a second time,
/// whichever parent comes first.
#[test]
fn shared_subtrees_are_found_at_the_second_parent() {
    let dir = tempfile::tempdir().unwrap();
    for (path, contents) in [("a/f.txt", "f\n"), ("b/c/g.txt", "g\n")] {
        let p = dir.path().join(path);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, contents).unwrap();
    }
    let ctx = context();
    let mut image = fixture_image(dir.path(), &ctx, false);
    let (a, b) = (dir_index(&image, 0, "a"), dir_index(&image, 0, "b"));
    let c = dir_index(&image, b, "c");
    // `a/f.txt` becomes `b/c` too: after both parents, so no cycle.
    assert!(c > a && c > b);
    image.dirs[a].entries[0].kind = fmt::EntryKind::Dir(c as u32);
    let shared = format!(
        "compiled image: corrupt brix image: directory tree: directory {c} is listed twice; run \
         with --no-compiled-inputs to evaluate from source"
    );
    for (first, second) in [("/a/f.txt", "/b/c"), ("/b/c", "/a/f.txt")] {
        let acc = ImageAccessor::new(Arc::new(open(&image, &ctx).unwrap()));
        // The first parent is reached and serves the subtree.
        assert_eq!(
            acc.read_dir(&p(first)).unwrap().keys().collect::<Vec<_>>(),
            ["g.txt"],
            "{first}"
        );
        // Reaching the second fails.
        let second_parent = p(second).parent().unwrap();
        assert_eq!(
            acc.read_dir(&second_parent).unwrap_err().to_string(),
            shared,
            "{second}"
        );
        assert_eq!(acc.read_dir(&p(second)).unwrap_err().to_string(), shared);
    }
}

/// The archive of the image file `bytes`, read in place, to find where
/// its parts lie in the file.
fn archive_of(bytes: &[u8]) -> &fmt::ArchivedImage {
    let len = format::read_preamble(bytes)
        .expect("a preamble")
        .archive_len as usize;
    rkyv::access::<fmt::ArchivedImage, rkyv::rancor::Error>(
        &bytes[format::PREAMBLE_LEN..format::PREAMBLE_LEN + len],
    )
    .expect("a valid archive")
}

/// Where `part`, which lies in `file`, starts in it.
fn offset_of<T: ?Sized>(file: &[u8], part: &T) -> usize {
    part as *const T as *const u8 as usize - file.as_ptr() as usize
}

/// The index of module `path` in an archived image.
fn archived_module(image: &fmt::ArchivedImage, path: &str) -> usize {
    (0..image.modules.paths.len())
        .find(|&i| image.modules.paths.get(i) == Some(path.as_bytes()))
        .unwrap_or_else(|| panic!("no module {path}"))
}

/// The entry `name` of archived directory `dir`.
fn archived_entry<'a>(
    image: &'a fmt::ArchivedImage,
    dir: usize,
    name: &str,
) -> &'a fmt::ArchivedEntry {
    image.dirs[dir]
        .entries
        .iter()
        .find(|e| e.name.as_slice() == name.as_bytes())
        .unwrap_or_else(|| panic!("no entry {name} in directory {dir}"))
}

/// What a flipped bit must lead to.
enum Caught {
    /// Opening the image fails with this message.
    Open(&'static str),
    /// Opening succeeds, and evaluating `expr` fails with this error line.
    Eval { expr: &'static str, error: String },
}

/// A flipped bit that leaves a valid image behind (in an integer literal,
/// a string, a name, a module path or record or error, a directory's NAR
/// hash or entry, a file's length or contents, a checksum) is caught by
/// the checksum of the part it hits, when that part is first used: the
/// evaluation fails naming the image, and the image keeps the damage for
/// its owner. One that hits the preamble or the header is caught when the
/// image is opened.
#[test]
fn bit_flips_are_caught_where_they_are_used() {
    let dir = fixture();
    let ctx = context();
    let label = "/cache/fixture.brix";
    let damaged = |what: &str| {
        format!(
            "error: compiled image '{label}': corrupt brix image: {what}; delete the image or run \
             with --no-compiled-inputs"
        )
    };
    let module = |path: &str| damaged(&format!("module '{path}': checksum mismatch"));
    for include_tree in [false, true] {
        let value = fixture_image(dir.path(), &ctx, include_tree);
        let bytes = to_bytes(&value, &ctx.fingerprint).unwrap();
        let file = &bytes[..];
        let image = archive_of(file);
        let a = archived_module(image, "lib/a.nix");
        let record = &image.modules.entries[a];
        let words_start = record.words_start.to_native() as usize;
        let words = &image.code.words.as_slice()
            [words_start..words_start + record.words_len.to_native() as usize];
        // `n = 40`: an INT node is its tag word (0), then the value's low
        // and high words.
        let forty = words
            .windows(3)
            .position(|w| w.iter().map(|x| x.to_native()).eq([0, 40, 0]))
            .expect("the literal 40")
            + 1;
        // The first name the module creates follows its 15-word header.
        let name = words[15].to_native() as usize;
        // The chunk of names it lies in.
        let name_chunk = {
            let first = name / fmt::STRINGS_PER_CHUNK * fmt::STRINGS_PER_CHUNK;
            first..image.names.len().min(first + fmt::STRINGS_PER_CHUNK)
        };
        let name_start = match name {
            0 => 0,
            _ => image.names.ends.as_slice()[name - 1].to_native() as usize,
        };
        let throws = &image.modules.entries[archived_module(image, "lib/throws.nix")];
        let throws_bytes = &image.code.bytes.as_slice()[throws.bytes_start.to_native() as usize..]
            [..throws.bytes_len.to_native() as usize];
        let boom = throws_bytes
            .windows(8)
            .position(|w| w == b"boom at ")
            .expect("the literal");
        let bad = &image.modules.entries[archived_module(image, "bad.nix")];
        let bad_msg = match &image.modules.errors[bad.error.to_native() as usize] {
            fmt::ArchivedModuleError::ParseError { msg, .. } => msg.as_slice(),
            fmt::ArchivedModuleError::UndefinedVariable { .. } => {
                panic!("bad.nix is a parse error")
            }
        };
        let sub = match archived_entry(image, 0, "sub").kind {
            fmt::ArchivedEntryKind::Dir(d) => d.to_native() as usize,
            _ => panic!("sub is a directory"),
        };
        let data = archived_entry(image, sub, "data.txt");
        let blob = match data.kind {
            fmt::ArchivedEntryKind::File { blob, .. } => blob.to_native(),
            _ => panic!("data.txt is a file"),
        };
        let data_blob = &image.blobs[blob as usize];
        let blob_damaged = damaged(&format!("blob {blob}: checksum mismatch"));
        // The checksum table: the header's slot, the directories', one per
        // chunk of names and paths, then the modules'.
        let slot = 1
            + image.dirs.len()
            + image.names.len().div_ceil(fmt::STRINGS_PER_CHUNK)
            + image.modules.paths.len().div_ceil(fmt::STRINGS_PER_CHUNK)
            + a;
        let table =
            format::PREAMBLE_LEN + format::read_preamble(file).unwrap().archive_len as usize;
        let a_n = "(import ./lib/a.nix).n";
        let read_data = "builtins.readFile ./sub/data.txt";
        let sub_damaged = damaged("directory '/sub': checksum mismatch");
        // (what, byte, bit, caught)
        let mut flips: Vec<(&str, usize, u8, Caught)> = vec![
            (
                "the archive's length",
                48,
                0,
                Caught::Open("corrupt brix image: preamble: checksum mismatch"),
            ),
            (
                "the header's NAR hash",
                offset_of(file, &image.header.nar_hash),
                3,
                Caught::Open("corrupt brix image: header: checksum mismatch"),
            ),
            (
                "an integer literal",
                offset_of(file, &words[forty]),
                0,
                Caught::Eval {
                    expr: a_n,
                    error: module("lib/a.nix"),
                },
            ),
            (
                "a string literal",
                offset_of(file, &throws_bytes[boom]),
                1,
                Caught::Eval {
                    expr: "import ./lib/throws.nix",
                    error: module("lib/throws.nix"),
                },
            ),
            (
                "a module's source length",
                offset_of(file, &record.len),
                4,
                Caught::Eval {
                    expr: a_n,
                    error: module("lib/a.nix"),
                },
            ),
            (
                "a module's checksum",
                table + 8 * slot,
                0,
                Caught::Eval {
                    expr: a_n,
                    error: module("lib/a.nix"),
                },
            ),
            (
                "a parse error's message",
                offset_of(file, &bad_msg[0]),
                0,
                Caught::Eval {
                    expr: "import ./bad.nix",
                    error: module("bad.nix"),
                },
            ),
            (
                "a name",
                offset_of(file, &image.names.bytes.as_slice()[name_start]),
                0,
                Caught::Eval {
                    expr: a_n,
                    error: damaged(&format!(
                        "module 'lib/a.nix': names {} to {}: checksum mismatch",
                        name_chunk.start,
                        name_chunk.end - 1
                    )),
                },
            ),
            (
                "a module path",
                offset_of(file, &image.modules.paths.get(a).unwrap()[4]),
                0,
                Caught::Eval {
                    expr: a_n,
                    error: damaged(&format!(
                        "module paths 0 to {}: checksum mismatch",
                        image.modules.paths.len() - 1
                    )),
                },
            ),
            (
                "a directory's NAR hash",
                offset_of(file, &image.dirs[sub].nar_hash),
                7,
                Caught::Eval {
                    expr: r#""${./sub}""#,
                    error: sub_damaged.clone(),
                },
            ),
            (
                // The last byte, so the names stay sorted.
                "a file's name",
                offset_of(file, &data.name.as_slice()[7]),
                0,
                Caught::Eval {
                    expr: read_data,
                    error: sub_damaged.clone(),
                },
            ),
            (
                // Checked when a `stat` reports it.
                "a file's length",
                offset_of(file, &data_blob.raw_len),
                1,
                Caught::Eval {
                    expr: "builtins.readFileType ./sub/data.txt",
                    error: blob_damaged.clone(),
                },
            ),
        ];
        if include_tree {
            flips.push((
                "a file's contents",
                offset_of(file, &data_blob.data.as_slice()[0]),
                0,
                Caught::Eval {
                    expr: read_data,
                    error: blob_damaged,
                },
            ));
        }
        for (what, at, bit, caught) in &flips {
            let what = format!("{what} (with the tree: {include_tree})");
            let mut flipped = bytes.to_vec();
            flipped[*at] ^= 1 << bit;
            let opened = Image::from_vec(flipped, Some(&ctx.fingerprint));
            match caught {
                Caught::Open(error) => match opened {
                    Ok(_) => panic!("{what}: the image opened"),
                    Err(e) => assert_eq!(e.to_string(), *error, "{what}"),
                },
                Caught::Eval { expr, error } => {
                    let image = Arc::new(
                        opened
                            .unwrap_or_else(|e| panic!("{what}: does not open: {e}"))
                            .with_label(label),
                    );
                    let image_host = host(image_tree(&image, dir.path()));
                    let (run, _, _) = evaluate(&image_host, Some(image.clone()), expr);
                    assert_eq!(error_line(&run), error, "{what}");
                    assert!(image.damage().is_some(), "{what}: the damage is not kept");
                }
            }
        }
    }

    // Without its checksum, the flipped literal is a valid image that
    // evaluates to something else.
    let mut value = fixture_image(dir.path(), &ctx, false);
    let a = module_index(&value, "lib/a.nix");
    let words = value.modules.entries[a].words_start as usize..;
    let forty = value.code.words[words]
        .windows(3)
        .position(|w| w == [0, 40, 0])
        .unwrap()
        + value.modules.entries[a].words_start as usize
        + 1;
    value.code.words[forty] ^= 1;
    let image = Arc::new(open(&value, &ctx).unwrap());
    let image_host = host(image_tree(&image, dir.path()));
    let (run, _, linked) = evaluate(&image_host, Some(image), "(import ./lib/a.nix).n");
    assert_eq!((run.stdout.as_bstr(), linked), (b"41".as_bstr(), 1));
}

/// The name and module path tables are checksummed in chunks of
/// [`fmt::STRINGS_PER_CHUNK`] strings: damage in a later chunk is caught
/// when a name or path in it is first used (a path when a lookup misses),
/// while the chunks before it keep serving.
#[test]
fn large_tables_are_checked_chunk_by_chunk() {
    let chunk = fmt::STRINGS_PER_CHUNK;
    let modules = 3 * chunk + 4;
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("m")).unwrap();
    for i in 0..modules {
        std::fs::write(
            dir.path().join(format!("m/{i:04}.nix")),
            format!("{{ n{i:04} = {i}; }}\n"),
        )
        .unwrap();
    }
    let ctx = context();
    let value = fixture_image(dir.path(), &ctx, false);
    // One new name per module, so name i is module i's.
    assert_eq!(
        (value.names.len(), value.modules.paths.len()),
        (modules, modules)
    );
    let bytes = to_bytes(&value, &ctx.fingerprint).unwrap();
    Image::from_vec(bytes.to_vec(), Some(&ctx.fingerprint))
        .unwrap()
        .verify()
        .expect("an intact image verifies");
    // Damage in the second chunk.
    let damaged = chunk + chunk / 2;
    let range = format!("{chunk} to {}", 2 * chunk - 1);
    let archive = archive_of(&bytes);
    let name = offset_of(&bytes, &archive.names.get(damaged).unwrap()[0]);
    let path = offset_of(&bytes, &archive.modules.paths.get(damaged).unwrap()[0]);
    for (at, damage) in [
        (
            name,
            format!("module 'm/{damaged:04}.nix': names {range}: checksum mismatch"),
        ),
        (path, format!("module paths {range}: checksum mismatch")),
    ] {
        let mut flipped = bytes.to_vec();
        flipped[at] ^= 1;
        let image = Arc::new(Image::from_vec(flipped, Some(&ctx.fingerprint)).unwrap());
        let image_host = host(image_tree(&image, dir.path()));
        let (run, _, linked) = evaluate(
            &image_host,
            Some(image.clone()),
            "(import ./m/0005.nix).n0005",
        );
        assert_eq!(
            (run.stdout.as_bstr(), linked),
            (b"5".as_bstr(), 1),
            "{damage}"
        );
        let (run, _, _) = evaluate(
            &image_host,
            Some(image.clone()),
            format!("(import ./m/{damaged:04}.nix).n{damaged:04}"),
        );
        assert_eq!(
            error_line(&run),
            format!(
                "error: compiled image: corrupt brix image: {damage}; run with \
                 --no-compiled-inputs to evaluate from source"
            )
        );
        let found = image.verify().unwrap_err().to_string();
        assert!(
            found.ends_with(&format!("{range}: checksum mismatch")),
            "{found}"
        );
    }
}

/// A module lookup that finds its path does not check the path table:
/// the module's own checksum covers its path. So a path damaged into
/// another module's (here `c.nix`'s into `a.nix`) finds the wrong
/// module, which fails its checksum when it is linked instead of running
/// as the other file.
#[test]
fn a_path_damaged_into_another_fails_at_link() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.nix"), "\"a\"\n").unwrap();
    std::fs::write(dir.path().join("c.nix"), "\"c\"\n").unwrap();
    let ctx = context();
    let bytes = to_bytes(&fixture_image(dir.path(), &ctx, false), &ctx.fingerprint).unwrap();
    let archive = archive_of(&bytes);
    let c = archived_module(archive, "c.nix");
    let at = offset_of(&bytes, &archive.modules.paths.get(c).unwrap()[0]);
    let mut flipped = bytes.to_vec();
    // `a` and `c` differ in one bit.
    flipped[at] ^= b'a' ^ b'c';
    let image = Arc::new(Image::from_vec(flipped, Some(&ctx.fingerprint)).unwrap());
    assert_eq!(
        image.find_module("a.nix").unwrap(),
        Some(c),
        "the search meets the damaged path"
    );
    let image_host = host(image_tree(&image, dir.path()));
    let (run, _, linked) = evaluate(&image_host, Some(image.clone()), "import ./a.nix");
    assert_eq!(
        error_line(&run),
        "error: compiled image: corrupt brix image: module 'a.nix': checksum mismatch; run with \
         --no-compiled-inputs to evaluate from source"
    );
    assert_eq!(linked, 0);
}

/// A small tree with one part of every kind: modules that compile, fail
/// to parse, refer to an undefined variable and escape the tree;
/// directories, a symlink, an executable, and data files short and long
/// (stored as they are, and deflated, in a full-tree image).
fn small_tree() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let w = |rel: &str, contents: &str| {
        let p = dir.path().join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, contents).unwrap();
    };
    w(
        "default.nix",
        "{ n = 40; s = \"str\"; f = ./sub/data.txt; b = import ./sub/b.nix 1; }\n",
    );
    w("sub/b.nix", "x: { y = x + 1; }\n");
    w("sub/data.txt", "hello\n");
    w("sub/long.txt", &"long and repetitive\n".repeat(8));
    w("bad.nix", "{ a = 1;\n");
    w("undef.nix", "nosuchvar\n");
    w("escape.nix", "../../outside\n");
    std::os::unix::fs::symlink("data.txt", dir.path().join("sub/link")).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(
        dir.path().join("sub/long.txt"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    dir
}

/// Every single-bit flip anywhere in an image file, slim or with the
/// tree, is refused: when the image is opened, or by the checksum of the
/// part it hits (here all compared at once by `verify`). The only flips
/// that go unseen are those that change nothing the loader reads
/// (alignment padding): the image they leave reads back exactly as
/// before. None panics.
#[test]
fn every_bit_flip_is_refused_or_harmless() {
    let dir = small_tree();
    let ctx = context();
    for include_tree in [false, true] {
        let value = fixture_image(dir.path(), &ctx, include_tree);
        let bytes = to_bytes(&value, &ctx.fingerprint).unwrap();
        let mut unseen = 0;
        for bit in 0..8 * bytes.len() {
            let mut flipped = bytes.to_vec();
            flipped[bit / 8] ^= 1 << (bit % 8);
            let refused = match Image::from_vec(flipped.clone(), Some(&ctx.fingerprint)) {
                Err(_) => true,
                Ok(image) => image.verify().is_err(),
            };
            if !refused {
                let len = format::read_preamble(&flipped).unwrap().archive_len as usize;
                let mut archive = rkyv::util::AlignedVec::<16>::new();
                archive.extend_from_slice(&flipped[format::PREAMBLE_LEN..][..len]);
                let read_back = rkyv::from_bytes::<fmt::Image, rkyv::rancor::Error>(&archive)
                    .expect("an image that opens reads back");
                assert!(
                    read_back == value,
                    "bit {} of byte {} changed the image (with the tree: {include_tree}) unseen",
                    bit % 8,
                    bit / 8
                );
                unseen += 1;
            }
        }
        // Padding is a small part of the file.
        assert!(
            unseen < bytes.len(),
            "{unseen} of {} bits unseen (with the tree: {include_tree})",
            8 * bytes.len()
        );
    }
}

/// A tree with a file an image cannot hold (4 GiB or more) is declined
/// with an error naming the file, before the file is read.
#[test]
fn huge_files_are_declined() {
    struct Huge(MemoryAccessor);
    impl SourceAccessor for Huge {
        fn maybe_lstat(&self, path: &CanonPath) -> Result<Option<Stat>, AccessorError> {
            let st = self.0.maybe_lstat(path)?;
            Ok(st.map(|st| match st.file_type {
                FileType::Regular => Stat {
                    size: Some(u64::from(u32::MAX) + 1),
                    ..st
                },
                _ => st,
            }))
        }
        fn read_dir(&self, path: &CanonPath) -> Result<DirEntries, AccessorError> {
            self.0.read_dir(path)
        }
        fn read_file(&self, _: &CanonPath) -> Result<Vec<u8>, AccessorError> {
            panic!("a file too large for an image is read")
        }
        fn read_link(&self, path: &CanonPath) -> Result<BString, AccessorError> {
            self.0.read_link(path)
        }
        fn display_prefix(&self) -> &BStr {
            BStr::new("/huge")
        }
    }
    let mut tree = MemoryAccessor::new();
    tree.add_file(&p("/big.iso"), "", false).unwrap();
    let err = compile_tree(
        &Huge(tree),
        &CanonPath::root(),
        &context(),
        &CompileOptions::default(),
    )
    .unwrap_err();
    assert_eq!(
        err.to_string(),
        "while compiling an image: '/huge/big.iso' is 4 GiB or larger, which an image cannot hold"
    );
}

/// Compiles nixpkgs to an image and checks that evaluating through it
/// gives the same derivation paths as the source tree, with no parsing.
/// Slow: `cargo test --release -p brix-eval --test image -- --ignored --nocapture`.
#[test]
#[ignore = "slow: compiles and evaluates nixpkgs"]
fn nixpkgs_through_image() {
    let nixpkgs = brix_testutil::nixpkgs();
    let ctx = context();
    let fs = FsAccessor::new(nixpkgs);
    let start = Instant::now();
    let bytes = compile_tree(&fs, &CanonPath::root(), &ctx, &CompileOptions::default()).unwrap();
    eprintln!(
        "compiled nixpkgs: {} bytes in {:?}",
        bytes.len(),
        start.elapsed()
    );
    let start = Instant::now();
    let image = Arc::new(Image::open(bytes, Some(&ctx.fingerprint)).unwrap());
    eprintln!(
        "opened in {:?}: {} modules",
        start.elapsed(),
        image.module_count()
    );
    let mount = CanonPath::new(nixpkgs).unwrap();
    let mk_host =
        |tree: Arc<dyn SourceAccessor>| TestHost::with_accessors(&[(nixpkgs, tree)], &[nixpkgs]);
    let source_host = mk_host(Arc::new(FsAccessor::new(nixpkgs)));
    // Files the slim image does not store are read from the tree, as the
    // facade arranges.
    let image_host = mk_host(Arc::new(
        ImageAccessor::new(image.clone()).with_fallback(Arc::new(FsAccessor::new(nixpkgs))),
    ));
    let cases = [
        format!("(import {nixpkgs} {{ system = \"x86_64-linux\"; }}).hello.drvPath"),
        format!("(import {nixpkgs} {{ system = \"x86_64-linux\"; }}).python3Packages.numpy.drvPath"),
        format!("(import {nixpkgs} {{ system = \"x86_64-linux\"; }}).firefox.drvPath"),
        format!("(import {nixpkgs}/nixos {{ configuration = {{ fileSystems.\"/\" = {{ device = \"/dev/null\"; fsType = \"ext4\"; }}; boot.loader.grub.enable = false; system.stateVersion = \"26.05\"; }}; system = \"x86_64-linux\"; }}).config.system.build.toplevel.drvPath"),
    ];
    let baseline = {
        let arena = Arena::new();
        let ev = Evaluator::new(&arena, Config::default(), &image_host);
        ev.eval_str(b"1", "/").expect("evaluates");
        ev.stats().files_parsed.get()
    };
    for expr in &cases {
        let run = |host: &TestHost, img: Option<Arc<Image>>| {
            let arena = Arena::new();
            let ev = Evaluator::new(&arena, Config::default(), host);
            if let Some(img) = img {
                ev.register_image(mount.clone(), img).unwrap();
            }
            let start = Instant::now();
            let out = ev
                .eval_str(expr.as_bytes(), "/")
                .and_then(|v| {
                    let cell = ev.alloc_cell(v);
                    ev.force_deep(cell)?;
                    ev.print_ambiguous(cell)
                })
                .unwrap_or_else(|e| e.render(true));
            let st = ev.stats();
            #[cfg(feature = "work-counters")]
            let work = [
                st.thunks.get(),
                st.thunk_forces.get(),
                st.envs.get(),
                st.lambda_calls.get(),
                st.attr_sets.get(),
            ];
            #[cfg(not(feature = "work-counters"))]
            let work = ();
            (
                out,
                start.elapsed(),
                st.files_parsed.get(),
                st.parse_time.get(),
                st.compile_time.get(),
                st.image_modules.get(),
                st.image_decode_time.get(),
                work,
            )
        };
        let s = run(&source_host, None);
        let i = run(&image_host, Some(image.clone()));
        eprintln!(
            "{}\n  source: {:?} (parsed {} files in {:?}, compile {:?})\n  image:  {:?} (parsed {} files, linked {} modules, decode {:?}, decode+link {:?})",
            s.0, s.1, s.2, s.3, s.4, i.1, i.2, i.5, i.6, i.4
        );
        assert!(
            s.0.starts_with_str("\"/nix/store/") && s.0.ends_with_str(".drv\""),
            "{expr}: expected a derivation path, got {}",
            s.0
        );
        assert_eq!(s.0, i.0, "{expr}");
        #[cfg(feature = "work-counters")]
        {
            assert!(s.7.iter().all(|&n| n > 0), "{expr}: work not counted");
            assert_eq!(s.7, i.7, "{expr}: work counters differ");
        }
        // Beyond the expression and the evaluator's helpers, the only file
        // parsed is `<nix/fetchurl.nix>`, which lives in the evaluator, not
        // in nixpkgs.
        assert!(
            i.2 <= baseline + 1,
            "{expr}: {} files parsed via the image, expected at most {}",
            i.2,
            baseline + 1
        );
    }
}

/// Without the tree, an image stores only `.nix` files: other files are
/// served from the source tree, or fail loudly without one. With
/// `include_tree` the image is self-contained.
#[test]
fn tree_contents_are_optional() {
    let dir = fixture();
    let ctx = context();
    let fs = FsAccessor::new(dir.path());
    let data = p("/sub/data.txt");
    let nix = p("/lib/a.nix");

    let slim = compile(dir.path(), &ctx);
    assert!(!slim.has_full_tree());
    let bare = ImageAccessor::new(slim.clone());
    assert_eq!(
        bare.maybe_lstat(&data).unwrap().unwrap().size,
        Some("hello data\n".len() as u64)
    );
    // Metadata and the line tables are there; file contents (even of
    // `.nix` files, whose code is in the image) are not, and asking for
    // them says so.
    for missing in [&data, &nix] {
        let err = bare.read_file(missing).unwrap_err().to_string();
        assert!(
            err.contains("not stored in the compiled image"),
            "unexpected error: {err}"
        );
    }
    // A file's NAR hash is not stored, a directory's is, regardless.
    assert!(bare.precomputed_nar_hash(&data).is_none());
    let (h, size) = brix_store::nar::hash(&fs, &p("/sub"), HashAlgo::Sha256).unwrap();
    assert_eq!(bare.precomputed_nar_hash(&p("/sub")), Some((h, size)));
    // With the source tree as fallback the file reads normally.
    let served = ImageAccessor::new(slim).with_fallback(Arc::new(FsAccessor::new(dir.path())));
    assert_eq!(served.read_file(&data).unwrap(), b"hello data\n");
    let mut out = Vec::new();
    assert_eq!(served.read_file_to(&data, &mut out).unwrap(), 11);
    assert_eq!(out, b"hello data\n");

    // A self-contained image needs no fallback.
    let bytes = compile_tree(
        &fs,
        &CanonPath::root(),
        &ctx,
        &CompileOptions {
            threads: 0,
            include_tree: true,
            notes: false,
        },
    )
    .unwrap();
    let full = Arc::new(Image::open(bytes, Some(&ctx.fingerprint)).unwrap());
    assert!(full.has_full_tree());
    let full_acc = ImageAccessor::new(full);
    assert_eq!(full_acc.read_file(&data).unwrap(), b"hello data\n");
    assert_eq!(
        full_acc.read_file(&nix).unwrap(),
        fs.read_file(&nix).unwrap()
    );
    assert!(full_acc.precomputed_nar_hash(&data).is_none());
}

/// Evaluation through a slim image with the source tree as fallback
/// reads data files exactly as from source.
#[test]
fn slim_image_evaluates_with_fallback() {
    let dir = fixture();
    let ctx = context();
    let image = compile(dir.path(), &ctx);
    let source_host = host(Arc::new(FsAccessor::new(dir.path())));
    let image_host = host(Arc::new(
        ImageAccessor::new(image.clone()).with_fallback(Arc::new(FsAccessor::new(dir.path()))),
    ));
    let (_, baseline, _) = evaluate(&image_host, Some(image.clone()), "1");
    for expr in [
        "(import ./. {}).data",
        "(import ./. {}).link",
        "builtins.hashFile \"sha256\" ./sub/data.txt",
        "(import ./. {}).treePath",
    ] {
        let (from_source, _, _) = evaluate(&source_host, None, expr);
        let (from_image, parsed, _) = evaluate(&image_host, Some(image.clone()), expr);
        assert_eq!(from_source, from_image, "{expr}");
        // Nothing but the expression itself is parsed.
        assert_eq!(parsed, baseline, "{expr}: files parsed via image");
    }
}

/// A slim image with no source tree at hand still evaluates and reports
/// positions; only the excerpt lines of an error are missing, as for a
/// file Nix cannot read.
#[test]
fn slim_image_without_tree_reports_positions() {
    let dir = fixture();
    let ctx = context();
    let image = compile(dir.path(), &ctx);
    let bare_host = host(Arc::new(ImageAccessor::new(image.clone())));
    let (_, baseline, _) = evaluate(&bare_host, Some(image.clone()), "1");
    let (run, parsed, _) = evaluate(&bare_host, Some(image), "import ./lib/throws.nix");
    assert!(!run.ok);
    assert!(
        run.stderr.contains_str("lib/throws.nix:1:"),
        "position missing: {}",
        run.stderr
    );
    assert!(
        !run.stderr.contains_str("throw \"boom"),
        "excerpt should be absent without the source: {}",
        run.stderr
    );
    assert_eq!(parsed, baseline, "only the expression is parsed");
}

/// Names written in a Latin-1 file keep their bytes in an image: in its
/// names table (`"\xff"` and `"\xfe"` stay two attributes, with the same
/// symbols as from source) and in the errors it stores for a file that
/// does not compile.
#[test]
fn latin1_names_keep_their_bytes_in_images() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("latin1.nix"),
        b"{ \"\xff\" = 1; \"\xfe\" = 2; a.\"\xc3\" = 3; }\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("dup.nix"),
        b"{ \"\xff\" = 1; \"\xff\" = 2; }\n",
    )
    .unwrap();
    std::fs::write(dir.path().join("undefined.nix"), b"{ inherit \"\xff\"; }\n").unwrap();
    let (source_host, image_host, image) = hosts(dir.path());
    // The output is text: the names are hashed.
    let expr = r#"let s = import ./latin1.nix; in [ (builtins.length (builtins.attrNames s)) (builtins.hashString "sha256" (builtins.concatStringsSep "," (builtins.attrNames s ++ builtins.attrNames s.a))) ]"#;
    let (from_source, _, _) = evaluate(&source_host, None, expr);
    let (from_image, _, linked) = evaluate(&image_host, Some(image), expr);
    assert_eq!(from_source, from_image);
    assert_eq!(linked, 1, "latin1.nix comes from the image");
    let expected =
        Hash::of(HashAlgo::Sha256, b"a,\xfe,\xff,\xc3").to_string(HashFormat::Base16, false);
    assert_eq!(from_image.stdout, format!("[ 3 \"{expected}\" ]"));

    let image = fixture_image(dir.path(), &context(), false);
    let error = |path: &str| {
        let module = image.modules.entries[module_index(&image, path)];
        assert_eq!(module.kind, fmt::MODULE_ERROR, "{path}");
        &image.modules.errors[module.error as usize]
    };
    match error("dup.nix") {
        fmt::ModuleError::ParseError { msg, .. } => {
            assert_eq!(
                msg.as_bstr(),
                b"attribute '\"\xff\"' already defined".as_bstr()
            )
        }
        other => panic!("{other:?}"),
    }
    match error("undefined.nix") {
        fmt::ModuleError::UndefinedVariable { name, .. } => assert_eq!(name, b"\xff"),
        other => panic!("{other:?}"),
    }
}

/// `dir` joined with `rel`, a relative path given as bytes (which need
/// not be UTF-8).
fn join_bytes(dir: &Path, rel: &[u8]) -> PathBuf {
    dir.join(OsStr::from_bytes(rel))
}

/// A tree whose names are not UTF-8, as Nix takes them (bytes): `.nix`
/// files with relative path literals in a Latin-1 directory `d\xe9`,
/// among them a parse error, an undefined variable and a `throw`;
/// modules and data files whose names sort differently as signed and
/// as unsigned bytes (`z.nix` before `\xff.nix`, as a NAR lists them); a
/// symlink with a Latin-1 target to that directory and a dangling one;
/// and a directory `s` listing `z`, then `\xe9`.
fn byte_tree() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let w = |rel: &[u8], contents: &[u8]| {
        let p = join_bytes(dir.path(), rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, contents).unwrap();
    };
    w(b"z", b"z\n");
    w(b"caf\xe9", b"caf\xe9\n");
    w(b"\xff", b"ff\n");
    w(b"a\x80b", b"a80b\n");
    w(b"z.nix", b"\"z\"\n");
    w(b"\xff.nix", b"\"\xff\"\n");
    w(
        b"caf\xe9.nix",
        b"{ here = ./.; data = builtins.readFile (./. + \"/caf\xe9\"); }\n",
    );
    w(
        b"d\xe9/default.nix",
        b"{\n  here = ./.;\n  sib = ./. + \"/caf\xe9.nix\";\n  v = import (./. + \"/caf\xe9.nix\");\n  pos = __curPos;\n  up = ../z;\n  s = toString ./.;\n}\n",
    );
    w(b"d\xe9/caf\xe9.nix", b"{ x = 1; up = ../z; f = ./f; }\n");
    w(b"d\xe9/bad.nix", b"{ x = 1;\n  e = 1 +; }\n");
    w(b"d\xe9/undef.nix", b"let a = 1; in a + nosuchvar\n");
    w(
        b"d\xe9/throws.nix",
        b"let x = { y = throw \"boom at ${toString __curPos.line}\"; }; in x.y\n",
    );
    w(b"s/z", b"z\n");
    w(b"s/\xe9", b"e9\n");
    let symlink = |target: &[u8], at: &[u8]| {
        std::os::unix::fs::symlink(OsStr::from_bytes(target), join_bytes(dir.path(), at)).unwrap();
    };
    symlink(b"t\xe9", b"l\xff");
    symlink(b"d\xe9", b"l\xe9");
    dir
}

/// An image of a tree whose names are not UTF-8 evaluates exactly as the
/// source does, byte for byte: path literals relative to a Latin-1
/// directory, positions in its files (`__curPos`, a parse error's, an
/// undefined variable's, a `throw`'s trace), directory listings, links,
/// and store paths made from the tree's NAR hashes. Modules are found by
/// their byte paths, in a table sorted as unsigned bytes (a signed order
/// would miss `\xff.nix` and parse it). What each expression prints is
/// what the reference `nix-instantiate --eval --strict` printed for the
/// same tree (with its directory in place of the mount point).
#[test]
fn byte_names_evaluate_as_from_source() {
    let dir = byte_tree();
    let (source_host, image_host, image) = hosts(dir.path());
    let differential = Differential::new(&source_host, &image_host, &image);
    // (expression, whether it succeeds, how its files are served, what
    // it prints: on stdout when it succeeds, on stderr when it fails;
    // `@` stands for the mount point)
    type Case<'a> = (&'a [u8], bool, Served, &'a [&'a [u8]]);
    let cases: &[Case<'_>] = &[
        (b"import ./z.nix", true, Served::Linked, &[b"\"z\""]),
        (b"import (./. + \"/\xff.nix\")", true, Served::Linked, &[b"\"\xff\""]),
        (
            b"import (./. + \"/caf\xe9.nix\")",
            true,
            Served::Linked,
            &[b"{ data = \"caf\xe9\\n\"; here = @; }"],
        ),
        (
            b"import (./. + \"/d\xe9\")",
            true,
            Served::Linked,
            &[
                b"here = @/d\xe9;",
                b"pos = { column = 9; file = \"@/d\xe9/default.nix\"; line = 5; };",
                b"s = \"@/d\xe9\";",
                b"sib = @/d\xe9/caf\xe9.nix;",
                b"v = { f = @/d\xe9/f; up = @/z; x = 1; };",
            ],
        ),
        (
            b"(import (./. + \"/d\xe9/caf\xe9.nix\")).f",
            true,
            Served::Linked,
            &[b"@/d\xe9/f"],
        ),
        (
            b"import (./. + \"/d\xe9/bad.nix\")",
            false,
            Served::Error,
            &[
                b"error: syntax error, unexpected ';'",
                b"at @/d\xe9/bad.nix:2:10:",
            ],
        ),
        (
            b"import (./. + \"/d\xe9/undef.nix\")",
            false,
            Served::Error,
            &[
                b"error: undefined variable 'nosuchvar'",
                b"at @/d\xe9/undef.nix:1:19:",
            ],
        ),
        (
            b"import (./. + \"/d\xe9/throws.nix\")",
            false,
            Served::Linked,
            &[b"at @/d\xe9/throws.nix:1:11:", b"error: boom at 1"],
        ),
        // Reached through a symlinked directory: parsed from source.
        (
            b"(import (./. + \"/l\xe9/caf\xe9.nix\")).up",
            true,
            Served::Parsed,
            &[b"@/z"],
        ),
        (
            b"builtins.readDir ./.",
            true,
            Served::Nothing,
            &[b"{ \"a\x80b\" = \"regular\"; \"caf\xe9\" = \"regular\"; \"caf\xe9.nix\" = \"regular\"; \
                \"d\xe9\" = \"directory\"; \"l\xe9\" = \"symlink\"; \"l\xff\" = \"symlink\"; \
                s = \"directory\"; z = \"regular\"; \"z.nix\" = \"regular\"; \"\xff\" = \"regular\"; \
                \"\xff.nix\" = \"regular\"; }"],
        ),
        (
            b"builtins.readDir (./. + \"/d\xe9\")",
            true,
            Served::Nothing,
            &[b"{ \"bad.nix\" = \"regular\"; \"caf\xe9.nix\" = \"regular\"; \"default.nix\" = \
                \"regular\"; \"throws.nix\" = \"regular\"; \"undef.nix\" = \"regular\"; }"],
        ),
        (
            b"builtins.readDir (./. + \"/l\xe9\")",
            true,
            Served::Nothing,
            &[b"\"caf\xe9.nix\" = \"regular\";"],
        ),
        (
            b"builtins.readFile (./. + \"/l\xe9/caf\xe9.nix\")",
            true,
            Served::Nothing,
            &[b"\"{ x = 1; up = ../z; f = ./f; }\\n\""],
        ),
        (
            b"builtins.readFileType (./. + \"/l\xff\")",
            true,
            Served::Nothing,
            &[b"\"symlink\""],
        ),
        (
            b"builtins.hashFile \"sha256\" (./. + \"/d\xe9/caf\xe9.nix\")",
            true,
            Served::Nothing,
            &[b"\"6216fa97f0bdf57bbc11a10ac18cc573ee71ba15768843c16ce70dbf3e6d4808\""],
        ),
        // Store paths from the NAR hashes the image stores.
        (
            b"builtins.path { path = ./. + \"/d\xe9\"; name = \"d\"; }",
            true,
            Served::Nothing,
            &[b"\"/nix/store/v9fvy2v6mn30jn38m4vnkvxnjfpmd5v0-d\""],
        ),
        (
            b"builtins.path { path = ./.; name = \"tree\"; }",
            true,
            Served::Nothing,
            &[b"\"/nix/store/5gqf4ah8zvfjys891acx10irh10wk4b5-tree\""],
        ),
        // The directory's name is not a valid store path name.
        (
            b"\"${./. + \"/d\xe9\"}\"",
            false,
            Served::Nothing,
            &[b"name 'd\xe9' contains illegal character '\xe9'"],
        ),
    ];
    for &(expr, ok, served, prints) in cases {
        let run = differential.check(expr, ok, served);
        let out = if ok { &run.stdout } else { &run.stderr };
        for expected in prints {
            let expected = expected.replace(b"@", MOUNT);
            assert!(
                out.contains_str(&expected),
                "{:?}: {:?} is not in\n{}",
                expr.as_bstr(),
                expected.as_bstr(),
                out
            );
        }
    }
}

/// The file tree of an image of a tree whose names are not UTF-8 is the
/// tree's: every listing (names as bytes, in unsigned byte order, as a
/// NAR has them), file, link target and stored NAR hash, also through a
/// symlink with a Latin-1 target; and the modules are found by their
/// byte paths.
#[test]
fn byte_names_image_tree_matches_source_tree() {
    let dir = byte_tree();
    let image = compile(dir.path(), &context());
    let fs = FsAccessor::new(dir.path());
    let img =
        ImageAccessor::new(image.clone()).with_fallback(Arc::new(FsAccessor::new(dir.path())));
    let mut paths = Vec::new();
    walk(&fs, &CanonPath::root(), &mut paths);
    for path in &paths {
        let a = fs.maybe_lstat(path).unwrap().expect("exists");
        let b = img.maybe_lstat(path).unwrap().expect("exists in the image");
        assert_eq!(a.file_type, b.file_type, "{path:?}");
        assert_eq!(a.executable, b.executable, "{path:?}");
        match a.file_type {
            FileType::Directory => {
                assert_eq!(
                    fs.read_dir(path).unwrap(),
                    img.read_dir(path).unwrap(),
                    "{path:?}"
                );
                let hashed = brix_store::nar::hash(&fs, path, HashAlgo::Sha256).unwrap();
                assert_eq!(img.precomputed_nar_hash(path), Some(hashed), "{path:?}");
            }
            FileType::Regular => {
                assert_eq!(
                    fs.read_file(path).unwrap(),
                    img.read_file(path).unwrap(),
                    "{path:?}"
                );
            }
            FileType::Symlink => {
                assert_eq!(
                    fs.read_link(path).unwrap(),
                    img.read_link(path).unwrap(),
                    "{path:?}"
                );
            }
            other => panic!("unexpected {other:?}"),
        }
    }
    // The root's names in unsigned byte order: `z` before the names that
    // start with a high byte.
    let listed: Vec<BString> = img
        .read_dir(&CanonPath::root())
        .unwrap()
        .into_keys()
        .collect();
    let expected: [&[u8]; 11] = [
        b"a\x80b",
        b"caf\xe9",
        b"caf\xe9.nix",
        b"d\xe9",
        b"l\xe9",
        b"l\xff",
        b"s",
        b"z",
        b"z.nix",
        b"\xff",
        b"\xff.nix",
    ];
    assert_eq!(listed, expected);
    assert_eq!(img.read_link(&p(b"/l\xff")).unwrap(), b"t\xe9".as_bstr());
    // Through the symlink `l\xe9` -> `d\xe9`, without the source tree.
    let bare = ImageAccessor::new(image.clone());
    for path in [b"/l\xe9/caf\xe9.nix".as_slice(), b"/l\xe9/bad.nix"] {
        assert_eq!(
            bare.maybe_lstat(&p(path)).unwrap(),
            fs.maybe_lstat(&p(path)).unwrap(),
            "{:?}",
            path.as_bstr()
        );
    }
    assert_eq!(
        bare.read_dir(&p(b"/l\xe9/")).unwrap_err().message_bytes(),
        AccessorError::PathIsASymlink(BString::from(b"/l\xe9".as_slice())).message_bytes()
    );
    assert_eq!(
        img.read_file(&p(b"/l\xe9/caf\xe9.nix")).unwrap(),
        b"{ x = 1; up = ../z; f = ./f; }\n"
    );
    // Without the source tree, a file the image does not store is named
    // with its bytes.
    assert_eq!(
        bare.read_file(&p(b"/caf\xe9")).unwrap_err().message_bytes(),
        b"'/caf\xe9' is not stored in the compiled image (compiled without the tree) and the \
          source tree is not available"
    );
    // The modules are found by their byte paths.
    for rel in [
        b"z.nix".as_slice(),
        b"\xff.nix",
        b"caf\xe9.nix",
        b"d\xe9/default.nix",
        b"d\xe9/caf\xe9.nix",
        b"d\xe9/bad.nix",
    ] {
        assert!(
            image.find_module(rel).unwrap().is_some(),
            "{:?} is not a module",
            rel.as_bstr()
        );
    }
    assert_eq!(image.find_module(b"d\xe9/nope.nix").unwrap(), None);
    assert_eq!(image.find_module(b"l\xe9/caf\xe9.nix").unwrap(), None);
    image.verify().expect("the image verifies");
}

/// The NAR hashes an image stores for a tree whose names are not UTF-8
/// are Nix's: `nix hash path` of the tree and of two of its directories
/// (names in unsigned byte order, a symlink with a Latin-1 target).
#[test]
fn byte_names_nar_hashes_are_nix_s() {
    let dir = byte_tree();
    let image = compile(dir.path(), &context());
    let img = ImageAccessor::new(image.clone());
    let fs = FsAccessor::new(dir.path());
    for rel in [b"".as_slice(), b"d\xe9", b"s"] {
        let on_disk = match rel {
            b"" => dir.path().to_path_buf(),
            _ => join_bytes(dir.path(), rel),
        };
        let nix = Oracle::new()
            .dummy_store()
            .args(["hash", "path"])
            .arg(&on_disk)
            .output();
        assert!(
            nix.success(),
            "nix hash path {:?}: {:?}",
            rel.as_bstr(),
            nix.stderr.as_bstr()
        );
        let at = p([b"/".as_slice(), rel].concat());
        let (hash, size) = img
            .precomputed_nar_hash(&at)
            .expect("a directory's NAR hash is stored");
        assert_eq!(
            hash.to_string(HashFormat::Sri, true).as_bytes().as_bstr(),
            nix.stdout.trim_end().as_bstr(),
            "{:?}",
            rel.as_bstr()
        );
        assert_eq!(
            brix_store::nar::hash(&fs, &at, HashAlgo::Sha256).unwrap(),
            (hash, size),
            "{:?}",
            rel.as_bstr()
        );
        if rel.is_empty() {
            assert_eq!(image.nar_hash(), hash);
            assert_eq!(image.nar_size(), size);
        }
    }
}

/// A directory's names must be sorted as unsigned bytes, the order of a
/// NAR (Nix's `std::map<std::string, …>`): `z` (0x7a) before `\xe9` is
/// sound, as the writer emits it; `\xe9` before `z`, the order of a
/// signed compare, is damage, found when the directory is first reached
/// and by `verify`.
#[test]
fn directory_names_are_sorted_as_unsigned_bytes() {
    let dir = byte_tree();
    let ctx = context();
    let clean = fixture_image(dir.path(), &ctx, false);
    let s = dir_index(&clean, 0, "s");
    let names: Vec<&[u8]> = clean.dirs[s]
        .entries
        .iter()
        .map(|e| e.name.as_slice())
        .collect();
    assert_eq!(names, [b"z".as_slice(), b"\xe9".as_slice()]);
    let image = Arc::new(open(&clean, &ctx).unwrap());
    let listed: Vec<BString> = ImageAccessor::new(image.clone())
        .read_dir(&p("/s"))
        .unwrap()
        .into_keys()
        .collect();
    assert_eq!(listed, [b"z".as_slice(), b"\xe9".as_slice()]);
    image
        .verify()
        .expect("an image sorted as unsigned bytes verifies");

    let mut swapped = fixture_image(dir.path(), &ctx, false);
    swapped.dirs[s].entries.swap(0, 1);
    let expected =
        format!("corrupt brix image: directory tree: directory {s} is not sorted at \"z\"");
    let image = Arc::new(open(&swapped, &ctx).unwrap());
    let refused = ImageAccessor::new(image.clone())
        .read_dir(&p("/s"))
        .unwrap_err()
        .message_bytes();
    assert_eq!(
        refused.as_bstr(),
        format!(
            "compiled image: {expected}; run with --no-compiled-inputs to evaluate from source"
        )
        .as_bytes()
        .as_bstr()
    );
    assert!(image.damage().is_some(), "the damage is not kept");
    assert_eq!(
        open(&swapped, &ctx)
            .unwrap()
            .verify()
            .unwrap_err()
            .to_string(),
        expected
    );
    // Shown escaped when a name is not UTF-8 itself.
    let mut swapped = fixture_image(dir.path(), &ctx, false);
    swapped.dirs[s].entries[1].name = b"a\xe9".to_vec();
    assert_eq!(
        open(&swapped, &ctx)
            .unwrap()
            .verify()
            .unwrap_err()
            .to_string(),
        format!("corrupt brix image: directory tree: directory {s} is not sorted at \"a\\xe9\"")
    );
}

/// An image of the previous format (version 10, whose names were UTF-8
/// strings) is refused by its version, before anything else is read, so
/// a cache compiles the input again.
#[test]
fn version_10_images_are_refused() {
    let dir = byte_tree();
    let ctx = context();
    let bytes = compile_tree(
        &FsAccessor::new(dir.path()),
        &CanonPath::root(),
        &ctx,
        &CompileOptions::default(),
    )
    .unwrap();
    assert_eq!(format::FORMAT_VERSION, 11);
    let mut old = bytes.to_vec();
    old[8..12].copy_from_slice(&10u32.to_le_bytes());
    match Image::from_vec(old, Some(&ctx.fingerprint)) {
        Err(ImageError::FormatVersion { found, supported }) => {
            assert_eq!((found, supported), (10, 11))
        }
        other => panic!("a version-10 image: {other:?}"),
    }
}
