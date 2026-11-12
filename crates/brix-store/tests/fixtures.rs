//! Derivation fixtures produced by the real Nix (see
//! `tests/fixtures/derivations/generate.sh`): every `.drv` must parse,
//! unparse byte-identically, land at the same store path, and be
//! reconstructible from its "unfilled" form the way `builtins.derivation`
//! builds it.

mod common;

use std::collections::{BTreeMap, BTreeSet};

use brix_store::{
    downstream_placeholder, Derivation, DerivationOutput, DrvHashModulo, StoreDir, StorePath,
    VirtualStore, Xp, XpSettings,
};
use bstr::BString;

/// The feature `generate.sh` runs Nix with, which the fixtures' floating
/// outputs need to be read.
const XP: XpSettings = XpSettings::NONE.with(Xp::CaDerivations);

fn drv_dir() -> std::path::PathBuf {
    common::fixtures_dir().join("derivations")
}

/// All fixture derivations by base name.
fn load_all() -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for entry in std::fs::read_dir(drv_dir().join("drvs")).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name().into_string().unwrap();
        out.insert(name, std::fs::read_to_string(entry.path()).unwrap());
    }
    assert!(out.len() >= 10, "fixtures missing; run generate.sh");
    out
}

fn case_path(attr: &str) -> StorePath {
    let s = std::fs::read_to_string(drv_dir().join(format!("{attr}.drvpath"))).unwrap();
    StorePath::new(s.trim()).unwrap()
}

#[test]
fn every_fixture_round_trips_and_hashes_to_its_path() {
    let store = StoreDir::default();
    for (base, text) in load_all() {
        let path = StorePath::new(&base).unwrap();
        let drv = Derivation::parse(&store, &text, path.derivation_name().unwrap(), XP)
            .unwrap_or_else(|e| panic!("{base}: {e}"));
        assert_eq!(
            drv.to_aterm(&store).unwrap(),
            text.as_bytes(),
            "{base}: unparse differs"
        );
        assert_eq!(
            drv.store_path(&store).unwrap(),
            path,
            "{base}: store path differs"
        );
    }
}

/// Damaged `.drv` text is an error, never a panic: every prefix of every
/// fixture, and every fixture with any one byte replaced by any other.
/// One thread per fixture, as there are half a million parses.
#[test]
fn parse_never_panics_on_damaged_fixtures() {
    let store = StoreDir::default();
    let all = load_all();
    std::thread::scope(|scope| {
        for (base, text) in &all {
            let store = &store;
            scope.spawn(move || {
                let bytes = text.as_bytes();
                let parse = |input: &[u8]| {
                    std::panic::catch_unwind(|| {
                        // Ok or Err are both fine; only a panic is a failure.
                        let _outcome = Derivation::parse(store, input, "x", XP);
                    })
                    .is_ok()
                };
                for len in 0..bytes.len() {
                    assert!(
                        parse(&bytes[..len]),
                        "{base}: panicked on its first {len} bytes"
                    );
                }
                let mut damaged = bytes.to_vec();
                for i in 0..bytes.len() {
                    for b in u8::MIN..=u8::MAX {
                        damaged[i] = b;
                        assert!(
                            parse(&damaged),
                            "{base}: panicked with byte {i} set to {b:#04x}"
                        );
                    }
                    damaged[i] = bytes[i];
                }
            });
        }
    });
}

