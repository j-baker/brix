//! Differential tests against the reference `nix` binary (`BRIX_NIX_BIN`).
//! Everything here computes a value both ways and compares.

mod common;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use brix_store::{
    nar, AccessorRegistry, BStr, CanonPath, ContentAddress, ContentAddressMethod, Derivation,
    DerivationOutput, FileIngestionMethod, FixedOutputInfo, FsAccessor, Hash, HashAlgo, HashFormat,
    PathIngestion, SourcePath, StoreDir, StorePath, StoreReferences, VirtualStore, Xp, XpSettings,
};
use brix_testutil::{
    join_bytes, nix_src, nixpkgs, os, scratch_store, symlink_bytes, write_bytes, Oracle,
};
use common::{make_sample_tree, nix, nix_bytes, nix_store_bytes, nix_store_status};

/// Enables BLAKE3 for the reference `nix`.
const BLAKE3_ON: [&str; 2] = ["--extra-experimental-features", "blake3-hashes"];

/// Enables BLAKE3 for brix.
fn blake3_on() -> XpSettings {
    XpSettings::NONE.with(Xp::Blake3Hashes)
}

#[test]
fn hash_conversions_match_nix_hash_convert() {
    let inputs: [&[u8]; 4] = [b"", b"abc", b"hello world\n", &[0u8, 255, 1, 254, 7]];
    for data in inputs {
        for algo in HashAlgo::ALL {
            let h = Hash::of(algo, data);
            let base16 = h.to_string(HashFormat::Base16, false);
            for (fmt, name) in [
                (HashFormat::Base16, "base16"),
                (HashFormat::Nix32, "nix32"),
                (HashFormat::Base64, "base64"),
                (HashFormat::Sri, "sri"),
            ] {
                let expected = nix(&[
                    BLAKE3_ON[0],
                    BLAKE3_ON[1],
                    "hash",
                    "convert",
                    "--hash-algo",
                    algo.name(),
                    "--to",
                    name,
                    &base16,
                ]);
                assert_eq!(
                    h.to_string(fmt, false),
                    expected,
                    "{algo} {name} of {data:?}"
                );
                // And every encoding parses back, prefixed or not.
                let ours = h.to_string(fmt, true);
                assert_eq!(Hash::parse_any_prefixed(&ours, blake3_on()).unwrap(), h);
                let back = nix(&[
                    BLAKE3_ON[0],
                    BLAKE3_ON[1],
                    "hash",
                    "convert",
                    "--to",
                    "base16",
                    &ours,
                ]);
                assert_eq!(back, base16);
            }
        }
    }
}

#[test]
fn hash_file_and_path_match_nix() {
    let dir = tempfile::tempdir().unwrap();
    make_sample_tree(dir.path());
    let file = dir.path().join("sub/deeper/data.bin");
    let file_s = file.to_str().unwrap();
    for algo in HashAlgo::ALL {
        let expected = nix(&[
            BLAKE3_ON[0],
            BLAKE3_ON[1],
            "hash",
            "file",
            "--type",
            algo.name(),
            "--base16",
            file_s,
        ]);
        let ours = Hash::of(algo, &std::fs::read(&file).unwrap());
        assert_eq!(ours.to_string(HashFormat::Base16, false), expected);
    }
    let accessor = FsAccessor::new(dir.path());
    for algo in HashAlgo::ALL {
        let expected = nix(&[
            BLAKE3_ON[0],
            BLAKE3_ON[1],
            "hash",
            "path",
            "--type",
            algo.name(),
            "--sri",
            dir.path().to_str().unwrap(),
        ]);
        let (ours, _) = nar::hash(&accessor, &CanonPath::root(), algo).unwrap();
        assert_eq!(ours.to_string(HashFormat::Sri, true), expected);
    }
}

#[test]
fn nar_dump_is_byte_identical_to_nix_store_dump() {
    let dir = tempfile::tempdir().unwrap();
    make_sample_tree(dir.path());
    let expected = nix_store_bytes(&["--dump", dir.path().to_str().unwrap()]);
    let accessor = FsAccessor::new(dir.path());
    let mut ours = Vec::new();
    nar::dump(&accessor, &CanonPath::root(), &mut ours).unwrap();
    assert_eq!(ours, expected);
    // A single file and a symlink dump too.
    for rel in ["sub/run.sh", "zero", "link", "empty"] {
        let expected = nix_store_bytes(&["--dump", dir.path().join(rel).to_str().unwrap()]);
        let mut ours = Vec::new();
        nar::dump(&accessor, &CanonPath::new(rel).unwrap(), &mut ours).unwrap();
        assert_eq!(ours, expected, "{rel}");
    }
    // And Nix's own NAR parses into the same tree we would dump.
    let parsed = nar::parse(&mut expected.as_slice()).unwrap();
    let mut again = Vec::new();
    nar::dump(&parsed, &CanonPath::root(), &mut again).unwrap();
    assert_eq!(again, expected);
}

