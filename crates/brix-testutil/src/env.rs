//! The variables the tests read.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// The opt-in for tests that talk to the running Nix daemon: builds,
/// store queries, writes through the daemon.
pub const REAL_DAEMON: &str = "BRIX_TEST_REAL_DAEMON";

/// The opt-in for tests that have the reference `nix` write into the
/// user's store (`nix flake prefetch`, derivations).
pub const REAL_STORE: &str = "BRIX_TEST_REAL_STORE";

/// The opt-in for tests that download from the network (GitHub).
pub const NETWORK: &str = "BRIX_TEST_NETWORK";

/// `$name`, which the devshell (`nix develop`) and the flake's checks
/// set. Panics, naming the variable, when it is unset, empty or not
/// Unicode.
pub fn required_env(name: &str) -> String {
    match std::env::var(name) {
        Ok(value) if !value.is_empty() => value,
        Ok(_) => panic!("{name} is empty; run the tests inside `nix develop`, which sets it"),
        Err(std::env::VarError::NotPresent) => {
            panic!("{name} is not set; run the tests inside `nix develop`, which sets it")
        }
        Err(std::env::VarError::NotUnicode(value)) => {
            panic!("{name}={value:?} is not valid Unicode")
        }
    }
}

/// `$BRIX_NIX_BIN`: the reference `nix`, the oracle of the differential
/// tests.
pub fn nix_bin() -> PathBuf {
    let path = PathBuf::from(required_env("BRIX_NIX_BIN"));
    assert!(
        path.is_file(),
        "BRIX_NIX_BIN={} is not a file",
        path.display()
    );
    path
}

/// A program of the reference Nix installation (`nix-instantiate`,
/// `nix-store`, `nix-daemon`, …): the one next to [`nix_bin`].
pub fn nix_tool(name: &str) -> PathBuf {
    let path = nix_bin().with_file_name(name);
    assert!(
        path.exists(),
        "{} does not exist: BRIX_NIX_BIN must be the `nix` of a complete Nix installation",
        path.display()
    );
    path
}

/// The reference `nix-instantiate`.
pub fn nix_instantiate() -> PathBuf {
    nix_tool("nix-instantiate")
}

/// `$BRIX_NIX_SRC`: the reference Nix's source tree, which holds the
/// language test corpus (`tests/functional/lang`) and the unit tests'
/// data.
pub fn nix_src() -> PathBuf {
    let path = PathBuf::from(required_env("BRIX_NIX_SRC"));
    assert!(
        path.join("src/libexpr").is_dir(),
        "BRIX_NIX_SRC={} is not a Nix source tree (it has no src/libexpr)",
        path.display()
    );
    path
}

/// `$BRIX_NIXPKGS`: the nixpkgs tree the corpus, end-to-end and benchmark
/// tests evaluate. Which revision it is varies, so expectations that
/// depend on it come from the reference `nix` on the same tree.
pub fn nixpkgs() -> &'static str {
    static NIXPKGS: OnceLock<String> = OnceLock::new();
    NIXPKGS.get_or_init(|| {
        let path = required_env("BRIX_NIXPKGS");
        assert!(
            Path::new(&path).join("flake.nix").is_file(),
            "BRIX_NIXPKGS={path} is not a nixpkgs tree (it has no flake.nix)"
        );
        path
    })
}

/// The opt-in of an `#[ignore]`d test that needs more than the devshell
/// gives ([`REAL_DAEMON`], [`REAL_STORE`], [`NETWORK`]): returns when
/// `$name` is `1`, and fails the test otherwise. Running an ignored test
/// is a decision, so one that runs without its opt-in fails rather than
/// passing vacuously; `0` fails the same way, and any other value is
/// rejected as a mistake. The test's `#[ignore = "…"]` reason names the
/// variable as `NAME=1`, which `scripts/test-all.sh` looks for.
pub fn require_env(name: &str) {
    if !opted_in(name) {
        panic!("set {name}=1 to run this test");
    }
}

/// Whether `$name` opts in: `1` does, `0` or unset does not, and any
/// other value is an error.
fn opted_in(name: &str) -> bool {
    match std::env::var_os(name) {
        None => false,
        Some(value) if value == "1" => true,
        Some(value) if value == "0" => false,
        Some(value) => panic!("{name} must be 1 or 0, not {value:?}"),
    }
}
