//! The worker-protocol client.
//!
//! One `DaemonClient` is one connection. Every operation follows the same
//! shape (`RemoteStore` in `src/libstore/remote-store.cc`):
//!
//! 1. write the opcode and the request;
//! 2. [`DaemonClient::process_stderr`]: forward log messages until the daemon
//!    says `STDERR_LAST` (reply follows) or `STDERR_ERROR` (operation failed);
//! 3. read the reply.
//!
//! Any failure other than an error the daemon reports may leave part of a
//! request or reply on the wire, so after one the client refuses further
//! operations ([`DaemonClient::op`]).

use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Read, Write};

use bstr::BString;

use crate::error::{chomp_bytes, DaemonError, RemoteError};
use crate::logger::{Activity, ActivityKind, ActivityType, Field, Logger, NullLogger, ResultType};
use crate::protocol::{self, feature, BuildMode, Op, ProtocolVersion, Trust, Verbosity};
use crate::transport::Connection;
use brix_store::{
    ContentAddress, ContentAddressMethod, Derivation, DerivationOutput, DerivedPath, Hash,
    HashAlgo, HashFormat, MemoryAccessor, StoreDir, StorePath, XpSettings,
};

use crate::types::{BuildResult, BuildStatus, KeyedBuildResult, Missing, PathInfo, ValidPathInfo};
use crate::uri::{StoreUri, Transport};
use crate::wire::{FramedWriter, WireRead, WireWrite};

/// The settings sent with `SetOptions` right after the handshake
/// (`RemoteStore::setOptions`), except to an `ssh-ng` remote, which never
/// gets them (`SSHStore::setOptions` is empty). Defaults match Nix's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClientSettings {
    pub keep_failed: bool,
    pub keep_going: bool,
    pub try_fallback: bool,
    /// Log level the daemon filters its messages at before sending them.
    pub verbosity: Verbosity,
    pub max_build_jobs: u64,
    pub max_silent_time: u64,
    /// Stream build output lines (`verbose-build`).
    pub verbose_build: bool,
    pub build_cores: u64,
    pub use_substitutes: bool,
    /// Other `nix.conf` settings to override for this connection; the daemon
    /// only honours most of them from trusted users.
    pub overrides: BTreeMap<String, String>,
}

impl Default for ClientSettings {
    fn default() -> Self {
        ClientSettings {
            keep_failed: false,
            keep_going: false,
            try_fallback: false,
            verbosity: Verbosity::Info,
            max_build_jobs: 1,
            max_silent_time: 0,
            verbose_build: true,
            build_cores: 0,
            use_substitutes: true,
            overrides: BTreeMap::new(),
        }
    }
}

/// How to open a connection.
pub struct ClientOptions {
    /// The daemon's store directory. Must match the daemon's; brix cannot
    /// query it.
    pub store_dir: StoreDir,
    pub settings: ClientSettings,
    pub logger: Box<dyn Logger + Send>,
}

impl Default for ClientOptions {
    fn default() -> Self {
        ClientOptions {
            store_dir: StoreDir::default(),
            settings: ClientSettings::default(),
            logger: Box::new(NullLogger),
        }
    }
}

impl std::fmt::Debug for ClientOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClientOptions")
            .field("store_dir", &self.store_dir)
            .field("settings", &self.settings)
            .finish_non_exhaustive()
    }
}

/// A connection to a Nix daemon.
pub struct DaemonClient {
    conn: Connection,
    version: ProtocolVersion,
    features: BTreeSet<String>,
    store_dir: StoreDir,
    daemon_nix_version: Option<String>,
    trust: Option<Trust>,
    logger: Box<dyn Logger + Send>,
    /// The experimental features the client's hashes and content
    /// addresses are read with ([`DaemonClient::set_xp_settings`]).
    xp: XpSettings,
    /// Set after an error that may have desynchronised the stream.
    poisoned: bool,
}

type Result<T> = std::result::Result<T, DaemonError>;

impl DaemonClient {
    /// Open a connection, perform the handshake and send the settings.
    ///
    /// A `?store=` setting in `uri` must name the store directory of
    /// `options`: brix evaluates with one store directory and cannot
    /// switch to the URI's.
    pub fn connect(uri: &StoreUri, options: ClientOptions) -> Result<Self> {
        if let Some(dir) = &uri.store_dir {
            if dir != options.store_dir.as_str() {
                return Err(DaemonError::Unsupported(format!(
                    "the store URI sets 'store' to '{dir}', but brix evaluates with the store directory '{}'; set NIX_STORE_DIR to match",
                    options.store_dir.as_str()
                )));
            }
        }
        let conn = Connection::open(&uri.transport)?;
        Self::from_connection(conn, uri, options)
    }

    /// The handshake and settings on an open connection
    /// (`RemoteStore::initConnection`). Failures up to the daemon's startup
    /// messages are reported as Nix reports them, inside "cannot open
    /// connection to remote store '…'".
    pub(crate) fn from_connection(
        mut conn: Connection,
        uri: &StoreUri,
        options: ClientOptions,
    ) -> Result<Self> {
        let ClientOptions {
            store_dir,
            settings,
            logger,
        } = options;
        let opening = |error: DaemonError| DaemonError::OpenConnection {
            store: uri.display().to_owned(),
            error: Box::new(error),
        };

        let (version, features) = handshake(&mut conn).map_err(opening)?;
        let mut client = DaemonClient {
            conn,
            version,
            features,
            store_dir,
            daemon_nix_version: None,
            trust: None,
            logger,
            xp: XpSettings::NONE,
            poisoned: false,
        };
        client.post_handshake().map_err(opening)?;
        // `SSHStore::setOptions` sends nothing: settings are not forwarded
        // to an ssh-ng remote. Other daemons get them unless they opted out.
        let forwards_settings = !matches!(uri.transport, Transport::SshNg(_));
        if forwards_settings && !client.has_feature(feature::DISABLE_SET_OPTIONS) {
            client.set_options(&settings)?;
        }
        Ok(client)
    }

