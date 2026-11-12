//! Fetcher names, archive charset handling, and cached trees against Nix.
//! Every oracle uses an isolated writable store and cache; no network is needed.

use std::cell::RefCell;
use std::path::Path;

use brix::{Config, FetchConfig, Session};
use brix_flake::fetch::{fetch_input, tree_nar_hash};
use brix_flake::native::{tarball::unpack_archive, FetchCache, NativeFetcher};
use brix_flake::{FetchSettings, Input};
use brix_store::{CanonPath, FsAccessor, HashFormat};
use brix_testutil::{scratch_store, GitRepo, Oracle, TarBuilder, ZipBuilder, ZIP_UTF8_FLAG};

fn oracle(root: &Path, operation: &str, url: &str) -> brix_testutil::OracleOutput {
    Oracle::new()
        .args(["flake", operation, "--json", url])
        .store(root.join("store").to_str().unwrap())
        .env("XDG_CACHE_HOME", root.join("nix-cache"))
        .env("LC_ALL", "C")
        .config("flake-registry = ")
        .output()
}

#[test]
fn archive_charsets_and_links_match_nix() {
    let root = scratch_store();
    let long_name = [b"top/".as_slice(), &[b'x'; 113], b"\xe9"].concat();
    let cases = [
        (
            "ustar.tar",
            TarBuilder::new()
                .dir(b"top/")
                .file(b"top/caf\xe9", b"1", false)
                .file(b"top/\xff", b"2", true)
                .symlink(b"top/l\xff", b"caf\xe9")
                .hardlink(b"top/h\xe9", b"top/caf\xe9")
                .finish(),
        ),
        (
            "gnu.tar",
            TarBuilder::new()
                .dir(b"top/")
                .gnu_long_name(&long_name)
                .file(&long_name, b"long", false)
                .gnu_long_link(b"target\xe9")
                .symlink(b"top/link", b"truncated")
                .finish(),
        ),
        (
            "pax-utf8.tar",
            TarBuilder::new()
                .dir(b"top/")
                .pax(&[("path", b"top/caf\xc3\xa9")])
                .file(b"top/placeholder", b"pax", false)
                .pax(&[("linkpath", b"caf\xc3\xa9")])
                .symlink(b"top/link", b"placeholder")
                .finish(),
        ),
        (
            "pax-binary.tar",
            TarBuilder::new()
                .dir(b"top/")
                .pax(&[("hdrcharset", b"BINARY"), ("path", b"top/caf\xe9")])
                .file(b"top/placeholder", b"binary", false)
                .finish(),
        ),
        (
            "pax-invalid.tar",
            TarBuilder::new()
                .dir(b"top/")
                .pax(&[("path", b"top/caf\xe9")])
                .file(b"top/placeholder", b"invalid", false)
                .finish(),
        ),
        (
            "raw.zip",
            ZipBuilder::new()
                .member(b"top/caf\x82", b"zip", 0o100644, 0)
                .member(b"top/link", b"caf\x82", 0o120777, ZIP_UTF8_FLAG)
                .finish(),
        ),
        (
            "flagged.zip",
            ZipBuilder::new()
                .member(b"top/caf\xc3\xa9", b"zip", 0o100644, ZIP_UTF8_FLAG)
                .finish(),
        ),
        (
            "flagged-invalid.zip",
            ZipBuilder::new()
                .member(b"top/caf\xe9", b"zip", 0o100644, ZIP_UTF8_FLAG)
                .finish(),
        ),
    ];
    for (name, archive) in cases {
        let path = root.path().join(name);
        std::fs::write(&path, &archive).unwrap();
        let theirs = oracle(
            root.path(),
            "prefetch",
            &format!("tarball+file://{}", path.display()),
        );
        let warnings = RefCell::new(Vec::new());
        let ours = unpack_archive(&archive, name, &|warning| {
            warnings.borrow_mut().push(warning.to_owned())
        });
        assert_eq!(
            ours.is_ok(),
            theirs.success(),
            "{name}: {}",
            theirs.stderr_lossy()
        );
        match ours {
            Ok(unpacked) => {
                let json: serde_json::Value = serde_json::from_slice(&theirs.stdout).unwrap();
                assert_eq!(
                    json["hash"],
                    tree_nar_hash(&unpacked.tree).unwrap().to_sri(),
                    "{name}"
                );
                // Fetcher diagnostics remain text; compare their documented lossy
                // rendering when Nix's warning contains a raw non-UTF-8 pathname.
                let stderr = theirs.stderr_lossy();
                let expected: Vec<_> = stderr
                    .lines()
                    .filter_map(|line| line.strip_prefix("warning: "))
                    .collect();
                assert_eq!(*warnings.borrow(), expected, "{name}");

                let hash = tree_nar_hash(&unpacked.tree).unwrap();
                let cache = FetchCache::open(root.path().join("fetch-cache"));
                let cached_path = cache.store_tree(&hash, &unpacked.tree).unwrap();
                let reopened = FetchCache::open(root.path().join("fetch-cache"));
                assert_eq!(reopened.tree_dir(&hash).unwrap(), Some(cached_path.clone()));
                let cached = FsAccessor::new(cached_path);
                assert_eq!(tree_nar_hash(&cached).unwrap(), hash, "cached {name}");
                assert_eq!(
                    brix_flake::fingerprint::git_tree_hash(&cached).unwrap(),
                    brix_flake::fingerprint::git_tree_hash(&unpacked.tree).unwrap()
                );
            }
            Err(error) => assert!(
                theirs.stderr_lossy().contains(&error.to_string()),
                "{name}: {error}; {}",
                theirs.stderr_lossy()
            ),
        }
    }
}

