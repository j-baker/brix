//! The client against a scripted fake daemon over a socket pair.
//!
//! The fake speaks the server side of the protocol exactly as
//! `src/libstore/daemon.cc` does for the opcodes brix uses, so these tests
//! pin the byte-level framing without needing a real `nix-daemon`.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufReader, BufWriter, Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use bstr::BString;

use crate::client::{ClientOptions, ClientSettings, DaemonClient, PathToAdd};
use crate::types::{PathInfo, ValidPathInfo};
use brix_store::{ContentAddressMethod, HashFormat};
use crate::error::DaemonError;
use crate::logger::{Field, RecordingLogger, ResultType};
use crate::protocol::{self, BuildMode, ProtocolVersion, Trust};
use crate::transport::Connection;
use brix_store::{
    CanonPath, ContentAddress, Derivation, DerivationOutput, DerivedPath, Hash, HashAlgo,
    MemoryAccessor, OutputsSpec, SourceAccessor, StoreDir, StorePath,
};

use crate::types::BuildStatus;
use crate::uri::StoreUri;
use crate::wire::{FramedReader, WireRead, WireWrite};

const HELLO: &str = "9xy8d7g1qf0n7hjbw4gj5zr3sx8yx3ra-hello-2.12.2";
const HELLO_DRV: &str = "4l8fc1b4rqc1wl6mv6ym3x0a2a9h4jpc-hello-2.12.2.drv";
const GLIBC: &str = "c4b0bxk3bx7bn2b7x7hqhd1zs4l2ssgy-glibc-2.40-66";

/// A failed build's message (`BuildResult::errorMsg`) for a builder that
/// printed bytes that are not UTF-8 (`printf 'first \377 line\n'; printf
/// 'second \351\n' >&2; exit 3`): Nix ends it with the last log lines, as
/// they are.
const FAILED_BUILD_MESSAGE: &[u8] = b"Cannot build '/nix/store/x-fails-bytes.drv'.\nReason: builder failed with exit code 3.\nLast 2 log lines:\n> first \xff line\n> second \xe9";

/// The tree the fake serves for `NarFromPath`: a directory with an
/// executable in a subdirectory, a symlink and a plain file.
fn sample_tree() -> MemoryAccessor {
    let mut tree = MemoryAccessor::new();
    tree.add_dir(&CanonPath::root()).unwrap();
    tree.add_file(
        &CanonPath::new("/bin/hello").unwrap(),
        b"#!/bin/sh\necho hello\n".to_vec(),
        true,
    )
    .unwrap();
    tree.add_symlink(&CanonPath::new("/link").unwrap(), "bin/hello")
        .unwrap();
    tree.add_file(&CanonPath::new("/readme").unwrap(), b"hi".to_vec(), false)
        .unwrap();
    tree
}

fn nar_of(tree: &MemoryAccessor) -> Vec<u8> {
    let mut nar = Vec::new();
    brix_store::nar::dump(tree, &CanonPath::root(), &mut nar).unwrap();
    nar
}

/// What the fake observed, for assertions after the client is done.
#[derive(Default)]
struct Observed {
    ops: Vec<u64>,
    set_options: Option<Vec<u64>>,
    overrides: BTreeMap<String, String>,
    client_features: BTreeSet<String>,
    added: Vec<(String, String, Vec<u8>)>,
    added_multiple: Vec<AddedPath>,
    built: Vec<Vec<String>>,
}

/// `AddMultipleToStore`: path, NAR hash, references, content address, NAR.
type AddedPath = (String, String, Vec<String>, String, Vec<u8>);

struct Fake {
    reader: BufReader<UnixStream>,
    writer: BufWriter<UnixStream>,
    version: ProtocolVersion,
    features: BTreeSet<String>,
    observed: Arc<Mutex<Observed>>,
}

impl Fake {
    fn path(&self, p: &str) -> String {
        format!("/nix/store/{p}")
    }

    /// `BasicServerConnection::handshake` + `postHandshake`.
    fn handshake(&mut self, daemon_version: ProtocolVersion, daemon_features: &[&str]) {
        let r = &mut self.reader;
        assert_eq!(r.read_u64().unwrap(), protocol::WORKER_MAGIC_1);
        let client_version = ProtocolVersion::from_wire(r.read_u64().unwrap());
        assert_eq!(client_version, ProtocolVersion::LATEST);
        self.writer.write_u64(protocol::WORKER_MAGIC_2).unwrap();
        self.writer.write_u64(daemon_version.to_wire()).unwrap();
        self.writer.flush().unwrap();
        self.version = client_version.min(daemon_version);
        if self.version.at_least(1, 38) {
            let client_features = self.reader.read_string_set().unwrap();
            self.writer.write_strings(daemon_features).unwrap();
            self.writer.flush().unwrap();
            self.features = daemon_features
                .iter()
                .map(|s| s.to_string())
                .filter(|f| client_features.contains(f))
                .collect();
            self.observed.lock().unwrap().client_features = client_features;
        }
        // postHandshake: cpu affinity (0 means no second int), reserveSpace.
        assert_eq!(self.reader.read_u64().unwrap(), 0);
        self.reader.read_u64().unwrap();
        self.writer.write_str("2.35.3").unwrap();
        self.writer.write_u64(1).unwrap(); // Trusted
                                           // Startup messages, then the reply marker. Nix chomps log lines
                                           // of spaces, tabs and line ends.
        self.writer.write_u64(protocol::STDERR_NEXT).unwrap();
        self.writer
            .write_str("warning: startup message \t\r\n")
            .unwrap();
        self.writer.write_u64(protocol::STDERR_LAST).unwrap();
        self.writer.flush().unwrap();
    }

    fn read_op(&mut self) -> Option<u64> {
        let op = self.reader.read_u64().ok()?;
        self.observed.lock().unwrap().ops.push(op);
        Some(op)
    }

    fn set_options(&mut self) {
        let mut ints = Vec::new();
        for _ in 0..12 {
            ints.push(self.reader.read_u64().unwrap());
        }
        let overrides = self.reader.read_string_map().unwrap();
        let mut o = self.observed.lock().unwrap();
        o.set_options = Some(ints);
        o.overrides = overrides;
        drop(o);
        self.last();
    }

    fn last(&mut self) {
        self.writer.write_u64(protocol::STDERR_LAST).unwrap();
        self.writer.flush().unwrap();
    }

    /// A string read as bytes, with `escape_ascii`'s escapes.
    fn read_shown(&mut self) -> String {
        self.reader.read_bytes().unwrap().escape_ascii().to_string()
    }

    fn error(&mut self, msg: &str, traces: &[&str]) {
        let w = &mut self.writer;
        w.write_u64(protocol::STDERR_ERROR).unwrap();
        w.write_str("Error").unwrap();
        w.write_u64(0).unwrap(); // level
        w.write_str("Error").unwrap(); // removed name field
        w.write_str(msg).unwrap();
        w.write_u64(0).unwrap(); // no position
        w.write_u64(traces.len() as u64).unwrap();
        for t in traces {
            w.write_u64(0).unwrap();
            w.write_str(t).unwrap();
        }
        w.flush().unwrap();
    }

    fn path_info(&mut self, deriver: &str, refs: &[&str]) {
        let deriver = if deriver.is_empty() {
            String::new()
        } else {
            self.path(deriver)
        };
        let refs: Vec<String> = refs.iter().map(|r| self.path(r)).collect();
        let w = &mut self.writer;
        w.write_str(&deriver).unwrap();
        w.write_str(&"ab".repeat(32)).unwrap();
        w.write_strings(&refs).unwrap();
        w.write_u64(1_700_000_000).unwrap();
        w.write_u64(123_456).unwrap();
        w.write_bool(true).unwrap();
        w.write_strings(["cache.nixos.org-1:c2lnbmF0dXJl"]).unwrap();
        w.write_str("").unwrap();
    }

    /// The start of a `BuildResult` Nix would reject, chosen by the path's
    /// name: an unknown status, a CPU time with a bad optional tag, or one
    /// that does not fit an `int64_t`.
    fn bad_build_result(&mut self, path: &str) {
        let w = &mut self.writer;
        if path.contains("-bad-status") {
            w.write_u64(99).unwrap();
            return;
        }
        w.write_u64(0).unwrap(); // Built
        w.write_str("").unwrap();
        w.write_u64(1).unwrap();
        w.write_bool(false).unwrap();
        w.write_u64(10).unwrap();
        w.write_u64(20).unwrap();
        if path.contains("-bad-tag") {
            w.write_u64(5).unwrap();
        } else {
            w.write_u64(1).unwrap();
            w.write_u64(u64::MAX).unwrap();
        }
    }

