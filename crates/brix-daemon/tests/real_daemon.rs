//! Integration tests against a real `nix-daemon`.
//!
//! Run with
//!
//! ```text
//! BRIX_TEST_REAL_DAEMON=1 cargo test -p brix-daemon --test real_daemon -- --ignored
//! ```
//!
//! They talk to the system daemon (`daemon`, i.e. `$NIX_DAEMON_SOCKET_PATH`
//! or `/nix/var/nix/daemon-socket/socket`) and, for the ssh-ng code path, to
//! the reference Nix's `nix-daemon --stdio` run locally (what Nix does for
//! `ssh-ng://localhost`). Like the rest of brix's tests they need
//! `BRIX_NIX_SRC` (the Nix source tree, a path in the daemon's store) and
//! `BRIX_NIX_BIN` (the reference `nix`).
//! Every path they add to the store is tiny and content-addressed; the one
//! build is `echo hi > $out`.

use std::collections::{BTreeMap, BTreeSet};

use brix_daemon::{
    BuildMode, BuildStatus, ClientOptions, ContentAddressMethod, DaemonClient, DerivedPath,
    HashAlgo, ProtocolVersion, StoreDir, StorePath, StoreUri,
};
use brix_store::{
    CanonPath, Derivation, DerivationOutput, FileIngestionMethod, FixedOutputInfo, Hash,
    HashFormat, MemoryAccessor, SourceAccessor, StoreReferences,
};
use brix_testutil::{nix_tool, require_env, Oracle, REAL_DAEMON};

/// The Nix source tree every brix test environment has in its store
/// (`$BRIX_NIX_SRC`).
fn nix_src() -> StorePath {
    let src = brix_testutil::nix_src();
    let base = src
        .strip_prefix("/nix/store/")
        .expect("BRIX_NIX_SRC is a path in /nix/store");
    StorePath::new(base.to_str().expect("a store path is UTF-8"))
        .expect("BRIX_NIX_SRC is a store path")
}

/// The reference `nix` run with `args`, which must succeed; its stdout.
fn nix(args: &[&str]) -> String {
    Oracle::new().args(args).stdout()
}

fn connect() -> DaemonClient {
    require_env(REAL_DAEMON);
    DaemonClient::connect(&StoreUri::daemon(), ClientOptions::default())
        .expect("connect to the system daemon")
}

/// A unique payload per test run so adding paths exercises the real work
/// rather than hitting an already valid path.
fn unique(tag: &str) -> Vec<u8> {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!(
        "brix-daemon test {tag} {nanos} pid {}\n",
        std::process::id()
    )
    .into_bytes()
}

#[test]
#[ignore = "needs a running nix-daemon; set BRIX_TEST_REAL_DAEMON=1"]
fn handshake_with_system_daemon() {
    let client = connect();
    assert!(client.protocol_version() >= ProtocolVersion::new(1, 35));
    assert!(
        client.daemon_nix_version().is_some(),
        "protocol >= 1.33 reports the version"
    );
    assert!(client.trust().is_some(), "protocol >= 1.35 reports trust");
    if client.protocol_version().at_least(1, 38) {
        assert!(client.has_feature("realisation-with-path-not-hash"));
    }
}

#[test]
#[ignore = "needs a running nix-daemon; set BRIX_TEST_REAL_DAEMON=1"]
fn query_path_info_matches_nix_path_info() {
    let mut client = connect();
    let path = nix_src();
    assert!(client.is_valid_path(&path).unwrap());
    let info = client
        .query_path_info(&path)
        .unwrap()
        .expect("nix-src is valid");
    assert!(info.nar_size > 1_000_000);
    assert!(
        info.references.is_empty(),
        "a fetched source tree has no references"
    );

    // Cross-check against the CLI.
    let out = nix(&["path-info", "--json", &format!("/nix/store/{path}")]);
    let json: serde_json::Value = serde_json::from_str(&out).unwrap();
    let entry = json
        .as_object()
        .and_then(|m| m.values().next())
        .or_else(|| json.as_array().and_then(|a| a.first()))
        .expect("one entry");
    assert_eq!(entry["narSize"].as_u64().unwrap(), info.nar_size);
    assert_eq!(
        entry["narHash"].as_str().unwrap(),
        info.nar_hash.to_string(HashFormat::Sri, true)
    );

    let mut set = BTreeSet::new();
    set.insert(path.clone());
    set.insert(StorePath::new("00000000000000000000000000000000-does-not-exist").unwrap());
    let valid = client.query_valid_paths(&set, false).unwrap();
    assert_eq!(
        valid.into_iter().collect::<Vec<_>>(),
        std::slice::from_ref(&path)
    );

    assert_eq!(
        client.query_path_from_hash_part(path.hash_part()).unwrap(),
        Some(path)
    );
    assert_eq!(
        client
            .query_path_from_hash_part("00000000000000000000000000000000")
            .unwrap(),
        None
    );
    assert!(client
        .query_path_info(
            &StorePath::new("00000000000000000000000000000000-does-not-exist").unwrap()
        )
        .unwrap()
        .is_none());
}