#[test]
fn git_byte_names_dirty_fingerprints_and_export_ignore_match_nix() {
    let repo = GitRepo::new();
    repo.write("flake.nix", "{ outputs = { self }: {}; }\n");
    repo.write_bytes(b"caf\xe9", b"initial");
    repo.write_bytes(b"d\xe9/keep", b"nested");
    repo.write_bytes(b"d\xe9/hide\xff", b"hidden");
    repo.write_bytes(b"d\xe9/.gitattributes", b"hide* export-ignore\n");
    repo.write_bytes(b"a\xff", b"delete");
    repo.write("a.b", b"initial");
    repo.write_bytes(b"a\xe9", b"initial");
    repo.write("a/b", b"initial");
    repo.symlink_bytes(b"l\xff", b"caf\xe9");
    let rev = repo.commit();
    let root = scratch_store();
    let settings = FetchSettings::default();
    let fetcher = NativeFetcher::new()
        .unwrap()
        .with_cache_dir(root.path().join("fetch-cache"));
    for dirty in [false, true] {
        if dirty {
            repo.write_bytes(b"caf\xe9", b"changed");
            repo.write("a.b", b"changed");
            repo.write_bytes(b"a\xe9", b"changed");
            repo.write("a/b", b"changed");
            std::fs::remove_file(repo.join_bytes(b"a\xff")).unwrap();
            repo.write_bytes(b"untracked\xff", b"must not be fetched");
        }
        for export_ignore in [false, true] {
            let url = format!(
                "git+file://{}?exportIgnore={}{}",
                repo.path().display(),
                u8::from(export_ignore),
                if dirty {
                    String::new()
                } else {
                    format!("&rev={rev}")
                }
            );
            let input = Input::from_url_str(&settings, &url, true).unwrap();
            let fetched = fetch_input(&fetcher, &settings, &input).unwrap();
            // Nix's allow-list checks only lower_bound("/a"). With
            // signed C char, /a\xe9 precedes /a/b and hides directory a
            // in a working tree, though an exported commit includes it.
            assert_eq!(
                fetched
                    .accessor
                    .path_exists(&CanonPath::new("/a").unwrap())
                    .unwrap(),
                !dirty || (0xffu8 as std::ffi::c_char) >= 0,
                "directory visibility: {url}"
            );
            if !dirty && !export_ignore {
                assert_eq!(
                    brix_flake::fingerprint::git_tree_hash(&*fetched.accessor)
                        .unwrap()
                        .to_hex(),
                    repo.git(&["write-tree"]).trim()
                );
            }
            let theirs = oracle(root.path(), "prefetch", &url);
            assert!(theirs.success(), "{}", theirs.stderr_lossy());
            let json: serde_json::Value = serde_json::from_slice(&theirs.stdout).unwrap();
            assert_eq!(json["hash"], fetched.nar_hash().unwrap().to_sri(), "{url}");
            assert_eq!(
                fetched
                    .accessor
                    .read_link(&CanonPath::new(b"/l\xff").unwrap())
                    .unwrap(),
                b"caf\xe9"
            );
            assert!(!fetched
                .accessor
                .path_exists(&CanonPath::new(b"/untracked\xff").unwrap())
                .unwrap());
            assert_eq!(
                fetched
                    .accessor
                    .path_exists(&CanonPath::new(b"/d\xe9/hide\xff").unwrap())
                    .unwrap(),
                !export_ignore
            );

            let session = Session::new(Config {
                fetch: FetchConfig {
                    cache_dir: Some(root.path().join("session-cache")),
                    ..Default::default()
                },
                ..Default::default()
            })
            .unwrap();
            let reference = session.parse_flake_ref(&url, "/").unwrap();
            let flake = session.flake(&reference).unwrap();
            let ours = flake
                .fingerprint()
                .unwrap()
                .unwrap()
                .to_string(HashFormat::Base16, false);
            let theirs = oracle(root.path(), "metadata", &url);
            assert!(theirs.success(), "{}", theirs.stderr_lossy());
            let json: serde_json::Value = serde_json::from_slice(&theirs.stdout).unwrap();
            assert_eq!(json["fingerprint"], ours, "{url}");
        }
    }
}
