//! The host the measurement examples evaluate against: a directory tree
//! mounted at a path (by default the nixpkgs checkout `$BRIX_NIXPKGS` at
//! its store path), with traces and warnings on stderr. Shared through
//! `#[path]` (a module here is not an example of its own).

use std::sync::Arc;

use brix_eval::{Config, Host};
use brix_store::{CanonPath, FsAccessor, MemoryAccessor, MountedAccessor, SourceAccessor};

pub struct MountHost {
    root: Arc<dyn SourceAccessor>,
    allowed: Vec<CanonPath>,
}

impl MountHost {
    /// Mounts `tree` at `at` and allows reading `allowed`.
    pub fn new(at: &str, tree: Arc<dyn SourceAccessor>, allowed: &str) -> MountHost {
        let mut mounted = MountedAccessor::new(Arc::new(MemoryAccessor::new()));
        mounted.mount(CanonPath::new(at).expect("absolute mount path"), tree);
        MountHost {
            root: Arc::new(mounted),
            allowed: vec![CanonPath::new(allowed).expect("absolute allowed path")],
        }
    }

    /// The directory [`mount_path`] names, mounted at its own path.
    pub fn from_env() -> MountHost {
        let at = mount_path();
        MountHost::new(&at, Arc::new(FsAccessor::new(&at)), &at)
    }
}

/// `$BRIX_MOUNT`, or else `$BRIX_NIXPKGS`; one of them must be set.
pub fn mount_path() -> String {
    match std::env::var("BRIX_MOUNT") {
        Ok(at) => at,
        Err(std::env::VarError::NotPresent) => std::env::var("BRIX_NIXPKGS").expect(
            "set BRIX_MOUNT to the tree to mount, or BRIX_NIXPKGS to the nixpkgs checkout \
             (run inside `nix develop`)",
        ),
        Err(e) => panic!("BRIX_MOUNT: {e}"),
    }
}

/// The evaluator configuration of the measurement examples: the default,
/// with [`Config::timings`] from `BRIX_TIMINGS` as the brix CLI reads it
/// (`1` on, `0` or unset off, anything else an error).
pub fn config_from_env() -> Config {
    let timings = match std::env::var_os("BRIX_TIMINGS") {
        None => false,
        Some(v) if v == "1" => true,
        Some(v) if v == "0" => false,
        Some(v) => panic!("BRIX_TIMINGS must be 1 or 0, not {v:?}"),
    };
    Config {
        timings,
        ..Config::default()
    }
}

impl Host for MountHost {
    fn root_accessor(&self) -> Arc<dyn SourceAccessor> {
        self.root.clone()
    }

    fn allowed_paths(&self) -> Vec<CanonPath> {
        self.allowed.clone()
    }
}
