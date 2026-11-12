//! The persistent fetch cache against the network: nixpkgs from GitHub is
//! downloaded once and served from the cache afterwards.
//!
//! Needs network access, so it is ignored by default:
//!
//! ```text
//! BRIX_TEST_NETWORK=1 cargo test -p brix-flake --release --test fetch_cache -- --ignored
//! ```

use std::time::Instant;

use brix_flake::{fetch_input, FetchSettings, Fetcher, Input, NativeFetcher};
use brix_store::StoreDir;

const NIXPKGS_REV: &str = "addf7cf5f383a3101ecfba091b98d0a1263dc9b8";
const NIXPKGS_NAR_HASH: &str = "sha256-hM20uyap1a0M9d344I692r+ik4gTMyj60cQWO+hAYP8=";

#[test]
#[ignore = "downloads nixpkgs from GitHub; set BRIX_TEST_NETWORK=1"]
fn nixpkgs_is_downloaded_once() {
    brix_testutil::require_env(brix_testutil::NETWORK);
    let cache_dir = tempfile::tempdir().unwrap();
    let settings = FetchSettings::default();
    let fetcher = NativeFetcher::new()
        .unwrap()
        .with_cache_dir(cache_dir.path().to_path_buf());
    let input = Input::from_url_str(
        &settings,
        &format!("github:NixOS/nixpkgs/{NIXPKGS_REV}"),
        true,
    )
    .unwrap();

    let cold = Instant::now();
    let first = fetch_input(&fetcher, &settings, &input).unwrap();
    let cold = cold.elapsed();
    assert_eq!(first.nar_hash().unwrap().to_sri(), NIXPKGS_NAR_HASH);

    let warm = Instant::now();
    let second = fetch_input(&fetcher, &settings, &input).unwrap();
    let warm = warm.elapsed();
    // The cache answered: `fetch_input` always returns a verified tree, so
    // the evidence is the attributes, the timing and `resolve_cached`.
    assert_eq!(second.input.attrs, first.input.attrs);
    assert_eq!(
        second.store_path(&StoreDir::default()).unwrap(),
        first.store_path(&StoreDir::default()).unwrap()
    );
    eprintln!("cold fetch {cold:?}, warm fetch {warm:?}");
    assert!(
        warm.as_millis() < 500,
        "a cached nixpkgs should load in well under a second, took {warm:?}"
    );

    // The cache resolved the locked attributes without network too.
    let resolved = fetcher.resolve_cached(&settings, &input).unwrap().unwrap();
    let mut expected = first.input.attrs.clone();
    expected.remove("__final"); // `fetch_input` marks its result final
    assert_eq!(resolved.attrs, expected);
}
