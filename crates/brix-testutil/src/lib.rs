//! Test support shared by brix's crates, as a dev-dependency.
//!
//! - The test environment: [`nix_bin`], [`nix_src`], [`nixpkgs`] and the
//!   tools next to the reference `nix` ([`nix_instantiate`],
//!   [`nix_tool`]). The devshell and the flake's checks set the
//!   variables; a test that needs one fails with one message when it is
//!   missing, rather than passing without having compared anything.
//! - [`require_env`]: the opt-in for `#[ignore]`d tests that need more
//!   than that (the running Nix daemon, a writable store, the network).
//! - [`Oracle`]: the reference `nix` (or one of its tools) with a pinned
//!   configuration, its stderr cleaned of what the environment adds.
//! - [`GitRepo`] and [`git`]: Git with a fixed identity and fixed dates;
//!   [`SshKey`] for commits signed with [`GitRepo::commit_signed`].
//! - [`HgRepo`] and [`hg`]: Mercurial with no configuration, a fixed
//!   user and fixed dates.
//! - [`scratch_store`] and [`copy_dir`]; trees with names that are not
//!   UTF-8: [`os`], [`write_bytes`], [`symlink_bytes`], [`join_bytes`]
//!   (in a repository, [`GitRepo::write_bytes`] and the like).
//! - [`LfsServer`]: a Git LFS batch API and object server on 127.0.0.1.
//! - [`TarBuilder`] and [`ZipBuilder`]: archives written byte by byte
//!   (names that are not UTF-8, pax headers, zip flags and extra fields).
//!
//! It depends on no brix crate, so that every member can use it.

#![forbid(unsafe_code)]

mod archive;
mod env;
mod fs;
mod git;
mod hg;
mod lfs;
mod oracle;
mod ssh;

pub use archive::{
    crc32, zip_unicode_path_field, TarBuilder, ZipBuilder, ARCHIVE_MTIME, ZIP_UTF8_FLAG,
};
pub use env::{
    nix_bin, nix_instantiate, nix_src, nix_tool, nixpkgs, require_env, required_env, NETWORK,
    REAL_DAEMON, REAL_STORE,
};
pub use fs::{copy_dir, join_bytes, os, scratch_store, symlink_bytes, write_bytes};
pub use git::{git, GitRepo, GIT_DATE};
pub use hg::{hg, hg_command, HgRepo, HG_DATE};
pub use lfs::{LfsMode, LfsRequest, LfsServer, LFS_AUTHORIZATION};
pub use oracle::{is_noise, strip_noise, Oracle, OracleOutput, ORACLE_NIX_CONFIG};
pub use ssh::SshKey;
