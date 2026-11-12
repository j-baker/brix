//! Receiving the daemon's log stream.
//!
//! While an operation runs, the daemon tunnels its logger to the client
//! (`TunnelLogger` in `src/libstore/daemon.cc`): plain lines, and the
//! activity/result events that `nix build` turns into its progress bar. The
//! client forwards them to a [`Logger`] chosen by the caller.
//!
//! Every text in the stream is bytes, as in Nix (`readString`): a build's
//! output lines are whatever the builder printed, and messages quote names
//! that need not be UTF-8. Nix writes them as they are, and so do brix's
//! loggers.

use std::collections::BTreeMap;

use bstr::BString;

use crate::protocol::Verbosity;

/// A typed value in an activity or result (`Logger::Field`): either an
/// integer or a string (bytes), tagged `0` or `1` on the wire.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Field {
    Int(u64),
    Str(BString),
}

pub type ActivityId = u64;

/// `ActivityType` values Nix defines; unknown ones are passed through as
/// [`ActivityType::Other`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ActivityType {
    Unknown,
    CopyPath,
    FileTransfer,
    Realise,
    CopyPaths,
    Builds,
    Build,
    OptimiseStore,
    VerifyPaths,
    Substitute,
    QueryPathInfo,
    PostBuildHook,
    BuildWaiting,
    FetchTree,
    Other(u64),
}

impl ActivityType {
    pub fn from_wire(n: u64) -> Self {
        match n {
            0 => ActivityType::Unknown,
            100 => ActivityType::CopyPath,
            101 => ActivityType::FileTransfer,
            102 => ActivityType::Realise,
            103 => ActivityType::CopyPaths,
            104 => ActivityType::Builds,
            105 => ActivityType::Build,
            106 => ActivityType::OptimiseStore,
            107 => ActivityType::VerifyPaths,
            108 => ActivityType::Substitute,
            109 => ActivityType::QueryPathInfo,
            110 => ActivityType::PostBuildHook,
            111 => ActivityType::BuildWaiting,
            112 => ActivityType::FetchTree,
            other => ActivityType::Other(other),
        }
    }
}

/// `ResultType` values Nix defines.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResultType {
    FileLinked,
    /// A line of build output; `fields[0]` is the line.
    BuildLogLine,
    UntrustedPath,
    CorruptedPath,
    SetPhase,
    Progress,
    SetExpected,
    PostBuildLogLine,
    FetchStatus,
    HashMismatch,
    BuildResult,
    Other(u64),
}

impl ResultType {
    pub fn from_wire(n: u64) -> Self {
        match n {
            100 => ResultType::FileLinked,
            101 => ResultType::BuildLogLine,
            102 => ResultType::UntrustedPath,
            103 => ResultType::CorruptedPath,
            104 => ResultType::SetPhase,
            105 => ResultType::Progress,
            106 => ResultType::SetExpected,
            107 => ResultType::PostBuildLogLine,
            108 => ResultType::FetchStatus,
            109 => ResultType::HashMismatch,
            110 => ResultType::BuildResult,
            other => ResultType::Other(other),
        }
    }
}

/// An activity started by the daemon.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Activity {
    pub id: ActivityId,
    pub level: Verbosity,
    pub kind: ActivityKind,
    /// Human-readable description (bytes, as Nix's).
    pub text: BString,
    pub parent: ActivityId,
}

/// Activities come in two shapes: the classic typed form and, since the
/// `named-activities` feature, a string-named form with keyed metadata.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ActivityKind {
    Typed {
        kind: ActivityType,
        fields: Vec<Field>,
    },
    Named {
        name: BString,
        metadata: BTreeMap<BString, Field>,
    },
}

/// Receives everything the daemon logs. Every method has a no-op default so
/// a caller can pick only what it cares about (for instance build log lines,
/// which arrive as [`ResultType::BuildLogLine`] results).
pub trait Logger {
    /// A plain message (`STDERR_NEXT`), already stripped of its trailing
    /// newline: bytes, which Nix's `printError` writes as they are.
    fn log(&mut self, _message: &[u8]) {}
    fn start_activity(&mut self, _activity: &Activity) {}
    fn stop_activity(&mut self, _id: ActivityId) {}
    fn result(&mut self, _id: ActivityId, _kind: ResultType, _fields: &[Field]) {}
}

/// Discards everything.
#[derive(Debug, Default, Clone, Copy)]
pub struct NullLogger;

impl Logger for NullLogger {}

/// Collects everything in memory, for tests.
#[cfg(test)]
#[derive(Debug, Default, Clone)]
pub(crate) struct RecordingLogger {
    pub messages: Vec<BString>,
    pub activities: Vec<Activity>,
    pub stopped: Vec<ActivityId>,
    pub results: Vec<(ActivityId, ResultType, Vec<Field>)>,
}

#[cfg(test)]
impl Logger for RecordingLogger {
    fn log(&mut self, message: &[u8]) {
        self.messages.push(message.into());
    }

    fn start_activity(&mut self, activity: &Activity) {
        self.activities.push(activity.clone());
    }

    fn stop_activity(&mut self, id: ActivityId) {
        self.stopped.push(id);
    }

    fn result(&mut self, id: ActivityId, kind: ResultType, fields: &[Field]) {
        self.results.push((id, kind, fields.to_vec()));
    }
}
