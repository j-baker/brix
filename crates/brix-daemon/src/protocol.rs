//! Constants of the worker protocol (`src/libstore/include/nix/store/worker-protocol.hh`).

use std::collections::BTreeSet;
use std::fmt;

/// Sent by the client to open a connection.
pub const WORKER_MAGIC_1: u64 = 0x6e69_7863;
/// Sent back by the daemon.
pub const WORKER_MAGIC_2: u64 = 0x6478_696f;
/// Sent back by the daemon when the user is not in `allowed-users`.
pub const WORKER_MAGIC_ACCESS_DENIED: u64 = 0xab9a_9ff0;

/// A log line (`STDERR_NEXT`): one string.
pub const STDERR_NEXT: u64 = 0x6f6c_6d67;
/// The daemon wants data from the client's source (`STDERR_READ`): a length.
pub const STDERR_READ: u64 = 0x6461_7461;
/// Data for the client's sink (`STDERR_WRITE`): one string.
pub const STDERR_WRITE: u64 = 0x6461_7416;
/// The operation's stderr phase is over; the reply follows (`STDERR_LAST`).
pub const STDERR_LAST: u64 = 0x616c_7473;
/// The operation failed (`STDERR_ERROR`): a structured error follows.
pub const STDERR_ERROR: u64 = 0x6378_7470;
pub const STDERR_START_ACTIVITY: u64 = 0x5354_5254;
pub const STDERR_STOP_ACTIVITY: u64 = 0x5354_4f50;
pub const STDERR_RESULT: u64 = 0x5253_4c54;
/// A string-named activity; only sent when `named-activities` was negotiated.
pub const STDERR_START_ACTIVITY_STRINGLY: u64 = 0x5354_524e;

/// A protocol version number: major in the high byte, minor in the low byte.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProtocolVersion {
    pub major: u8,
    pub minor: u8,
}

impl ProtocolVersion {
    pub const fn new(major: u8, minor: u8) -> Self {
        ProtocolVersion { major, minor }
    }

    /// The newest version this client speaks: `WorkerProto::latest` in
    /// `worker-protocol.cc`, 1.38, which is what both the client
    /// (`RemoteStore::initConnection`) and the daemon (`processConnection`)
    /// send. (The `PROTOCOL_VERSION` macro in `worker-protocol.hh` says 1.39,
    /// but nothing uses it.)
    pub const LATEST: ProtocolVersion = ProtocolVersion::new(1, 38);

    /// The oldest daemon this client accepts. Nix itself goes back to 1.18;
    /// brix only implements the modern forms of each operation, so it
    /// requires 1.35 (Nix 2.19+), the version that also reports whether the
    /// daemon trusts the client.
    pub const MINIMUM: ProtocolVersion = ProtocolVersion::new(1, 35);

    pub const fn to_wire(self) -> u64 {
        ((self.major as u64) << 8) | self.minor as u64
    }

    pub const fn from_wire(wire: u64) -> Self {
        ProtocolVersion {
            major: ((wire & 0xff00) >> 8) as u8,
            minor: (wire & 0xff) as u8,
        }
    }

    pub const fn at_least(self, major: u8, minor: u8) -> bool {
        self.major > major || (self.major == major && self.minor >= minor)
    }
}

impl fmt::Display for ProtocolVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.major, self.minor)
    }
}

/// Protocol features, negotiated as strings since 1.38 (`WorkerProto::Feature`).
/// Only the ones this client offers or must recognise are listed.
pub mod feature {
    /// `ValidPathInfo` carries a provenance string. Never offered by this
    /// client, but checked so the framing stays right if it ever is.
    pub const PROVENANCE: &str = "provenance";
    /// `AddTempRoots` adds a whole set in one operation.
    pub const ADD_TEMP_ROOTS: &str = "addTempRoots";
    /// Build results carry output paths directly instead of JSON realisations.
    pub const REALISATION_WITH_PATH: &str = "realisation-with-path-not-hash";
    /// The daemon does not want a `SetOptions` after the handshake.
    pub const DISABLE_SET_OPTIONS: &str = "disable-set-options";
    /// Activities may be named strings with metadata
    /// (`STDERR_START_ACTIVITY_STRINGLY`).
    pub const NAMED_ACTIVITIES: &str = "named-activities";
}