    /// The negotiated protocol version.
    pub fn protocol_version(&self) -> ProtocolVersion {
        self.version
    }

    /// The protocol features both sides support.
    pub fn features(&self) -> &BTreeSet<String> {
        &self.features
    }

    pub fn has_feature(&self, name: &str) -> bool {
        self.features.contains(name)
    }

    /// The daemon's Nix version string, e.g. `2.35.3`.
    pub fn daemon_nix_version(&self) -> Option<&str> {
        self.daemon_nix_version.as_deref()
    }

    /// Whether the daemon treats us as a trusted user.
    pub fn trust(&self) -> Option<Trust> {
        self.trust
    }

    pub fn store_dir(&self) -> &StoreDir {
        &self.store_dir
    }

    /// The experimental features in effect (Nix's global
    /// `experimentalFeatureSettings`), which decide whether a BLAKE3 hash
    /// the daemon sends in a path's NAR hash or content address can be
    /// read. Without a call, none are enabled, Nix's default.
    pub fn set_xp_settings(&mut self, xp: XpSettings) {
        self.xp = xp;
    }

    // ---- handshake -------------------------------------------------------

    /// What the client sends after the version exchange and what it reads
    /// back (`BasicClientConnection::postHandshake` + `ClientHandshakeInfo`).
    fn post_handshake(&mut self) -> Result<()> {
        // protocol >= 1.14: obsolete CPU affinity (0 = none).
        self.conn.writer.write_u64(0)?;
        // protocol >= 1.11: obsolete reserveSpace.
        self.conn.writer.write_bool(false)?;
        // protocol >= 1.33: flush before reading the daemon's info.
        self.conn.writer.flush()?;
        // protocol >= 1.33
        self.daemon_nix_version = Some(self.conn.reader.read_string()?);
        // protocol >= 1.35
        self.trust = match self.conn.reader.read_u8()? {
            0 => None,
            1 => Some(Trust::Trusted),
            2 => Some(Trust::NotTrusted),
            _ => return Err(DaemonError::protocol("Invalid trusted status from remote")),
        };
        // The daemon then sends any startup messages, ending in STDERR_LAST.
        self.process_stderr()
    }

    /// `SetOptions` (opcode 19).
    fn set_options(&mut self, s: &ClientSettings) -> Result<()> {
        self.start_op(Op::SetOptions)?;
        let w = &mut self.conn.writer;
        w.write_bool(s.keep_failed)?;
        w.write_bool(s.keep_going)?;
        w.write_bool(s.try_fallback)?;
        w.write_u64(s.verbosity as u64)?;
        w.write_u64(s.max_build_jobs)?;
        w.write_u64(s.max_silent_time)?;
        w.write_bool(true)?; // obsolete useBuildHook
        w.write_u64(if s.verbose_build {
            Verbosity::Error as u64
        } else {
            Verbosity::Vomit as u64
        })?;
        w.write_u64(0)?; // obsolete logType
        w.write_u64(0)?; // obsolete printBuildTrace
        w.write_u64(s.build_cores)?;
        w.write_bool(s.use_substitutes)?;
        w.write_string_map(&s.overrides)?;
        self.process_stderr()
    }

    // ---- plumbing --------------------------------------------------------

    /// Runs one operation. A connection that failed in a way that may have
    /// left bytes of a request or reply on the wire is refused; any error
    /// other than one the daemon reported marks it so. This is what Nix's
    /// `ConnectionHandle` does when it is destroyed by an exception that is
    /// not a daemon error (`remote-store.cc`), except that Nix then opens a
    /// fresh connection for the next operation, while brix keeps the one
    /// connection its temporary roots live on.
    fn op<T>(&mut self, f: impl FnOnce(&mut Self) -> Result<T>) -> Result<T> {
        if self.poisoned {
            return Err(DaemonError::ConnectionUnusable);
        }
        let result = f(self);
        if let Err(e) = &result {
            if !matches!(e, DaemonError::Remote(_)) {
                self.poisoned = true;
            }
        }
        result
    }

    fn start_op(&mut self, op: Op) -> Result<()> {
        self.conn.writer.write_u64(op as u64)
    }

    fn write_path(&mut self, path: &StorePath) -> Result<()> {
        let s = self.store_dir.display(path);
        self.conn.writer.write_str(&s)
    }

    fn write_path_set(&mut self, paths: &BTreeSet<StorePath>) -> Result<()> {
        let printed: Vec<String> = paths.iter().map(|p| self.store_dir.display(p)).collect();
        self.conn.writer.write_strings(&printed)
    }

    /// Derived paths in the `!` form, bytes as their output names are.
    fn write_derived_paths(&mut self, paths: &[DerivedPath]) -> Result<()> {
        let printed: Vec<BString> = paths
            .iter()
            .map(|p| p.display_legacy(&self.store_dir))
            .collect();
        self.conn.writer.write_strings(&printed)
    }

