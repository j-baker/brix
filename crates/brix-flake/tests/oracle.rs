//! Comparisons against the reference `nix` binary (`BRIX_NIX_BIN`) and
//! against lock files Nix wrote.
//!
//! Tests that only need `nix eval` run everywhere (with `NIX_REMOTE=dummy://`
//! so no store is needed); so do those that compare with values Nix
//! reported once. Tests that need a writable Nix store (the default one, as
//! `nix flake prefetch` uses it, opted into with `BRIX_TEST_REAL_STORE=1`),
//! the network (`BRIX_TEST_NETWORK=1`) or a long time are `#[ignore]`d;
//! the reason is on each one. Every test that runs `nix` needs
//! `BRIX_NIX_BIN` and fails without it.

use std::path::{Path, PathBuf};
use std::process::Command;

use brix_flake::attrs::{attrs_to_json, Attrs};
use brix_flake::fetch::fetch_input;
use brix_flake::flakeref::{parse_flake_ref, ParseOptions};
use brix_flake::native::NativeFetcher;
use brix_flake::{FetchSettings, Input, LockFile};
use brix_store::StoreDir;
use brix_testutil::{nixpkgs, require_env, GitRepo, Oracle, NETWORK, REAL_STORE};

/// `nix eval --json` or `--raw` on `expr`, with no store and no flake
/// registry.
fn nix_eval(expr: &str, json: bool) -> String {
    Oracle::new()
        .arg("eval")
        .arg(if json { "--json" } else { "--raw" })
        .arg("--expr")
        .arg(expr)
        .dummy_store()
        .config("flake-registry = ")
        .stdout()
}

/// The flake references whose parsing is compared with
/// `builtins.parseFlakeRef` and whose printing with
/// `builtins.flakeRefToString`.
const REFS: &[&str] = &[
    "github:NixOS/nixpkgs",
    "github:NixOS/nixpkgs/nixos-25.05",
    "github:NixOS/nixpkgs/nixos-25.05?dir=lib",
    "github:NixOS/nixpkgs/addf7cf5f383a3101ecfba091b98d0a1263dc9b8",
    "github:NixOS/nixpkgs?ref=release-24.11&host=github.example.com",
    "github:foo/bar/branch%23",
    "gitlab:owner/repo/main",
    "sourcehut:~user/repo",
    "git+https://github.com/NixOS/nix?ref=master",
    "git+https://github.com/NixOS/nix?rev=2aae6c35c94fcfb415dbe95f408b9ce91ee846ed&ref=master",
    "git+ssh://git@github.com/NixOS/nix.git?shallow=1&submodules=1",
    "git+file:///tmp/repo?dir=sub",
    "git://somewhere/repo?ref=branch",
    "https://example.org/foo.tar.gz",
    "https://example.org/foo.tar.gz?narHash=sha256-hM20uyap1a0M9d344I692r+ik4gTMyj60cQWO+hAYP8=",
    "tarball+https://example.org/archive?rev=2aae6c35c94fcfb415dbe95f408b9ce91ee846ed",
    "file+https://example.org/file.txt",
    "path:/tmp/foo",
    "path:/tmp/foo?lastModified=1&narHash=sha256-hM20uyap1a0M9d344I692r+ik4gTMyj60cQWO+hAYP8=",
    "/tmp/foo bar?dir=bla space",
    "flake:nixpkgs",
    "nixpkgs",
    "nixpkgs/nixos-25.05",
    "nixpkgs/nixos-25.05/2aae6c35c94fcfb415dbe95f408b9ce91ee846ed",
    "http://localhost:8181/test/+3d.tar.gz",
    "hg+https://example.org/repo",
    "hg+https://example.org/repo?ref=default&rev=2aae6c35c94fcfb415dbe95f408b9ce91ee846ed",
    "hg+file:///tmp/repo?foo=bar&ref=stable",
];

#[test]
fn flakeref_parsing_matches_builtins_parse_flake_ref() {
    let settings = FetchSettings::default();
    for r in REFS {
        let expr = format!("builtins.parseFlakeRef {}", nix_string(r));
        let expected: serde_json::Value =
            serde_json::from_str(&nix_eval(&expr, true)).expect("json");
        let ours = parse_flake_ref(&settings, r, &ParseOptions::flake())
            .unwrap_or_else(|e| panic!("{r}: {e}"));
        assert_eq!(attrs_to_json(&ours.to_attrs()), expected, "attrs of {r}");

        let expr = format!(
            "builtins.flakeRefToString (builtins.parseFlakeRef {})",
            nix_string(r)
        );
        let expected = nix_eval(&expr, false);
        assert_eq!(ours.to_string(), expected, "string form of {r}");
    }
}