#[test]
fn store_paths_match_nix_store_add() {
    let dir = tempfile::tempdir().unwrap();
    make_sample_tree(dir.path());
    let mut registry = AccessorRegistry::new();
    let id = registry.register(Arc::new(FsAccessor::new(dir.path())));
    let mut store = VirtualStore::default();
    let cases = [
        (
            "nar",
            "src",
            ContentAddressMethod::NixArchive,
            HashAlgo::Sha256,
            ".",
        ),
        (
            "nar",
            "src-sha1",
            ContentAddressMethod::NixArchive,
            HashAlgo::Sha1,
            ".",
        ),
        (
            "nar",
            "src-sha512",
            ContentAddressMethod::NixArchive,
            HashAlgo::Sha512,
            ".",
        ),
        (
            "flat",
            "b.txt",
            ContentAddressMethod::Flat,
            HashAlgo::Sha256,
            "b.txt",
        ),
        (
            "flat",
            "data",
            ContentAddressMethod::Flat,
            HashAlgo::Md5,
            "sub/deeper/data.bin",
        ),
        (
            "text",
            "t",
            ContentAddressMethod::Text,
            HashAlgo::Sha256,
            "b.txt",
        ),
    ];
    for (mode, name, method, algo, rel) in cases {
        let fs_path = dir.path().join(rel);
        let expected = nix(&[
            "store",
            "add",
            "--dry-run",
            "--mode",
            mode,
            "--hash-algo",
            algo.name(),
            "--name",
            name,
            fs_path.to_str().unwrap(),
        ]);
        let source = SourcePath::new(id, CanonPath::new(rel).unwrap());
        let ingestion = PathIngestion {
            method,
            algo,
            references: BTreeSet::new(),
        };
        let ours = store
            .add_path(&registry, &source, name, &ingestion, None)
            .unwrap();
        assert_eq!(store.store_dir().display(&ours), expected, "{mode} {name}");
    }
}

#[test]
fn to_file_matches_nix() {
    let scratch = scratch_store();
    let store_uri = scratch.path().to_str().expect("UTF-8 temporary directory");
    let expected = nix(&[
        "eval",
        "--store",
        store_uri,
        "--raw",
        "--expr",
        r#"builtins.toFile "foo" "bar""#,
    ]);
    let mut store = VirtualStore::default();
    let ours = store.add_text("foo", b"bar", BTreeSet::new()).unwrap();
    assert_eq!(store.store_dir().display(&ours), expected);
    // With a reference (Nix forbids self-references in toFile, so one
    // text referring to another).
    let expected = nix(&[
        "eval",
        "--store",
        store_uri,
        "--raw",
        "--expr",
        r#"builtins.toFile "outer" "see ${builtins.toFile "inner" "x"}""#,
    ]);
    let inner = store.add_text("inner", b"x", BTreeSet::new()).unwrap();
    let contents = format!("see {}", store.store_dir().display(&inner));
    let outer = store
        .add_text("outer", contents.as_bytes(), BTreeSet::from([inner]))
        .unwrap();
    assert_eq!(store.store_dir().display(&outer), expected);
}

#[test]
fn ad_hoc_derivation_paths_match_nix_eval() {
    // Nothing here is a committed fixture: a fresh name each run guards
    // against accidentally hard-coded values.
    let name = format!("adhoc-{}", std::process::id());
    let expr = format!(
        r#"let a = derivation {{ name = "{name}-a"; system = "aarch64-linux"; builder = "/bin/sh"; args = [ "-c" "x" ]; }};
             b = derivation {{ name = "{name}"; system = "aarch64-linux"; builder = "/bin/sh"; dep = a; outputs = [ "out" "lib" ]; }};
         in {{ aDrv = a.drvPath; aOut = a.outPath; bDrv = b.drvPath; bOut = b.outPath; bLib = b.lib.outPath; }}"#
    );
    let scratch = scratch_store();
    let store_uri = scratch.path().to_str().expect("UTF-8 temporary directory");
    let json: serde_json::Value = serde_json::from_str(&nix(&[
        "eval", "--store", store_uri, "--json", "--expr", &expr,
    ]))
    .unwrap();
    let store_dir = StoreDir::default();
    let mut store = VirtualStore::default();
    let mut a = Derivation {
        name: format!("{name}-a"),
        platform: "aarch64-linux".into(),
        builder: "/bin/sh".into(),
        args: vec!["-c".into(), "x".into()],
        ..Default::default()
    };
    a.env.insert("builder".into(), "/bin/sh".into());
    a.env.insert("name".into(), a.name.as_str().into());
    a.env.insert("out".into(), "".into());
    a.env.insert("system".into(), "aarch64-linux".into());
    a.outputs.insert("out".into(), DerivationOutput::Deferred);
    store.fill_in_output_paths(&mut a).unwrap();
    let a_out = a.outputs[b"out".as_slice()]
        .path(&store_dir, &a.name, b"out")
        .unwrap()
        .unwrap();
    assert_eq!(store_dir.display(&a_out), json["aOut"].as_str().unwrap());
    let a_drv = store.add_derivation(a).unwrap();
    assert_eq!(store_dir.display(&a_drv), json["aDrv"].as_str().unwrap());

    let mut b = Derivation {
        name: name.clone(),
        platform: "aarch64-linux".into(),
        builder: "/bin/sh".into(),
        ..Default::default()
    };
    b.env.insert("builder".into(), "/bin/sh".into());
    b.env.insert("dep".into(), store_dir.display(&a_out).into());
    b.env.insert("lib".into(), "".into());
    b.env.insert("name".into(), name.as_str().into());
    b.env.insert("out".into(), "".into());
    b.env.insert("outputs".into(), "out lib".into());
    b.env.insert("system".into(), "aarch64-linux".into());
    b.input_drvs
        .insert(a_drv, BTreeSet::from([bstr::BString::from("out")]));
    b.outputs.insert("out".into(), DerivationOutput::Deferred);
    b.outputs.insert("lib".into(), DerivationOutput::Deferred);
    store.fill_in_output_paths(&mut b).unwrap();
    let b_out = b.outputs[b"out".as_slice()]
        .path(&store_dir, &name, b"out")
        .unwrap()
        .unwrap();
    let b_lib = b.outputs[b"lib".as_slice()]
        .path(&store_dir, &name, b"lib")
        .unwrap()
        .unwrap();
    assert_eq!(store_dir.display(&b_out), json["bOut"].as_str().unwrap());
    assert_eq!(store_dir.display(&b_lib), json["bLib"].as_str().unwrap());
    let b_drv = store.add_derivation(b).unwrap();
    assert_eq!(store_dir.display(&b_drv), json["bDrv"].as_str().unwrap());
}