#[test]
fn fixtures_agree_with_nix_derivation_show() {
    // The JSON from `nix derivation show` is a second view of the same
    // derivations: outputs, inputs, env and builder must all agree.
    let store = StoreDir::default();
    for entry in std::fs::read_dir(drv_dir()).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name().into_string().unwrap();
        let Some(_attr) = name.strip_suffix(".json") else {
            continue;
        };
        let json: serde_json::Value =
            serde_json::from_slice(&std::fs::read(entry.path()).unwrap()).unwrap();
        for (base, j) in json["derivations"].as_object().unwrap() {
            let text = std::fs::read_to_string(drv_dir().join("drvs").join(base)).unwrap();
            let path = StorePath::new(base).unwrap();
            let drv =
                Derivation::parse(&store, &text, path.derivation_name().unwrap(), XP).unwrap();
            assert_eq!(drv.name, j["name"].as_str().unwrap());
            assert_eq!(drv.builder, j["builder"].as_str().unwrap());
            assert_eq!(drv.platform, j["system"].as_str().unwrap());
            let args: Vec<&str> = j["args"]
                .as_array()
                .unwrap()
                .iter()
                .map(|a| a.as_str().unwrap())
                .collect();
            assert_eq!(drv.args, args);
            for (k, v) in j["env"].as_object().unwrap() {
                assert_eq!(
                    drv.env.get(k.as_bytes()).map(|v| v.as_slice()),
                    v.as_str().map(str::as_bytes),
                    "{base}: env {k}"
                );
            }
            // Structured attrs are shown as a JSON object; our env keeps
            // the serialised text under __json.
            if let Some(sa) = j.get("structuredAttrs") {
                let ours: serde_json::Value =
                    serde_json::from_slice(&drv.env["__json".as_bytes()]).unwrap();
                assert_eq!(&ours, sa, "{base}: structured attrs");
            }
            for (out_name, o) in j["outputs"].as_object().unwrap() {
                let ours = drv.outputs[out_name.as_bytes()]
                    .path(&store, &drv.name, out_name.as_bytes())
                    .unwrap();
                // Input-addressed outputs show their path; fixed outputs show
                // the hash (SRI) and method and leave the path to be computed.
                match (o.get("path"), o.get("hash")) {
                    (Some(p), _) => assert_eq!(
                        ours.unwrap().as_str(),
                        p.as_str().unwrap(),
                        "{base}: output {out_name}"
                    ),
                    (None, Some(h)) => {
                        let DerivationOutput::CAFixed(ca) = &drv.outputs[out_name.as_bytes()]
                        else {
                            panic!("{base}: expected fixed output")
                        };
                        assert_eq!(
                            ca.hash.to_string(brix_store::HashFormat::Sri, true),
                            h.as_str().unwrap()
                        );
                        assert_eq!(
                            ca.method.render(),
                            o["method"].as_str().unwrap(),
                            "{base}: method"
                        );
                        assert!(ours.is_some());
                    }
                    (None, None) => assert!(
                        ours.is_none(),
                        "{base}: output {out_name} should have no path"
                    ),
                }
            }
            let inputs = &j["inputs"];
            let srcs: BTreeSet<String> = inputs["srcs"]
                .as_array()
                .unwrap()
                .iter()
                .map(|s| s.as_str().unwrap().to_owned())
                .collect();
            assert_eq!(
                drv.input_srcs
                    .iter()
                    .map(|p| p.as_str().to_owned())
                    .collect::<BTreeSet<_>>(),
                srcs
            );
            let drvs = inputs["drvs"].as_object().unwrap();
            assert_eq!(drv.input_drvs.len(), drvs.len());
            for (d, outs) in drvs {
                let expected: BTreeSet<BString> = outs["outputs"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|s| BString::from(s.as_str().unwrap()))
                    .collect();
                assert_eq!(drv.input_drvs[&StorePath::new(d).unwrap()], expected);
            }
        }
    }
}

/// Rebuild `drv` the way `builtins.derivation` sees it before output paths
/// are known: input-addressed outputs become `Deferred` with empty env
/// vars, fixed outputs keep their hash but lose the env var.
fn unfill(drv: &Derivation) -> Derivation {
    let mut d = drv.clone();
    for (name, output) in d.outputs.iter_mut() {
        match output {
            DerivationOutput::InputAddressed(_) | DerivationOutput::Deferred => {
                *output = DerivationOutput::Deferred;
                d.env.insert(name.clone(), "".into());
            }
            DerivationOutput::CAFixed(_) => {
                d.env.insert(name.clone(), "".into());
            }
            _ => {}
        }
    }
    d
}

/// Add `base` and (first) its input derivations to the store, rebuilding
/// each from its unfilled form, and check the result matches the fixture.
fn add_recursively(
    store: &mut VirtualStore,
    all: &BTreeMap<String, String>,
    base: &str,
) -> StorePath {
    let path = StorePath::new(base).unwrap();
    if store.contains(&path) {
        return path;
    }
    let original = Derivation::parse(
        store.store_dir(),
        &all[base],
        path.derivation_name().unwrap(),
        XP,
    )
    .unwrap();
    for input in original.input_drvs.keys() {
        add_recursively(store, all, input.as_str());
    }
    let mut rebuilt = unfill(&original);
    store
        .fill_in_output_paths(&mut rebuilt)
        .unwrap_or_else(|e| panic!("{base}: {e}"));
    assert_eq!(
        rebuilt, original,
        "{base}: filling in output paths differs from Nix"
    );
    let added = store.add_derivation(rebuilt).unwrap();
    assert_eq!(added, path, "{base}: derivation path differs");
    path
}

#[test]
fn output_paths_are_recomputed_from_scratch() {
    let all = load_all();
    let mut store = VirtualStore::default();
    for base in all.keys() {
        add_recursively(&mut store, &all, base);
    }
    // Fixed-output derivations hash per output; plain ones get one hash.
    assert!(matches!(
        store.drv_hash(&case_path("fixedFlat")),
        Some(DrvHashModulo::CaOutputHashes(_))
    ));
    assert!(matches!(
        store.drv_hash(&case_path("plain")),
        Some(DrvHashModulo::DrvHash(_))
    ));
    assert!(matches!(
        store.drv_hash(&case_path("contentAddressed")),
        Some(DrvHashModulo::Deferred)
    ));
}