    fn build_result(&mut self, status: BuildStatus, msg: &[u8], outputs: &[(&str, &str)]) {
        let outs: Vec<(String, String)> = outputs
            .iter()
            .map(|(n, p)| (n.to_string(), self.path(p)))
            .collect();
        let with_path = self
            .features
            .contains(protocol::feature::REALISATION_WITH_PATH);
        let v = self.version;
        let w = &mut self.writer;
        w.write_u64(status.to_wire() as u64).unwrap();
        w.write_bytes(msg).unwrap();
        w.write_u64(1).unwrap(); // timesBuilt
        w.write_bool(false).unwrap();
        w.write_u64(10).unwrap(); // startTime
        w.write_u64(20).unwrap(); // stopTime
        if v.at_least(1, 37) {
            w.write_u64(1).unwrap();
            w.write_u64(5000).unwrap(); // cpuUser
            w.write_u64(0).unwrap(); // no cpuSystem
        }
        if with_path {
            w.write_u64(outs.len() as u64).unwrap();
            for (n, p) in &outs {
                w.write_str(n).unwrap();
                w.write_str(p).unwrap();
                w.write_strings(Vec::<String>::new()).unwrap();
            }
        } else {
            w.write_u64(outs.len() as u64).unwrap();
            for (n, p) in &outs {
                let base = p.rsplit('/').next().unwrap();
                w.write_str(&format!("sha256:{}!{n}", "0".repeat(64)))
                    .unwrap();
                w.write_str(&format!("{{\"id\":\"x\",\"outPath\":\"{base}\"}}"))
                    .unwrap();
            }
        }
    }

    /// What a 2.35 daemon's `TunnelLogger` sends for a build and for a
    /// named activity (`named-activities` negotiated): a typed activity
    /// with string and integer fields, then
    /// `STDERR_START_ACTIVITY_STRINGLY` with keyed metadata.
    fn activities(&mut self) {
        let w = &mut self.writer;
        w.write_u64(protocol::STDERR_START_ACTIVITY).unwrap();
        w.write_u64(11).unwrap(); // id
        w.write_u64(3).unwrap(); // lvlInfo
        w.write_u64(105).unwrap(); // actBuild
        w.write_str("building '/nix/store/x-hello.drv'").unwrap();
        w.write_u64(4).unwrap();
        for (tag, s) in [(1, "/nix/store/x-hello.drv"), (1, "")] {
            w.write_u64(tag).unwrap();
            w.write_str(s).unwrap();
        }
        for n in [1, 1] {
            w.write_u64(0).unwrap();
            w.write_u64(n).unwrap();
        }
        w.write_u64(0).unwrap(); // parent
        w.write_u64(protocol::STDERR_START_ACTIVITY_STRINGLY)
            .unwrap();
        w.write_u64(12).unwrap();
        w.write_u64(3).unwrap();
        w.write_str("WaitingForLock").unwrap();
        w.write_str("waiting for lock on '/nix/store/x-hello'")
            .unwrap();
        w.write_u64(2).unwrap();
        w.write_str("nix.path").unwrap();
        w.write_u64(1).unwrap();
        w.write_str("/nix/store/x-hello").unwrap();
        w.write_str("attempt").unwrap();
        w.write_u64(0).unwrap();
        w.write_u64(2).unwrap();
        w.write_u64(11).unwrap(); // parent
        w.write_u64(protocol::STDERR_STOP_ACTIVITY).unwrap();
        w.write_u64(12).unwrap();
    }

    /// Log traffic whose texts are not UTF-8, as a daemon tunnels it for a
    /// builder that prints such bytes (`TunnelLogger` sends every string
    /// as it is): a plain message with trailing blanks, a typed and a
    /// named activity, and a build log line.
    fn byte_log(&mut self) {
        let w = &mut self.writer;
        w.write_u64(protocol::STDERR_NEXT).unwrap();
        w.write_bytes(b"warning: caf\xe9 \t\r\n").unwrap();
        w.write_u64(protocol::STDERR_START_ACTIVITY).unwrap();
        w.write_u64(21).unwrap(); // id
        w.write_u64(3).unwrap(); // lvlInfo
        w.write_u64(105).unwrap(); // actBuild
        w.write_bytes(b"building '\xff'").unwrap();
        w.write_u64(1).unwrap();
        w.write_u64(1).unwrap();
        w.write_bytes(b"/nix/store/x-caf\xe9.drv").unwrap();
        w.write_u64(0).unwrap(); // parent
        w.write_u64(protocol::STDERR_START_ACTIVITY_STRINGLY)
            .unwrap();
        w.write_u64(22).unwrap();
        w.write_u64(3).unwrap();
        w.write_bytes(b"N\xe9").unwrap();
        w.write_bytes(b"text \xe9").unwrap();
        w.write_u64(1).unwrap();
        w.write_bytes(b"k\xff").unwrap();
        w.write_u64(1).unwrap();
        w.write_bytes(b"v\xe9").unwrap();
        w.write_u64(21).unwrap(); // parent
        w.write_u64(protocol::STDERR_RESULT).unwrap();
        w.write_u64(21).unwrap();
        w.write_u64(101).unwrap(); // resBuildLogLine
        w.write_u64(1).unwrap();
        w.write_u64(1).unwrap();
        w.write_bytes(b"first \xff line").unwrap();
        w.write_u64(protocol::STDERR_STOP_ACTIVITY).unwrap();
        w.write_u64(22).unwrap();
        w.write_u64(protocol::STDERR_STOP_ACTIVITY).unwrap();
        w.write_u64(21).unwrap();
    }

    /// Replies that break the protocol, chosen by the name of the path an
    /// operation asks about. Returns `Some(stop)` when it answered instead
    /// of the normal handler, with `stop` set when the fake hung up.
    fn misbehave(&mut self, path: &str) -> Option<bool> {
        let w = &mut self.writer;
        if path.ends_with("-unknown-msg") {
            w.write_u64(0xdead).unwrap();
        } else if path.ends_with("-stderr-write") {
            w.write_u64(protocol::STDERR_WRITE).unwrap();
            w.write_str("data for a sink").unwrap();
        } else if path.ends_with("-stderr-read") {
            w.write_u64(protocol::STDERR_READ).unwrap();
            w.write_u64(8).unwrap();
        } else if path.ends_with("-bad-field") {
            w.write_u64(protocol::STDERR_RESULT).unwrap();
            w.write_u64(7).unwrap();
            w.write_u64(101).unwrap();
            w.write_u64(1).unwrap();
            w.write_u64(2).unwrap(); // neither 0 (int) nor 1 (string)
        } else if path.ends_with("-error-with-pos") {
            w.write_u64(protocol::STDERR_ERROR).unwrap();
            w.write_str("Error").unwrap();
            w.write_u64(0).unwrap();
            w.write_str("Error").unwrap();
            w.write_str("boom").unwrap();
            w.write_u64(1).unwrap(); // a position follows: unsupported
        } else if path.ends_with("-error-type") {
            w.write_u64(protocol::STDERR_ERROR).unwrap();
            w.write_str("SysError").unwrap();
        } else if path.ends_with("-hang-up") {
            // Half of the reply's eight bytes, then the daemon is gone.
            w.write_u64(protocol::STDERR_LAST).unwrap();
            w.write_all(&[1, 0, 0, 0]).unwrap();
            w.flush().unwrap();
            return Some(true);
        } else {
            return None;
        }
        w.flush().unwrap();
        Some(false)
    }