#[test]
fn placeholders_match_nix() {
    for out in ["out", "dev", "lib"] {
        let expected = nix(&[
            "eval",
            "--raw",
            "--expr",
            &format!(r#"builtins.placeholder "{out}""#),
        ]);
        assert_eq!(brix_store::hash_placeholder(out.as_bytes()), expected);
    }
}

#[test]
fn nar_parse_handles_nix_dump_of_single_file() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("f"), "contents").unwrap();
    let dump = nix_bytes(&["nar", "pack", dir.path().join("f").to_str().unwrap()]);
    let parsed = nar::parse(&mut dump.as_slice()).unwrap();
    assert_eq!(
        parsed.lookup(&CanonPath::root()),
        Some(&brix_store::accessor::MemoryNode::Regular {
            executable: false,
            contents: b"contents".to_vec()
        })
    );
}

/// The nixpkgs tree's NAR hash must match what `nix hash path` computes,
/// and the fixed-output path derived from it must be where the tree
/// lives (and what `nix store add` would give it). Large (hundreds of
/// MB), so ignored by default: `cargo test -p brix-store -- --ignored nixpkgs`.
#[test]
#[ignore = "hashes the whole nixpkgs checkout; run on demand"]
fn nixpkgs_tree_nar_hash_matches_nix() {
    let path = nixpkgs();
    let accessor = FsAccessor::new(path);
    let start = std::time::Instant::now();
    let (h, size) = nar::hash(&accessor, &CanonPath::root(), HashAlgo::Sha256).unwrap();
    eprintln!("hashed {size} NAR bytes in {:?}", start.elapsed());
    assert_eq!(
        h.to_string(HashFormat::Sri, true),
        nix(&["hash", "path", "--sri", path])
    );
    let store = StoreDir::default();
    let expected = nix(&[
        "store",
        "add",
        "--dry-run",
        "--mode",
        "nar",
        "--name",
        "source",
        path,
    ]);
    let p = store
        .make_fixed_output_path(
            "source",
            &brix_store::FixedOutputInfo {
                method: brix_store::FileIngestionMethod::NixArchive,
                hash: h,
                references: Default::default(),
            },
        )
        .unwrap();
    assert_eq!(store.display(&p), expected);
    // The checkout is that very fixed-output path in the store.
    assert_eq!(store.display(&p), path);
}

/// sha256 of `abc`, as an SRI hash.
const SHA256_ABC_SRI: &str = "sha256-ungWv48Bz+pBQUDeXa4iI7ADYaOWF3qctBD/YfIAFa0=";

/// What `builtins.derivation` makes of `attrs`, all strings and among
/// them `name`, `system` and `builder`, before its output paths are
/// known: every attribute in the environment, and the one output `out`,
/// deferred or fixed by `fixed`.
fn unfilled(attrs: &[(&str, &str)], fixed: Option<ContentAddress>) -> Derivation {
    let attr = |name: &str| {
        attrs
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, v)| (*v).to_owned())
            .expect("name, system and builder are given")
    };
    let mut drv = Derivation {
        name: attr("name"),
        platform: attr("system").into(),
        builder: attr("builder").into(),
        ..Default::default()
    };
    for (k, v) in attrs {
        drv.env.insert((*k).into(), (*v).into());
    }
    drv.env.insert("out".into(), "".into());
    let output = match fixed {
        Some(ca) => DerivationOutput::CAFixed(ca),
        None => DerivationOutput::Deferred,
    };
    drv.outputs.insert("out".into(), output);
    drv
}

/// `attrs` as a Nix attribute set of strings.
fn nix_attrs(attrs: &[(&str, &str)]) -> String {
    let body: String = attrs
        .iter()
        .map(|(k, v)| format!("{k} = \"{v}\"; "))
        .collect();
    format!("{{ {body}}}")
}

/// Fills in `drv`'s output paths and adds it to `store`, as
/// `derivationStrict` does: its `.drv` path and its output path.
fn add(store: &mut VirtualStore, mut drv: Derivation) -> (StorePath, StorePath) {
    store.fill_in_output_paths(&mut drv).unwrap();
    let out = drv.outputs[b"out".as_slice()]
        .path(store.store_dir(), &drv.name, b"out")
        .unwrap()
        .expect("the output path is known");
    (store.add_derivation(drv).unwrap(), out)
}

/// The sha256 of the NAR of a non-executable file holding `text`.
fn nar_sha256(text: &str) -> Hash {
    let mut nar = Vec::new();
    nar::dump_bytes(text.as_bytes(), &mut nar).unwrap();
    Hash::of(HashAlgo::Sha256, &nar)
}