#[test]
#[ignore = "needs a running nix-daemon; set BRIX_TEST_REAL_DAEMON=1"]
fn add_text_and_read_it_back() {
    let mut client = connect();
    let contents = unique("text");
    let refs = BTreeSet::new();
    let added = client.add_text("brix-test.txt", &contents, &refs).unwrap();

    // The daemon's path must be what brix-store computes locally.
    let expected = StoreDir::default()
        .make_text_path(
            "brix-test.txt",
            &Hash::of(HashAlgo::Sha256, &contents),
            &refs,
        )
        .unwrap();
    assert_eq!(added.path, expected);
    assert_eq!(
        added.info.ca.as_ref().unwrap().to_string(),
        format!(
            "text:{}",
            Hash::of(HashAlgo::Sha256, &contents).to_string(HashFormat::Nix32, true)
        )
    );

    let tree = client.read_path_into_memory(&added.path).unwrap();
    assert_eq!(tree.read_file(&CanonPath::root()).unwrap(), contents);

    let mut nar = Vec::new();
    brix_store::nar::dump(&tree, &CanonPath::root(), &mut nar).unwrap();
    let mut expected_nar = Vec::new();
    brix_store::nar::dump_bytes(&contents, &mut expected_nar).unwrap();
    assert_eq!(nar, expected_nar);
    assert_eq!(nar.len() as u64, added.info.nar_size);
}

#[test]
#[ignore = "needs a running nix-daemon; set BRIX_TEST_REAL_DAEMON=1"]
fn add_nar_tree() {
    let mut client = connect();
    let mut tree = MemoryAccessor::new();
    tree.add_dir(&CanonPath::root()).unwrap();
    tree.add_file(&CanonPath::new("/hello").unwrap(), unique("nar"), true)
        .unwrap();
    tree.add_file(
        &CanonPath::new("/sub/data").unwrap(),
        b"data".to_vec(),
        false,
    )
    .unwrap();
    tree.add_symlink(&CanonPath::new("/link").unwrap(), "hello")
        .unwrap();
    let mut nar = Vec::new();
    brix_store::nar::dump(&tree, &CanonPath::root(), &mut nar).unwrap();
    let (nar_hash, _) = brix_store::nar::hash(&tree, &CanonPath::root(), HashAlgo::Sha256).unwrap();

    let added = client.add_nar("brix-tree", &nar, &BTreeSet::new()).unwrap();
    let expected = StoreDir::default()
        .make_fixed_output_path(
            "brix-tree",
            &FixedOutputInfo {
                method: FileIngestionMethod::NixArchive,
                hash: nar_hash,
                references: StoreReferences::default(),
            },
        )
        .unwrap();
    assert_eq!(added.path, expected);
    assert_eq!(added.info.nar_hash, nar_hash);

    let back = client.read_path_into_memory(&added.path).unwrap();
    assert_eq!(
        back.read_file(&CanonPath::new("/sub/data").unwrap())
            .unwrap(),
        b"data"
    );
    let mut back_nar = Vec::new();
    brix_store::nar::dump(&back, &CanonPath::root(), &mut back_nar).unwrap();
    assert_eq!(back_nar, nar);

    client
        .add_temp_roots(&[added.path.clone()].into_iter().collect())
        .unwrap();
    client.ensure_path(&added.path).unwrap();
}

/// A derivation that writes `payload` to `$out`, with its output path
/// filled in, plus its `.drv` text.
fn trivial_derivation(payload: &[u8]) -> (Derivation, StorePath, Vec<u8>) {
    let script = format!("echo '{}' > $out", String::from_utf8_lossy(payload).trim());
    script_derivation("brix-trivial", &script)
}