    /// The scripted server loop: answers every opcode brix sends with
    /// canned data and records what it saw.
    fn serve(mut self, daemon_version: ProtocolVersion, daemon_features: &[&str]) {
        self.handshake(daemon_version, daemon_features);
        while let Some(op) = self.read_op() {
            match op {
                19 => self.set_options(),
                1 => {
                    let p = self.reader.read_string().unwrap();
                    match self.misbehave(&p) {
                        Some(true) => return,
                        Some(false) => continue,
                        None => {}
                    }
                    if p.ends_with("-activities") {
                        self.activities();
                    } else if p.ends_with("-bytes") {
                        self.byte_log();
                    }
                    self.last();
                    self.writer.write_bool(p.ends_with(HELLO)).unwrap();
                }
                33 => {
                    let p = self.reader.read_string().unwrap();
                    self.last();
                    // `hello` was built from `HELLO_DRV`; nothing else has a deriver.
                    let derivers: Vec<String> = if p.ends_with(HELLO) {
                        vec![format!("/nix/store/{HELLO_DRV}")]
                    } else {
                        Vec::new()
                    };
                    self.writer.write_strings(&derivers).unwrap();
                }
                31 => {
                    let paths = self.reader.read_strings().unwrap();
                    let _subst = self.reader.read_bool().unwrap();
                    self.last();
                    // A path ending in `-outside` comes back outside the store.
                    let valid: Vec<String> = paths
                        .into_iter()
                        .filter(|p| p.ends_with(HELLO) || p.ends_with("-outside"))
                        .map(|p| match p.strip_suffix("-outside") {
                            Some(_) => p.replace("/nix/store/", "/elsewhere/"),
                            None => p,
                        })
                        .collect();
                    self.writer.write_strings(&valid).unwrap();
                }
                26 => {
                    let p = self.reader.read_string().unwrap();
                    if p.ends_with("-missing") {
                        self.error("path '/nix/store/x-missing' is not valid", &[]);
                        continue;
                    }
                    if p.ends_with("-blake3") {
                        // A path whose content address is a BLAKE3 hash.
                        self.last();
                        self.writer.write_bool(true).unwrap();
                        self.writer.write_str("").unwrap();
                        self.writer.write_str(&"ab".repeat(32)).unwrap();
                        self.writer.write_strings(Vec::<String>::new()).unwrap();
                        self.writer.write_u64(0).unwrap();
                        self.writer.write_u64(8).unwrap();
                        self.writer.write_bool(false).unwrap();
                        self.writer.write_strings(Vec::<String>::new()).unwrap();
                        self.writer
                            .write_str(&format!("fixed:r:blake3:{}", "cd".repeat(32)))
                            .unwrap();
                        self.writer.flush().unwrap();
                        continue;
                    }
                    if p.ends_with("-badhash") {
                        // A valid path whose info carries a NAR hash Nix
                        // could not parse, followed by the rest of the info.
                        self.last();
                        self.writer.write_bool(true).unwrap();
                        self.writer.write_str("").unwrap();
                        self.writer.write_str("not-a-hash").unwrap();
                        self.writer.write_strings(Vec::<String>::new()).unwrap();
                        self.writer.flush().unwrap();
                        continue;
                    }
                    // Some log traffic first.
                    self.writer
                        .write_u64(protocol::STDERR_START_ACTIVITY)
                        .unwrap();
                    self.writer.write_u64(7).unwrap();
                    self.writer.write_u64(3).unwrap();
                    self.writer.write_u64(109).unwrap();
                    self.writer.write_str("querying info").unwrap();
                    self.writer.write_u64(2).unwrap();
                    self.writer.write_u64(1).unwrap();
                    self.writer.write_str("s").unwrap();
                    self.writer.write_u64(0).unwrap();
                    self.writer.write_u64(42).unwrap();
                    self.writer.write_u64(0).unwrap();
                    self.writer.write_u64(protocol::STDERR_RESULT).unwrap();
                    self.writer.write_u64(7).unwrap();
                    self.writer.write_u64(101).unwrap();
                    self.writer.write_u64(1).unwrap();
                    self.writer.write_u64(1).unwrap();
                    self.writer.write_str("a log line").unwrap();
                    self.writer
                        .write_u64(protocol::STDERR_STOP_ACTIVITY)
                        .unwrap();
                    self.writer.write_u64(7).unwrap();
                    self.last();
                    if p.ends_with(HELLO) {
                        self.writer.write_bool(true).unwrap();
                        self.path_info(HELLO_DRV, &[GLIBC, HELLO]);
                    } else {
                        self.writer.write_bool(false).unwrap();
                    }
                }
                29 => {
                    let hp = self.reader.read_string().unwrap();
                    self.last();
                    let reply = if HELLO.starts_with(&hp) {
                        self.path(HELLO)
                    } else {
                        String::new()
                    };
                    self.writer.write_str(&reply).unwrap();
                }
                7 => {
                    let name = self.reader.read_string().unwrap();
                    let cam = self.reader.read_string().unwrap();
                    let _refs = self.reader.read_strings().unwrap();
                    let _repair = self.reader.read_bool().unwrap();
                    let mut data = Vec::new();
                    FramedReader::new(&mut self.reader)
                        .read_to_end(&mut data)
                        .unwrap();
                    self.observed
                        .lock()
                        .unwrap()
                        .added
                        .push((name.clone(), cam, data));
                    if name == "partial" {
                        // A real daemon would now report the truncated NAR;
                        // the client has stopped listening.
                        continue;
                    }
                    self.last();
                    let p = self.path(&format!("{}-{name}", "0".repeat(32)));
                    self.writer.write_str(&p).unwrap();
                    self.path_info("", &[]);
                }
                44 => {
                    assert!(!self.reader.read_bool().unwrap(), "repair");
                    assert!(!self.reader.read_bool().unwrap(), "dontCheckSigs");
                    let mut data = Vec::new();
                    FramedReader::new(&mut self.reader)
                        .read_to_end(&mut data)
                        .unwrap();
                    let mut r = std::io::Cursor::new(data);
                    // A truncated stream is what a failed upload leaves.
                    let mut complete = true;
                    for _ in 0..r.read_u64().unwrap() {
                        let Ok(path) = r.read_string() else {
                            complete = false;
                            break;
                        };
                        let _deriver = r.read_string().unwrap();
                        let nar_hash = r.read_string().unwrap();
                        let references = r.read_strings().unwrap();
                        let _registration_time = r.read_u64().unwrap();
                        let nar_size = r.read_u64().unwrap();
                        let _ultimate = r.read_bool().unwrap();
                        let _sigs = r.read_strings().unwrap();
                        let ca = r.read_string().unwrap();
                        let mut nar = vec![0; nar_size as usize];
                        if std::io::Read::read_exact(&mut r, &mut nar).is_err() {
                            complete = false;
                            break;
                        }
                        self.observed
                            .lock()
                            .unwrap()
                            .added_multiple
                            .push((path, nar_hash, references, ca, nar));
                    }
                    if !complete {
                        continue;
                    }
                    assert_eq!(r.position() as usize, r.get_ref().len());
                    self.last();
                }
                9 => {
                    // Bytes, shown with escapes: an output name need not
                    // be UTF-8.
                    let paths: Vec<String> = (0..self.reader.read_u64().unwrap())
                        .map(|_| self.read_shown())
                        .collect();
                    let _mode = self.reader.read_u64().unwrap();
                    self.observed.lock().unwrap().built.push(paths.clone());
                    if paths.iter().any(|p| p.contains("fails")) {
                        self.error(
                            "builder for '/nix/store/x-fails.drv' failed with exit code 1",
                            &["while building"],
                        );
                        continue;
                    }
                    self.last();
                    self.writer.write_u64(1).unwrap();
                }
                46 => {
                    let paths = self.reader.read_strings().unwrap();
                    let _mode = self.reader.read_u64().unwrap();
                    self.last();
                    self.writer.write_u64(paths.len() as u64).unwrap();
                    for p in paths {
                        self.writer.write_str(&p).unwrap();
                        if p.contains("-bad-") || p.contains("-huge-") {
                            self.bad_build_result(&p);
                        } else if p.contains("fails-bytes") {
                            self.build_result(
                                BuildStatus::PermanentFailure,
                                FAILED_BUILD_MESSAGE,
                                &[],
                            );
                        } else if p.contains("fails") {
                            self.build_result(BuildStatus::PermanentFailure, b"boom", &[]);
                        } else {
                            self.build_result(BuildStatus::Built, b"", &[("out", HELLO)]);
                        }
                    }
                }
                36 => {
                    let _drv_path = self.reader.read_string().unwrap();
                    let n = self.reader.read_u64().unwrap();
                    let mut outs = Vec::new();
                    for _ in 0..n {
                        let name = self.read_shown();
                        let path = self.reader.read_string().unwrap();
                        let ma = self.reader.read_string().unwrap();
                        let h = self.reader.read_string().unwrap();
                        outs.push(format!("{name}|{path}|{ma}|{h}"));
                    }
                    let srcs = self.reader.read_strings().unwrap();
                    // The rest are bytes, shown with escapes: a derivation's
                    // strings need not be UTF-8.
                    let platform = self.read_shown();
                    let builder = self.read_shown();
                    let args: Vec<String> = (0..self.reader.read_u64().unwrap())
                        .map(|_| self.read_shown())
                        .collect();
                    let env: Vec<(String, String)> = (0..self.reader.read_u64().unwrap())
                        .map(|_| (self.read_shown(), self.read_shown()))
                        .collect();
                    let _mode = self.reader.read_u64().unwrap();
                    let mut seen = outs;
                    seen.extend(srcs);
                    seen.push(platform);
                    seen.push(builder);
                    seen.extend(args);
                    seen.extend(env.into_iter().map(|(k, v)| format!("{k}={v}")));
                    self.observed.lock().unwrap().built.push(seen);
                    self.last();
                    self.build_result(BuildStatus::Built, b"", &[("out", HELLO)]);
                }
                41 => {
                    let _p = self.reader.read_string().unwrap();
                    self.last();
                    self.writer.write_u64(2).unwrap();
                    self.writer.write_str("dev").unwrap();
                    self.writer.write_str("").unwrap();
                    self.writer.write_str("out").unwrap();
                    let p = self.path(HELLO);
                    self.writer.write_str(&p).unwrap();
                }
                40 => {
                    let _targets = self.reader.read_strings().unwrap();
                    self.last();
                    let (a, b) = (self.path(HELLO_DRV), self.path(GLIBC));
                    self.writer.write_strings([a]).unwrap();
                    self.writer.write_strings([b]).unwrap();
                    self.writer.write_strings(Vec::<String>::new()).unwrap();
                    self.writer.write_u64(1000).unwrap();
                    self.writer.write_u64(2000).unwrap();
                }
                38 => {
                    let _p = self.reader.read_string().unwrap();
                    self.last();
                    self.writer.write_all(&nar_of(&sample_tree())).unwrap();
                }
                11 | 10 => {
                    let _p = self.reader.read_string().unwrap();
                    self.last();
                    self.writer.write_u64(1).unwrap();
                }
                49 => {
                    let _p = self.reader.read_strings().unwrap();
                    self.last();
                    self.writer.write_u64(1).unwrap();
                }
                32 => {
                    let paths = self.reader.read_strings().unwrap();
                    self.last();
                    self.writer.write_strings(&paths).unwrap();
                }
                other => panic!("fake daemon: unexpected op {other}"),
            }
            self.writer.flush().unwrap();
        }
    }
}