#[test]
fn paths_under_a_non_default_store_dir_match_nix() {
    let uri = "dummy://?store=/foo/store";
    let store_dir = StoreDir::parse_setting("/foo/store").unwrap();
    let mut store = VirtualStore::new(store_dir.clone(), XpSettings::NONE);
    let shown = |p: &StorePath| store_dir.display(p);

    // A text file.
    let expected = nix(&[
        "eval",
        "--store",
        uri,
        "--raw",
        "--expr",
        r#"builtins.toFile "foo" "bar""#,
    ]);
    let text = store.add_text("foo", b"bar", BTreeSet::new()).unwrap();
    assert_eq!(shown(&text), expected);

    // A plain derivation, a fixed-output one, and one using both.
    let plain = [
        ("name", "plain"),
        ("system", "x86_64-linux"),
        ("builder", "/bin/sh"),
    ];
    let fixed = [
        ("name", "fixed"),
        ("system", "x86_64-linux"),
        ("builder", "/bin/sh"),
        ("outputHash", SHA256_ABC_SRI),
        ("outputHashMode", "recursive"),
    ];
    let expr = format!(
        r#"let plain = derivation {}; fixed = derivation {};
               uses = derivation {{ name = "uses"; system = "x86_64-linux"; builder = "/bin/sh"; a = plain; b = fixed; }};
           in map (d: [ d.drvPath d.outPath ]) [ plain fixed uses ]"#,
        nix_attrs(&plain),
        nix_attrs(&fixed)
    );
    let expected: serde_json::Value =
        serde_json::from_str(&nix(&["eval", "--store", uri, "--json", "--expr", &expr])).unwrap();
    let (plain_drv, plain_out) = add(&mut store, unfilled(&plain, None));
    let abc = ContentAddress {
        method: ContentAddressMethod::NixArchive,
        hash: Hash::of(HashAlgo::Sha256, b"abc"),
    };
    let (fixed_drv, fixed_out) = add(&mut store, unfilled(&fixed, Some(abc)));
    let mut uses = unfilled(
        &[
            ("name", "uses"),
            ("system", "x86_64-linux"),
            ("builder", "/bin/sh"),
        ],
        None,
    );
    uses.env.insert("a".into(), shown(&plain_out).into());
    uses.env.insert("b".into(), shown(&fixed_out).into());
    for d in [&plain_drv, &fixed_drv] {
        uses.input_drvs
            .insert(d.clone(), BTreeSet::from([bstr::BString::from("out")]));
    }
    let (uses_drv, uses_out) = add(&mut store, uses);
    let ours = serde_json::json!([
        [shown(&plain_drv), shown(&plain_out)],
        [shown(&fixed_drv), shown(&fixed_out)],
        [shown(&uses_drv), shown(&uses_out)],
    ]);
    assert_eq!(ours, expected);

    // Source paths.
    let dir = tempfile::tempdir().unwrap();
    make_sample_tree(dir.path());
    let mut registry = AccessorRegistry::new();
    let id = registry.register(Arc::new(FsAccessor::new(dir.path())));
    for (mode, method, rel) in [
        ("nar", ContentAddressMethod::NixArchive, "."),
        ("flat", ContentAddressMethod::Flat, "b.txt"),
        ("text", ContentAddressMethod::Text, "b.txt"),
    ] {
        let expected = nix(&[
            "store",
            "add",
            "--dry-run",
            "--store",
            uri,
            "--mode",
            mode,
            "--name",
            "src",
            dir.path().join(rel).to_str().unwrap(),
        ]);
        let ingestion = PathIngestion {
            method,
            algo: HashAlgo::Sha256,
            references: BTreeSet::new(),
        };
        let source = SourcePath::new(id, CanonPath::new(rel).unwrap());
        let ours = store
            .add_path(&registry, &source, "src", &ingestion, None)
            .unwrap();
        assert_eq!(shown(&ours), expected, "{mode}");
    }
}