#[test]
fn flakeref_from_attrs_matches_flake_ref_to_string() {
    let settings = FetchSettings::default();
    let cases: Vec<Attrs> = vec![
        brix_flake::attrs::attrs([
            ("type", "github".into()),
            ("owner", "NixOS".into()),
            ("repo", "nixpkgs".into()),
            ("rev", "addf7cf5f383a3101ecfba091b98d0a1263dc9b8".into()),
            (
                "narHash",
                "sha256-hM20uyap1a0M9d344I692r+ik4gTMyj60cQWO+hAYP8=".into(),
            ),
            ("lastModified", 1765186076u64.into()),
        ]),
        brix_flake::attrs::attrs([
            ("type", "git".into()),
            ("url", "https://github.com/NixOS/nix".into()),
            ("ref", "master".into()),
            ("rev", "2aae6c35c94fcfb415dbe95f408b9ce91ee846ed".into()),
            ("shallow", true.into()),
            ("revCount", 12u64.into()),
            ("dir", "perl".into()),
        ]),
        brix_flake::attrs::attrs([
            ("type", "tarball".into()),
            ("url", "https://example.org/x.tar.gz".into()),
            (
                "narHash",
                "sha256-hM20uyap1a0M9d344I692r+ik4gTMyj60cQWO+hAYP8=".into(),
            ),
        ]),
        brix_flake::attrs::attrs([
            ("type", "path".into()),
            ("path", "/tmp/x".into()),
            ("lastModified", 5u64.into()),
            ("dir", "sub".into()),
        ]),
        brix_flake::attrs::attrs([
            ("type", "indirect".into()),
            ("id", "nixpkgs".into()),
            ("ref", "x".into()),
        ]),
    ];
    for attrs in cases {
        let json = attrs_to_json(&attrs).to_string();
        let expected = nix_eval(
            &format!(
                "builtins.flakeRefToString (builtins.fromJSON {})",
                nix_string(&json)
            ),
            false,
        );
        let ours = brix_flake::FlakeRef::from_attrs(&settings, &attrs).unwrap();
        assert_eq!(ours.to_string(), expected, "{json}");
    }
}

fn nix_string(s: &str) -> String {
    format!(
        "\"{}\"",
        s.replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace("${", "\\${")
    )
}

#[test]
fn committed_lock_files_round_trip_byte_for_byte() {
    let settings = FetchSettings::default();
    for name in ["brix.flake.lock", "trustless.flake.lock"] {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name);
        let text = std::fs::read_to_string(&path).unwrap();
        let lock = LockFile::parse(&settings, &text, name).unwrap();
        assert_eq!(format!("{}\n", lock.to_json_string()), text, "{name}");
        assert!(lock.is_unlocked(&settings).unwrap().is_none(), "{name}");
        lock.check().unwrap();
    }
}

/// Builds a tree of flakes with relative `path:` inputs and `follows`,
/// locks it with `nix flake lock`, and checks that brix reads and
/// re-serialises the result identically.
#[test]
fn lock_files_written_by_nix_round_trip() {
    let settings = FetchSettings::default();
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    std::fs::create_dir_all(root.join("b")).unwrap();
    std::fs::create_dir_all(root.join("c")).unwrap();
    std::fs::write(
        root.join("flake.nix"),
        r#"{
  inputs.b.url = "path:./b";
  inputs.c.url = "path:./c";
  inputs.c.inputs.b.follows = "b";
  inputs.d = { url = "path:./b/data.txt"; flake = false; };
  outputs = { ... }: { };
}"#,
    )
    .unwrap();
    std::fs::write(root.join("b/flake.nix"), "{ outputs = { self }: { }; }").unwrap();
    std::fs::write(root.join("b/data.txt"), "data\n").unwrap();
    std::fs::write(
        root.join("c/flake.nix"),
        r#"{ inputs.b.url = "path:../b"; outputs = { ... }: { }; }"#,
    )
    .unwrap();
    Oracle::new()
        .args(["flake", "lock", root.to_str().unwrap()])
        .stdout();
    let text = std::fs::read_to_string(root.join("flake.lock")).unwrap();
    let lock = LockFile::parse(&settings, &text, "flake.lock").unwrap();
    assert_eq!(format!("{}\n", lock.to_json_string()), text);
    let all = lock.all_inputs();
    assert!(all.contains_key(&vec!["c".to_string(), "b".to_string()]));
    let c_b = &all[&vec!["c".to_string(), "b".to_string()]];
    assert_eq!(*c_b, brix_flake::Edge::Follows(vec!["b".into()]));
    let d = match &all[&vec!["d".to_string()]] {
        brix_flake::Edge::Node(id) => lock.node(*id).locked.as_ref().unwrap(),
        other => panic!("{other:?}"),
    };
    assert!(!d.is_flake);
    assert_eq!(d.parent_input_attr_path, Some(vec![]));
}