struct Harness {
    client: DaemonClient,
    observed: Arc<Mutex<Observed>>,
    server: JoinHandle<()>,
}

impl Harness {
    fn start(
        daemon_version: ProtocolVersion,
        daemon_features: &'static [&'static str],
        options: ClientOptions,
    ) -> Result<Self, DaemonError> {
        Self::start_as(&fake_uri(), daemon_version, daemon_features, options)
    }

    /// [`Harness::start`] for a client that believes it reached `uri`.
    fn start_as(
        uri: &StoreUri,
        daemon_version: ProtocolVersion,
        daemon_features: &'static [&'static str],
        options: ClientOptions,
    ) -> Result<Self, DaemonError> {
        let (a, b) = UnixStream::pair().unwrap();
        let observed = Arc::new(Mutex::new(Observed::default()));
        let fake = Fake {
            reader: BufReader::new(b.try_clone().unwrap()),
            writer: BufWriter::new(b),
            version: ProtocolVersion::LATEST,
            features: BTreeSet::new(),
            observed: observed.clone(),
        };
        let server = thread::spawn(move || fake.serve(daemon_version, daemon_features));
        let conn = Connection::from_unix_stream(a)?;
        let client = DaemonClient::from_connection(conn, uri, options)?;
        Ok(Harness {
            client,
            observed,
            server,
        })
    }

    /// Closes the connection, waits for the fake to see it and returns
    /// what it observed.
    fn finish(self) -> Observed {
        let Harness {
            client,
            observed,
            server,
        } = self;
        drop(client);
        server.join().unwrap();
        Arc::try_unwrap(observed)
            .ok()
            .unwrap()
            .into_inner()
            .unwrap()
    }
}

const MODERN: &[&str] = &[
    "realisation-with-path-not-hash",
    "addTempRoots",
    "named-activities",
    "queryPathInfos",
    "provenance", // offered by the daemon but not by us: must not be negotiated
];

/// The URI the fake daemon pretends to be reached at.
fn fake_uri() -> StoreUri {
    StoreUri::parse("unix:///fake/daemon-socket", &|w| {
        panic!("unexpected warning: {w}")
    })
    .unwrap()
}

fn hello() -> StorePath {
    StorePath::new(HELLO).unwrap()
}

fn with_logger() -> (ClientOptions, Arc<Mutex<RecordingLogger>>) {
    struct Shared(Arc<Mutex<RecordingLogger>>);
    impl crate::logger::Logger for Shared {
        fn log(&mut self, m: &[u8]) {
            self.0.lock().unwrap().log(m)
        }
        fn start_activity(&mut self, a: &crate::logger::Activity) {
            self.0.lock().unwrap().start_activity(a)
        }
        fn stop_activity(&mut self, id: u64) {
            self.0.lock().unwrap().stop_activity(id)
        }
        fn result(&mut self, id: u64, k: ResultType, f: &[Field]) {
            self.0.lock().unwrap().result(id, k, f)
        }
    }
    let log = Arc::new(Mutex::new(RecordingLogger::default()));
    let options = ClientOptions {
        logger: Box::new(Shared(log.clone())),
        ..Default::default()
    };
    (options, log)
}

#[test]
fn handshake_negotiates_version_features_and_sends_options() {
    let mut settings = ClientSettings::default();
    settings
        .overrides
        .insert("substituters".into(), "https://cache.example".into());
    settings.max_build_jobs = 4;
    let (mut options, log) = with_logger();
    options.settings = settings;
    let h = Harness::start(ProtocolVersion::new(1, 38), MODERN, options).unwrap();
    assert_eq!(h.client.protocol_version(), ProtocolVersion::new(1, 38));
    assert_eq!(h.client.daemon_nix_version(), Some("2.35.3"));
    assert_eq!(h.client.trust(), Some(Trust::Trusted));
    let feats: Vec<&str> = h.client.features().iter().map(String::as_str).collect();
    assert_eq!(
        feats,
        [
            "addTempRoots",
            "named-activities",
            "realisation-with-path-not-hash"
        ]
    );
    assert_eq!(log.lock().unwrap().messages, ["warning: startup message"]);
    let o = h.finish();
    assert_eq!(o.ops, [19], "SetOptions and nothing else");
    assert!(o.client_features.contains("disable-set-options"));
    // keepFailed keepGoing tryFallback verbosity maxJobs maxSilent useBuildHook verboseBuild logType printBuildTrace cores useSubstitutes
    assert_eq!(o.set_options.unwrap(), [0, 0, 0, 3, 4, 0, 1, 0, 0, 0, 0, 1]);
    assert_eq!(
        o.overrides.get("substituters").unwrap(),
        "https://cache.example"
    );
}

#[test]
fn newer_daemon_is_negotiated_down_to_latest() {
    // Nix 2.35 sends `WorkerProto::latest`, 1.38; a daemon claiming more
    // still talks 1.38 with us.
    assert_eq!(ProtocolVersion::LATEST, ProtocolVersion::new(1, 38));
    let h = Harness::start(
        ProtocolVersion::new(1, 40),
        MODERN,
        ClientOptions::default(),
    )
    .unwrap();
    assert_eq!(h.client.protocol_version(), ProtocolVersion::new(1, 38));
    h.finish();
}

#[test]
fn older_daemon_skips_feature_exchange() {
    let h = Harness::start(
        ProtocolVersion::new(1, 35),
        MODERN,
        ClientOptions::default(),
    )
    .unwrap();
    assert_eq!(h.client.protocol_version(), ProtocolVersion::new(1, 35));
    assert!(h.client.features().is_empty());
    let o = h.finish();
    assert!(o.client_features.is_empty());
    assert_eq!(o.ops, [19]);
}

#[test]
fn daemon_without_set_options_feature_suppresses_it() {
    let h = Harness::start(
        ProtocolVersion::new(1, 38),
        &["disable-set-options"],
        ClientOptions::default(),
    )
    .unwrap();
    let o = h.finish();
    assert!(o.ops.is_empty());
}

#[test]
fn query_operations() {
    let (options, log) = with_logger();
    let mut h = Harness::start(ProtocolVersion::new(1, 38), MODERN, options).unwrap();
    let c = &mut h.client;
    assert!(c.is_valid_path(&hello()).unwrap());
    assert!(!c.is_valid_path(&StorePath::new(GLIBC).unwrap()).unwrap());

    let set: BTreeSet<StorePath> = [hello(), StorePath::new(GLIBC).unwrap()]
        .into_iter()
        .collect();
    let valid = c.query_valid_paths(&set, true).unwrap();
    assert_eq!(valid.into_iter().collect::<Vec<_>>(), [hello()]);
    assert_eq!(
        c.query_valid_derivers(&hello())
            .unwrap()
            .into_iter()
            .collect::<Vec<_>>(),
        [StorePath::new(HELLO_DRV).unwrap()]
    );
    assert!(c
        .query_valid_derivers(&StorePath::new(GLIBC).unwrap())
        .unwrap()
        .is_empty());

    let info = c.query_path_info(&hello()).unwrap().unwrap();
    assert_eq!(info.deriver.unwrap().as_str(), HELLO_DRV);
    assert_eq!(
        info.nar_hash,
        Hash::parse_any(
            &"ab".repeat(32),
            Some(HashAlgo::Sha256),
            brix_store::XpSettings::NONE
        )
        .unwrap()
    );
    assert_eq!(info.references.len(), 2);
    assert_eq!(info.registration_time, 1_700_000_000);
    assert_eq!(info.nar_size, 123_456);
    assert!(info.ultimate);
    assert_eq!(info.sigs.len(), 1);
    assert_eq!(info.ca, None);
    assert!(c
        .query_path_info(&StorePath::new(GLIBC).unwrap())
        .unwrap()
        .is_none());
    assert!(c
        .query_path_info(&StorePath::new(format!("{}-missing", "0".repeat(32))).unwrap())
        .unwrap()
        .is_none());

    assert_eq!(
        c.query_path_from_hash_part("9xy8d7g1").unwrap(),
        Some(hello())
    );
    assert_eq!(c.query_path_from_hash_part("zzz").unwrap(), None);

    let subst = c.query_substitutable_paths(&set).unwrap();
    assert_eq!(subst, set);

    let outputs = c
        .query_derivation_output_map(&StorePath::new(HELLO_DRV).unwrap())
        .unwrap();
    assert_eq!(outputs.get(b"dev".as_slice()).unwrap(), &None);
    assert_eq!(outputs.get(b"out".as_slice()).unwrap(), &Some(hello()));

    let missing = c
        .query_missing(&[DerivedPath::all_outputs(StorePath::new(HELLO_DRV).unwrap())])
        .unwrap();
    assert_eq!(missing.will_build.len(), 1);
    assert_eq!(missing.will_substitute.len(), 1);
    assert!(missing.unknown.is_empty());
    assert_eq!((missing.download_size, missing.nar_size), (1000, 2000));

    c.add_temp_root(&hello()).unwrap();
    c.add_temp_roots(&set).unwrap();
    c.ensure_path(&hello()).unwrap();

    let log = log.lock().unwrap();
    let act = &log.activities[0];
    assert_eq!(act.id, 7);
    assert_eq!(act.text, "querying info");
    assert!(
        matches!(&act.kind, crate::logger::ActivityKind::Typed { kind: crate::logger::ActivityType::QueryPathInfo, fields } if fields == &[Field::Str("s".into()), Field::Int(42)])
    );
    assert_eq!(
        log.results[0],
        (
            7,
            ResultType::BuildLogLine,
            vec![Field::Str("a log line".into())]
        )
    );
    assert_eq!(
        log.stopped,
        vec![7, 7],
        "the invalid path errors before any activity"
    );
    drop(log);

    let o = h.finish();
    assert_eq!(
        o.ops,
        [19, 1, 1, 31, 33, 33, 26, 26, 26, 29, 29, 32, 41, 40, 11, 49, 10]
    );
}