/// [`add_recursively`] through `fill_in_and_add_derivation`, the single
/// serialisation `derivationStrict` uses.
fn add_recursively_fused(
    store: &mut VirtualStore,
    all: &BTreeMap<String, String>,
    base: &str,
) -> StorePath {
    let path = StorePath::new(base).unwrap();
    if store.contains(&path) {
        return path;
    }
    let original = Derivation::parse(
        store.store_dir(),
        &all[base],
        path.derivation_name().unwrap(),
        XP,
    )
    .unwrap();
    for input in original.input_drvs.keys() {
        add_recursively_fused(store, all, input.as_str());
    }
    let added = store
        .fill_in_and_add_derivation(unfill(&original))
        .unwrap_or_else(|e| panic!("{base}: {e}"));
    assert_eq!(added, path, "{base}: derivation path differs");
    assert_eq!(
        store.derivation(&added),
        Some(&original),
        "{base}: filling in output paths differs from Nix"
    );
    path
}

#[test]
fn single_serialisation_matches_the_general_path() {
    let all = load_all();
    let mut general = VirtualStore::default();
    let mut fused = VirtualStore::default();
    for base in all.keys() {
        add_recursively(&mut general, &all, base);
        add_recursively_fused(&mut fused, &all, base);
    }
    for base in all.keys() {
        let path = StorePath::new(base).unwrap();
        assert_eq!(fused.drv_hash(&path), general.drv_hash(&path), "{base}");
    }
}

#[test]
fn derivation_depending_on_ca_outputs_is_deferred() {
    // `usesCa` could not be written by the generating machine's daemon, so
    // only its path is known (from `nix eval`). Build it by hand from
    // cases.nix: its env refers to the CA outputs through downstream
    // placeholders, and its own output stays deferred.
    let all = load_all();
    let mut store = VirtualStore::default();
    let ca = add_recursively(&mut store, &all, case_path("contentAddressed").as_str());
    let ca_flat = add_recursively(&mut store, &all, case_path("caFlat").as_str());
    let plain = add_recursively(&mut store, &all, case_path("plain").as_str());
    let plain_out = store.derivation(&plain).unwrap().outputs[b"out".as_slice()]
        .path(store.store_dir(), "plain", b"out")
        .unwrap()
        .unwrap();
    let mut drv = Derivation {
        name: "uses-ca".into(),
        platform: "x86_64-linux".into(),
        builder: "/bin/sh".into(),
        ..Default::default()
    };
    drv.env.insert("builder".into(), "/bin/sh".into());
    drv.env.insert(
        "dep".into(),
        downstream_placeholder(&ca, b"out").unwrap().into(),
    );
    drv.env.insert(
        "dev".into(),
        downstream_placeholder(&ca, b"dev").unwrap().into(),
    );
    drv.env.insert(
        "flat".into(),
        downstream_placeholder(&ca_flat, b"out").unwrap().into(),
    );
    drv.env.insert("name".into(), "uses-ca".into());
    drv.env.insert("out".into(), "".into());
    drv.env.insert(
        "plainDep".into(),
        store.store_dir().display(&plain_out).into(),
    );
    drv.env.insert("system".into(), "x86_64-linux".into());
    drv.input_drvs.insert(
        ca,
        BTreeSet::from([BString::from("dev"), BString::from("out")]),
    );
    drv.input_drvs
        .insert(ca_flat, BTreeSet::from([BString::from("out")]));
    drv.input_drvs
        .insert(plain, BTreeSet::from([BString::from("out")]));
    drv.outputs.insert("out".into(), DerivationOutput::Deferred);
    let unfilled = drv.clone();
    store.fill_in_output_paths(&mut drv).unwrap();
    assert_eq!(drv.outputs[b"out".as_slice()], DerivationOutput::Deferred);
    assert_eq!(drv.env["out".as_bytes()], "");
    let path = store.add_derivation(drv).unwrap();
    assert_eq!(path, case_path("usesCa"));
    // The single-serialisation path falls back to the general one for a
    // deferred input.
    let mut fresh = VirtualStore::default();
    for p in unfilled.input_drvs.keys() {
        add_recursively_fused(&mut fresh, &all, p.as_str());
    }
    assert_eq!(
        fresh.fill_in_and_add_derivation(unfilled).unwrap(),
        case_path("usesCa")
    );
    assert_eq!(
        fresh.drv_hash(&case_path("usesCa")),
        Some(&DrvHashModulo::Deferred)
    );
}

#[test]
fn ca_derivation_env_uses_hash_placeholders() {
    // The CA fixture's env vars for its outputs are `hashPlaceholder`s.
    let all = load_all();
    let store = StoreDir::default();
    let path = case_path("contentAddressed");
    let drv = Derivation::parse(&store, &all[path.as_str()], "content-addressed", XP).unwrap();
    assert_eq!(
        drv.env["out".as_bytes()],
        brix_store::hash_placeholder(b"out")
    );
    assert_eq!(
        drv.env["dev".as_bytes()],
        brix_store::hash_placeholder(b"dev")
    );
    assert!(matches!(
        drv.outputs[b"out".as_slice()],
        DerivationOutput::CAFloating { .. }
    ));
}