/// `nix store make-content-addressed` turns paths into `fixed:r:sha256`
/// ones that keep their references, a self reference included, which no
/// expression can produce. The paths are registered by hand in a chroot
/// store with the references wanted.
#[test]
fn fixed_output_paths_with_references_match_nix() {
    let scratch = scratch_store();
    let root = scratch.path();
    let uri = root.to_str().expect("UTF-8 temporary directory");
    std::fs::create_dir_all(root.join("nix/store")).unwrap();
    let old = |n: u8, name: &str| format!("/nix/store/{}{n}-{name}", "0".repeat(31));
    let (other, refers, selfish) = (old(1, "other"), old(2, "refers"), old(3, "selfish"));
    let selfish_text = "nothing refers to anything in here";
    for (path, text) in [
        (&other, "other".to_owned()),
        (&refers, format!("see {other}")),
        (&selfish, selfish_text.to_owned()),
    ] {
        std::fs::write(root.join(&path[1..]), text).unwrap();
    }
    // Path, deriver, number of references, references.
    let registration =
        format!("{other}\n\n0\n{refers}\n\n1\n{other}\n{selfish}\n\n2\n{other}\n{selfish}\n");
    let (ok, stderr) = nix_store_status(
        &["--store", uri, "--register-validity"],
        registration.as_bytes(),
    );
    assert!(ok, "registering the paths failed:\n{stderr}");
    let rewrites: serde_json::Value = serde_json::from_str(&nix(&[
        "store",
        "make-content-addressed",
        "--store",
        uri,
        "--json",
        &other,
        &refers,
        &selfish,
    ]))
    .unwrap();
    let rewritten = |p: &str| rewrites["rewrites"][p].as_str().unwrap().to_owned();

    let store = StoreDir::default();
    let fixed = |name: &str, hash: Hash, others: &[&StorePath], self_reference: bool| {
        let info = FixedOutputInfo {
            method: FileIngestionMethod::NixArchive,
            hash,
            references: StoreReferences {
                others: others.iter().map(|&p| p.clone()).collect(),
                self_reference,
            },
        };
        store.make_fixed_output_path(name, &info).unwrap()
    };
    let other2 = fixed("other", nar_sha256("other"), &[], false);
    assert_eq!(store.display(&other2), rewritten(&other));
    // The reference in the text is rewritten before hashing.
    let refers_text = format!("see {}", store.display(&other2));
    let refers2 = fixed("refers", nar_sha256(&refers_text), &[&other2], false);
    assert_eq!(store.display(&refers2), rewritten(&refers));
    let selfish2 = fixed("selfish", nar_sha256(selfish_text), &[&other2], true);
    assert_eq!(store.display(&selfish2), rewritten(&selfish));

    // `nix path-info` shows their content addresses, rendered and as
    // JSON; a text file's too.
    let text_path = nix(&[
        "eval",
        "--store",
        uri,
        "--raw",
        "--expr",
        r#"builtins.toFile "t" "x""#,
    ]);
    for (path, ca) in [
        (store.display(&other2), nar_sha256("other")),
        (store.display(&refers2), nar_sha256(&refers_text)),
        (store.display(&selfish2), nar_sha256(selfish_text)),
        (text_path, Hash::of(HashAlgo::Sha256, b"x")),
    ]
    .map(|(path, hash)| {
        let method = if path.ends_with("-t") {
            ContentAddressMethod::Text
        } else {
            ContentAddressMethod::NixArchive
        };
        (path, ContentAddress { method, hash })
    }) {
        let v1: serde_json::Value = serde_json::from_str(&nix(&[
            "path-info",
            "--store",
            uri,
            "--json",
            "--json-format",
            "1",
            &path,
        ]))
        .unwrap();
        let rendered = v1[&path]["ca"].as_str().unwrap();
        assert_eq!(ca.render(), rendered, "{path}");
        assert_eq!(
            ContentAddress::parse(rendered, XpSettings::NONE).unwrap(),
            ca
        );
        let v2: serde_json::Value = serde_json::from_str(&nix(&[
            "path-info",
            "--store",
            uri,
            "--json",
            "--json-format",
            "2",
            &path,
        ]))
        .unwrap();
        let base = path.rsplit('/').next().unwrap();
        let json = &v2["info"][base]["ca"];
        assert_eq!(json["method"], ca.method.render(), "{path}");
        assert_eq!(json["hash"], ca.hash.to_string(HashFormat::Sri, true));
    }
}

/// Vectors from Nix's own unit-test data (`src/libstore-tests/data`), for
/// what the command line does not show directly.
#[test]
fn content_addresses_and_self_references_match_nix_test_data() {
    let data = nix_src().join("src/libstore-tests/data");
    let read_json = |rel: &str| -> serde_json::Value {
        serde_json::from_slice(&std::fs::read(data.join(rel)).unwrap()).unwrap()
    };
    let store = StoreDir::default();

    // path-info.cc `makeFullKeyed`: `makeFromCA` of "foo" with a NAR
    // hash of "(...)", a reference to "bar" and one to itself.
    let info = read_json("path-info/json-1/pure.json");
    let bar = StorePath::new("g1w7hy3qg1w7hy3qg1w7hy3qg1w7hy3q-bar").unwrap();
    let hash = Hash::of(HashAlgo::Sha256, b"(...)");
    let foo = store
        .make_fixed_output_path(
            "foo",
            &FixedOutputInfo {
                method: FileIngestionMethod::NixArchive,
                hash,
                references: StoreReferences {
                    others: BTreeSet::from([bar.clone()]),
                    self_reference: true,
                },
            },
        )
        .unwrap();
    assert_eq!(
        info["references"],
        serde_json::json!([store.display(&bar), store.display(&foo)])
    );
    let ca = ContentAddress {
        method: ContentAddressMethod::NixArchive,
        hash,
    };
    assert_eq!(info["ca"], ca.render());

    // common-protocol.cc `contentAddress`: the rendered forms in the
    // binary protocol, and the same addresses as JSON.
    let cas = [
        (
            ContentAddressMethod::Text,
            HashAlgo::Sha256,
            &b"Derive(...)"[..],
        ),
        (ContentAddressMethod::Flat, HashAlgo::Sha1, b"blob blob..."),
        (ContentAddressMethod::NixArchive, HashAlgo::Sha256, b"(...)"),
    ]
    .map(|(method, algo, data)| ContentAddress {
        method,
        hash: Hash::of(algo, data),
    });
    let bin = std::fs::read(data.join("common-protocol/content-address.bin")).unwrap();
    let mut rest = bin.as_slice();
    for ca in &cas {
        let (len, tail) = rest.split_at(8);
        let len = u64::from_le_bytes(len.try_into().unwrap()) as usize;
        let rendered = std::str::from_utf8(&tail[..len]).unwrap();
        assert_eq!(ca.render(), rendered);
        assert_eq!(
            &ContentAddress::parse(rendered, XpSettings::NONE).unwrap(),
            ca
        );
        rest = &tail[len.div_ceil(8) * 8..];
    }
    assert!(rest.is_empty());
    let json = read_json("common-protocol/content-address.json");
    for (ca, j) in cas.iter().zip(json.as_array().unwrap()) {
        assert_eq!(j["method"], ca.method.render());
        assert_eq!(j["hash"], ca.hash.to_string(HashFormat::Sri, true));
    }
}