#[test]
fn add_temp_roots_without_feature_falls_back() {
    let mut h = Harness::start(
        ProtocolVersion::new(1, 38),
        &["realisation-with-path-not-hash"],
        ClientOptions::default(),
    )
    .unwrap();
    let set: BTreeSet<StorePath> = [hello(), StorePath::new(GLIBC).unwrap()]
        .into_iter()
        .collect();
    h.client.add_temp_roots(&set).unwrap();
    let o = h.finish();
    assert_eq!(o.ops, [19, 11, 11]);
}

#[test]
fn add_to_store_streams_framed_data() {
    let mut h = Harness::start(
        ProtocolVersion::new(1, 38),
        MODERN,
        ClientOptions::default(),
    )
    .unwrap();
    let refs: BTreeSet<StorePath> = [hello()].into_iter().collect();
    let info = h.client.add_text("foo.txt", b"hello world", &refs).unwrap();
    assert_eq!(info.path.name(), "foo.txt");
    let big = vec![9u8; 200_000];
    let info = h.client.add_nar("tree", &big, &BTreeSet::new()).unwrap();
    assert_eq!(info.path.name(), "tree");
    let o = h.finish();
    assert_eq!(o.added[0].0, "foo.txt");
    assert_eq!(o.added[0].1, "text:sha256");
    assert_eq!(o.added[0].2, b"hello world");
    assert_eq!(o.added[1].1, "fixed:r:sha256");
    assert_eq!(o.added[1].2, big);
}

#[test]
fn add_multiple_to_store_streams_infos_and_nars() {
    let mut h = Harness::start(
        ProtocolVersion::new(1, 38),
        MODERN,
        ClientOptions::default(),
    )
    .unwrap();
    let store_dir = StoreDir::default();
    let item = |name: &str, contents: Vec<u8>, references: BTreeSet<StorePath>| {
        let mut nar = Vec::new();
        brix_store::nar::dump_bytes(&contents, &mut nar).unwrap();
        let ca = ContentAddress {
            method: ContentAddressMethod::Text,
            hash: Hash::of(HashAlgo::Sha256, &contents),
        };
        let path = StorePath::new(format!("{}-{name}", "0".repeat(32))).unwrap();
        let info = ValidPathInfo {
            path,
            info: PathInfo {
                deriver: None,
                nar_hash: Hash::of(HashAlgo::Sha256, &nar),
                references,
                registration_time: 0,
                nar_size: nar.len() as u64,
                ultimate: false,
                sigs: BTreeSet::new(),
                ca: Some(ca),
            },
        };
        (info, nar)
    };
    let (small, small_nar) = item("small", b"hello".to_vec(), [hello()].into_iter().collect());
    let (big, big_nar) = item("big", vec![9u8; 200_000], BTreeSet::new());
    let mut paths = [(small.clone(), &small_nar), (big.clone(), &big_nar)]
        .into_iter()
        .map(|(info, nar)| {
            Ok(PathToAdd {
                info,
                nar: Box::new(move |w| w.write_all(nar)),
            })
        });
    h.client
        .add_multiple_to_store(2, &mut paths, false)
        .unwrap();
    // The connection is in step for the next operation.
    assert!(h.client.is_valid_path(&hello()).unwrap());

    // A NAR that is not the size announced ends the stream and the
    // connection.
    let mut wrong = small.clone();
    wrong.info.nar_size += 8;
    let mut paths = std::iter::once(Ok(PathToAdd {
        info: wrong,
        nar: Box::new(|w| w.write_all(&small_nar)),
    }));
    let err = h
        .client
        .add_multiple_to_store(1, &mut paths, false)
        .unwrap_err();
    assert!(err.to_string().contains("not the"), "{err}");
    assert!(matches!(
        h.client.is_valid_path(&hello()),
        Err(DaemonError::ConnectionUnusable)
    ));

    let o = h.finish();
    assert_eq!(o.added_multiple.len(), 2);
    let (path, nar_hash, references, ca, nar) = &o.added_multiple[0];
    assert_eq!(*path, store_dir.display(&small.path));
    assert_eq!(
        *nar_hash,
        small.info.nar_hash.to_string(HashFormat::Base16, false)
    );
    assert_eq!(*references, [store_dir.display(&hello())]);
    assert_eq!(*ca, small.info.ca.as_ref().unwrap().render());
    assert_eq!(*nar, small_nar);
    assert_eq!(o.added_multiple[1].2, Vec::<String>::new());
    assert_eq!(o.added_multiple[1].4, big_nar);
}

#[test]
fn builds_and_results() {
    let mut h = Harness::start(
        ProtocolVersion::new(1, 38),
        MODERN,
        ClientOptions::default(),
    )
    .unwrap();
    let drv = StorePath::new(HELLO_DRV).unwrap();
    let fails = StorePath::new(format!("{}-fails.drv", "0".repeat(32))).unwrap();
    h.client
        .build_paths(
            &[DerivedPath::outputs(drv.clone(), ["out"])],
            BuildMode::Normal,
        )
        .unwrap();
    let err = h
        .client
        .build_paths(
            &[DerivedPath::all_outputs(fails.clone())],
            BuildMode::Normal,
        )
        .unwrap_err();
    match err {
        DaemonError::Remote(e) => {
            assert!(bstr::ByteSlice::contains_str(
                e.message.as_slice(),
                "exit code 1"
            ));
            assert_eq!(e.traces, ["while building"]);
        }
        other => panic!("{other:?}"),
    }
    // The connection is still usable after a daemon-side error.
    let results = h
        .client
        .build_paths_with_results(
            &[
                DerivedPath::all_outputs(drv.clone()),
                DerivedPath::all_outputs(fails.clone()),
            ],
            BuildMode::Check,
        )
        .unwrap();
    assert_eq!(results.len(), 2);
    assert_eq!(results[0].path, DerivedPath::all_outputs(drv.clone()));
    assert!(results[0].result.is_success());
    assert_eq!(
        results[0]
            .result
            .built_outputs
            .get(b"out".as_slice())
            .unwrap(),
        &hello()
    );
    assert_eq!(results[0].result.cpu_user_us, Some(5000));
    assert_eq!(results[0].result.cpu_system_us, None);
    assert_eq!(results[0].result.times_built, 1);
    assert_eq!(results[1].result.status, BuildStatus::PermanentFailure);
    assert_eq!(results[1].result.error_msg, "boom");
    assert!(matches!(
        &results[1].path,
        DerivedPath::Built {
            outputs: OutputsSpec::All,
            ..
        }
    ));

    let mut outputs = BTreeMap::new();
    outputs.insert(
        BString::from("out"),
        DerivationOutput::InputAddressed(hello()),
    );
    let fixed = ContentAddress {
        method: brix_store::ContentAddressMethod::NixArchive,
        hash: Hash::parse_any(
            &"aa".repeat(32),
            Some(HashAlgo::Sha256),
            brix_store::XpSettings::NONE,
        )
        .unwrap(),
    };
    outputs.insert(
        BString::from("src"),
        DerivationOutput::CAFixed(fixed.clone()),
    );
    let src_path = StoreDir::default()
        .make_fixed_output_path_from_ca(
            "hello-2.12.2-src",
            &brix_store::ContentAddressWithReferences::without_refs(&fixed),
        )
        .unwrap();
    let wire_drv = Derivation {
        name: "hello-2.12.2".into(),
        outputs,
        input_drvs: BTreeMap::new(),
        input_srcs: [StorePath::new(GLIBC).unwrap()].into_iter().collect(),
        platform: "x86_64-linux".into(),
        builder: "/bin/sh".into(),
        args: vec!["-c".into(), "echo hi > $out".into()],
        env: [
            ("out".into(), format!("/nix/store/{HELLO}").into()),
            ("name".into(), "hello".into()),
            (b"\xc3".into(), b"\xff\n".into()),
        ]
        .into_iter()
        .collect(),
    };
    let result = h
        .client
        .build_derivation(&drv, &wire_drv, BuildMode::Normal)
        .unwrap();
    assert!(result.is_success());

    let o = h.finish();
    assert_eq!(o.ops, [19, 9, 9, 46, 36]);
    assert_eq!(o.built[0], [format!("/nix/store/{HELLO_DRV}!out")]);
    assert_eq!(
        o.built[2],
        [
            format!("out|/nix/store/{HELLO}||"),
            format!(
                "src|{}|r:sha256|{}",
                StoreDir::default().display(&src_path),
                "aa".repeat(32)
            ),
            format!("/nix/store/{GLIBC}"),
            "x86_64-linux".to_owned(),
            "/bin/sh".to_owned(),
            "-c".to_owned(),
            "echo hi > $out".to_owned(),
            "name=hello".to_owned(),
            format!("out=/nix/store/{HELLO}"),
            // Bytes as they are.
            "\\xc3=\\xff\\n".to_owned(),
        ]
    );
}

