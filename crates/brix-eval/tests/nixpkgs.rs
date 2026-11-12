//! Evaluating real nixpkgs: the checkout `$BRIX_NIXPKGS` points at (a
//! `…-source` tree in the store, whichever revision the devshell or the
//! flake's checks provide), mounted at its store path so that path
//! literals resolve exactly as they do for Nix.
//!
//! Every expectation comes from the reference `nix` (`$BRIX_NIX_BIN`)
//! evaluating the same expression on the same tree at test time, so the
//! suite holds for any nixpkgs revision. Since `nix eval` is pure, the
//! oracle imports the tree through `builtins.fetchTree` locked by its NAR
//! hash, which gives the very same store path as the literal brix
//! imports.
//!
//! These tests are slow in debug builds; run them with
//! `cargo test --release -p brix-eval --test nixpkgs -- --ignored --nocapture`.

mod common;

use std::path::Path;
use std::sync::OnceLock;
use std::time::Instant;

use brix_eval::Config;
use brix_testutil::{nixpkgs, Oracle};
use bstr::{BString, ByteSlice};
use common::{run_brix_expr, Format, TestHost};

fn nixpkgs_host() -> TestHost {
    let nixpkgs = nixpkgs();
    TestHost::new(&[(nixpkgs, Path::new(nixpkgs))])
}

/// The placeholder in expression templates that stands for the nixpkgs
/// tree: brix gets the store path literal, `nix` the locked `fetchTree`
/// call (see [`nix_expr`]).
const TREE: &str = "@NIXPKGS@";

/// Evaluates `template` (with [`TREE`] substituted) through brix and
/// returns the printed value.
fn eval_raw(template: &str) -> BString {
    let out = brix_outcome(template);
    assert!(out.ok, "evaluation failed:\n{}", out.stderr);
    out.stdout
}

fn brix_expr(template: &str) -> String {
    template.replace(TREE, nixpkgs())
}

fn brix_outcome(template: &str) -> common::Outcome {
    let expr = brix_expr(template);
    let host = nixpkgs_host();
    let start = Instant::now();
    let out = run_brix_expr(&host, Config::default(), &expr, "/", Format::Plain);
    eprintln!(
        "{expr}\n  -> {} in {:?}",
        out.stdout.trim().as_bstr(),
        start.elapsed()
    );
    out
}

/// Runs the reference `nix` with `args`, returning stdout on success and
/// stderr on failure.
fn nix(args: &[&str]) -> Result<String, String> {
    let out = Oracle::new().args(args).output();
    if out.success() {
        Ok(String::from_utf8(out.stdout).expect("nix printed UTF-8"))
    } else {
        Err(out.stderr_lossy())
    }
}

/// The checkout's NAR hash as `nix hash path` computes it, once per test
/// binary.
fn nixpkgs_nar_hash() -> &'static str {
    static HASH: OnceLock<String> = OnceLock::new();
    HASH.get_or_init(|| {
        nix(&["hash", "path", "--sri", nixpkgs()])
            .unwrap_or_else(|e| panic!("nix hash path {} failed:\n{e}", nixpkgs()))
            .trim()
            .to_owned()
    })
}

/// `template` for the oracle: the tree as a `fetchTree` call locked by
/// its NAR hash, which pure `nix eval` accepts and which lands on the
/// same store path.
fn nix_expr(template: &str) -> String {
    template.replace(
        TREE,
        &format!(
            "(builtins.fetchTree {{ type = \"path\"; path = \"{}\"; narHash = \"{}\"; }})",
            nixpkgs(),
            nixpkgs_nar_hash()
        ),
    )
}

/// `nix eval --raw` (bypassing the evaluation cache) on `template`.
fn nix_raw(template: &str) -> Result<String, String> {
    let expr = nix_expr(template);
    let start = Instant::now();
    let r = nix(&["eval", "--no-eval-cache", "--raw", "--expr", &expr]);
    eprintln!(
        "nix: {}\n  -> {} in {:?}",
        brix_expr(template),
        r.as_deref().unwrap_or("<error>"),
        start.elapsed()
    );
    r
}

/// What brix's plain printer makes of the string `nix eval --raw` gave.
fn quoted(raw: &str) -> String {
    format!("\"{raw}\"\n")
}

fn oracle_raw(template: &str) -> String {
    nix_raw(template).unwrap_or_else(|e| panic!("nix failed on {}:\n{e}", brix_expr(template)))
}

/// `(import <tree> { system = "x86_64-linux"; }).<attr>`.
fn pkgs_attr(attr: &str) -> String {
    format!("(import {TREE} {{ system = \"x86_64-linux\"; }}).{attr}")
}

#[test]
#[ignore = "slow: evaluates nixpkgs"]
fn lib_version() {
    let template = pkgs_attr("lib.version");
    assert_eq!(eval_raw(&template), quoted(&oracle_raw(&template)));
}

#[test]
#[ignore = "slow: evaluates nixpkgs"]
fn hello_drv_path() {
    let template = pkgs_attr("hello.drvPath");
    assert_eq!(eval_raw(&template), quoted(&oracle_raw(&template)));
}

