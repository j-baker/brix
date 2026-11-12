//! Paths copied to the (virtual) store are readable at their store path,
//! as they are in Nix once `copyPathToStore` has written them: a filtered
//! copy (`builtins.path { filter = …; }`) shows only what the filter kept.

mod common;

use brix_eval::Config;
use bstr::ByteSlice;
use common::{run_brix_expr, Format, TestHost};

fn eval(host: &TestHost, expr: &str) -> String {
    let out = run_brix_expr(host, Config::default(), expr, "/src", Format::Plain);
    assert!(out.ok, "{}", out.stderr);
    out.stdout
        .trim_end()
        .to_str()
        .expect("brix prints UTF-8")
        .to_owned()
}

#[test]
fn copied_sources_are_readable_at_their_store_path() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("keep"), "K").unwrap();
    std::fs::write(dir.path().join("skip"), "S").unwrap();
    std::fs::create_dir(dir.path().join("sub")).unwrap();
    std::fs::write(dir.path().join("sub/inner"), "I").unwrap();
    let host = TestHost::new(&[("/src", dir.path())]);

    // An unfiltered copy: everything is there, served from the source.
    assert_eq!(
        eval(
            &host,
            r#"let s = "${/src}"; in [ (builtins.readFile "${s}/skip") (builtins.attrNames (builtins.readDir s)) ]"#
        ),
        r#"[ "S" [ "keep" "skip" "sub" ] ]"#
    );

    // A filtered copy: excluded entries do not exist at the store path.
    assert_eq!(
        eval(
            &host,
            r#"let s = builtins.path { path = /src; filter = p: t: baseNameOf p != "skip"; };
               in [ (builtins.readFile "${s}/keep") (builtins.pathExists "${s}/skip")
                    (builtins.attrNames (builtins.readDir s)) (builtins.readFile "${s}/sub/inner")
                    (builtins.pathExists "${s}/sub") ]"#
        ),
        r#"[ "K" false [ "keep" "sub" ] "I" true ]"#
    );

    // A filter that drops a directory drops everything below it.
    assert_eq!(
        eval(
            &host,
            r#"let s = builtins.filterSource (p: t: t != "directory") /src;
               in [ (builtins.attrNames (builtins.readDir s)) (builtins.pathExists "${s}/sub/inner") ]"#
        ),
        r#"[ [ "keep" "skip" ] false ]"#
    );
}

/// Copying a tree that already lives at its store path (nixpkgs'
/// `pkgs.path` is `builtins.path` over the input tree with a filter that
/// keeps everything) yields the same path, and reading through it must not
/// loop: brix serves copies from their source, so a copy whose source is
/// its own mount point must not be mounted again. This overflowed the
/// stack on a flake that imports `"${pkgs.path}/nixos/lib/eval-config.nix"`.
#[test]
fn copying_a_store_path_onto_itself_does_not_recurse() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("keep"), "K").unwrap();
    std::fs::create_dir(dir.path().join("sub")).unwrap();
    std::fs::write(dir.path().join("sub/inner"), "I").unwrap();
    let host = TestHost::new(&[("/src", dir.path())]);

    // The first copy is what a fetched input looks like: a tree mounted at
    // its store path. The second copies that store path with an identity
    // filter, as `pkgs.path` does, and lands on the same path.
    assert_eq!(
        eval(
            &host,
            r#"let s = "${/src}";
                   c = builtins.path { path = s; name = "src"; filter = _: _: true; };
                   d = builtins.path { path = "${s}/sub"; name = "sub"; };
               in [ (c == s) (builtins.readFile "${c}/keep") (builtins.readFile "${c}/sub/inner")
                    (builtins.attrNames (builtins.readDir c)) (builtins.readFile "${d}/inner")
                    (builtins.readFile "${builtins.path { path = c; name = "src"; }}/keep") ]"#
        ),
        r#"[ true "K" "I" [ "keep" "sub" ] "I" "K" ]"#
    );
}

/// Uploads must reproduce the filtered copy, including nested exclusions,
/// executable bits and symlinks, even when no real store contains it yet.
#[test]
fn filtered_sources_can_be_uploaded() {
    use brix_eval::managed_api::{GcConfig, GcEvaluator};
    use brix_eval::{Arena, EvalStoreView, Evaluator};
    use brix_store::{CanonPath, FsAccessor, StoreDir};
    use std::os::unix::fs::{symlink, PermissionsExt};

    let dir = tempfile::tempdir().unwrap();
    let expected = tempfile::tempdir().unwrap();
    for root in [dir.path(), expected.path()] {
        std::fs::write(root.join("keep"), "K").unwrap();
        std::fs::set_permissions(root.join("keep"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        std::fs::create_dir(root.join("sub")).unwrap();
        std::fs::write(root.join("sub/inner"), "I").unwrap();
        symlink("keep", root.join("link")).unwrap();
    }
    std::fs::write(dir.path().join("skip"), "S").unwrap();
    std::fs::write(dir.path().join("sub/skip"), "S").unwrap();
    std::fs::create_dir(dir.path().join("drop")).unwrap();
    std::fs::write(dir.path().join("drop/keep"), "excluded subtree").unwrap();
    let mut expected_nar = Vec::new();
    brix_store::nar::dump(
        &FsAccessor::new(expected.path()),
        &CanonPath::root(),
        &mut expected_nar,
    )
    .unwrap();
    let host = TestHost::new(&[("/src", dir.path())]);
    for expr in [
        r#"builtins.path { path = /src; filter = p: t: !(builtins.elem (baseNameOf p) [ "skip" "drop" ]); }"#,
        r#"builtins.filterSource (p: t: !(builtins.elem (baseNameOf p) [ "skip" "drop" ])) /src"#,
        r#"let s = builtins.filterSource (p: t: baseNameOf p != "drop") /src;
           in builtins.path { path = s; name = "nested"; filter = p: t: baseNameOf p != "skip"; }"#,
        r#"builtins.path { path = /src/keep; recursive = false; filter = _: _: true; }"#,
    ] {
        let arena = Arena::new();
        let mut ev = GcEvaluator::new(
            Evaluator::new(&arena, Config::default(), &host),
            GcConfig::default(),
        )
        .unwrap();
        let value = ev.eval_str(expr.as_bytes(), b"/src").unwrap();
        let path = ev.string_bytes(&value).unwrap();
        let path = StoreDir::default().parse(&path).unwrap();
        ev.collect().unwrap();
        // What the store is told before the NAR is sent is what is sent.
        let announced = ev.services().source_nar(&path).unwrap();
        let mut nar = Vec::new();
        ev.services().write_source_nar(&path, &mut nar).unwrap();
        assert_eq!(announced.size, nar.len() as u64, "{expr}");
        assert_eq!(
            announced.hash,
            brix_store::Hash::of(brix_store::HashAlgo::Sha256, &nar),
            "{expr}"
        );
        match announced.ca.method {
            brix_store::ContentAddressMethod::NixArchive => {
                assert_eq!(nar, expected_nar, "{expr}")
            }
            // A flat object is not executable, though its source is.
            _ => {
                let mut flat = Vec::new();
                brix_store::nar::dump_bytes(b"K", &mut flat).unwrap();
                assert_eq!(nar, flat, "{expr}")
            }
        }
    }
}