    fn read_path(&mut self) -> Result<StorePath> {
        let s = self.conn.reader.read_string()?;
        Ok(self.store_dir.parse(&s)?)
    }

    fn read_path_opt(&mut self) -> Result<Option<StorePath>> {
        let s = self.conn.reader.read_string()?;
        if s.is_empty() {
            Ok(None)
        } else {
            Ok(Some(self.store_dir.parse(&s)?))
        }
    }

    fn read_path_set(&mut self) -> Result<BTreeSet<StorePath>> {
        let strs = self.conn.reader.read_strings()?;
        strs.iter()
            .map(|s| self.store_dir.parse(s).map_err(DaemonError::from))
            .collect()
    }

    /// Forward the daemon's log stream until the operation's reply is ready
    /// (`processStderr`). Returns the daemon's error for `STDERR_ERROR`.
    ///
    /// None of the operations brix sends stream data through the log
    /// channel, so, as in Nix when no sink or source is given, a
    /// `STDERR_WRITE` or `STDERR_READ` is an error.
    fn process_stderr(&mut self) -> Result<()> {
        self.conn.writer.flush()?;
        loop {
            let msg = self.conn.reader.read_u64()?;
            match msg {
                protocol::STDERR_WRITE => return Err(DaemonError::protocol("no sink")),
                protocol::STDERR_READ => return Err(DaemonError::protocol("no source")),
                protocol::STDERR_ERROR => {
                    // protocol >= 1.26: a structured error (`readError`).
                    return Err(DaemonError::Remote(read_error(&mut self.conn.reader)?));
                }
                protocol::STDERR_NEXT => {
                    let s = self.conn.reader.read_bytes()?;
                    // `printError(chomp(…))`, the bytes as they are.
                    self.logger.log(chomp_bytes(&s));
                }
                // An activity's text, its name and keys and its string
                // fields are bytes, as everything in the log stream is.
                protocol::STDERR_START_ACTIVITY => {
                    let r = &mut self.conn.reader;
                    let id = r.read_u64()?;
                    let level = Verbosity::from_wire(r.read_u64()?);
                    let kind = ActivityType::from_wire(r.read_u64()?);
                    let text = BString::from(r.read_bytes()?);
                    let fields = read_fields(r)?;
                    let parent = r.read_u64()?;
                    self.logger.start_activity(&Activity {
                        id,
                        level,
                        kind: ActivityKind::Typed { kind, fields },
                        text,
                        parent,
                    });
                }
                protocol::STDERR_START_ACTIVITY_STRINGLY => {
                    let r = &mut self.conn.reader;
                    let id = r.read_u64()?;
                    let level = Verbosity::from_wire(r.read_u64()?);
                    let name = BString::from(r.read_bytes()?);
                    let text = BString::from(r.read_bytes()?);
                    let n = r.read_count()?;
                    let mut metadata = BTreeMap::new();
                    for _ in 0..n {
                        let key = BString::from(r.read_bytes()?);
                        let value = read_field(r)?;
                        metadata.insert(key, value);
                    }
                    let parent = r.read_u64()?;
                    self.logger.start_activity(&Activity {
                        id,
                        level,
                        kind: ActivityKind::Named { name, metadata },
                        text,
                        parent,
                    });
                }
                protocol::STDERR_STOP_ACTIVITY => {
                    let id = self.conn.reader.read_u64()?;
                    self.logger.stop_activity(id);
                }
                protocol::STDERR_RESULT => {
                    let r = &mut self.conn.reader;
                    let id = r.read_u64()?;
                    let kind = ResultType::from_wire(r.read_u64()?);
                    let fields = read_fields(r)?;
                    self.logger.result(id, kind, &fields);
                }
                protocol::STDERR_LAST => return Ok(()),
                other => {
                    return Err(DaemonError::protocol(format!(
                        "got unknown message type {other:x} from Nix daemon"
                    )))
                }
            }
        }
    }

    // ---- operations ------------------------------------------------------

    /// `IsValidPath` (1).
    pub fn is_valid_path(&mut self, path: &StorePath) -> Result<bool> {
        self.op(|c| {
            c.start_op(Op::IsValidPath)?;
            c.write_path(path)?;
            c.process_stderr()?;
            c.conn.reader.read_bool()
        })
    }

    /// `QueryValidPaths` (31): the subset of `paths` that is valid, optionally
    /// after trying to substitute the missing ones.
    pub fn query_valid_paths(
        &mut self,
        paths: &BTreeSet<StorePath>,
        maybe_substitute: bool,
    ) -> Result<BTreeSet<StorePath>> {
        self.op(|c| {
            c.start_op(Op::QueryValidPaths)?;
            c.write_path_set(paths)?;
            // protocol >= 1.27
            c.conn.writer.write_bool(maybe_substitute)?;
            c.process_stderr()?;
            c.read_path_set()
        })
    }

    /// `QueryValidDerivers` (33): the derivations in the store that
    /// produce `path`.
    pub fn query_valid_derivers(&mut self, path: &StorePath) -> Result<BTreeSet<StorePath>> {
        self.op(|c| {
            c.start_op(Op::QueryValidDerivers)?;
            c.write_path(path)?;
            c.process_stderr()?;
            c.read_path_set()
        })
    }