/// A derivation named `name` whose builder is `/bin/sh -c script`, with
/// its output path filled in, plus its `.drv` text.
fn script_derivation(name: &str, script: &str) -> (Derivation, StorePath, Vec<u8>) {
    let store = StoreDir::default();
    let mut outputs = BTreeMap::new();
    outputs.insert(bstr::BString::from("out"), DerivationOutput::Deferred);
    let mut drv = Derivation {
        name: name.into(),
        outputs,
        input_srcs: BTreeSet::new(),
        input_drvs: BTreeMap::new(),
        platform: current_system().into(),
        builder: "/bin/sh".into(),
        args: vec!["-c".into(), script.into()],
        env: [
            ("builder".into(), "/bin/sh".into()),
            ("name".into(), name.into()),
            ("out".into(), "".into()),
            ("system".into(), current_system().into()),
        ]
        .into_iter()
        .collect(),
    };
    drv.fill_in_output_paths(&store, brix_store::XpSettings::NONE, &mut |p| {
        panic!("no inputs expected, asked for {p}")
    })
    .unwrap();
    let drv_path = drv.store_path(&store).unwrap();
    let text = drv.to_aterm(&store).unwrap();
    (drv, drv_path, text)
}

fn current_system() -> String {
    nix(&[
        "eval",
        "--impure",
        "--raw",
        "--expr",
        "builtins.currentSystem",
    ])
    .trim()
    .to_owned()
}

#[test]
#[ignore = "needs a running nix-daemon and runs one tiny build; set BRIX_TEST_REAL_DAEMON=1"]
fn build_paths_with_results_builds_a_trivial_derivation() {
    let mut client = connect();
    let payload = unique("build");
    let (drv, drv_path, text) = trivial_derivation(&payload);

    // `writeDerivation`: the .drv is a text path referring to its inputs.
    let added = client
        .add_text(&format!("{}.drv", drv.name), &text, &drv.references())
        .unwrap();
    assert_eq!(
        added.path, drv_path,
        "brix-store's .drv path matches the daemon's"
    );

    let outputs = client.query_derivation_output_map(&drv_path).unwrap();
    let out_path = outputs[b"out".as_slice()]
        .clone()
        .expect("input-addressed output is known");
    assert_eq!(
        drv.outputs[b"out".as_slice()],
        DerivationOutput::InputAddressed(out_path.clone())
    );

    let missing = client
        .query_missing(&[DerivedPath::all_outputs(drv_path.clone())])
        .unwrap();
    assert!(missing.will_build.contains(&drv_path));

    let results = client
        .build_paths_with_results(
            &[DerivedPath::all_outputs(drv_path.clone())],
            BuildMode::Normal,
        )
        .unwrap();
    assert_eq!(results.len(), 1);
    let r = &results[0].result;
    assert!(r.is_success(), "{:?}", r);
    assert_eq!(r.status, BuildStatus::Built);
    assert_eq!(r.built_outputs[b"out".as_slice()], out_path);

    let built = client.read_path_into_memory(&out_path).unwrap();
    let got = built.read_file(&CanonPath::root()).unwrap();
    assert_eq!(
        String::from_utf8_lossy(&got).trim(),
        String::from_utf8_lossy(&payload).trim()
    );

    // Building again is a no-op reported as AlreadyValid.
    let again = client
        .build_paths_with_results(
            &[DerivedPath::all_outputs(drv_path.clone())],
            BuildMode::Normal,
        )
        .unwrap();
    assert_eq!(again[0].result.status, BuildStatus::AlreadyValid);

    client
        .build_paths(&[DerivedPath::Opaque(out_path)], BuildMode::Normal)
        .unwrap();
}

#[test]
#[ignore = "needs a running nix-daemon and runs one tiny build; set BRIX_TEST_REAL_DAEMON=1"]
fn build_derivation_without_registering_it() {
    let mut client = connect();
    let payload = unique("build-derivation");
    let (drv, drv_path, _text) = trivial_derivation(&payload);
    let result = client
        .build_derivation(&drv_path, &drv, BuildMode::Normal)
        .unwrap();
    assert!(result.is_success(), "{result:?}");
    let out_path = match &drv.outputs[b"out".as_slice()] {
        DerivationOutput::InputAddressed(p) => p.clone(),
        other => panic!("{other:?}"),
    };
    assert_eq!(result.built_outputs[b"out".as_slice()], out_path);
    assert!(client.is_valid_path(&out_path).unwrap());
}

/// The build log lines the daemon streams, as a logger gets them.
#[derive(Clone, Default)]
struct LogLines(std::sync::Arc<std::sync::Mutex<Vec<bstr::BString>>>);

