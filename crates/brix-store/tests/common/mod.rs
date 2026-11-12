//! Shared helpers for the integration tests: the reference `nix` and
//! `nix-store`, and the trees and fixtures they are compared on.
#![allow(dead_code)]

use std::path::{Path, PathBuf};

use brix_testutil::Oracle;

/// The reference `nix` (or `nix-store`) with ca-derivations on, which the
/// content-addressing comparisons need.
fn oracle(tool: Option<&str>, args: &[&str]) -> Oracle {
    let mut cmd = match tool {
        Some(tool) => Oracle::tool(tool),
        None => Oracle::new(),
    };
    cmd.config("extra-experimental-features = ca-derivations")
        .args(args);
    cmd
}

/// Runs `nix <args>`, which must succeed; its stdout without the final
/// newline.
pub fn nix(args: &[&str]) -> String {
    oracle(None, args).stdout().trim_end().to_owned()
}

/// Runs `nix <args>`, which must succeed; its stdout as bytes.
pub fn nix_bytes(args: &[&str]) -> Vec<u8> {
    let cmd = oracle(None, args);
    let out = cmd.output();
    assert!(
        out.success(),
        "{} failed:\n{}",
        cmd.describe(),
        out.stderr_lossy()
    );
    out.stdout
}

/// Runs `nix-store <args>` with `stdin`; whether it succeeded, and its
/// stderr.
pub fn nix_store_status(args: &[&str], stdin: &[u8]) -> (bool, String) {
    let out = oracle(Some("nix-store"), args).stdin(stdin).output();
    (out.success(), out.stderr_lossy())
}

/// Runs `nix-store <args>`, which must succeed; its stdout as bytes.
pub fn nix_store_bytes(args: &[&str]) -> Vec<u8> {
    let cmd = oracle(Some("nix-store"), args);
    let out = cmd.output();
    assert!(
        out.success(),
        "{} failed:\n{}",
        cmd.describe(),
        out.stderr_lossy()
    );
    out.stdout
}

pub fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

/// Build a small but varied tree on disk: nested directories, an empty
/// directory, executable and plain files, a symlink, names that sort
/// differently by byte and by locale.
pub fn make_sample_tree(root: &Path) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(root.join("sub/deeper")).unwrap();
    std::fs::create_dir_all(root.join("empty")).unwrap();
    std::fs::write(root.join("b.txt"), "hello\n").unwrap();
    std::fs::write(root.join("B.txt"), "upper first in byte order\n").unwrap();
    std::fs::write(
        root.join("sub/deeper/data.bin"),
        (0u8..=255).collect::<Vec<u8>>(),
    )
    .unwrap();
    std::fs::write(root.join("sub/run.sh"), "#!/bin/sh\necho hi\n").unwrap();
    std::fs::set_permissions(
        root.join("sub/run.sh"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    std::fs::write(root.join("sub/seven"), "1234567").unwrap();
    std::fs::write(root.join("sub/eight"), "12345678").unwrap();
    std::fs::write(root.join("zero"), "").unwrap();
    std::os::unix::fs::symlink("b.txt", root.join("link")).unwrap();
    std::os::unix::fs::symlink("/absolute/target", root.join("abslink")).unwrap();
}
