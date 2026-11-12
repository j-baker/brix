//! Shared helpers for the facade's integration tests: the nixpkgs tree
//! (`$BRIX_NIXPKGS`), locked for pure evaluation, and what the reference
//! `nix` evaluates in it.

#![allow(dead_code)]

use std::sync::OnceLock;

use brix_testutil::{nixpkgs, Oracle};

/// The NAR hash of the checkout, as `nix hash path` computes it; it locks
/// the `fetchTree` call that pure evaluation needs. Computed once per test
/// binary.
pub fn nixpkgs_nar_hash() -> &'static str {
    static HASH: OnceLock<String> = OnceLock::new();
    HASH.get_or_init(|| nix(&["hash", "path", "--sri", nixpkgs()]).trim().to_owned())
}

/// `(builtins.fetchTree { type = "path"; … })` for the checkout, locked by
/// its NAR hash so that it evaluates in pure mode, for `nix` and for brix.
pub fn locked_nixpkgs_expr() -> String {
    format!(
        "(builtins.fetchTree {{ type = \"path\"; path = \"{}\"; narHash = \"{}\"; }})",
        nixpkgs(),
        nixpkgs_nar_hash()
    )
}

/// What the reference `nix` evaluates `hello.drvPath` to in the checkout
/// (`nix eval --raw` on the locked tree, bypassing the evaluation cache).
/// Which revision the checkout is varies, so every expectation that
/// depends on it comes from here, never from a literal. Computed once per
/// test binary.
pub fn hello_drv() -> &'static str {
    static DRV: OnceLock<String> = OnceLock::new();
    DRV.get_or_init(|| {
        let expr = format!(
            "(import {} {{ system = \"x86_64-linux\"; }}).hello.drvPath",
            locked_nixpkgs_expr()
        );
        nix(&["eval", "--no-eval-cache", "--raw", "--expr", &expr])
    })
}

/// The derivation name (`hello-<version>`) of [`hello_drv`].
pub fn hello_drv_name() -> &'static str {
    let base = hello_drv().rsplit('/').next().unwrap();
    base[33..].strip_suffix(".drv").unwrap()
}

/// Runs the reference `nix` with `args`, which must succeed; its stdout.
pub fn nix(args: &[&str]) -> String {
    Oracle::new().args(args).stdout()
}

/// A session with `Config::default()`: hermetic (no daemon), fetching on.
pub fn session() -> brix::Session {
    brix::Session::new(brix::Config::default()).expect("create a session")
}