/// Output names are bytes on the wire, as Nix sends them (`DerivedPath`'s
/// `to_string_legacy`, `BasicDerivation` outputs): a floating output's
/// name need not be UTF-8.
#[test]
fn output_names_go_on_the_wire_as_bytes() {
    let mut h = Harness::start(
        ProtocolVersion::new(1, 38),
        MODERN,
        ClientOptions::default(),
    )
    .unwrap();
    let drv = StorePath::new(HELLO_DRV).unwrap();
    h.client
        .build_paths(
            &[DerivedPath::outputs(
                drv.clone(),
                [b"a\xff".as_slice(), b"out".as_slice()],
            )],
            BuildMode::Normal,
        )
        .unwrap();
    let mut outputs = BTreeMap::new();
    outputs.insert(
        BString::from(b"a\xff".as_slice()),
        DerivationOutput::CAFloating {
            method: brix_store::ContentAddressMethod::NixArchive,
            hash_algo: HashAlgo::Sha256,
        },
    );
    let wire_drv = Derivation {
        name: "x".into(),
        outputs,
        platform: "x".into(),
        builder: "x".into(),
        ..Default::default()
    };
    h.client
        .build_derivation(&drv, &wire_drv, BuildMode::Normal)
        .unwrap();
    let o = h.finish();
    assert_eq!(o.ops, [19, 9, 36]);
    assert_eq!(o.built[0], [format!("/nix/store/{HELLO_DRV}!a\\xff,out")]);
    assert_eq!(o.built[1], ["a\\xff||r:sha256|", "x", "x"]);
}

#[test]
fn build_results_without_realisation_feature_use_json() {
    let mut h = Harness::start(ProtocolVersion::new(1, 36), &[], ClientOptions::default()).unwrap();
    let drv = StorePath::new(HELLO_DRV).unwrap();
    let results = h
        .client
        .build_paths_with_results(&[DerivedPath::all_outputs(drv)], BuildMode::Normal)
        .unwrap();
    assert_eq!(
        results[0]
            .result
            .built_outputs
            .get(b"out".as_slice())
            .unwrap(),
        &hello()
    );
    assert_eq!(
        results[0].result.cpu_user_us, None,
        "no cpu times before 1.37"
    );
    h.finish();
}

#[test]
fn read_path_into_memory_leaves_stream_in_sync() {
    let mut h = Harness::start(
        ProtocolVersion::new(1, 38),
        MODERN,
        ClientOptions::default(),
    )
    .unwrap();
    let tree = h.client.read_path_into_memory(&hello()).unwrap();
    assert_eq!(nar_of(&tree), nar_of(&sample_tree()));
    assert_eq!(
        tree.read_file(&CanonPath::new("/bin/hello").unwrap())
            .unwrap(),
        b"#!/bin/sh\necho hello\n"
    );
    // Another operation afterwards proves we consumed exactly the NAR.
    assert!(h.client.is_valid_path(&hello()).unwrap());
    let o = h.finish();
    assert_eq!(o.ops, [19, 38, 1]);
}

/// A path whose name makes the fake misbehave.
fn odd_path(suffix: &str) -> StorePath {
    StorePath::new(format!("{}-x{suffix}", "0".repeat(32))).unwrap()
}

/// Runs `fail` against a fresh fake, expects it to fail with an error that
/// leaves the stream in an unknown state, and checks that the client then
/// refuses to send anything more: the fake sees no further operation.
fn assert_poisons(
    fail: impl FnOnce(&mut DaemonClient) -> DaemonError,
    check: impl FnOnce(&DaemonError),
    op: u64,
) {
    let mut h = Harness::start(
        ProtocolVersion::new(1, 38),
        MODERN,
        ClientOptions::default(),
    )
    .unwrap();
    let err = fail(&mut h.client);
    check(&err);
    let again = h.client.is_valid_path(&hello()).unwrap_err();
    assert!(
        matches!(again, DaemonError::ConnectionUnusable),
        "{again:?}"
    );
    assert_eq!(
        again.to_string(),
        "the connection to the Nix daemon is unusable after an earlier error"
    );
    let o = h.finish();
    assert_eq!(o.ops, [19, op], "nothing is sent after the failure");
}

#[test]
fn unknown_message_poisons_connection() {
    assert_poisons(
        |c| c.is_valid_path(&odd_path("-unknown-msg")).unwrap_err(),
        |e| {
            assert_eq!(
                e.to_string(),
                "got unknown message type dead from Nix daemon"
            )
        },
        1,
    );
}

#[test]
fn bad_nar_hash_in_reply_poisons_connection() {
    assert_poisons(
        |c| c.query_path_info(&odd_path("-badhash")).unwrap_err(),
        |e| assert!(matches!(e, DaemonError::InvalidData(_)), "{e:?}"),
        26,
    );
}

/// A BLAKE3 content address from the daemon needs `blake3-hashes`, as
/// Nix's `ContentAddress::parse` does: refused without the feature,
/// read with it.
#[test]
fn blake3_content_addresses_need_the_feature() {
    assert_poisons(
        |c| c.query_path_info(&odd_path("-blake3")).unwrap_err(),
        |e| {
            assert!(
                e.to_string()
                    .contains("experimental Nix feature 'blake3-hashes' is disabled"),
                "{e}"
            )
        },
        26,
    );
    let mut h = Harness::start(
        ProtocolVersion::new(1, 38),
        MODERN,
        ClientOptions::default(),
    )
    .unwrap();
    h.client
        .set_xp_settings(brix_store::XpSettings::NONE.with(brix_store::Xp::Blake3Hashes));
    let info = h
        .client
        .query_path_info(&odd_path("-blake3"))
        .unwrap()
        .unwrap();
    let ca = info.ca.unwrap();
    assert_eq!(ca.hash.algo(), HashAlgo::Blake3);
    assert_eq!(ca.method, brix_store::ContentAddressMethod::NixArchive);
    h.finish();
}

#[test]
fn path_outside_the_store_in_reply_poisons_connection() {
    assert_poisons(
        |c| {
            let set = [odd_path("-outside")].into_iter().collect();
            c.query_valid_paths(&set, false).unwrap_err()
        },
        |e| assert!(matches!(e, DaemonError::InvalidStorePath(_)), "{e:?}"),
        31,
    );
}

#[test]
fn stderr_write_and_read_are_protocol_errors() {
    // brix never gives `processStderr` a sink or a source.
    assert_poisons(
        |c| c.is_valid_path(&odd_path("-stderr-write")).unwrap_err(),
        |e| assert_eq!(e.to_string(), "no sink"),
        1,
    );
    assert_poisons(
        |c| c.is_valid_path(&odd_path("-stderr-read")).unwrap_err(),
        |e| assert_eq!(e.to_string(), "no source"),
        1,
    );
}

#[test]
fn daemon_errors_leave_the_connection_usable() {
    let mut h = Harness::start(
        ProtocolVersion::new(1, 38),
        MODERN,
        ClientOptions::default(),
    )
    .unwrap();
    let fails = StorePath::new(format!("{}-fails.drv", "0".repeat(32))).unwrap();
    for _ in 0..2 {
        let err = h
            .client
            .build_paths(
                &[DerivedPath::all_outputs(fails.clone())],
                BuildMode::Normal,
            )
            .unwrap_err();
        assert!(matches!(err, DaemonError::Remote(_)), "{err:?}");
    }
    assert!(h.client.is_valid_path(&hello()).unwrap());
    assert_eq!(h.finish().ops, [19, 9, 9, 1]);
}