impl brix_daemon::Logger for LogLines {
    fn result(
        &mut self,
        _id: brix_daemon::logger::ActivityId,
        kind: brix_daemon::logger::ResultType,
        fields: &[brix_daemon::logger::Field],
    ) {
        if let (
            brix_daemon::logger::ResultType::BuildLogLine,
            Some(brix_daemon::logger::Field::Str(line)),
        ) = (kind, fields.first())
        {
            self.0.lock().unwrap().push(line.clone());
        }
    }
}

/// A builder that prints bytes that are not UTF-8 and fails: its log
/// lines reach the logger as it printed them, and the failed build's
/// message ends with them, as they are (what `nix build` shows under
/// `error:`: "Last 2 log lines:\n> first \xff line\n> second \xe9").
#[test]
#[ignore = "needs a running nix-daemon and runs one tiny failing build; set BRIX_TEST_REAL_DAEMON=1"]
fn a_failed_build_keeps_the_bytes_of_its_log() {
    require_env(REAL_DAEMON);
    let lines = LogLines::default();
    let options = ClientOptions {
        logger: Box::new(lines.clone()),
        ..ClientOptions::default()
    };
    let mut client =
        DaemonClient::connect(&StoreUri::daemon(), options).expect("connect to the system daemon");
    let nonce = String::from_utf8(unique("bytes")).unwrap();
    // `echo` the nonce so that the derivation is new to the store, then
    // the bytes, with `printf`'s octal escapes.
    let script = format!(
        "echo '{}' >/dev/null; printf 'first \\377 line\\n'; printf 'second \\351\\n' >&2; exit 3",
        nonce.trim()
    );
    let (drv, drv_path, text) = script_derivation("brix-bytes", &script);
    client
        .add_text(&format!("{}.drv", drv.name), &text, &drv.references())
        .unwrap();
    let results = client
        .build_paths_with_results(&[DerivedPath::all_outputs(drv_path)], BuildMode::Normal)
        .unwrap();
    let r = &results[0].result;
    assert!(!r.is_success(), "{r:?}");
    let expected_tail: &[u8] = b"Last 2 log lines:\n> first \xff line\n> second \xe9\n";
    assert!(
        bstr::ByteSlice::find(r.error_msg.as_slice(), expected_tail).is_some(),
        "{:?}",
        r.error_msg
    );
    assert_eq!(
        *lines.0.lock().unwrap(),
        [
            bstr::BString::from(b"first \xff line"),
            bstr::BString::from(b"second \xe9")
        ]
    );
}

#[test]
#[ignore = "runs the reference nix-daemon --stdio; set BRIX_TEST_REAL_DAEMON=1"]
fn ssh_ng_localhost_runs_nix_daemon_stdio() {
    require_env(REAL_DAEMON);
    let daemon = nix_tool("nix-daemon");
    let uri = StoreUri::parse(
        &format!("ssh-ng://localhost?remote-program={}", daemon.display()),
        &|w| panic!("unexpected warning: {w}"),
    )
    .unwrap();
    let mut client =
        DaemonClient::connect(&uri, ClientOptions::default()).expect("nix-daemon --stdio");
    let path = nix_src();
    let info = client.query_path_info(&path).unwrap().expect("valid");
    assert!(info.nar_size > 0);
    assert!(client.is_valid_path(&path).unwrap());
    drop(client);
}

#[test]
#[ignore = "needs a running nix-daemon; set BRIX_TEST_REAL_DAEMON=1"]
fn flat_file_add_uses_fixed_sha256() {
    let mut client = connect();
    let contents = unique("flat");
    let added = client
        .add_to_store(
            "brix-flat",
            ContentAddressMethod::Flat,
            HashAlgo::Sha256,
            &BTreeSet::new(),
            false,
            &mut |w| w.write_all(&contents),
        )
        .unwrap();
    let expected = StoreDir::default()
        .make_fixed_output_path(
            "brix-flat",
            &FixedOutputInfo {
                method: FileIngestionMethod::Flat,
                hash: Hash::of(HashAlgo::Sha256, &contents),
                references: StoreReferences::default(),
            },
        )
        .unwrap();
    assert_eq!(added.path, expected);
}