    /// `QueryPathInfo` (26): `None` if the path is not valid.
    pub fn query_path_info(&mut self, path: &StorePath) -> Result<Option<PathInfo>> {
        self.op(|c| {
            c.start_op(Op::QueryPathInfo)?;
            c.write_path(path)?;
            match c.process_stderr() {
                Ok(()) => {}
                // Nix's "ugly backwards compatibility hack" for daemons that
                // raised instead of answering `valid = false`.
                Err(DaemonError::Remote(e))
                    if bstr::ByteSlice::contains_str(e.message.as_slice(), "is not valid") =>
                {
                    return Ok(None)
                }
                Err(e) => return Err(e),
            }
            // protocol >= 1.17
            if !c.conn.reader.read_bool()? {
                return Ok(None);
            }
            c.read_unkeyed_path_info().map(Some)
        })
    }

    fn read_unkeyed_path_info(&mut self) -> Result<PathInfo> {
        let deriver = self.read_path_opt()?;
        // `Hash::parseAny(…, SHA256)`: the daemon sends base16.
        let nar_hash = Hash::parse_any(
            &self.conn.reader.read_string()?,
            Some(HashAlgo::Sha256),
            self.xp,
        )?;
        let references = self.read_path_set()?;
        let registration_time = self.conn.reader.read_u64()?;
        let nar_size = self.conn.reader.read_u64()?;
        // protocol >= 1.16
        let ultimate = self.conn.reader.read_bool()?;
        let sigs = self.conn.reader.read_string_set()?;
        let ca = ContentAddress::parse_opt(&self.conn.reader.read_string()?, self.xp)?;
        if self.has_feature(feature::PROVENANCE) {
            // Never negotiated by this client, but keep the framing honest.
            let _provenance = self.conn.reader.read_string()?;
        }
        Ok(PathInfo {
            deriver,
            nar_hash,
            references,
            registration_time,
            nar_size,
            ultimate,
            sigs,
            ca,
        })
    }

    fn read_valid_path_info(&mut self) -> Result<ValidPathInfo> {
        let path = self.read_path()?;
        let info = self.read_unkeyed_path_info()?;
        Ok(ValidPathInfo { path, info })
    }

    /// `QueryPathFromHashPart` (29): find the valid path with this hash part.
    pub fn query_path_from_hash_part(&mut self, hash_part: &str) -> Result<Option<StorePath>> {
        self.op(|c| {
            c.start_op(Op::QueryPathFromHashPart)?;
            c.conn.writer.write_str(hash_part)?;
            c.process_stderr()?;
            c.read_path_opt()
        })
    }

    /// `QuerySubstitutablePaths` (32): which of `paths` a substituter has.
    pub fn query_substitutable_paths(
        &mut self,
        paths: &BTreeSet<StorePath>,
    ) -> Result<BTreeSet<StorePath>> {
        self.op(|c| {
            c.start_op(Op::QuerySubstitutablePaths)?;
            c.write_path_set(paths)?;
            c.process_stderr()?;
            c.read_path_set()
        })
    }

    /// `AddToStore` (7), protocol >= 1.25 form: content-address `data` with
    /// `method`/`algo`, register it under `name` with `references`.
    ///
    /// `data` writes the serialisation the method expects: a NAR for
    /// [`ContentAddressMethod::NixArchive`] and [`ContentAddressMethod::Git`],
    /// the raw bytes for `Flat` and `Text`. It is streamed in framed chunks.
    /// If `data` fails, the frame is still terminated (as `FramedSink`'s
    /// destructor does) so the daemon stops reading, and the connection
    /// becomes unusable.
    pub fn add_to_store(
        &mut self,
        name: &str,
        method: ContentAddressMethod,
        algo: HashAlgo,
        references: &BTreeSet<StorePath>,
        repair: bool,
        data: &mut dyn FnMut(&mut dyn Write) -> io::Result<()>,
    ) -> Result<ValidPathInfo> {
        self.op(|c| {
            c.start_op(Op::AddToStore)?;
            c.conn.writer.write_str(name)?;
            c.conn.writer.write_str(&method.render_with_algo(algo))?;
            c.write_path_set(references)?;
            c.conn.writer.write_bool(repair)?;
            if c.has_feature(feature::PROVENANCE) {
                c.conn.writer.write_str("")?;
            }
            c.conn.writer.flush()?;
            let mut framed = FramedWriter::new(&mut c.conn.writer);
            let sent = data(&mut framed);
            let finished = framed.finish();
            sent?;
            finished?;
            c.process_stderr()?;
            c.read_valid_path_info()
        })
    }