#[test]
fn store_dir_is_configurable() {
    let (a, b) = UnixStream::pair().unwrap();
    let seen = Arc::new(Mutex::new(String::new()));
    let seen2 = seen.clone();
    thread::spawn(move || {
        let mut r = BufReader::new(b.try_clone().unwrap());
        let mut w = BufWriter::new(b);
        r.read_u64().unwrap();
        r.read_u64().unwrap();
        w.write_u64(protocol::WORKER_MAGIC_2).unwrap();
        w.write_u64(ProtocolVersion::new(1, 37).to_wire()).unwrap();
        w.flush().unwrap();
        r.read_u64().unwrap();
        r.read_u64().unwrap();
        w.write_str("2.24.0").unwrap();
        w.write_u64(0).unwrap();
        w.write_u64(protocol::STDERR_LAST).unwrap();
        w.flush().unwrap();
        assert_eq!(r.read_u64().unwrap(), 19);
        for _ in 0..12 {
            r.read_u64().unwrap();
        }
        r.read_string_map().unwrap();
        w.write_u64(protocol::STDERR_LAST).unwrap();
        w.flush().unwrap();
        assert_eq!(r.read_u64().unwrap(), 1);
        *seen2.lock().unwrap() = r.read_string().unwrap();
        w.write_u64(protocol::STDERR_LAST).unwrap();
        w.write_bool(true).unwrap();
        w.flush().unwrap();
    });
    let conn = Connection::from_unix_stream(a).unwrap();
    let options = ClientOptions {
        store_dir: StoreDir::new("/tmp/store"),
        ..Default::default()
    };
    let mut c = DaemonClient::from_connection(conn, &fake_uri(), options).unwrap();
    assert_eq!(c.trust(), None);
    assert!(c.is_valid_path(&hello()).unwrap());
    assert_eq!(*seen.lock().unwrap(), format!("/tmp/store/{HELLO}"));
}

// ---- the handshake --------------------------------------------------------

type ScriptReader = BufReader<UnixStream>;
type ScriptWriter = BufWriter<UnixStream>;

/// Runs the client's side of the handshake against `script`, which plays
/// the daemon on the raw socket, and returns the client's error.
fn handshake_error(
    script: impl FnOnce(&mut ScriptReader, &mut ScriptWriter) + Send + 'static,
) -> DaemonError {
    let (a, b) = UnixStream::pair().unwrap();
    // A client that waits for the wrong thing fails the test, not hangs it.
    b.set_read_timeout(Some(std::time::Duration::from_secs(10)))
        .unwrap();
    let daemon = thread::spawn(move || {
        let mut r = BufReader::new(b.try_clone().unwrap());
        let mut w = BufWriter::new(b);
        assert_eq!(r.read_u64().unwrap(), protocol::WORKER_MAGIC_1);
        assert_eq!(
            r.read_u64().unwrap(),
            ProtocolVersion::LATEST.to_wire(),
            "the client's version"
        );
        script(&mut r, &mut w);
        w.flush().unwrap();
    });
    let conn = Connection::from_unix_stream(a).unwrap();
    let err = DaemonClient::from_connection(conn, &fake_uri(), ClientOptions::default())
        .map(drop)
        .unwrap_err();
    daemon.join().unwrap();
    err
}

/// The magic and version of a daemon speaking `version`, then its
/// `postHandshake` reply up to the trust flag.
fn greet(r: &mut ScriptReader, w: &mut ScriptWriter, version: ProtocolVersion, trust: u64) {
    w.write_u64(protocol::WORKER_MAGIC_2).unwrap();
    w.write_u64(version.to_wire()).unwrap();
    w.flush().unwrap();
    assert!(
        !version.at_least(1, 38),
        "no feature exchange in this script"
    );
    r.read_u64().unwrap(); // CPU affinity
    r.read_u64().unwrap(); // reserveSpace
    w.write_str("2.24.0").unwrap();
    w.write_u64(trust).unwrap();
}

const OPENING: &str = "cannot open connection to remote store 'unix:///fake/daemon-socket'";

#[test]
fn access_denied_and_mismatch() {
    // `BasicClientConnection::handshake`'s messages inside
    // `RemoteStore::initConnection`'s.
    let err = handshake_error(|_, w| w.write_u64(protocol::WORKER_MAGIC_ACCESS_DENIED).unwrap());
    assert_eq!(
        err.to_string(),
        format!("{OPENING}: access denied by the Nix daemon (check the `allowed-users` setting in the daemon)")
    );
    let err = handshake_error(|_, w| w.write_u64(0x1234).unwrap());
    assert_eq!(
        err.to_string(),
        format!("{OPENING}: Nix daemon protocol mismatch")
    );
    let err = handshake_error(|_, _| {});
    assert_eq!(
        err.to_string(),
        format!("{OPENING}: Nix daemon disconnected unexpectedly (maybe it crashed?)")
    );
}

#[test]
fn non_protocol_reply_is_shown() {
    // What `nix store info` reports for an ssh-ng remote program that
    // prints "hello garbage" and exits: the first eight bytes are no
    // `unsigned int`, so the client closes its side, drains the rest and
    // shows all of it, chomped.
    let err = handshake_error(|r, w| {
        w.write_all(b"hello garbage \t\r\n").unwrap();
        w.flush().unwrap();
        // Waits for the client's half-close, as a remote waiting for
        // input would.
        let mut rest = Vec::new();
        r.read_to_end(&mut rest).unwrap();
        assert!(rest.is_empty());
    });
    assert_eq!(
        err.to_string(),
        format!("{OPENING}: protocol mismatch, got 'hello garbage'")
    );
}

/// What the other side sent is shown as it is, bytes that are not UTF-8
/// included: `nix store info --store
/// 'ssh-ng://localhost?remote-program=…'` with a program that prints
/// `hello \xff garbage` says "protocol mismatch, got 'hello \xff
/// garbage'" with the byte.
#[test]
fn non_protocol_reply_keeps_its_bytes() {
    let err = handshake_error(|r, w| {
        w.write_all(b"hello \xff garbage\n").unwrap();
        w.flush().unwrap();
        let mut rest = Vec::new();
        r.read_to_end(&mut rest).unwrap();
        assert!(rest.is_empty());
    });
    let expected = [
        b"error: ".as_slice(),
        OPENING.as_bytes(),
        b": protocol mismatch, got 'hello \xff garbage'",
    ]
    .concat();
    assert_eq!(err.render_bytes(false), expected);
}

#[test]
fn unsupported_and_too_old_daemons() {
    let err = handshake_error(|_, w| {
        w.write_u64(protocol::WORKER_MAGIC_2).unwrap();
        w.write_u64(ProtocolVersion::new(2, 0).to_wire()).unwrap();
    });
    assert_eq!(
        err.to_string(),
        format!("{OPENING}: Nix daemon protocol version not supported")
    );
    let err = handshake_error(|_, w| {
        w.write_u64(protocol::WORKER_MAGIC_2).unwrap();
        w.write_u64(ProtocolVersion::new(1, 34).to_wire()).unwrap();
    });
    let DaemonError::OpenConnection { error, .. } = &err else {
        panic!("{err:?}")
    };
    assert!(
        matches!(**error, DaemonError::DaemonTooOld(v, _) if v == ProtocolVersion::new(1, 34)),
        "{err:?}"
    );
}

#[test]
fn invalid_trusted_status() {
    let err = handshake_error(|r, w| greet(r, w, ProtocolVersion::new(1, 37), 3));
    assert_eq!(
        err.to_string(),
        format!("{OPENING}: Invalid trusted status from remote")
    );
}

#[test]
fn unknown_message_during_startup_fails_the_connection() {
    let err = handshake_error(|r, w| {
        greet(r, w, ProtocolVersion::new(1, 37), 2);
        w.write_u64(0xdead).unwrap(); // not a STDERR_* code
    });
    assert_eq!(
        err.to_string(),
        format!("{OPENING}: got unknown message type dead from Nix daemon")
    );
}

#[test]
fn ssh_ng_sends_no_set_options() {
    // `SSHStore::setOptions` is empty: an ssh-ng remote never gets op 19,
    // whatever the daemon negotiated.
    for features in [MODERN, &[] as &[&str]] {
        let uri =
            StoreUri::parse("ssh-ng://localhost", &|w| panic!("unexpected warning: {w}")).unwrap();
        let mut h = Harness::start_as(
            &uri,
            ProtocolVersion::new(1, 38),
            features,
            ClientOptions::default(),
        )
        .unwrap();
        assert!(h.client.is_valid_path(&hello()).unwrap());
        assert_eq!(h.finish().ops, [1]);
    }
}

// ---- the log stream and errors --------------------------------------------

#[test]
fn typed_and_named_activities() {
    let (options, log) = with_logger();
    let mut h = Harness::start(ProtocolVersion::new(1, 38), MODERN, options).unwrap();
    assert!(!h.client.is_valid_path(&odd_path("-activities")).unwrap());
    h.finish();
    let log = log.lock().unwrap();
    assert_eq!(log.activities.len(), 2);
    let build = &log.activities[0];
    assert_eq!(build.text, "building '/nix/store/x-hello.drv'");
    assert_eq!(
        build.kind,
        crate::logger::ActivityKind::Typed {
            kind: crate::logger::ActivityType::Build,
            fields: vec![
                Field::Str("/nix/store/x-hello.drv".into()),
                Field::Str(BString::default()),
                Field::Int(1),
                Field::Int(1),
            ],
        }
    );
    let lock = &log.activities[1];
    assert_eq!((lock.id, lock.parent), (12, 11));
    assert_eq!(lock.text, "waiting for lock on '/nix/store/x-hello'");
    assert_eq!(
        lock.kind,
        crate::logger::ActivityKind::Named {
            name: "WaitingForLock".into(),
            metadata: [
                (BString::from("attempt"), Field::Int(2)),
                (
                    BString::from("nix.path"),
                    Field::Str("/nix/store/x-hello".into())
                ),
            ]
            .into_iter()
            .collect(),
        }
    );
    assert_eq!(log.stopped, [12]);
}

