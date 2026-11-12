//! A client for the Nix daemon's worker protocol.
//!
//! brix evaluates Nix expressions but never builds anything itself. When
//! evaluation needs a build (import-from-derivation) or the content of a
//! store path, it asks a real Nix daemon through this crate: over the
//! daemon's unix socket, or over `ssh-ng://` by running `nix-daemon --stdio`
//! on another machine. Nothing here manages a store, a database or garbage
//! collection; it is the subset of `RemoteStore` an evaluator needs.
//!
//! The reference is Nix 2.35 (`src/libstore/worker-protocol*.{cc,hh}`,
//! `remote-store.cc`, `daemon.cc`); every message format is cited at its
//! implementation.
//!
//! ```no_run
//! use brix_daemon::{ClientOptions, DaemonClient, StorePath, StoreUri};
//!
//! let mut client = DaemonClient::connect(&StoreUri::daemon(), ClientOptions::default())?;
//! let path = StorePath::new("9xy8d7g1qf0n7hjbw4gj5zr3sx8yx3ra-hello-2.12.2")?;
//! if let Some(info) = client.query_path_info(&path)? {
//!     println!("{} references", info.references.len());
//! }
//! # Ok::<(), brix_daemon::DaemonError>(())
//! ```

#![cfg(unix)]
#![forbid(unsafe_code)]

mod client;
#[cfg(test)]
mod client_tests;
mod error;
pub mod logger;
mod protocol;
mod transport;
mod types;
mod uri;
mod wire;

pub use client::{ClientOptions, ClientSettings, DaemonClient, PathToAdd};
pub use error::{DaemonError, RemoteError};
pub use logger::{Logger, NullLogger};
pub use protocol::{BuildMode, ProtocolVersion, Trust, Verbosity};
pub use types::{BuildResult, BuildStatus, KeyedBuildResult, Missing, PathInfo, ValidPathInfo};
// The store types that appear in this crate's API, for callers that do not
// depend on `brix-store` directly.
pub use brix_store::{
    ContentAddress, ContentAddressMethod, Derivation, DerivationOutput, DerivedPath, Hash,
    HashAlgo, MemoryAccessor, OutputsSpec, StoreDir, StorePath,
};
pub use uri::{default_socket_path, SshTarget, StoreUri, Transport};