    /// `AddMultipleToStore` (44): registers every path of `paths` in one
    /// operation, each as its info followed by its NAR, all in one framed
    /// stream the daemon consumes as it arrives. `count` is how many
    /// `paths` yields. A path must come after the paths it refers to.
    ///
    /// The daemon checks each NAR against its info's hash and size, and
    /// (signatures are not waived) that a path without signatures is the
    /// one its content address names: what an untrusted client may add.
    ///
    /// If `paths` or a NAR fails, or a NAR is not the size its info says,
    /// the frame is still terminated so the daemon stops reading, and the
    /// connection becomes unusable.
    pub fn add_multiple_to_store<'a>(
        &mut self,
        count: usize,
        paths: &mut dyn Iterator<Item = io::Result<PathToAdd<'a>>>,
        repair: bool,
    ) -> Result<()> {
        self.op(|c| {
            c.start_op(Op::AddMultipleToStore)?;
            c.conn.writer.write_bool(repair)?;
            // `dontCheckSigs`, which only a trusted client may ask for.
            c.conn.writer.write_bool(false)?;
            let store_dir = c.store_dir.clone();
            let mut framed = FramedWriter::new(&mut c.conn.writer);
            let sent = (|| -> Result<()> {
                framed.write_u64(count as u64)?;
                let mut written = 0;
                for path in paths {
                    let PathToAdd { info, nar } = path?;
                    write_valid_path_info(&mut framed, &store_dir, &info)?;
                    let mut counter = CountingWriter {
                        inner: &mut framed,
                        written: 0,
                    };
                    nar(&mut counter)?;
                    if counter.written != info.info.nar_size {
                        return Err(DaemonError::Io(io::Error::other(format!(
                            "the NAR of '{}' is {} bytes, not the {} announced",
                            store_dir.display(&info.path),
                            counter.written,
                            info.info.nar_size
                        ))));
                    }
                    written += 1;
                }
                if written != count {
                    return Err(DaemonError::Io(io::Error::other(format!(
                        "{written} store paths were sent where {count} were announced"
                    ))));
                }
                Ok(())
            })();
            let finished = framed.finish();
            sent?;
            finished?;
            c.process_stderr()
        })
    }

    /// Add a text file (`text:sha256`), what `builtins.toFile` produces.
    pub fn add_text(
        &mut self,
        name: &str,
        contents: &[u8],
        references: &BTreeSet<StorePath>,
    ) -> Result<ValidPathInfo> {
        self.add_to_store(
            name,
            ContentAddressMethod::Text,
            HashAlgo::Sha256,
            references,
            false,
            &mut |w| w.write_all(contents),
        )
    }

    /// Add a tree from its NAR serialisation (`fixed:r:sha256`), the default
    /// way source paths enter the store.
    pub fn add_nar(
        &mut self,
        name: &str,
        nar: &[u8],
        references: &BTreeSet<StorePath>,
    ) -> Result<ValidPathInfo> {
        self.add_to_store(
            name,
            ContentAddressMethod::NixArchive,
            HashAlgo::Sha256,
            references,
            false,
            &mut |w| w.write_all(nar),
        )
    }

    /// `AddTempRoot` (11): keep `path` alive for the connection's lifetime.
    pub fn add_temp_root(&mut self, path: &StorePath) -> Result<()> {
        self.op(|c| {
            c.start_op(Op::AddTempRoot)?;
            c.write_path(path)?;
            c.process_stderr()?;
            c.conn.reader.read_u64()?;
            Ok(())
        })
    }

    /// `AddTempRoots` (49) when negotiated, otherwise one `AddTempRoot` each.
    pub fn add_temp_roots(&mut self, paths: &BTreeSet<StorePath>) -> Result<()> {
        if paths.is_empty() {
            return Ok(());
        }
        if !self.has_feature(feature::ADD_TEMP_ROOTS) {
            for p in paths {
                self.add_temp_root(p)?;
            }
            return Ok(());
        }
        self.op(|c| {
            c.start_op(Op::AddTempRoots)?;
            c.write_path_set(paths)?;
            c.process_stderr()?;
            c.conn.reader.read_u64()?;
            Ok(())
        })
    }

    /// `AddIndirectRoot` (12): register the symlink at `symlink` (an
    /// absolute path outside the store, pointing at a store path) as a GC
    /// root. This is how `nix build` makes its `result` link keep the
    /// output alive: the client creates the symlink, the daemon records
    /// it under `/nix/var/nix/gcroots/auto`. The path goes over the wire as
    /// its bytes, as Nix sends `std::filesystem::path::string()`.
    pub fn add_indirect_root(&mut self, symlink: &std::path::Path) -> Result<()> {
        use std::os::unix::ffi::OsStrExt;
        self.op(|c| {
            c.start_op(Op::AddIndirectRoot)?;
            c.conn.writer.write_bytes(symlink.as_os_str().as_bytes())?;
            c.process_stderr()?;
            c.conn.reader.read_u64()?;
            Ok(())
        })
    }

    /// `EnsurePath` (10): make `path` valid by substituting it.
    pub fn ensure_path(&mut self, path: &StorePath) -> Result<()> {
        self.op(|c| {
            c.start_op(Op::EnsurePath)?;
            c.write_path(path)?;
            c.process_stderr()?;
            c.conn.reader.read_u64()?;
            Ok(())
        })
    }

    /// `BuildPaths` (9): build or substitute everything in `paths`. Fails
    /// with the daemon's error if any of them fails.
    pub fn build_paths(&mut self, paths: &[DerivedPath], mode: BuildMode) -> Result<()> {
        self.op(|c| {
            c.start_op(Op::BuildPaths)?;
            c.write_derived_paths(paths)?;
            c.conn.writer.write_u64(mode as u64)?;
            c.process_stderr()?;
            c.conn.reader.read_u64()?;
            Ok(())
        })
    }

    /// `BuildPathsWithResults` (46, protocol >= 1.34): like
    /// [`DaemonClient::build_paths`] but returns one result per request
    /// instead of failing on the first failure.
    pub fn build_paths_with_results(
        &mut self,
        paths: &[DerivedPath],
        mode: BuildMode,
    ) -> Result<Vec<KeyedBuildResult>> {
        self.op(|c| {
            c.start_op(Op::BuildPathsWithResults)?;
            c.write_derived_paths(paths)?;
            c.conn.writer.write_u64(mode as u64)?;
            c.process_stderr()?;
            let n = c.conn.reader.read_count()?;
            let mut results = Vec::with_capacity(n.min(1024));
            for _ in 0..n {
                let s = c.conn.reader.read_string()?;
                let path = DerivedPath::parse_legacy(&c.store_dir, &s)?;
                let result = c.read_build_result()?;
                results.push(KeyedBuildResult { path, result });
            }
            Ok(results)
        })
    }

    /// `BuildDerivation` (36): build a derivation the daemon need not have
    /// in its store, from its serialised form. The daemon resolves the
    /// input derivations itself, so `drv.input_drvs` is not sent
    /// (`BasicDerivation`).
    pub fn build_derivation(
        &mut self,
        drv_path: &StorePath,
        drv: &Derivation,
        mode: BuildMode,
    ) -> Result<BuildResult> {
        self.op(|c| {
            c.start_op(Op::BuildDerivation)?;
            c.write_path(drv_path)?;
            c.write_derivation(drv)?;
            c.conn.writer.write_u64(mode as u64)?;
            c.process_stderr()?;
            c.read_build_result()
        })
    }
    /// `writeDerivation(Sink &, …)` in `src/libstore/derivations.cc`: three
    /// strings per output (path, `<method-prefix><algo>`, hash), then the
    /// input sources, platform, builder, args and the env as pairs. The env
    /// already carries `__json` when the derivation uses structured attrs.
    fn write_derivation(&mut self, drv: &Derivation) -> Result<()> {
        let store = self.store_dir.clone();
        let w = &mut self.conn.writer;
        w.write_u64(drv.outputs.len() as u64)?;
        for (name, output) in &drv.outputs {
            // Output names are bytes, as Nix sends them.
            w.write_bytes(name)?;
            let (path, method_algo, hash) = match output {
                DerivationOutput::InputAddressed(p) => {
                    (store.display(p), String::new(), String::new())
                }
                DerivationOutput::CAFixed(ca) => {
                    let path = output
                        .path(&store, &drv.name, name)?
                        .expect("a fixed output always has a path");
                    (
                        store.display(&path),
                        ca.print_method_algo(),
                        ca.hash.to_string(HashFormat::Base16, false),
                    )
                }
                DerivationOutput::CAFloating { method, hash_algo } => (
                    String::new(),
                    format!("{}{}", method.prefix(), hash_algo.name()),
                    String::new(),
                ),
                DerivationOutput::Deferred => (String::new(), String::new(), String::new()),
                DerivationOutput::Impure { method, hash_algo } => (
                    String::new(),
                    format!("{}{}", method.prefix(), hash_algo.name()),
                    "impure".to_owned(),
                ),
            };
            w.write_str(&path)?;
            w.write_str(&method_algo)?;
            w.write_str(&hash)?;
        }
        let srcs: Vec<String> = drv.input_srcs.iter().map(|p| store.display(p)).collect();
        w.write_strings(&srcs)?;
        w.write_bytes(&drv.platform)?;
        w.write_bytes(&drv.builder)?;
        w.write_strings(&drv.args)?;
        w.write_string_map(&drv.env)?;
        Ok(())
    }

    /// `WorkerProto::Serialise<BuildResult>::read`.
    fn read_build_result(&mut self) -> Result<BuildResult> {
        let status = BuildStatus::from_wire(self.conn.reader.read_u8()?)?;
        // Bytes: it ends with the builder's last log lines.
        let error_msg = BString::from(self.conn.reader.read_bytes()?);
        // protocol >= 1.29
        let times_built = self.conn.reader.read_u64()?;
        let is_non_deterministic = self.conn.reader.read_bool()?;
        let start_time = self.conn.reader.read_u64()?;
        let stop_time = self.conn.reader.read_u64()?;
        let (cpu_user_us, cpu_system_us) = if self.version.at_least(1, 37) {
            (self.read_opt_micros()?, self.read_opt_micros()?)
        } else {
            (None, None)
        };
        // Output names are bytes on the wire, as in Nix.
        let mut built_outputs = BTreeMap::new();
        if self.has_feature(feature::REALISATION_WITH_PATH) {
            let n = self.conn.reader.read_count()?;
            for _ in 0..n {
                let output = BString::from(self.conn.reader.read_bytes()?);
                let out_path = self.read_path()?;
                let _signatures = self.conn.reader.read_strings()?;
                built_outputs.insert(output, out_path);
            }
        } else {
            // protocol >= 1.28: `<drv-hash>!<output>` → JSON realisation.
            let n = self.conn.reader.read_count()?;
            for _ in 0..n {
                let key = self.conn.reader.read_bytes()?;
                let json = self.conn.reader.read_string()?;
                let Some(bang) = key.iter().position(|&b| b == b'!') else {
                    return Err(DaemonError::protocol(format!(
                        "Invalid derivation output id {}",
                        bstr::BStr::new(&key)
                    )));
                };
                let v: serde_json::Value = serde_json::from_str(&json).map_err(|e| {
                    DaemonError::protocol(format!(
                        "invalid realisation JSON from the Nix daemon: {e}"
                    ))
                })?;
                let out_path = v.get("outPath").and_then(|p| p.as_str()).ok_or_else(|| {
                    DaemonError::protocol("realisation from the Nix daemon has no outPath")
                })?;
                built_outputs.insert(BString::from(&key[bang + 1..]), StorePath::new(out_path)?);
            }
        }
        Ok(BuildResult {
            status,
            error_msg,
            times_built,
            is_non_deterministic,
            start_time,
            stop_time,
            cpu_user_us,
            cpu_system_us,
            built_outputs,
        })
    }

    /// `std::optional<std::chrono::microseconds>`: a `uint8_t` tag, then
    /// for `1` a signed 64-bit count (`readNum<int64_t>`).
    fn read_opt_micros(&mut self) -> Result<Option<i64>> {
        match self.conn.reader.read_u8()? {
            0 => Ok(None),
            1 => {
                let n = self.conn.reader.read_u64()?;
                let micros = i64::try_from(n).map_err(|_| {
                    DaemonError::protocol(format!(
                        "serialised integer {n} is too large for type 'l'"
                    ))
                })?;
                Ok(Some(micros))
            }
            _ => Err(DaemonError::protocol("Invalid optional tag from remote")),
        }
    }

    /// `QueryDerivationOutputMap` (41, protocol >= 1.22): output name (bytes,
    /// as Nix's) → path, `None` for outputs whose path is not known yet
    /// (floating CA).
    pub fn query_derivation_output_map(
        &mut self,
        drv_path: &StorePath,
    ) -> Result<BTreeMap<BString, Option<StorePath>>> {
        self.op(|c| {
            c.start_op(Op::QueryDerivationOutputMap)?;
            c.write_path(drv_path)?;
            c.process_stderr()?;
            let n = c.conn.reader.read_count()?;
            let mut out = BTreeMap::new();
            for _ in 0..n {
                let name = BString::from(c.conn.reader.read_bytes()?);
                let path = c.read_path_opt()?;
                out.insert(name, path);
            }
            Ok(out)
        })
    }

    /// `QueryMissing` (40): what building `targets` would build, substitute
    /// or cannot resolve.
    pub fn query_missing(&mut self, targets: &[DerivedPath]) -> Result<Missing> {
        self.op(|c| {
            c.start_op(Op::QueryMissing)?;
            c.write_derived_paths(targets)?;
            c.process_stderr()?;
            let will_build = c.read_path_set()?;
            let will_substitute = c.read_path_set()?;
            let unknown = c.read_path_set()?;
            let download_size = c.conn.reader.read_u64()?;
            let nar_size = c.conn.reader.read_u64()?;
            Ok(Missing {
                will_build,
                will_substitute,
                unknown,
                download_size,
                nar_size,
            })
        })
    }

    /// `NarFromPath` (38) parsed straight into an in-memory tree, so the
    /// evaluator can `import` a built output. The reply is a bare NAR with
    /// no length prefix: parsing it (`parseDump`, as Nix's `copyNAR` does)
    /// is the only way to find where it ends.
    pub fn read_path_into_memory(&mut self, path: &StorePath) -> Result<MemoryAccessor> {
        self.op(|c| {
            c.start_op(Op::NarFromPath)?;
            c.write_path(path)?;
            c.process_stderr()?;
            Ok(brix_store::nar::parse(&mut c.conn.reader)?)
        })
    }
}

