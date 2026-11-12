//! `import`: each file is evaluated once however it is named (through a
//! symlink, as a directory, by another path value with the same text),
//! and a path is never mistaken for another one with the same text in a
//! different tree.

use std::cell::RefCell;
use std::sync::Arc;

use brix_eval::{Arena, Config, Evaluator, Host};
use brix_store::{CanonPath, MemoryAccessor, MountedAccessor, SourceAccessor};
use bstr::{BString, ByteSlice};

const MOUNT: &str = "/nix/store/0000000000000000000000000000000z-imports";

fn p(s: &str) -> CanonPath {
    CanonPath::new(s).unwrap()
}

/// A tree of Nix files, symlinks included, mounted at `MOUNT`, with
/// `/fetchurl.nix` on the root file system next to it.
struct ImportHost {
    root: Arc<dyn SourceAccessor>,
    traces: RefCell<Vec<String>>,
}

impl ImportHost {
    fn new() -> ImportHost {
        let mut tree = MemoryAccessor::new();
        let mut file = |path: &str, text: &str| tree.add_file(&p(path), text, false).unwrap();
        file("/a.nix", r#"builtins.trace "a" { x = 1; }"#);
        file("/dir/default.nix", r#"builtins.trace "dir" 2"#);
        file("/real/b.nix", r#"builtins.trace "b" (import ./c.nix)"#);
        file("/real/c.nix", r#""c beside the target""#);
        file("/sub/c.nix", r#""c beside the link""#);
        file("/self.nix", "import ./self.nix");
        tree.add_symlink(&p("/link.nix"), "a.nix").unwrap();
        tree.add_symlink(&p("/dirlink"), "dir").unwrap();
        tree.add_symlink(&p("/sub/link.nix"), "../real/b.nix")
            .unwrap();

        let mut top = MemoryAccessor::new();
        top.add_file(&p("/fetchurl.nix"), r#""fetchurl.nix on the root""#, false)
            .unwrap();
        let mut root = MountedAccessor::new(Arc::new(top));
        root.mount(p(MOUNT), Arc::new(tree));
        ImportHost {
            root: Arc::new(root),
            traces: RefCell::new(Vec::new()),
        }
    }
}

impl Host for ImportHost {
    fn root_accessor(&self) -> Arc<dyn SourceAccessor> {
        self.root.clone()
    }

    fn allowed_paths(&self) -> Vec<CanonPath> {
        vec![p(MOUNT), p("/fetchurl.nix")]
    }

    fn trace(&self, msg: &[u8]) {
        self.traces
            .borrow_mut()
            .push(String::from_utf8(msg.to_vec()).expect("the traces in these tests are text"));
    }
}

/// The value of `expr` (relative paths resolve inside the tree), or the
/// error; the traces it printed; how many files it parsed.
fn eval(expr: &str) -> (Result<BString, BString>, Vec<String>, u64) {
    let host = ImportHost::new();
    let arena = Arena::new();
    let ev = Evaluator::new(&arena, Config::default(), &host);
    let result = ev
        .eval_str(expr.as_bytes(), MOUNT)
        .and_then(|v| {
            let cell = ev.alloc_cell(v);
            ev.force_deep(cell)?;
            ev.print_ambiguous(cell)
        })
        .map_err(|e| e.render(false));
    let parsed = ev.stats().files_parsed.get();
    drop(ev);
    (result, host.traces.into_inner(), parsed)
}

#[test]
fn a_file_is_evaluated_once_whatever_names_it() {
    let (value, traces, parsed) = eval(
        r#"[
          (import ./a.nix).x (import ./link.nix).x (import ./a.nix).x
          (import (./. + "/a.nix")).x (import "${toString ./.}/a.nix").x
          (import ./dir) (import ./dir/default.nix) (import ./dirlink) (import ./dir)
        ]"#,
    );
    assert_eq!(value.unwrap(), "[ 1 1 1 1 1 2 2 2 2 ]");
    assert_eq!(traces, ["a", "dir"]);
    // As many files parsed as by importing each file once.
    let (_, _, parsed_once) = eval("[ (import ./a.nix).x (import ./dir) ]");
    assert_eq!(parsed, parsed_once);
}

#[test]
fn a_symlinked_file_resolves_relative_paths_beside_its_target() {
    let (value, traces, _) =
        eval("[ (import ./sub/link.nix) (import ./real/b.nix) (import ./sub/link.nix) ]");
    assert_eq!(
        value.unwrap(),
        r#"[ "c beside the target" "c beside the target" "c beside the target" ]"#
    );
    assert_eq!(traces, ["b"]);
}

/// `<nix/fetchurl.nix>` is `/fetchurl.nix` in the evaluator's own tree of
/// built-in expressions: the same text as the root file system's
/// `/fetchurl.nix`, but another file.
#[test]
fn the_same_path_in_another_tree_is_another_file() {
    let (value, _, _) = eval(
        r#"[
          (builtins.typeOf (import <nix/fetchurl.nix>)) (import /fetchurl.nix)
          (builtins.typeOf (import <nix/fetchurl.nix>)) (import /fetchurl.nix)
        ]"#,
    );
    assert_eq!(
        value.unwrap(),
        r#"[ "lambda" "fetchurl.nix on the root" "lambda" "fetchurl.nix on the root" ]"#
    );
}

/// A failed import leaves nothing behind: importing the path again, from
/// the same evaluator, fails the same way.
#[test]
fn a_missing_file_fails_every_time_it_is_imported() {
    let host = ImportHost::new();
    let arena = Arena::new();
    let ev = Evaluator::new(&arena, Config::default(), &host);
    for _ in 0..2 {
        let err = ev
            .eval_str(b"import ./nope.nix", MOUNT)
            .map(|_| ())
            .unwrap_err()
            .render(false);
        assert!(
            err.contains_str(format!("path '{MOUNT}/nope.nix' does not exist")),
            "{err}"
        );
    }
}

#[test]
fn a_file_importing_itself_is_infinite_recursion() {
    let err = eval("import ./self.nix").0.unwrap_err();
    assert!(err.contains_str("infinite recursion encountered"), "{err}");
}