/// The features this client offers in the handshake. Only those that change
/// framing the client implements are listed, so a daemon can never make us
/// send or expect bytes we do not understand.
pub fn client_features() -> BTreeSet<String> {
    [
        feature::REALISATION_WITH_PATH,
        feature::ADD_TEMP_ROOTS,
        feature::NAMED_ACTIVITIES,
        feature::DISABLE_SET_OPTIONS,
    ]
    .into_iter()
    .map(String::from)
    .collect()
}

/// The worker operation codes this client sends (`WorkerProto::Op`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u64)]
pub enum Op {
    IsValidPath = 1,
    AddToStore = 7,
    BuildPaths = 9,
    EnsurePath = 10,
    AddTempRoot = 11,
    AddIndirectRoot = 12,
    SetOptions = 19,
    QueryPathInfo = 26,
    QueryPathFromHashPart = 29,
    QueryValidPaths = 31,
    QuerySubstitutablePaths = 32,
    QueryValidDerivers = 33,
    BuildDerivation = 36,
    NarFromPath = 38,
    QueryMissing = 40,
    QueryDerivationOutputMap = 41,
    AddMultipleToStore = 44,
    BuildPathsWithResults = 46,
    AddTempRoots = 49,
}

/// `BuildMode`, sent as a `uint8_t`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BuildMode {
    #[default]
    Normal = 0,
    Repair = 1,
    Check = 2,
}

/// Nix's `Verbosity` (`src/libutil/include/nix/util/error.hh`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Verbosity {
    Error = 0,
    Warn = 1,
    Notice = 2,
    Info = 3,
    Talkative = 4,
    Chatty = 5,
    Debug = 6,
    Vomit = 7,
}

impl Verbosity {
    pub fn from_wire(n: u64) -> Verbosity {
        match n {
            0 => Verbosity::Error,
            1 => Verbosity::Warn,
            2 => Verbosity::Notice,
            3 => Verbosity::Info,
            4 => Verbosity::Talkative,
            5 => Verbosity::Chatty,
            6 => Verbosity::Debug,
            // Nix casts blindly; anything above the range is as verbose as it gets.
            _ => Verbosity::Vomit,
        }
    }
}

/// Whether the daemon trusts the client (`TrustedFlag`, protocol >= 1.35).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Trust {
    Trusted,
    NotTrusted,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_wire_format() {
        assert_eq!(ProtocolVersion::LATEST.to_wire(), (1 << 8) | 38);
        assert_eq!(
            ProtocolVersion::from_wire(0x0123),
            ProtocolVersion::new(1, 0x23)
        );
        assert!(ProtocolVersion::new(1, 38).at_least(1, 38));
        assert!(!ProtocolVersion::new(1, 37).at_least(1, 38));
        assert!(ProtocolVersion::new(2, 0).at_least(1, 38));
        assert!(ProtocolVersion::new(1, 39) > ProtocolVersion::new(1, 38));
    }

    #[test]
    fn magic_numbers_spell_their_names() {
        // The constants are ASCII mnemonics; a typo would be caught here.
        assert_eq!(&WORKER_MAGIC_1.to_be_bytes()[4..], b"nixc");
        assert_eq!(&WORKER_MAGIC_2.to_be_bytes()[4..], b"dxio");
        assert_eq!(&STDERR_NEXT.to_be_bytes()[4..], b"olmg");
        assert_eq!(&STDERR_LAST.to_be_bytes()[4..], b"alts");
        assert_eq!(&STDERR_ERROR.to_be_bytes()[4..], b"cxtp");
        assert_eq!(&STDERR_START_ACTIVITY.to_be_bytes()[4..], b"STRT");
        assert_eq!(&STDERR_STOP_ACTIVITY.to_be_bytes()[4..], b"STOP");
        assert_eq!(&STDERR_RESULT.to_be_bytes()[4..], b"RSLT");
    }
}