/// `nix flake prefetch --json <url>`, which copies the tree into the
/// default Nix store.
fn nix_flake_prefetch(url: &str) -> serde_json::Value {
    let out = Oracle::new()
        .args(["flake", "prefetch", "--json", url])
        .stdout();
    serde_json::from_str(&out).unwrap()
}

/// A tarball holding `sub/f` ("hi\n"), whose singleton directory is
/// dereferenced when unpacked.
fn make_tarball(dir: &Path) -> PathBuf {
    let tree = dir.join("t");
    std::fs::create_dir_all(tree.join("sub")).unwrap();
    std::fs::write(tree.join("sub/f"), "hi\n").unwrap();
    let tarball = dir.join("t.tar.gz");
    let status = Command::new("tar")
        .args([
            "czf",
            tarball.to_str().unwrap(),
            "-C",
            tree.to_str().unwrap(),
            "sub",
        ])
        .status()
        .expect("run tar");
    assert!(status.success());
    tarball
}

fn fetch_tarball(cache: &Path, tarball: &Path) -> brix_flake::FetchedTree {
    let settings = FetchSettings::default();
    let input =
        Input::from_url_str(&settings, &format!("file://{}", tarball.display()), true).unwrap();
    let fetcher = NativeFetcher::new().unwrap().with_cache_dir(cache.into());
    fetch_input(&fetcher, &settings, &input).unwrap()
}

/// The NAR hash and store path of an unpacked tarball, as `nix flake
/// prefetch` reported them for the same content.
#[test]
fn tarball_nar_hash_and_store_path_match_nix() {
    let tmp = tempfile::tempdir().unwrap();
    let fetched = fetch_tarball(&tmp.path().join("cache"), &make_tarball(tmp.path()));
    assert_eq!(
        fetched.nar_hash().unwrap().to_sri(),
        "sha256-MyEwKR7frLkJXyCTAYmhACAemt2h4EszAo04ltEu78o="
    );
    assert_eq!(
        StoreDir::new("/nix/store")
            .display(&fetched.store_path(&StoreDir::new("/nix/store")).unwrap()),
        "/nix/store/s1gbqa4hazf27kcgbjz2rc5mbjjl2n9j-source"
    );
    assert!(fetched.input.last_modified().unwrap().is_some());
}

/// The same tarball through `nix flake prefetch` itself.
#[test]
#[ignore = "writes to the user's Nix store; set BRIX_TEST_REAL_STORE=1"]
fn tarball_matches_nix_flake_prefetch() {
    require_env(REAL_STORE);
    let tmp = tempfile::tempdir().unwrap();
    let tarball = make_tarball(tmp.path());
    let fetched = fetch_tarball(&tmp.path().join("cache"), &tarball);
    let json = nix_flake_prefetch(&format!("file://{}", tarball.display()));
    assert_eq!(json["hash"], fetched.nar_hash().unwrap().to_sri());
    assert_eq!(
        json["locked"]["lastModified"].as_u64(),
        fetched.input.last_modified().unwrap()
    );
}

/// A Git repository with two commits on `main` (at brix-testutil's fixed
/// date, `lastModified` 1704164645), an executable and a symlink.
fn make_repo() -> GitRepo {
    let repo = GitRepo::new();
    repo.write("hello", "hi\n");
    repo.write("bin/run", "#!/bin/sh\n");
    std::os::unix::fs::symlink("hello", repo.path().join("link")).unwrap();
    repo.git(&["add", "."]);
    repo.git(&["update-index", "--chmod=+x", "bin/run"]);
    repo.git(&["commit", "-q", "-m", "one"]);
    repo.write("hello", "hi again\n");
    repo.git(&["commit", "-q", "-am", "two"]);
    repo
}