/// Writes a NAR to the connection.
pub type NarWriter<'a> = Box<dyn FnOnce(&mut dyn Write) -> io::Result<()> + 'a>;

/// A store path for [`DaemonClient::add_multiple_to_store`]: what the
/// store is to register, and the NAR, written when its turn comes.
pub struct PathToAdd<'a> {
    pub info: ValidPathInfo,
    /// Writes the NAR: exactly `info.info.nar_size` bytes.
    pub nar: NarWriter<'a>,
}

/// `WorkerProto::Serialise<ValidPathInfo>::write` as `AddMultipleToStore`
/// sends it to a daemon the `versioned-add-to-store-multiple` feature was
/// not negotiated with (this client does not offer it): the protocol 1.16
/// form, without the fields of later features.
fn write_valid_path_info<W: Write>(
    w: &mut W,
    store_dir: &StoreDir,
    info: &ValidPathInfo,
) -> Result<()> {
    let ValidPathInfo { path, info } = info;
    w.write_str(&store_dir.display(path))?;
    match &info.deriver {
        Some(deriver) => w.write_str(&store_dir.display(deriver))?,
        None => w.write_str("")?,
    }
    w.write_str(&info.nar_hash.to_string(HashFormat::Base16, false))?;
    let references: Vec<String> = info
        .references
        .iter()
        .map(|p| store_dir.display(p))
        .collect();
    w.write_strings(&references)?;
    w.write_u64(info.registration_time)?;
    w.write_u64(info.nar_size)?;
    w.write_bool(info.ultimate)?;
    let sigs: Vec<&String> = info.sigs.iter().collect();
    w.write_strings(&sigs)?;
    match &info.ca {
        Some(ca) => w.write_str(&ca.render()),
        None => w.write_str(""),
    }
}