/// `AddMultipleToStore` as brix uploads what evaluation produced: a
/// source tree, a flat file and a text that refers to the tree, in one
/// operation, content-addressed and unsigned. The daemon must register
/// each under the path brix computed, and refuse a path its content
/// address does not name.
#[test]
#[ignore = "needs a running nix-daemon; set BRIX_TEST_REAL_DAEMON=1"]
fn add_multiple_content_addressed_paths() {
    use brix_daemon::{PathInfo, PathToAdd, ValidPathInfo};
    use brix_store::ContentAddress;

    let mut client = connect();
    let store_dir = StoreDir::default();
    let sha256 = |bytes: &[u8]| Hash::of(HashAlgo::Sha256, bytes);
    let entry = |path: StorePath, nar: Vec<u8>, references: BTreeSet<StorePath>, ca| {
        let info = ValidPathInfo {
            path,
            info: PathInfo {
                deriver: None,
                nar_hash: sha256(&nar),
                references,
                registration_time: 0,
                nar_size: nar.len() as u64,
                ultimate: false,
                sigs: BTreeSet::new(),
                ca: Some(ca),
            },
        };
        (info, nar)
    };
    let fixed = |name: &str, method, hash: Hash| {
        store_dir
            .make_fixed_output_path(
                name,
                &FixedOutputInfo {
                    method,
                    hash,
                    references: StoreReferences::default(),
                },
            )
            .unwrap()
    };

    let mut tree = MemoryAccessor::new();
    tree.add_file(&CanonPath::new("/run").unwrap(), unique("multi tree"), true)
        .unwrap();
    tree.add_symlink(&CanonPath::new("/link").unwrap(), "run")
        .unwrap();
    let mut tree_nar = Vec::new();
    brix_store::nar::dump(&tree, &CanonPath::root(), &mut tree_nar).unwrap();
    let tree_path = fixed(
        "brix-multi-tree",
        FileIngestionMethod::NixArchive,
        sha256(&tree_nar),
    );
    let tree_ca = ContentAddress {
        method: ContentAddressMethod::NixArchive,
        hash: sha256(&tree_nar),
    };

    let flat = unique("multi flat");
    let mut flat_nar = Vec::new();
    brix_store::nar::dump_bytes(&flat, &mut flat_nar).unwrap();
    let flat_path = fixed("brix-multi-flat", FileIngestionMethod::Flat, sha256(&flat));
    let flat_ca = ContentAddress {
        method: ContentAddressMethod::Flat,
        hash: sha256(&flat),
    };

    let text = unique("multi text");
    let mut text_nar = Vec::new();
    brix_store::nar::dump_bytes(&text, &mut text_nar).unwrap();
    let text_refs: BTreeSet<StorePath> = [tree_path.clone()].into_iter().collect();
    let text_path = store_dir
        .make_text_path("brix-multi-text", &sha256(&text), &text_refs)
        .unwrap();
    let text_ca = ContentAddress {
        method: ContentAddressMethod::Text,
        hash: sha256(&text),
    };

    let entries = [
        entry(tree_path.clone(), tree_nar, BTreeSet::new(), tree_ca),
        entry(
            flat_path.clone(),
            flat_nar,
            BTreeSet::new(),
            flat_ca.clone(),
        ),
        entry(text_path.clone(), text_nar, text_refs.clone(), text_ca),
    ];
    let mut paths = entries.iter().map(|(info, nar)| {
        Ok(PathToAdd {
            info: info.clone(),
            nar: Box::new(move |out| out.write_all(nar)),
        })
    });
    client
        .add_multiple_to_store(entries.len(), &mut paths, false)
        .unwrap();

    for (expected, _) in &entries {
        let info = client
            .query_path_info(&expected.path)
            .unwrap()
            .expect("the path was registered");
        assert_eq!(info.nar_hash, expected.info.nar_hash);
        assert_eq!(info.nar_size, expected.info.nar_size);
        assert_eq!(info.references, expected.info.references);
        assert_eq!(info.ca, expected.info.ca);
    }
    let read = client.read_path_into_memory(&flat_path).unwrap();
    assert_eq!(read.read_file(&CanonPath::root()).unwrap(), flat);

    // Contents under a path they do not hash to: refused, and the
    // connection goes on.
    let other = unique("multi mismatch");
    let mut other_nar = Vec::new();
    brix_store::nar::dump_bytes(&other, &mut other_nar).unwrap();
    let wrong = fixed("brix-multi-wrong", FileIngestionMethod::Flat, sha256(&flat));
    let (info, nar) = entry(wrong.clone(), other_nar, BTreeSet::new(), flat_ca);
    let mut paths = std::iter::once(Ok(PathToAdd {
        info,
        nar: Box::new(|out| out.write_all(&nar)),
    }));
    let err = client
        .add_multiple_to_store(1, &mut paths, false)
        .unwrap_err();
    assert!(
        matches!(err, brix_daemon::DaemonError::Remote(_)),
        "{err:?}"
    );
    assert!(!client.is_valid_path(&wrong).unwrap());
}
