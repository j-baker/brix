//! End-to-end evaluation of the nixpkgs checkout `$BRIX_NIXPKGS` through
//! the facade, compared with the reference `nix` on the same tree.

mod common;

use brix::{Format, StoreObject};
use brix_testutil::nixpkgs;
use common::{hello_drv, hello_drv_name, locked_nixpkgs_expr, nix, session};

fn path_flake_ref() -> String {
    format!("path:{}", nixpkgs())
}

#[test]
fn hello_drv_path_from_expression() {
    // Pure evaluation cannot read an unmounted store path directly; going
    // through `fetchTree` is what `nix eval --expr` needs too (and is how
    // the oracle value behind `hello_drv()` is obtained).
    let expr = format!(
        "(import {} {{ system = \"x86_64-linux\"; }}).hello.drvPath",
        locked_nixpkgs_expr()
    );
    let session = session();
    let got = session
        .eval_expr_to_string(&expr, "/", Format::Raw)
        .expect("evaluate hello.drvPath");
    assert_eq!(got, hello_drv());
}

#[test]
fn hello_drv_path_from_flake() {
    let session = session();
    let flake_ref = session.parse_flake_ref(&path_flake_ref(), "/").unwrap();
    let flake = session.flake(&flake_ref).expect("load the nixpkgs flake");
    let got = flake
        .eval_attr_to_string("legacyPackages.x86_64-linux.hello.drvPath", Format::Raw)
        .expect("evaluate hello.drvPath");
    assert_eq!(got, hello_drv());

    // The derivation is in the virtual store, with the same ATerm text as
    // the `.drv` Nix wrote when it evaluated the same thing (read through
    // Nix's store, which is a chroot store in the build sandbox).
    let drv_path = session.config().store_dir().parse(hello_drv()).unwrap();
    let drv = session
        .derivation(&drv_path)
        .expect("hello's derivation was recorded");
    assert_eq!(drv.name, hello_drv_name());
    let text = nix(&["store", "cat", hello_drv()]);
    assert_eq!(
        drv.to_aterm(session.config().store_dir()).unwrap(),
        text.as_bytes(),
        "ATerm text differs from the .drv in the real store"
    );
    let objects = session.store_objects();
    assert!(objects
        .iter()
        .any(|(p, o)| *p == drv_path && matches!(o, StoreObject::Derivation(_))));
    // Every derivation in the closure that brix evaluated is recorded too.
    let closure = session
        .derivation_closure(&[drv_path])
        .expect("evaluation recorded every derivation of the closure");
    assert!(
        closure.len() > 10,
        "expected the whole closure, got {}",
        closure.len()
    );
    assert!(closure.iter().all(|(p, _)| p.is_derivation()));
}

#[test]
fn lib_version_matches_nix() {
    let session = session();
    let flake_ref = session.parse_flake_ref(&path_flake_ref(), "/").unwrap();
    let flake = session.flake(&flake_ref).unwrap();
    let got = flake
        .eval_attr_to_string("lib.version", Format::Nix)
        .unwrap();
    let expected = nix(&["eval", &format!("{}#lib.version", path_flake_ref())]);
    assert_eq!(format!("{got}\n"), expected);
}

#[test]
fn owned_values() {
    let session = session();
    let v = session
        .eval_expr("{ a = 1; b = [ true null \"x\" ]; }", "/")
        .unwrap();
    let brix::Value::Attrs(attrs) = v else {
        panic!("expected attrs, got {v:?}")
    };
    assert_eq!(attrs[&b"a"[..]], brix::Value::Int(1));
    assert_eq!(
        attrs[&b"b"[..]],
        brix::Value::List(vec![
            brix::Value::Bool(true),
            brix::Value::Null,
            brix::Value::String {
                bytes: b"x".to_vec(),
                context: vec![]
            }
        ])
    );
}

#[test]
fn eval_errors_render_like_nix() {
    let session = session();
    let err = session
        .eval_expr_to_string("throw \"boom\"", "/", Format::Nix)
        .unwrap_err();
    assert_eq!(err.exit_status(), 1);
    assert!(
        err.render(false).starts_with("error:"),
        "{}",
        err.render(false)
    );
    assert!(err.render(false).contains("boom"));
}

#[test]
fn unlocked_get_flake_is_rejected() {
    let session = session();
    let err = session
        .eval_expr_to_string(
            "(builtins.getFlake \"github:NixOS/nixpkgs\").lib",
            "/",
            Format::Nix,
        )
        .unwrap_err();
    assert!(
        err.render(false)
            .contains("cannot call 'getFlake' on unlocked flake reference 'github:NixOS/nixpkgs'"),
        "{}",
        err.render(false)
    );
}

#[test]
fn parse_flake_ref_builtins() {
    let session = session();
    let got = session
        .eval_expr_to_string(
            "builtins.parseFlakeRef \"github:NixOS/nixpkgs/23.05?dir=lib\"",
            "/",
            Format::Nix,
        )
        .unwrap();
    assert_eq!(
        got,
        "{ dir = \"lib\"; owner = \"NixOS\"; ref = \"23.05\"; repo = \"nixpkgs\"; type = \"github\"; }"
    );
    let back = session
        .eval_expr_to_string(
            "builtins.flakeRefToString { dir = \"lib\"; owner = \"NixOS\"; ref = \"23.05\"; repo = \"nixpkgs\"; type = \"github\"; }",
            "/",
            Format::Raw,
        )
        .unwrap();
    assert_eq!(back, "github:NixOS/nixpkgs/23.05?dir=lib");
}

#[test]
fn unlocked_fetch_tree_is_rejected_in_pure_mode() {
    let session = session();
    let err = session
        .eval_expr_to_string(
            format!(
                "builtins.fetchTree {{ type = \"path\"; path = \"{}\"; }}",
                nixpkgs()
            ),
            "/",
            Format::Nix,
        )
        .unwrap_err();
    assert!(
        err.render(false).contains(&format!(
            "in pure evaluation mode, 'fetchTree' doesn't fetch unlocked input 'path:{}'",
            nixpkgs()
        )),
        "{}",
        err.render(false)
    );
}