/// A broad sample of nixpkgs whose derivation paths are compared with
/// `nix eval --no-eval-cache --raw` on the same expression. The first two
/// reach a `finalAttrs` fixpoint through `overrideAttrs` and once reported
/// a spurious infinite recursion, because `?` forced the final attribute's
/// value.
const PACKAGES: &[&str] = &[
    "firefox",
    "gobject-introspection",
    "python3Packages.numpy",
    "nodejs",
    "rustc",
    "go",
    "gcc",
    "llvmPackages.clang",
    "chromium",
    "libreoffice",
    "nautilus",
    "texlive.combined.scheme-small",
    "haskellPackages.pandoc",
    "jdk",
    "emacs",
    "vim",
    "git",
    "openssl",
    "curl",
    "systemd",
    "linux",
    "nixos-rebuild",
    "docker",
    "kubernetes",
    "postgresql",
    "ffmpeg",
    "qemu",
    "wine",
];

/// Does `nix`'s stderr say the requested attribute does not exist (as
/// opposed to any other evaluation failure)?
fn reports_missing_attribute(stderr: &str) -> bool {
    stderr
        .lines()
        .any(|l| l.contains("error: attribute '") && l.contains("' missing"))
}

#[test]
#[ignore = "slow: evaluates nixpkgs"]
fn package_drv_paths() {
    let mut failures = Vec::new();
    let mut skipped = Vec::new();
    for attr in PACKAGES {
        let template = pkgs_attr(&format!("{attr}.drvPath"));
        let want = match nix_raw(&template) {
            Ok(raw) => quoted(&raw),
            Err(stderr) if reports_missing_attribute(&stderr) => {
                // The attribute is gone from this nixpkgs revision: nothing
                // to compare, but brix must agree that it is missing.
                eprintln!(
                    "NOTE: skipping {attr}: nix reports it missing from {}:\n{stderr}",
                    nixpkgs()
                );
                let out = brix_outcome(&template);
                if out.ok {
                    failures.push(format!(
                        "{attr}: nix reports the attribute missing, brix evaluated it to {:?}",
                        out.stdout
                    ));
                }
                skipped.push(*attr);
                continue;
            }
            Err(stderr) => panic!("nix failed on {}:\n{stderr}", brix_expr(&template)),
        };
        let got = eval_raw(&template);
        if got != want {
            failures.push(format!("{attr}: got {got:?}, nix gives {want:?}"));
        }
    }
    if !skipped.is_empty() {
        eprintln!(
            "NOTE: {} of {} packages skipped because nix reports them missing from {}: {}",
            skipped.len(),
            PACKAGES.len(),
            nixpkgs(),
            skipped.join(", ")
        );
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// A whole NixOS system: `nix eval --no-eval-cache --raw` on the same
/// configuration gives the same derivation.
#[test]
#[ignore = "slow: evaluates a NixOS system"]
fn nixos_system_toplevel() {
    let template = format!(
        "(import ({TREE} + \"/nixos\") {{ configuration = {{ fileSystems.\"/\" = {{ device = \"/dev/null\"; fsType = \"ext4\"; }}; boot.loader.grub.enable = false; system.stateVersion = \"26.05\"; }}; system = \"x86_64-linux\"; }}).config.system.build.toplevel.drvPath"
    );
    assert_eq!(eval_raw(&template), quoted(&oracle_raw(&template)));
}

/// Nix's lazy trees show a `fetchTree`d file under a store path of its
/// own; brix shows the tree where it is mounted. Rewrites the former to
/// the latter so that messages can be compared.
fn with_tree_paths(message: &str) -> String {
    let mut out = String::new();
    let mut rest = message;
    while let Some(i) = rest.find("/nix/store/") {
        let (before, from_store) = rest.split_at(i);
        out.push_str(before);
        let base_end = from_store[11..]
            .find(|c: char| c == '/' || c.is_whitespace())
            .map_or(from_store.len(), |n| 11 + n);
        let base = &from_store[11..base_end];
        if base.len() == 32 + "-source".len() && base.ends_with("-source") {
            out.push_str(nixpkgs());
        } else {
            out.push_str(&from_store[..base_end]);
        }
        rest = &from_store[base_end..];
    }
    out.push_str(rest);
    out
}

/// Unfree packages are refused with nixpkgs' own message, as in Nix.
#[test]
#[ignore = "slow: evaluates nixpkgs"]
fn unfree_package_is_refused() {
    let template = pkgs_attr("vscode.drvPath");
    let stderr = match nix_raw(&template) {
        Err(stderr) => stderr,
        Ok(raw) => panic!("nix evaluated vscode.drvPath to {raw:?}; expected an unfree refusal"),
    };
    // The last `error:` line of the trace is the message itself.
    let message = stderr
        .lines()
        .map(str::trim_start)
        .rfind(|l| l.starts_with("error:"))
        .unwrap_or_else(|| panic!("no error line in nix's output:\n{stderr}"));
    assert!(
        message.contains("unfree"),
        "nix refused vscode for another reason: {message}"
    );
    let expected = with_tree_paths(message);
    let out = brix_outcome(&template);
    assert!(!out.ok);
    assert!(out.stderr.contains_str(&expected), "{}", out.stderr);
}
