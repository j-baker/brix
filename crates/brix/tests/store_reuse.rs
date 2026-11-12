//! A locked input whose tree already exists under the store directory is
//! read from there, never fetched: the `fetchToStore` shortcut Nix takes
//! when `isValidPath` says yes.

use std::path::Path;

use brix::{Config, FetchConfig, Format, StoreDir};
use brix_flake::{FetchSettings, Input};
use brix_store::{nar, CanonPath, FsAccessor, HashAlgo, HashFormat};

/// The tree the lock will point at.
fn write_tree(dir: &Path) -> String {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join("hello"), "from the store\n").unwrap();
    let (hash, _) = nar::hash(&FsAccessor::new(dir), &CanonPath::root(), HashAlgo::Sha256).unwrap();
    hash.to_string(HashFormat::Sri, true)
}

#[test]
fn locked_input_present_in_the_store_is_not_fetched() {
    let tmp = tempfile::tempdir().unwrap();
    let store_root = tmp.path().join("store");
    let store_dir = StoreDir::new(store_root.to_str().unwrap());
    let staging = tmp.path().join("staging");
    let nar_hash = write_tree(&staging);

    // The URL does not exist: any attempt to fetch fails loudly.
    let url = "file:///nonexistent/brix-store-reuse.tar.gz";
    let locked = Input::from_attrs(
        &FetchSettings::default(),
        [
            (
                "type".to_owned(),
                brix_flake::Attr::String("tarball".into()),
            ),
            ("url".to_owned(), brix_flake::Attr::String(url.into())),
            (
                "narHash".to_owned(),
                brix_flake::Attr::String(nar_hash.clone()),
            ),
        ]
        .into_iter()
        .collect(),
    )
    .unwrap();
    let store_path = brix_flake::fetch::compute_store_path(&store_dir, &locked).unwrap();
    std::fs::create_dir_all(&store_root).unwrap();
    std::fs::rename(&staging, store_dir.display(&store_path)).unwrap();

    let flake = tmp.path().join("flake");
    std::fs::create_dir_all(&flake).unwrap();
    std::fs::write(
        flake.join("flake.nix"),
        format!(
            r#"{{
  inputs.foo = {{ url = "tarball+{url}"; flake = false; }};
  outputs = {{ self, foo }}: {{ x = builtins.readFile "${{foo}}/hello"; out = foo.outPath; }};
}}
"#
        ),
    )
    .unwrap();
    std::fs::write(
        flake.join("flake.lock"),
        format!(
            r#"{{
  "nodes": {{
    "foo": {{
      "flake": false,
      "locked": {{
        "narHash": "{nar_hash}",
        "type": "tarball",
        "url": "{url}"
      }},
      "original": {{
        "type": "tarball",
        "url": "{url}"
      }}
    }},
    "root": {{
      "inputs": {{
        "foo": "foo"
      }}
    }}
  }},
  "root": "root",
  "version": 7
}}
"#
        ),
    )
    .unwrap();

    let config = Config {
        eval: brix_eval::Config {
            store_dir: store_dir.clone(),
            ..brix_eval::Config::default()
        },
        fetch: FetchConfig {
            cache_dir: Some(tmp.path().join("cache")),
            ..FetchConfig::default()
        },
        ..Config::default()
    };
    let session = brix::Session::new(config).unwrap();
    let flake_ref = session
        .parse_flake_ref(&format!("path:{}", flake.display()), "/")
        .unwrap();
    let loaded = session
        .flake(&flake_ref)
        .expect("the lock's tree is in the store");
    assert_eq!(
        loaded.eval_attr_to_string("x", Format::Raw).unwrap(),
        "from the store\n"
    );
    assert_eq!(
        loaded.eval_attr_to_string("out", Format::Raw).unwrap(),
        store_dir.display(&store_path)
    );

    // Without the tree in the store the same lock has to fetch, and the
    // fetch fails because the URL does not exist.
    std::fs::remove_dir_all(store_dir.display(&store_path)).unwrap();
    let session = brix::Session::new(brix::Config {
        eval: brix_eval::Config {
            store_dir,
            ..brix_eval::Config::default()
        },
        fetch: FetchConfig {
            cache_dir: Some(tmp.path().join("cache2")),
            ..FetchConfig::default()
        },
        ..Config::default()
    })
    .unwrap();
    // Reading the root flake needs no inputs; evaluating `x` does.
    let loaded = session.flake(&flake_ref).unwrap();
    let err = match loaded.eval_attr_to_string("x", Format::Raw) {
        Ok(_) => panic!("the lock's tree was removed from the store, yet nothing was fetched"),
        Err(e) => e.to_string(),
    };
    assert!(err.contains("nonexistent"), "{err}");
}

/// `readInvalidDerivation` on a store on this machine reads the `.drv`
/// file from the store directory, registered or not (Nix's
/// `LocalFSStore::getFSAccessor` without `requireValidPath`), asking the
/// daemon nothing; without the file it is "store path '…' does not
/// exist". `readDerivation` still asks the daemon whether it is valid.
#[test]
fn an_invalid_derivation_is_read_from_a_local_store_directory() {
    let tmp = tempfile::tempdir().unwrap();
    let store_root = tmp.path().join("store");
    std::fs::create_dir_all(&store_root).unwrap();
    let mut config = Config::default();
    config.eval.store_dir = StoreDir::new(store_root.to_str().unwrap());
    config.daemon = Some("unix:///nonexistent/brix-invalid-drv.sock".into());
    config.fetch = FetchConfig {
        enabled: false,
        ..FetchConfig::default()
    };
    let store_dir = config.store_dir().clone();
    let expr = r#"(derivation { name = "x"; system = "x"; builder = "/b"; }).drvPath"#;
    let evaluated = brix::Session::new(config.clone()).unwrap();
    let drv_path = evaluated
        .eval_expr_to_string(expr, "/", Format::Raw)
        .unwrap();
    let path = store_dir.parse(&drv_path).unwrap();
    let drv = evaluated.derivation(&path).unwrap();

    // A session that did not evaluate it, before and after the file is
    // there.
    let session = brix::Session::new(config).unwrap();
    assert_eq!(
        session
            .read_invalid_derivation(&path)
            .unwrap_err()
            .to_string(),
        format!("store path '{drv_path}' does not exist")
    );
    // Something that is no file there is refused as Nix refuses it.
    std::fs::create_dir(&drv_path).unwrap();
    assert_eq!(
        session
            .read_invalid_derivation(&path)
            .unwrap_err()
            .to_string(),
        format!("'{drv_path}' is not a regular file")
    );
    std::fs::remove_dir(&drv_path).unwrap();
    std::fs::write(&drv_path, drv.to_aterm(&store_dir).unwrap()).unwrap();
    assert_eq!(session.read_invalid_derivation(&path).unwrap(), drv);
    assert_eq!(
        session.read_derivation(&path).unwrap_err().to_string(),
        "cannot connect to socket at '/nonexistent/brix-invalid-drv.sock': No such file or directory"
    );
}