struct CountingWriter<'a> {
    inner: &'a mut dyn Write,
    written: u64,
}

impl Write for CountingWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.written += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// `BasicClientConnection::handshake`, with the `SerialisationError`
/// handling `RemoteStore::initConnection` wraps around it.
fn handshake(conn: &mut Connection) -> Result<(ProtocolVersion, BTreeSet<String>)> {
    conn.writer.write_u64(protocol::WORKER_MAGIC_1)?;
    conn.writer.write_u64(ProtocolVersion::LATEST.to_wire())?;
    conn.writer.flush()?;

    let mut saved = Vec::new();
    let result = negotiate(
        &mut Tee {
            inner: &mut conn.reader,
            saved: &mut saved,
        },
        &mut conn.writer,
    );
    match result {
        // The reply is not made of protocol integers and strings at all:
        // close our side in case the other is waiting for input, read what
        // it still sends, and show everything it sent, bytes as they are.
        Err(DaemonError::Protocol(_)) => {
            conn.close_write()?;
            conn.reader.read_to_end(&mut saved)?;
            Err(DaemonError::UnexpectedReply(chomp_bytes(&saved).into()))
        }
        other => other,
    }
}

/// The daemon's half of the handshake: magic, version, features.
fn negotiate(r: &mut impl Read, w: &mut impl Write) -> Result<(ProtocolVersion, BTreeSet<String>)> {
    let magic = read_int(r)?;
    if magic != protocol::WORKER_MAGIC_2 {
        return Err(if magic == protocol::WORKER_MAGIC_ACCESS_DENIED {
            DaemonError::AccessDenied
        } else {
            DaemonError::ProtocolMismatch
        });
    }
    let daemon_version = ProtocolVersion::from_wire(read_int(r)?);
    if daemon_version.major != ProtocolVersion::LATEST.major {
        return Err(DaemonError::UnsupportedMajorVersion);
    }
    let version = daemon_version.min(ProtocolVersion::LATEST);
    if version < ProtocolVersion::MINIMUM {
        return Err(DaemonError::DaemonTooOld(version, ProtocolVersion::MINIMUM));
    }

    // protocol >= 1.38: exchange feature sets.
    let mut features = BTreeSet::new();
    if version.at_least(1, 38) {
        let ours = protocol::client_features();
        w.write_strings(&ours)?;
        w.flush()?;
        // Names need not be UTF-8; one that is not cannot match ours.
        let n = r.read_count()?;
        let mut theirs = BTreeSet::new();
        for _ in 0..n {
            theirs.insert(String::from_utf8_lossy(&r.read_bytes()?).into_owned());
        }
        features = ours.intersection(&theirs).cloned().collect();
    }
    Ok((version, features))
}