/// A local Git repository fetched in place: its revision, `lastModified`,
/// `revCount` and branch.
#[test]
fn git_repository_matches_nix() {
    let repo = make_repo();
    let settings = FetchSettings::default();
    let url = format!("git+file://{}", repo.path().display());
    let input = Input::from_url_str(&settings, &url, true).unwrap();
    let fetcher = NativeFetcher::new()
        .unwrap()
        .with_cache_dir(repo.scratch().join("cache"));
    let fetched = fetch_input(&fetcher, &settings, &input).unwrap();
    assert_eq!(fetched.input.rev_count().unwrap(), Some(2));
    assert_eq!(fetched.input.last_modified().unwrap(), Some(1704164645));
    assert_eq!(fetched.input.ref_().unwrap(), Some("refs/heads/main"));
    let rev = fetched.input.rev().unwrap().unwrap();

    // Fetching by revision through the clone cache yields the same tree.
    let by_rev =
        Input::from_url_str(&settings, &format!("{url}?rev={rev}&ref=main"), true).unwrap();
    let again = fetch_input(&fetcher, &settings, &by_rev).unwrap();
    assert_eq!(again.nar_hash().unwrap(), fetched.nar_hash().unwrap());
}

/// The same repository through `nix flake prefetch`.
#[test]
#[ignore = "writes to the user's Nix store; set BRIX_TEST_REAL_STORE=1"]
fn git_repository_matches_nix_flake_prefetch() {
    require_env(REAL_STORE);
    let repo = make_repo();
    let settings = FetchSettings::default();
    let url = format!("git+file://{}", repo.path().display());
    let input = Input::from_url_str(&settings, &url, true).unwrap();
    let fetcher = NativeFetcher::new()
        .unwrap()
        .with_cache_dir(repo.scratch().join("cache"));
    let fetched = fetch_input(&fetcher, &settings, &input).unwrap();
    let json = nix_flake_prefetch(&url);
    assert_eq!(json["hash"], fetched.nar_hash().unwrap().to_sri());
    assert_eq!(
        json["locked"]["rev"],
        fetched.input.rev().unwrap().unwrap().to_hex()
    );
    assert_eq!(json["locked"]["ref"], "refs/heads/main");
    assert_eq!(json["locked"]["revCount"], 2);
    assert_eq!(json["locked"]["lastModified"], 1704164645);
}

/// Needs the network: fetches the locked nixpkgs of this repository from
/// GitHub and checks the NAR hash in `flake.lock`.
#[test]
#[ignore = "downloads nixpkgs from GitHub; set BRIX_TEST_NETWORK=1"]
fn github_nixpkgs_fetch_matches_lock_file() {
    require_env(NETWORK);
    let settings = FetchSettings::default();
    let input = Input::from_url_str(
        &settings,
        "github:NixOS/nixpkgs/addf7cf5f383a3101ecfba091b98d0a1263dc9b8",
        true,
    )
    .unwrap();
    let fetcher = NativeFetcher::new().unwrap();
    let start = std::time::Instant::now();
    let fetched = fetch_input(&fetcher, &settings, &input).unwrap();
    eprintln!("fetched nixpkgs in {:?}", start.elapsed());
    assert_eq!(
        fetched.nar_hash().unwrap().to_sri(),
        "sha256-hM20uyap1a0M9d344I692r+ik4gTMyj60cQWO+hAYP8="
    );
    assert_eq!(
        StoreDir::new("/nix/store")
            .display(&fetched.store_path(&StoreDir::new("/nix/store")).unwrap()),
        nixpkgs()
    );
    assert_eq!(fetched.input.last_modified().unwrap(), Some(1765186076));
}

/// Hashes the nixpkgs checkout from the local store through the `path`
/// fetcher (hundreds of MB; slow) and compares with `nix hash path`.
#[test]
#[ignore = "hashes a full nixpkgs checkout, which takes a while"]
fn nixpkgs_path_nar_hash_matches_nix() {
    let path = nixpkgs();
    let expected = Oracle::new().args(["hash", "path", "--sri", path]).stdout();
    let settings = FetchSettings::default();
    let input = Input::from_url_str(&settings, &format!("path:{path}"), true).unwrap();
    let fetcher = NativeFetcher::new().unwrap();
    let start = std::time::Instant::now();
    let fetched = fetch_input(&fetcher, &settings, &input).unwrap();
    eprintln!("hashed nixpkgs in {:?}", start.elapsed());
    assert_eq!(fetched.nar_hash().unwrap().to_sri(), expected.trim());
}