#[test]
fn git_fixed_outputs_match_nix() {
    let xp = ["--extra-experimental-features", "git-hashing"];
    for (algo, hex) in [
        (HashAlgo::Sha1, "a9993e364706816aba3e25717850c26c9cd0d89d"),
        (
            HashAlgo::Sha256,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
        ),
    ] {
        let attrs = [
            ("name", "g"),
            ("system", "x86_64-linux"),
            ("builder", "/bin/sh"),
            ("outputHashMode", "git"),
            ("outputHashAlgo", algo.name()),
            ("outputHash", hex),
        ];
        let expr = format!(
            "let d = derivation {}; in [ d.drvPath d.outPath ]",
            nix_attrs(&attrs)
        );
        let expected: serde_json::Value = serde_json::from_str(&nix(&[
            xp[0], xp[1], "eval", "--store", "dummy://", "--json", "--expr", &expr,
        ]))
        .unwrap();
        let ca = ContentAddress {
            method: ContentAddressMethod::Git,
            hash: Hash::parse_non_sri_unprefixed(hex, algo, XpSettings::NONE).unwrap(),
        };
        let mut store = VirtualStore::default();
        let (drv, out) = add(&mut store, unfilled(&attrs, Some(ca)));
        let shown = |p: &StorePath| store.store_dir().display(p);
        assert_eq!(serde_json::json!([shown(&drv), shown(&out)]), expected);
    }
    // Nix's message for any other algorithm (`nix eval` of the same
    // derivation with md5).
    let md5 = FixedOutputInfo {
        method: FileIngestionMethod::Git,
        hash: Hash::of(HashAlgo::Md5, b""),
        references: StoreReferences::default(),
    };
    assert_eq!(
        StoreDir::default()
            .make_fixed_output_path("g", &md5)
            .unwrap_err()
            .to_string(),
        "Git file ingestion must use SHA-1 or SHA-256 hash, but instead using: md5"
    );
}

#[test]
fn blake3_fixed_outputs_match_nix() {
    let hash = Hash::of(HashAlgo::Blake3, b"abc");
    let base16 = hash.to_string(HashFormat::Base16, false);
    let sri = hash.to_string(HashFormat::Sri, true);
    for (attrs, method) in [
        (
            vec![
                ("outputHashAlgo", "blake3"),
                ("outputHash", base16.as_str()),
            ],
            ContentAddressMethod::Flat,
        ),
        (
            vec![
                ("outputHashMode", "recursive"),
                ("outputHash", sri.as_str()),
            ],
            ContentAddressMethod::NixArchive,
        ),
    ] {
        let mut attrs = attrs;
        attrs.extend([
            ("name", "b"),
            ("system", "x86_64-linux"),
            ("builder", "/bin/sh"),
        ]);
        let expr = format!(
            "let d = derivation {}; in [ d.drvPath d.outPath ]",
            nix_attrs(&attrs)
        );
        let expected: serde_json::Value = serde_json::from_str(&nix(&[
            BLAKE3_ON[0],
            BLAKE3_ON[1],
            "eval",
            "--store",
            "dummy://",
            "--json",
            "--expr",
            &expr,
        ]))
        .unwrap();
        let mut store = VirtualStore::new(StoreDir::default(), blake3_on());
        let (drv, out) = add(
            &mut store,
            unfilled(&attrs, Some(ContentAddress { method, hash })),
        );
        let shown = |p: &StorePath| store.store_dir().display(p);
        assert_eq!(serde_json::json!([shown(&drv), shown(&out)]), expected);
    }
}

#[test]
fn nar_symlink_target_checks_match_nix_store_restore() {
    for target in [&b""[..], b"a\0b", b"ok", b"t\xe9"] {
        let mut nar = Vec::new();
        for s in [
            &b"nix-archive-1"[..],
            b"(",
            b"type",
            b"symlink",
            b"target",
            target,
            b")",
        ] {
            nar::write_str(&mut nar, s).unwrap();
        }
        let dir = tempfile::tempdir().unwrap();
        let restored = dir.path().join("restored");
        let (ok, stderr) = nix_store_status(&["--restore", restored.to_str().unwrap()], &nar);
        match nar::parse(&mut nar.as_slice()) {
            Ok(_) => assert!(ok, "nix-store refused {target:?}:\n{stderr}"),
            Err(e) => {
                assert!(!ok, "nix-store accepted {target:?}");
                assert_eq!(stderr.trim_end(), format!("error: {e}"));
            }
        }
    }
}

/// Runs `oracle`, which must succeed; its stdout.
fn succeeds(oracle: &Oracle) -> Vec<u8> {
    let out = oracle.output();
    assert!(
        out.success(),
        "{} failed:\n{}",
        oracle.describe(),
        out.stderr_lossy()
    );
    out.stdout
}