/// Everything in the log stream is bytes, as in Nix (`readString`), and
/// reaches the logger as the daemon sent it: a plain message (chomped,
/// as `printError(chomp(…))` has it), an activity's text, name, keys and
/// string fields, and a build log line.
#[test]
fn log_texts_are_bytes() {
    let (options, log) = with_logger();
    let mut h = Harness::start(ProtocolVersion::new(1, 38), MODERN, options).unwrap();
    assert!(!h.client.is_valid_path(&odd_path("-bytes")).unwrap());
    h.finish();
    let log = log.lock().unwrap();
    // After the fake's startup message.
    assert_eq!(
        log.messages,
        [
            BString::from("warning: startup message"),
            BString::from(b"warning: caf\xe9")
        ]
    );
    assert_eq!(log.activities.len(), 2);
    let build = &log.activities[0];
    assert_eq!(build.text, b"building '\xff'");
    assert_eq!(
        build.kind,
        crate::logger::ActivityKind::Typed {
            kind: crate::logger::ActivityType::Build,
            fields: vec![Field::Str(b"/nix/store/x-caf\xe9.drv".into())],
        }
    );
    let named = &log.activities[1];
    assert_eq!((named.id, named.parent), (22, 21));
    assert_eq!(named.text, b"text \xe9");
    assert_eq!(
        named.kind,
        crate::logger::ActivityKind::Named {
            name: b"N\xe9".into(),
            metadata: [(BString::from(b"k\xff"), Field::Str(b"v\xe9".into()))]
                .into_iter()
                .collect(),
        }
    );
    assert_eq!(
        log.results,
        [(
            21,
            ResultType::BuildLogLine,
            vec![Field::Str(b"first \xff line".into())]
        )]
    );
    assert_eq!(log.stopped, [22, 21]);
}

/// A failed build's message is bytes: Nix ends it with the builder's last
/// log lines, as they are (what `nix build` shows under `error:`).
#[test]
fn failed_build_messages_are_bytes() {
    let mut h = Harness::start(
        ProtocolVersion::new(1, 38),
        MODERN,
        ClientOptions::default(),
    )
    .unwrap();
    let drv = StorePath::new(format!("{}-fails-bytes.drv", "0".repeat(32))).unwrap();
    let results = h
        .client
        .build_paths_with_results(&[DerivedPath::all_outputs(drv)], BuildMode::Normal)
        .unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].result.status, BuildStatus::PermanentFailure);
    assert_eq!(results[0].result.error_msg, FAILED_BUILD_MESSAGE);
    h.finish();
}

#[test]
fn unsupported_field_type_poisons_connection() {
    assert_poisons(
        |c| c.is_valid_path(&odd_path("-bad-field")).unwrap_err(),
        |e| {
            assert_eq!(
                e.to_string(),
                "got unsupported field type 2 from Nix daemon"
            )
        },
        1,
    );
}

#[test]
fn malformed_errors_poison_connection() {
    // `readError` supports neither positions nor other error types.
    assert_poisons(
        |c| c.is_valid_path(&odd_path("-error-with-pos")).unwrap_err(),
        |e| {
            assert_eq!(
                e.to_string(),
                "deserializing error positions is not supported"
            )
        },
        1,
    );
    assert_poisons(
        |c| c.is_valid_path(&odd_path("-error-type")).unwrap_err(),
        |e| assert_eq!(e.to_string(), "unexpected error type 'SysError'"),
        1,
    );
}

#[test]
fn daemon_hanging_up_mid_reply() {
    assert_poisons(
        |c| c.is_valid_path(&odd_path("-hang-up")).unwrap_err(),
        |e| {
            assert!(matches!(e, DaemonError::Disconnected), "{e:?}");
            assert_eq!(
                e.to_string(),
                "Nix daemon disconnected unexpectedly (maybe it crashed?)"
            );
        },
        1,
    );
}

#[test]
fn malformed_build_results_poison_connection() {
    let build = |suffix: &'static str| {
        move |c: &mut DaemonClient| {
            let drv = StorePath::new(format!("{}-x{suffix}.drv", "0".repeat(32))).unwrap();
            c.build_paths_with_results(&[DerivedPath::all_outputs(drv)], BuildMode::Normal)
                .unwrap_err()
        }
    };
    assert_poisons(
        build("-bad-status"),
        |e| assert_eq!(e.to_string(), "Invalid BuildResult status 99 from remote"),
        46,
    );
    assert_poisons(
        build("-bad-tag"),
        |e| assert_eq!(e.to_string(), "Invalid optional tag from remote"),
        46,
    );
    // `readNum<int64_t>`.
    assert_poisons(
        build("-huge-cpu"),
        |e| {
            assert_eq!(
                e.to_string(),
                "serialised integer 18446744073709551615 is too large for type 'l'"
            )
        },
        46,
    );
}

#[test]
fn failing_add_to_store_source_terminates_the_frame() {
    let mut h = Harness::start(
        ProtocolVersion::new(1, 38),
        MODERN,
        ClientOptions::default(),
    )
    .unwrap();
    let err = h
        .client
        .add_to_store(
            "partial",
            brix_store::ContentAddressMethod::NixArchive,
            HashAlgo::Sha256,
            &BTreeSet::new(),
            false,
            &mut |w| {
                w.write_all(b"half a NAR")?;
                Err(std::io::Error::other("the source failed"))
            },
        )
        .unwrap_err();
    assert!(
        matches!(&err, DaemonError::Io(e) if e.to_string() == "the source failed"),
        "{err:?}"
    );
    assert!(matches!(
        h.client.is_valid_path(&hello()).unwrap_err(),
        DaemonError::ConnectionUnusable
    ));
    let o = h.finish();
    // The fake read the frame up to its terminator: what was sent, then
    // the end.
    assert_eq!(
        o.added,
        [(
            "partial".to_owned(),
            "fixed:r:sha256".to_owned(),
            b"half a NAR".to_vec()
        )]
    );
    assert_eq!(o.ops, [19, 7]);
}

#[test]
fn daemon_errors_render_with_their_traces() {
    let mut h = Harness::start(
        ProtocolVersion::new(1, 38),
        MODERN,
        ClientOptions::default(),
    )
    .unwrap();
    let fails = StorePath::new(format!("{}-fails.drv", "0".repeat(32))).unwrap();
    let err = h
        .client
        .build_paths(&[DerivedPath::all_outputs(fails)], BuildMode::Normal)
        .unwrap_err();
    // `showErrorInfo` as `nix` prints it, traces first.
    assert_eq!(
        err.render(false),
        "error:\n       … while building\n\n       error: builder for '/nix/store/x-fails.drv' failed with exit code 1"
    );
    assert_eq!(
        err.to_string(),
        "builder for '/nix/store/x-fails.drv' failed with exit code 1"
    );
    h.finish();
}

#[test]
fn store_setting_must_match_the_store_dir() {
    let uri = StoreUri::parse("unix:///nonexistent/socket?store=/other/store", &|w| {
        panic!("unexpected warning: {w}")
    })
    .unwrap();
    let err = DaemonClient::connect(&uri, ClientOptions::default()).unwrap_err();
    assert_eq!(
        err.to_string(),
        "the store URI sets 'store' to '/other/store', but brix evaluates with the store directory '/nix/store'; set NIX_STORE_DIR to match"
    );
    // A matching one is fine: the connection itself is what fails here.
    let uri = StoreUri::parse("unix:///nonexistent/socket?store=/nix//store", &|w| {
        panic!("unexpected warning: {w}")
    })
    .unwrap();
    let err = DaemonClient::connect(&uri, ClientOptions::default()).unwrap_err();
    assert_eq!(
        err.to_string(),
        "cannot connect to socket at '/nonexistent/socket': No such file or directory"
    );
}

#[test]
fn remote_program_banner_is_a_protocol_mismatch() {
    // `nix store info --store 'ssh-ng://localhost?remote-program=…'` with a
    // program that greets instead of speaking the protocol.
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("banner.sh");
    std::fs::write(&script, "#!/bin/sh\necho 'hello garbage'\n").unwrap();
    std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let uri = StoreUri::parse(
        &format!("ssh-ng://localhost?remote-program={}", script.display()),
        &|w| panic!("unexpected warning: {w}"),
    )
    .unwrap();
    let err = DaemonClient::connect(&uri, ClientOptions::default()).unwrap_err();
    assert_eq!(
        err.to_string(),
        "cannot open connection to remote store 'ssh-ng://localhost': protocol mismatch, got 'hello garbage'"
    );
}