/// `readInt`: a number that must fit an `unsigned int`.
fn read_int(r: &mut impl Read) -> Result<u64> {
    let n = r.read_u64()?;
    if n > u64::from(u32::MAX) {
        return Err(DaemonError::protocol(format!(
            "serialised integer {n} is too large for type 'j'"
        )));
    }
    Ok(n)
}

/// Records what the handshake reads (`TeeSource`), for the "protocol
/// mismatch, got '…'" message.
struct Tee<'a, R> {
    inner: &'a mut R,
    saved: &'a mut Vec<u8>,
}

impl<R: Read> Read for Tee<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.saved.extend_from_slice(&buf[..n]);
        Ok(n)
    }
}

/// `readError` in `src/libutil/serialise.cc`.
fn read_error<R: Read>(r: &mut R) -> Result<RemoteError> {
    let ty = r.read_string()?;
    if ty != "Error" {
        return Err(DaemonError::protocol(format!(
            "unexpected error type '{ty}'"
        )));
    }
    let level = Verbosity::from_wire(r.read_u64()?);
    let _name = r.read_string()?; // removed field, always "Error"
                                  // The message and traces are bytes, as Nix's are: they can quote a
                                  // name that is not UTF-8.
    let message = BString::from(r.read_bytes()?);
    if r.read_u64()? != 0 {
        return Err(DaemonError::protocol(
            "deserializing error positions is not supported",
        ));
    }
    let n = r.read_count()?;
    let mut traces = Vec::with_capacity(n.min(64));
    for _ in 0..n {
        if r.read_u64()? != 0 {
            return Err(DaemonError::protocol(
                "deserializing error positions is not supported",
            ));
        }
        traces.push(BString::from(r.read_bytes()?));
    }
    Ok(RemoteError {
        level,
        message,
        traces,
    })
}

/// A field of an activity or a result: a string is bytes, as in Nix (a
/// build log line is whatever the builder printed).
fn read_field<R: Read>(r: &mut R) -> Result<Field> {
    match r.read_u64()? {
        0 => Ok(Field::Int(r.read_u64()?)),
        1 => Ok(Field::Str(BString::from(r.read_bytes()?))),
        t => Err(DaemonError::protocol(format!(
            "got unsupported field type {t:x} from Nix daemon"
        ))),
    }
}

fn read_fields<R: Read>(r: &mut R) -> Result<Vec<Field>> {
    let n = r.read_count()?;
    let mut fields = Vec::with_capacity(n.min(64));
    for _ in 0..n {
        fields.push(read_field(r)?);
    }
    Ok(fields)
}

impl std::fmt::Debug for DaemonClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DaemonClient")
            .field("version", &self.version)
            .field("features", &self.features)
            .field("store_dir", &self.store_dir)
            .field("daemon_nix_version", &self.daemon_nix_version)
            .field("trust", &self.trust)
            .finish_non_exhaustive()
    }
}