/// Runs `oracle`, which must fail; its stderr, bytes as they are.
fn fails(oracle: &Oracle) -> Vec<u8> {
    let out = oracle.output();
    assert!(
        !out.success(),
        "{} succeeded:\n{}",
        oracle.describe(),
        out.stdout_lossy()
    );
    out.stderr
}

/// `error: <message>` and a newline, as Nix prints an error.
fn error_line(message: &[u8]) -> Vec<u8> {
    [&b"error: "[..], message, b"\n"].concat()
}

/// A tree whose names and symlink target are not UTF-8: names that sort
/// differently as signed and as unsigned bytes (`z`, 0x7a, before `\xff`;
/// `a\x80b`), a directory `d\xe9`, and a symlink `l\xff` to `t\xe9`.
fn make_byte_name_tree(root: &Path) {
    for name in [&b"z"[..], b"caf\xe9", b"\xff", b"a\x80b", b"d\xe9/f"] {
        write_bytes(root, name, name);
    }
    symlink_bytes(root, b"l\xff", b"t\xe9");
}

/// Names and targets are bytes, as Nix's `PosixSourceAccessor` reads
/// them: the NAR of a tree with names that are not UTF-8 is `nix-store
/// --dump`'s byte for byte (entries in unsigned byte order), its hash is
/// `nix hash path`'s, and Nix's NAR parses into a tree that dumps the
/// same again.
#[test]
fn nar_of_byte_names_is_byte_identical_to_nix_store_dump() {
    let dir = tempfile::tempdir().unwrap();
    make_byte_name_tree(dir.path());
    let expected = succeeds(Oracle::tool("nix-store").arg("--dump").arg(dir.path()));
    let accessor = FsAccessor::new(dir.path());
    let mut ours = Vec::new();
    nar::dump(&accessor, &CanonPath::root(), &mut ours).unwrap();
    assert_eq!(BStr::new(&ours), BStr::new(&expected));
    // A directory, a file and a symlink with such names on their own.
    for rel in [&b"d\xe9"[..], b"caf\xe9", b"l\xff"] {
        let expected = succeeds(
            Oracle::tool("nix-store")
                .arg("--dump")
                .arg(join_bytes(dir.path(), rel)),
        );
        let mut ours = Vec::new();
        nar::dump(&accessor, &CanonPath::new(rel).unwrap(), &mut ours).unwrap();
        assert_eq!(
            BStr::new(&ours),
            BStr::new(&expected),
            "{:?}",
            BStr::new(rel)
        );
    }
    // And with the accessor's root itself below a name that is not UTF-8.
    let below = FsAccessor::new(join_bytes(dir.path(), b"d\xe9"));
    let mut ours = Vec::new();
    nar::dump(&below, &CanonPath::root(), &mut ours).unwrap();
    assert_eq!(
        ours,
        succeeds(
            Oracle::tool("nix-store")
                .arg("--dump")
                .arg(join_bytes(dir.path(), b"d\xe9"))
        )
    );
    // `nix hash path`.
    let (hash, size) = nar::hash(&accessor, &CanonPath::root(), HashAlgo::Sha256).unwrap();
    let shown = succeeds(
        Oracle::new()
            .args(["hash", "path", "--sri"])
            .arg(dir.path()),
    );
    assert_eq!(
        format!("{}\n", hash.to_string(HashFormat::Sri, true)).as_bytes(),
        shown.as_slice()
    );
    assert_eq!(size, expected.len() as u64);
    // Nix's NAR parses into the same tree, which dumps the same NAR.
    let parsed = nar::parse(&mut expected.as_slice()).unwrap();
    let mut again = Vec::new();
    nar::dump(&parsed, &CanonPath::root(), &mut again).unwrap();
    assert_eq!(BStr::new(&again), BStr::new(&expected));
}

/// Nix restores a NAR with such names from brix's dump of an in-memory
/// tree, and dumps the restored tree to the same bytes.
#[test]
fn nar_with_byte_names_restores_like_nix() {
    let mut tree = brix_store::MemoryAccessor::new();
    for name in [&b"z"[..], b"caf\xe9", b"\xff", b"a\x80b", b"d\xe9/f"] {
        tree.add_file(&CanonPath::new(name).unwrap(), name, false)
            .unwrap();
    }
    tree.add_symlink(&CanonPath::new(b"l\xff").unwrap(), b"t\xe9")
        .unwrap();
    let mut nar = Vec::new();
    nar::dump(&tree, &CanonPath::root(), &mut nar).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let restored = dir.path().join("restored");
    succeeds(
        Oracle::tool("nix-store")
            .arg("--restore")
            .arg(&restored)
            .stdin(nar.clone()),
    );
    assert_eq!(
        std::fs::read_link(join_bytes(&restored, b"l\xff")).unwrap(),
        PathBuf::from(os(b"t\xe9"))
    );
    let dumped = succeeds(Oracle::tool("nix-store").arg("--dump").arg(&restored));
    assert_eq!(BStr::new(&dumped), BStr::new(&nar));
}

