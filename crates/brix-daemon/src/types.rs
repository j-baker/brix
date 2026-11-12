//! Data carried by the protocol that has no counterpart in `brix-store`:
//! path infos and build results. Store paths, derived paths, hashes,
//! content addresses and derivations are `brix-store`'s types.

use std::collections::{BTreeMap, BTreeSet};

use brix_store::{ContentAddress, DerivedPath, Hash, StorePath};

use crate::error::DaemonError;

/// A path the daemon reports as valid (`UnkeyedValidPathInfo`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PathInfo {
    pub deriver: Option<StorePath>,
    /// SHA-256 of the NAR serialisation.
    pub nar_hash: Hash,
    pub references: BTreeSet<StorePath>,
    /// Seconds since the epoch.
    pub registration_time: u64,
    pub nar_size: u64,
    /// Built locally rather than substituted.
    pub ultimate: bool,
    /// `<key-name>:<base64 signature>` strings.
    pub sigs: BTreeSet<String>,
    /// Content address, if the path is content-addressed.
    pub ca: Option<ContentAddress>,
}

/// A path plus its info (`ValidPathInfo`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValidPathInfo {
    pub path: StorePath,
    pub info: PathInfo,
}

/// `BuildResult` status codes, in wire order (`buildResultStatusTable`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BuildStatus {
    Built,
    Substituted,
    AlreadyValid,
    PermanentFailure,
    InputRejected,
    OutputRejected,
    TransientFailure,
    CachedFailure,
    TimedOut,
    MiscFailure,
    DependencyFailed,
    LogLimitExceeded,
    NotDeterministic,
    ResolvesToAlreadyValid,
    NoSubstituters,
    HashMismatch,
    Cancelled,
}

impl BuildStatus {
    const TABLE: [BuildStatus; 17] = [
        BuildStatus::Built,
        BuildStatus::Substituted,
        BuildStatus::AlreadyValid,
        BuildStatus::PermanentFailure,
        BuildStatus::InputRejected,
        BuildStatus::OutputRejected,
        BuildStatus::TransientFailure,
        BuildStatus::CachedFailure,
        BuildStatus::TimedOut,
        BuildStatus::MiscFailure,
        BuildStatus::DependencyFailed,
        BuildStatus::LogLimitExceeded,
        BuildStatus::NotDeterministic,
        BuildStatus::ResolvesToAlreadyValid,
        BuildStatus::NoSubstituters,
        BuildStatus::HashMismatch,
        BuildStatus::Cancelled,
    ];

    pub fn from_wire(n: u8) -> Result<Self, DaemonError> {
        Self::TABLE.get(usize::from(n)).copied().ok_or_else(|| {
            DaemonError::protocol(format!("Invalid BuildResult status {n} from remote"))
        })
    }

    #[cfg(test)]
    pub(crate) fn to_wire(self) -> u8 {
        Self::TABLE
            .iter()
            .position(|s| *s == self)
            .expect("status in table") as u8
    }

    pub fn is_success(self) -> bool {
        matches!(
            self,
            BuildStatus::Built
                | BuildStatus::Substituted
                | BuildStatus::AlreadyValid
                | BuildStatus::ResolvesToAlreadyValid
        )
    }
}

/// The outcome of building one derived path (`BuildResult`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BuildResult {
    pub status: BuildStatus,
    /// Empty on success. Bytes, as Nix's: a failed build's message ends
    /// with the builder's last log lines, which are whatever it printed.
    pub error_msg: bstr::BString,
    pub times_built: u64,
    pub is_non_deterministic: bool,
    pub start_time: u64,
    pub stop_time: u64,
    /// CPU time in microseconds (protocol >= 1.37).
    pub cpu_user_us: Option<i64>,
    pub cpu_system_us: Option<i64>,
    /// Output name (bytes, as Nix's) → output path, for the outputs that
    /// were built.
    pub built_outputs: BTreeMap<bstr::BString, StorePath>,
}

impl BuildResult {
    pub fn is_success(&self) -> bool {
        self.status.is_success()
    }
}

/// A build result tagged with what was asked for (`KeyedBuildResult`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyedBuildResult {
    pub path: DerivedPath,
    pub result: BuildResult,
}

/// What `QueryMissing` reports.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Missing {
    pub will_build: BTreeSet<StorePath>,
    pub will_substitute: BTreeSet<StorePath>,
    pub unknown: BTreeSet<StorePath>,
    pub download_size: u64,
    pub nar_size: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_status_table_round_trips() {
        for n in 0..17u8 {
            assert_eq!(BuildStatus::from_wire(n).unwrap().to_wire(), n);
        }
        assert!(BuildStatus::from_wire(17).is_err());
        assert!(BuildStatus::Built.is_success());
        assert!(!BuildStatus::HashMismatch.is_success());
    }
}