/// What `nix-store --restore` says of a damaged NAR quotes its bytes as
/// they are, as brix's [`nar::NarError::message_bytes`] does.
#[test]
fn bad_nar_messages_match_nix_store_restore() {
    let nar_of = |strings: &[&[u8]]| {
        let mut nar = Vec::new();
        for s in strings {
            nar::write_str(&mut nar, s).unwrap();
        }
        nar
    };
    for nar in [
        nar_of(&[
            &b"nix-archive-1"[..],
            b"(",
            b"type",
            b"directory",
            b"entry",
            b"(",
            b"name",
            b"a/\xff",
            b"node",
        ]),
        nar_of(&[&b"nix-archive-1"[..], b"(", b"\xe9"]),
        nar_of(&[&b"nix-archive-1"[..], b"(", b"type", b"\xe9"]),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let stderr = fails(
            Oracle::tool("nix-store")
                .arg("--restore")
                .arg(dir.path().join("restored"))
                .stdin(nar.clone()),
        );
        let ours = nar::parse(&mut nar.as_slice()).unwrap_err().message_bytes();
        assert_eq!(BStr::new(&stderr), BStr::new(&error_line(&ours)));
    }
}

/// Store paths are parsed from bytes, with Nix's messages quoting them
/// as they are. `nix-store -q <path>` parses its argument with
/// `followLinksToStorePath`: `toStorePath` of the canonical path, so
/// `parseStorePath` of its store path part, after "path \"…\" is not in
/// the Nix store" for a path outside the store (the text
/// `parseStorePath` has for one that is not directly below it).
#[test]
fn store_paths_of_bytes_match_nix() {
    let store = StoreDir::default();
    let h = b"ffffffffffffffffffffffffffffffff";
    let in_store = |rest: &[u8]| [&b"/nix/store/"[..], rest].concat();
    let base = |name: &[u8]| [&h[..], name].concat();
    let cases: Vec<(Vec<u8>, Vec<u8>)> = vec![
        (in_store(b"caf\xe9"), in_store(b"caf\xe9")),
        (
            in_store(&[&b"\xe9"[..], &h[1..], b"-x"].concat()),
            in_store(&[&b"\xe9"[..], &h[1..], b"-x"].concat()),
        ),
        (in_store(&base(b"\xe9x")), in_store(&base(b"\xe9x"))),
        (in_store(&base(b"-caf\xe9")), in_store(&base(b"-caf\xe9"))),
        (
            [in_store(&base(b"-caf\xe9")), b"/sub".to_vec()].concat(),
            in_store(&base(b"-caf\xe9")),
        ),
        (b"/tmp/caf\xe9".to_vec(), b"/tmp/caf\xe9".to_vec()),
        (
            b"/nix/store/../tmp/caf\xe9".to_vec(),
            b"/nix/store/../tmp/caf\xe9".to_vec(),
        ),
    ];
    for (arg, store_path_part) in cases {
        let stderr = fails(
            Oracle::tool("nix-store")
                .args(["--store", "dummy://", "-q"])
                .arg(os(&arg)),
        );
        let ours = store.parse(&store_path_part).unwrap_err().message_bytes();
        assert_eq!(
            BStr::new(&stderr),
            BStr::new(&error_line(&ours)),
            "{:?}",
            BStr::new(&arg)
        );
        if store.is_in_store(&arg) {
            assert_eq!(store.store_path_of(&arg).unwrap_err().message_bytes(), ours);
        }
    }
}

/// Store paths in a `.drv` file are bytes: parsing one that is not a
/// valid store path fails with Nix's message, quoting it as it is.
/// Registering such a file in a chroot store makes Nix parse it
/// (`readDerivation`, which puts "error parsing derivation '…': " before
/// a `FormatError` and nothing before a store path error).
#[test]
fn store_paths_in_drv_files_match_nix() {
    let h = b"ffffffffffffffffffffffffffffffff";
    let drv_path = "/nix/store/00000000000000000000000000000000-x.drv";
    let tail = &b"\"x86_64-linux\",\"/bin/sh\",[],[])"[..];
    let texts: Vec<Vec<u8>> = vec![
        [
            &b"Derive([(\"out\",\"/nix/store/s668cfx1vrqfjryf96jj8wfhg7f67dhj-x\",\"\",\"\")],[],[\"/nix/store/"[..],
            h,
            b"-caf\xe9\"],",
            tail,
        ]
        .concat(),
        [
            &b"Derive([(\"out\",\"/nix/store/"[..],
            h,
            b"-caf\xe9\",\"\",\"\")],[],[],",
            tail,
        ]
        .concat(),
        [&b"Derive([(\"out\",\"caf\xe9\",\"\",\"\")],[],[],"[..], tail].concat(),
        [
            &b"Derive([(\"out\",\"\",\"\",\"\")],[(\"/tmp/caf\xe9.drv\",[\"out\"])],[],"[..],
            tail,
        ]
        .concat(),
        b"DrvWithVersion(\"caf\xe9\",[])".to_vec(),
    ];
    let store_dir = StoreDir::default();
    for text in texts {
        let scratch = scratch_store();
        let file = scratch.path().join(&drv_path[1..]);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, &text).unwrap();
        let stderr = fails(
            Oracle::tool("nix-store")
                .arg("--store")
                .arg(scratch.path())
                .arg("--register-validity")
                .stdin(format!("{drv_path}\n\n0\n")),
        );
        let err = Derivation::parse(&store_dir, &text, "x", XpSettings::NONE).unwrap_err();
        let mut ours = Vec::new();
        if matches!(err, brix_store::DerivationError::Format(_)) {
            ours.extend_from_slice(format!("error parsing derivation '{drv_path}': ").as_bytes());
        }
        ours.extend_from_slice(&err.message_bytes());
        assert_eq!(
            BStr::new(&stderr),
            BStr::new(&error_line(&ours)),
            "{:?}",
            BStr::new(&text)
        );
    }
}
