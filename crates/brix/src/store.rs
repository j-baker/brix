//! The Nix store brix delegates to: a daemon reached lazily, the logger
//! that renders what it says, import-from-derivation
//! (`daemon_build`), and the store operations of a [`Session`] (build,
//! query, GC roots). brix has no store of its own; everything
//! here is a request to the daemon, with the derivations evaluation
//! produced uploaded first.

use std::collections::{BTreeMap, BTreeSet};

use brix_store::{Derivation, StoreDir, StorePath};
use bstr::BString;

use crate::config::{BuildLogMode, Config};
use crate::error::{Error, Result};
use crate::nixconf;
use crate::session::{Ctx, Session};

pub use brix_daemon::{
    BuildMode, BuildResult, BuildStatus, DerivedPath, KeyedBuildResult, Missing, OutputsSpec,
    PathInfo,
};

/// The daemon connection, made on first use so that evaluation-only
/// commands never need one.
pub(crate) enum DaemonState {
    /// No store configured (`dummy://`).
    None,
    /// Configured but not connected yet.
    Lazy {
        uri: brix_daemon::StoreUri,
        options: Option<brix_daemon::ClientOptions>,
        /// The experimental features hashes from the daemon are read
        /// with.
        xp: brix_store::XpSettings,
    },
    Connected(brix_daemon::DaemonClient),
    /// The connection failed once, and is not tried again: every later
    /// request fails as Nix's `RemoteStore::openConnectionWrapper` makes
    /// it fail. Holds the store's name in messages.
    Failed(String),
    /// The session has ended and closed the connection
    /// ([`DaemonState::close`]); nothing may use the store any more.
    Closed,
}

impl DaemonState {
    pub(crate) fn new(config: &Config, store_dir: &StoreDir) -> Result<DaemonState> {
        let Some(uri) = &config.daemon else {
            return Ok(DaemonState::None);
        };
        if uri == "dummy://" {
            return Ok(DaemonState::None);
        }
        let uri = brix_daemon::StoreUri::parse(uri, &|msg| {
            crate::log::stderr_bytes(&brix_eval::indent_message("warning", msg))
        })
        .map_err(|e| match e {
            brix_daemon::DaemonError::Usage(msg) => Error::Usage(msg.into()),
            e => Error::Daemon(e),
        })?;
        let mut settings = brix_daemon::ClientSettings {
            verbosity: match config.build_log {
                BuildLogMode::Quiet => brix_daemon::Verbosity::Warn,
                BuildLogMode::Normal | BuildLogMode::Logs => brix_daemon::Verbosity::Info,
            },
            verbose_build: true,
            ..brix_daemon::ClientSettings::default()
        };
        for (k, v) in &config.daemon_settings {
            match k.as_str() {
                "max-jobs" => settings.max_build_jobs = nixconf::parse_max_jobs(v)?.into(),
                "keep-going" => settings.keep_going = nixconf::parse_bool(k, v)?,
                "keep-failed" => settings.keep_failed = nixconf::parse_bool(k, v)?,
                "fallback" => settings.try_fallback = nixconf::parse_bool(k, v)?,
                // `unsigned int`.
                "cores" => settings.build_cores = nixconf::parse_int::<u32>(k, v)?.into(),
                "substitute" => settings.use_substitutes = nixconf::parse_bool(k, v)?,
                // `time_t`, sent as the `uint64_t` it converts to.
                "max-silent-time" => {
                    settings.max_silent_time = nixconf::parse_int::<i64>(k, v)? as u64
                }
                _ => {
                    settings.overrides.insert(k.clone(), v.clone());
                }
            }
        }
        let options = brix_daemon::ClientOptions {
            store_dir: store_dir.clone(),
            settings,
            logger: Box::new(
                BuildLogger::new(config.build_log).with_progress(config.build_progress.clone()),
            ),
        };
        Ok(DaemonState::Lazy {
            uri,
            options: Some(options),
            xp: brix_store::XpSettings::from_names(
                config.eval.experimental_features.iter().map(String::as_str),
            ),
        })
    }

    /// The connection, made now if necessary. `purpose` names what needs
    /// it, for the error when there is no store.
    pub(crate) fn client(&mut self, purpose: &str) -> Result<&mut brix_daemon::DaemonClient> {
        match self {
            DaemonState::None => Err(Error::Usage(format!(
                "{purpose} needs a Nix store, but none is configured; pass '--store daemon' (or a 'unix://' or 'ssh-ng://' URI)"
            ).into())),
            DaemonState::Failed(uri) => Err(Error::Config(format!(
                "opening a connection to remote store '{uri}' previously failed"
            ))),
            DaemonState::Closed => panic!("the Nix store was used after its session ended"),
            DaemonState::Connected(c) => Ok(c),
            DaemonState::Lazy { uri, options, xp } => {
                let options = options.take().expect("options are consumed once");
                match brix_daemon::DaemonClient::connect(uri, options) {
                    Ok(mut client) => {
                        client.set_xp_settings(*xp);
                        *self = DaemonState::Connected(client);
                        match self {
                            DaemonState::Connected(c) => Ok(c),
                            _ => unreachable!(),
                        }
                    }
                    // brix-daemon's errors are Nix's own, which say what
                    // could not be reached.
                    Err(e) => {
                        *self = DaemonState::Failed(uri.display().to_owned());
                        Err(Error::Daemon(e))
                    }
                }
            }
        }
    }

    /// The connection if a store is configured, `None` if there is none
    /// (`dummy://`) or the session has ended (a background fetch may
    /// still be finishing then). For queries brix can answer without a
    /// store (NAR hashes of store paths, validity checks); a store that
    /// is configured but cannot be reached is the error it is in Nix.
    pub(crate) fn try_client(&mut self) -> Result<Option<&mut brix_daemon::DaemonClient>> {
        match self {
            DaemonState::None | DaemonState::Closed => Ok(None),
            _ => self.client("").map(Some),
        }
    }

    pub(crate) fn is_configured(&self) -> bool {
        !matches!(self, DaemonState::None)
    }

    /// Closes the connection, if one was made: the client is dropped,
    /// which flushes it and, for `ssh-ng`, kills the `ssh` child (a
    /// local `nix-daemon --stdio` gets up to 10 s to exit first).
    pub(crate) fn close(&mut self) {
        *self = DaemonState::Closed;
    }
}

/// Whether the configured store has `path` (`isValidPath`): `false`
/// without a store; a store that cannot be reached, or fails the query,
/// is an error, as it is in Nix.
pub(crate) fn store_has(
    daemon: &std::sync::Mutex<DaemonState>,
    path: &brix_store::StorePath,
) -> Result<bool> {
    let mut daemon = daemon.lock().expect("daemon state poisoned");
    match daemon.try_client()? {
        Some(client) => Ok(client.is_valid_path(path)?),
        None => Ok(false),
    }
}

/// Receives the daemon's structured activities independently of build log lines.
/// A terminal front end can render these with its fetch progress bar.
pub trait BuildProgress: Send + Sync + std::fmt::Debug {
    fn start(&self, activity: &brix_daemon::logger::Activity);
    fn stop(&self, id: brix_daemon::logger::ActivityId);
    fn result(
        &self,
        id: brix_daemon::logger::ActivityId,
        kind: brix_daemon::logger::ResultType,
        fields: &[brix_daemon::logger::Field],
    );
}

type LogOutput = Box<dyn FnMut(&[u8]) + Send>;

/// Renders the daemon's messages the way `nix`'s logger (its progress
/// bar, which `nix` uses whether or not stderr is a terminal) renders
/// them: activity texts (`building '…'...`) and plain messages, and with
/// [`BuildLogMode::Logs`] each build output line prefixed with the name
/// of its derivation (`hello> …`, or `hello (post)> …` from a post-build
/// hook), like `nix -L`. Everything goes through
/// [`crate::log::stderr_bytes`], which keeps colours only on a terminal
/// (`filterANSIEscapes(s, !isTTY)`). The texts are bytes, as the daemon
/// sends them: a builder's output is whatever it printed, and Nix writes
/// it as it is.
pub struct BuildLogger {
    mode: BuildLogMode,
    progress: Option<std::sync::Arc<dyn BuildProgress>>,
    activities: BTreeSet<brix_daemon::logger::ActivityId>,
    /// Build and post-build-hook activities by id, with the name their
    /// lines are prefixed with.
    names: BTreeMap<brix_daemon::logger::ActivityId, BString>,
    /// Where messages go.
    emit: LogOutput,
}

impl BuildLogger {
    pub fn new(mode: BuildLogMode) -> BuildLogger {
        BuildLogger::with_output(mode, crate::log::stderr_bytes)
    }

    /// A logger that hands each message to `emit` instead of stderr.
    pub fn with_output(
        mode: BuildLogMode,
        emit: impl FnMut(&[u8]) + Send + 'static,
    ) -> BuildLogger {
        BuildLogger {
            mode,
            progress: None,
            activities: BTreeSet::new(),
            names: BTreeMap::new(),
            emit: Box::new(emit),
        }
    }
    pub fn with_progress(mut self, progress: Option<std::sync::Arc<dyn BuildProgress>>) -> Self {
        self.progress = progress;
        self
    }
}

const ANSI_FAINT: &str = "\x1b[2m";
const ANSI_NORMAL: &str = "\x1b[0m";

/// `storePathToNameWithoutDrvSuffix`: the name of the store path `path`
/// (after the first `-` of its base name), without `.drv`. Bytes, as the
/// daemon sends them.
fn store_path_name_without_drv_suffix(path: &[u8]) -> &[u8] {
    let base = path.rsplit(|&b| b == b'/').next().unwrap_or(path);
    let name = base
        .iter()
        .position(|&b| b == b'-')
        .map_or(&b""[..], |dash| &base[dash + 1..]);
    name.strip_suffix(b".drv").unwrap_or(name)
}

/// `chomp`: without trailing spaces, tabs and line ends.
fn chomp(line: &[u8]) -> &[u8] {
    let end = line
        .iter()
        .rposition(|&b| !matches!(b, b' ' | b'\n' | b'\r' | b'\t'))
        .map_or(0, |i| i + 1);
    &line[..end]
}

impl brix_daemon::Logger for BuildLogger {
    fn log(&mut self, message: &[u8]) {
        if self.mode != BuildLogMode::Quiet
            || message.starts_with(b"warning:")
            || message.starts_with(b"error:")
        {
            (self.emit)(message);
        }
    }

    fn start_activity(&mut self, activity: &brix_daemon::logger::Activity) {
        use brix_daemon::logger::{ActivityKind, ActivityType, Field};
        if let Some(progress) = &self.progress {
            progress.start(activity);
            self.activities.insert(activity.id);
        }
        let kind = match &activity.kind {
            ActivityKind::Typed { kind, fields } => {
                // `ProgressBar::startActivity`: builds and post-build
                // hooks are named after their derivation's `DrvName`.
                if let (ActivityType::Build | ActivityType::PostBuildHook, Some(Field::Str(drv))) =
                    (kind, fields.first())
                {
                    let name = store_path_name_without_drv_suffix(drv);
                    self.names
                        .insert(activity.id, brix_eval::parse_drv_name(name).0.into());
                }
                Some(*kind)
            }
            ActivityKind::Named { .. } => None,
        };
        // `logActivity`: shown at the activity's own level, followed by
        // "..."; the daemon's "querying info about missing paths" is
        // talkative, "building '…'" is info. Waiting for a build slot is
        // never shown.
        if self.mode != BuildLogMode::Quiet
            && self.progress.is_none()
            && !activity.text.is_empty()
            && activity.level <= brix_daemon::Verbosity::Info
            && kind != Some(ActivityType::BuildWaiting)
        {
            (self.emit)(&[activity.text.as_slice(), b"..."].concat());
        }
    }

    fn stop_activity(&mut self, id: brix_daemon::logger::ActivityId) {
        self.names.remove(&id);
        self.activities.remove(&id);
        if let Some(progress) = &self.progress {
            progress.stop(id);
        }
    }

    fn result(
        &mut self,
        id: brix_daemon::logger::ActivityId,
        kind: brix_daemon::logger::ResultType,
        fields: &[brix_daemon::logger::Field],
    ) {
        use brix_daemon::logger::{Field, ResultType};
        if let Some(progress) = &self.progress {
            // With -L the lines are printed above the bar instead.
            if self.mode != BuildLogMode::Logs
                || !matches!(
                    kind,
                    ResultType::BuildLogLine | ResultType::PostBuildLogLine
                )
            {
                progress.result(id, kind, fields);
            }
        }
        if self.mode != BuildLogMode::Logs {
            return;
        }
        let suffix = match kind {
            ResultType::BuildLogLine => "> ",
            ResultType::PostBuildLogLine => " (post)> ",
            _ => return,
        };
        let Some(Field::Str(line)) = fields.first() else {
            return;
        };
        let name = self
            .names
            .get(&id)
            .map_or(&b"unnamed"[..], |name| name.as_slice());
        (self.emit)(
            &[
                ANSI_FAINT.as_bytes(),
                name,
                suffix.as_bytes(),
                ANSI_NORMAL.as_bytes(),
                chomp(line),
            ]
            .concat(),
        );
    }
}

impl Drop for BuildLogger {
    fn drop(&mut self) {
        // A failed/disconnected operation may leave activities without a stop.
        if let Some(progress) = &self.progress {
            for id in &self.activities {
                progress.stop(*id);
            }
        }
    }
}

/// Import-from-derivation through the daemon: upload what the daemon
/// lacks, build, and mount the outputs under the store directory.
pub(crate) mod daemon_build {
    #[cfg(brix_parallel)]
    use brix_eval::parallel_support::cell::RefCell;
    #[cfg(not(brix_parallel))]
    use std::cell::RefCell;
    use std::collections::{BTreeMap, BTreeSet, HashMap};
    use std::sync::Arc;

    use brix_daemon::{BuildMode, DaemonClient, DerivedPath, PathInfo, PathToAdd, ValidPathInfo};
    use brix_eval::{BuildRequest, EvalStoreView};
    use brix_flake::Input;
    use brix_store::{CanonPath, Hash, HashAlgo, SourceAccessor, StoreDir, StoreObject, StorePath};

    use crate::error::{Error, Result};
    use crate::host::Tree;
    use crate::root_fs::HostRoot;

    pub(crate) fn build(
        client: &mut DaemonClient,
        store_dir: &StoreDir,
        root: &HostRoot,
        trees: &RefCell<HashMap<StorePath, Tree>>,
        requests: &[BuildRequest],
        view: &dyn EvalStoreView,
    ) -> Result<Vec<StorePath>> {
        upload_closures(
            client,
            store_dir,
            view,
            requests.iter().map(|req| req.drv_path.clone()).collect(),
        )?;
        let paths: Vec<DerivedPath> = requests
            .iter()
            .map(|r| DerivedPath::outputs(r.drv_path.clone(), r.outputs.iter().cloned()))
            .collect();
        let results = client.build_paths_with_results(&paths, BuildMode::Normal)?;
        let mut outputs: Vec<StorePath> = Vec::new();
        for keyed in &results {
            if !keyed.result.is_success() {
                // The daemon's message is bytes (it ends with the build's
                // last log lines).
                let message = [
                    format!(
                        "build of '{}' failed: ",
                        describe_derived(store_dir, &keyed.path)
                    )
                    .as_bytes(),
                    keyed.result.error_msg.as_slice(),
                ]
                .concat();
                return Err(Error::Store(message.into()));
            }
            outputs.extend(keyed.result.built_outputs.values().cloned());
        }
        // Input-addressed derivations know their output paths without the
        // daemon telling us; fill in whatever the results left out.
        for req in requests {
            if let Some(StoreObject::Derivation(drv)) = view.object(&req.drv_path) {
                for name in &req.outputs {
                    if let Some(brix_store::DerivationOutput::InputAddressed(p)) =
                        drv.outputs.get(name)
                    {
                        if !outputs.contains(p) {
                            outputs.push(p.clone());
                        }
                    }
                }
            }
        }
        for out in &outputs {
            let mem = client.read_path_into_memory(out)?;
            let accessor: Arc<dyn SourceAccessor> = Arc::new(mem);
            let display = store_dir.display(out);
            let at = CanonPath::new(&display)?;
            root.mount(at, accessor.clone());
            let input = Input::from_attrs(
                &brix_flake::FetchSettings::default(),
                [
                    ("type".to_owned(), brix_flake::Attr::String("path".into())),
                    ("path".to_owned(), brix_flake::Attr::String(display)),
                ]
                .into_iter()
                .collect(),
            )?;
            trees.borrow_mut().insert(
                out.clone(),
                Tree {
                    accessor,
                    input,
                    store_path: out.clone(),
                    nar_hash: None,
                    from_store: false,
                },
            );
        }
        output_closure(client, store_dir, outputs)
    }

    /// `computeFSClosure` of `outputs`: the outputs and, recursively, what
    /// they refer to, as the daemon records it (`QueryPathInfo`).
    fn output_closure(
        client: &mut DaemonClient,
        store_dir: &StoreDir,
        outputs: Vec<StorePath>,
    ) -> Result<Vec<StorePath>> {
        let mut closure = BTreeSet::new();
        let mut todo = outputs;
        while let Some(path) = todo.pop() {
            if closure.contains(&path) {
                continue;
            }
            let info = client.query_path_info(&path)?.ok_or_else(|| {
                Error::Store(format!("path '{}' is not valid", store_dir.display(&path)).into())
            })?;
            todo.extend(info.references.into_iter().filter(|r| !closure.contains(r)));
            closure.insert(path);
        }
        Ok(closure.into_iter().collect())
    }

    fn describe_derived(store_dir: &StoreDir, p: &DerivedPath) -> String {
        match p {
            DerivedPath::Opaque(p) => store_dir.display(p),
            DerivedPath::Built { drv_path, .. } => store_dir.display(drv_path),
        }
    }

    /// Query one dependency frontier at a time, pruning valid closures:
    /// a wide graph takes one validity RPC per depth rather than per
    /// path. What is missing is then sent in one `AddMultipleToStore`,
    /// each object serialised as it is written to the connection: a
    /// source is neither read before it is known to be missing nor held
    /// in memory.
    pub(crate) fn upload_closures(
        client: &mut DaemonClient,
        store_dir: &StoreDir,
        view: &dyn EvalStoreView,
        paths: BTreeSet<StorePath>,
    ) -> Result<()> {
        let mut objects = missing_closure(store_dir, view, paths.clone(), |paths| {
            Ok(client.query_valid_paths(paths, false)?)
        })?;
        let mut ordered = Vec::with_capacity(objects.len());
        for path in &paths {
            dependency_order(path, &mut objects, &mut ordered);
        }
        if ordered.is_empty() {
            return Ok(());
        }
        // An object that cannot be prepared ends the stream; its error is
        // the one to report, not the connection's.
        let mut failure = None;
        let count = ordered.len();
        let mut to_add = ordered.into_iter().map(|(path, object)| {
            path_to_add(store_dir, view, path, object).map_err(|e| {
                let stopped = std::io::Error::other(e.to_string());
                failure = Some(e);
                stopped
            })
        });
        let added = client.add_multiple_to_store(count, &mut to_add, false);
        drop(to_add);
        match failure {
            Some(e) => Err(e),
            None => Ok(added?),
        }
    }

    fn missing_closure(
        store_dir: &StoreDir,
        view: &dyn EvalStoreView,
        paths: BTreeSet<StorePath>,
        mut query_valid: impl FnMut(&BTreeSet<StorePath>) -> Result<BTreeSet<StorePath>>,
    ) -> Result<BTreeMap<StorePath, StoreObject>> {
        let mut todo = paths;
        let mut seen = BTreeSet::new();
        let mut valid = BTreeSet::new();
        let mut objects = BTreeMap::new();
        while !todo.is_empty() {
            seen.extend(todo.iter().cloned());
            valid.extend(query_valid(&todo)?);
            let mut next = BTreeSet::new();
            for path in todo.difference(&valid) {
                let object = view.object(path).ok_or_else(|| Error::Store(format!(
                    "path '{}' is needed for a build but is neither in the Nix store nor known to brix",
                    store_dir.display(path)).into()))?;
                match &object {
                    StoreObject::Derivation(drv) => next.extend(drv.references()),
                    StoreObject::Text { references, .. } => next.extend(references.iter().cloned()),
                    StoreObject::SourceTree { .. } => {}
                }
                objects.insert(path.clone(), object);
            }
            todo = next.difference(&seen).cloned().collect();
        }
        Ok(objects)
    }

    /// The missing objects `path` needs, then `path`, each once: the
    /// order a store registers them in. Taking an object out of the plan
    /// also deduplicates dependencies shared between roots.
    fn dependency_order(
        path: &StorePath,
        objects: &mut BTreeMap<StorePath, StoreObject>,
        ordered: &mut Vec<(StorePath, StoreObject)>,
    ) {
        let Some(object) = objects.remove(path) else {
            return;
        };
        match &object {
            StoreObject::Derivation(drv) => {
                for input in drv.input_drvs.keys().chain(drv.input_srcs.iter()) {
                    dependency_order(input, objects, ordered);
                }
            }
            StoreObject::Text { references, .. } => {
                for input in references {
                    dependency_order(input, objects, ordered);
                }
            }
            StoreObject::SourceTree { .. } => {}
        }
        ordered.push((path.clone(), object));
    }

    /// What the store is told about `object` (a content-addressed path
    /// without signatures, which the daemon checks against its contents),
    /// and its NAR.
    fn path_to_add<'a>(
        store_dir: &StoreDir,
        view: &'a dyn EvalStoreView,
        path: StorePath,
        object: StoreObject,
    ) -> Result<PathToAdd<'a>> {
        let info = |nar_hash, nar_size, references, ca| PathInfo {
            deriver: None,
            nar_hash,
            references,
            registration_time: 0,
            nar_size,
            ultimate: false,
            sigs: BTreeSet::new(),
            ca: Some(ca),
        };
        let text = |path, contents: Vec<u8>, references| -> Result<PathToAdd<'a>> {
            let mut nar = Vec::with_capacity(contents.len() + 128);
            brix_store::nar::dump_bytes(&contents, &mut nar)
                .map_err(|e| Error::Store(e.message_bytes().into()))?;
            let ca = brix_store::ContentAddress {
                method: brix_store::ContentAddressMethod::Text,
                hash: Hash::of(HashAlgo::Sha256, &contents),
            };
            Ok(PathToAdd {
                info: ValidPathInfo {
                    path,
                    info: info(
                        Hash::of(HashAlgo::Sha256, &nar),
                        nar.len() as u64,
                        references,
                        ca,
                    ),
                },
                nar: Box::new(move |out| out.write_all(&nar)),
            })
        };
        match object {
            StoreObject::Derivation(drv) => {
                let contents = drv.to_aterm(store_dir)?;
                text(path, contents, drv.references())
            }
            StoreObject::Text {
                contents,
                references,
            } => text(path, contents, references),
            StoreObject::SourceTree { .. } => {
                let nar = view.source_nar(&path).map_err(|e| Error::Store(e.into()))?;
                Ok(PathToAdd {
                    info: ValidPathInfo {
                        path: path.clone(),
                        info: info(nar.hash, nar.size, BTreeSet::new(), nar.ca),
                    },
                    nar: Box::new(move |out| {
                        view.write_source_nar(&path, out)
                            .map_err(std::io::Error::other)
                    }),
                })
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        struct View(BTreeMap<StorePath, StoreObject>);
        impl EvalStoreView for View {
            fn object(&self, path: &StorePath) -> Option<StoreObject> {
                self.0.get(path).cloned()
            }
            fn source_nar(
                &self,
                path: &StorePath,
            ) -> std::result::Result<brix_eval::SourceNar, String> {
                self.write_source_nar(path, &mut std::io::sink())?;
                unreachable!()
            }
            fn write_source_nar(
                &self,
                _: &StorePath,
                _: &mut dyn std::io::Write,
            ) -> std::result::Result<(), String> {
                panic!("planning must not serialise sources")
            }
        }
        fn path(n: usize) -> StorePath {
            StorePath::new(format!("00000000000000000000000000000000-path-{n}")).unwrap()
        }
        fn text(references: BTreeSet<StorePath>) -> StoreObject {
            StoreObject::Text {
                contents: vec![],
                references,
            }
        }

        #[test]
        fn wide_shared_closures_are_queried_by_depth_and_valid_paths_are_pruned() {
            let roots: BTreeSet<_> = (0..1000).map(path).collect();
            let shared = path(1000);
            let cached = path(1001);
            let mut objects: BTreeMap<_, _> = roots
                .iter()
                .map(|p| (p.clone(), text([shared.clone(), cached.clone()].into())))
                .collect();
            objects.insert(shared.clone(), text(BTreeSet::new()));
            // The cached path is deliberately absent from the virtual store:
            // neither it nor its references should be read.
            let view = View(objects);
            let mut queries = vec![];
            let plan = missing_closure(&StoreDir::default(), &view, roots.clone(), |paths| {
                queries.push(paths.clone());
                Ok(paths
                    .intersection(&[cached.clone()].into())
                    .cloned()
                    .collect())
            })
            .unwrap();
            assert_eq!(queries, [roots.clone(), [shared, cached].into()]);
            assert_eq!(plan.len(), 1001);

            queries.clear();
            let plan = missing_closure(&StoreDir::default(), &view, roots.clone(), |paths| {
                queries.push(paths.clone());
                Ok(paths.clone())
            })
            .unwrap();
            assert_eq!(queries, [roots]);
            assert!(plan.is_empty());
        }

        #[test]
        fn missing_unknown_inputs_and_query_failures_are_not_silently_skipped() {
            let root = path(0);
            let missing = path(1);
            let view = View([(root.clone(), text([missing.clone()].into()))].into());
            let err = missing_closure(&StoreDir::default(), &view, [root.clone()].into(), |_| {
                Ok(BTreeSet::new())
            })
            .unwrap_err();
            assert!(err.to_string().contains(&missing.to_string()), "{err}");
            let err = missing_closure(&StoreDir::default(), &view, [root].into(), |_| {
                Err(Error::Config("query failed".into()))
            })
            .unwrap_err();
            assert!(err.to_string().contains("query failed"), "{err}");
        }
    }
}

/// Store operations: everything here talks to the daemon, uploading the
/// derivations evaluation produced when the daemon does not have them.
impl Session {
    fn with_daemon<R>(
        &self,
        purpose: &str,
        f: impl for<'a> FnOnce(&Ctx<'_, 'a>, &mut brix_daemon::DaemonClient) -> Result<R>,
    ) -> Result<R> {
        let _waiting = brix_flake::Waiting::new(&*self.config.progress);
        self.with(|ctx| {
            let mut state = ctx.host.daemon.lock().expect("daemon state poisoned");
            let client = state.client(purpose)?;
            f(ctx, client)
        })
    }

    /// Makes the store paths among `paths` that evaluation produced (the
    /// derivations behind them, and what it "added": a path value, a
    /// `toFile` text) valid in the daemon's store with their inputs,
    /// uploading from the virtual store as needed: Nix's evaluation writes
    /// those to the store as it makes them. Other store paths, derivations
    /// included, are the daemon's to find, substitute or report missing.
    fn upload_for(&self, paths: &[crate::store::DerivedPath], purpose: &str) -> Result<()> {
        let _activity = brix_flake::Activity::new(
            &*self.config.progress,
            brix_flake::ActivityKind::Other,
            "preparing build inputs",
        );
        self.with_daemon(purpose, |ctx, client| {
            let roots = paths
                .iter()
                .filter_map(|p| {
                    let (DerivedPath::Built { drv_path: path, .. } | DerivedPath::Opaque(path)) = p;
                    ctx.ev
                        .services()
                        .virtual_store()
                        .get(path)
                        .map(|_| path.clone())
                })
                .collect();
            daemon_build::upload_closures(client, self.config.store_dir(), ctx.ev.services(), roots)
        })
    }

    /// Writes the store paths among `paths` that evaluation produced
    /// (derivations, `toFile` texts, sources it "added"), with their
    /// closures, to the daemon's store, as Nix's evaluation writes them as
    /// it makes them. Paths evaluation did not produce are left alone.
    pub fn store_upload(&self, paths: &[StorePath]) -> Result<()> {
        let paths: Vec<crate::store::DerivedPath> = paths
            .iter()
            .map(|p| crate::store::DerivedPath::Opaque(p.clone()))
            .collect();
        self.upload_for(&paths, "adding to the store")
    }

    /// Builds (or substitutes) `paths`, returning one result per request
    /// the daemon answered (`buildPathsWithResults`).
    pub fn store_realise(
        &self,
        paths: &[crate::store::DerivedPath],
        mode: crate::store::BuildMode,
    ) -> Result<Vec<crate::store::KeyedBuildResult>> {
        self.upload_for(paths, "building")?;
        self.with_daemon("building", |_, client| {
            Ok(client.build_paths_with_results(paths, mode)?)
        })
    }

    /// `queryMissing`: what building `paths` would build, fetch, or fail on.
    pub fn store_query_missing(
        &self,
        paths: &[crate::store::DerivedPath],
    ) -> Result<crate::store::Missing> {
        self.upload_for(paths, "a dry run")?;
        self.with_daemon("a dry run", |_, client| Ok(client.query_missing(paths)?))
    }

    pub fn store_path_info(&self, path: &StorePath) -> Result<Option<crate::store::PathInfo>> {
        self.with_daemon("querying a store path", |_, client| {
            Ok(client.query_path_info(path)?)
        })
    }

    pub fn store_is_valid_path(&self, path: &StorePath) -> Result<bool> {
        self.with_daemon("querying a store path", |_, client| {
            Ok(client.is_valid_path(path)?)
        })
    }

    /// Opens the connection to the store now, if one is configured
    /// (`dummy://` has none), for a question whose answer brix knows
    /// without asking (whether a lazily mounted tree's virtual store path
    /// is valid: it is not): Nix asks, so a store that cannot be reached
    /// fails it.
    pub fn store_connect(&self) -> Result<()> {
        self.with(|ctx| {
            let mut daemon = ctx.host.daemon.lock().expect("daemon state poisoned");
            daemon.try_client().map(|_| ())
        })
    }

    /// `queryValidDerivers`: the derivations in the store that produce
    /// `path`.
    pub fn store_valid_derivers(&self, path: &StorePath) -> Result<BTreeSet<StorePath>> {
        self.with_daemon("querying a store path", |_, client| {
            Ok(client.query_valid_derivers(path)?)
        })
    }

    /// `ensurePath`: make a store path valid by substituting it.
    pub fn store_ensure_path(&self, path: &StorePath) -> Result<()> {
        self.with_daemon("fetching a store path", |_, client| {
            Ok(client.ensure_path(path)?)
        })
    }

    /// Registers `symlink` (which must point into the store) as a GC root.
    pub fn store_add_indirect_root(&self, symlink: &std::path::Path) -> Result<()> {
        self.with_daemon("creating a result symlink", |_, client| {
            Ok(client.add_indirect_root(symlink)?)
        })
    }

    /// Writes a derivation to the daemon's store (`writeDerivation`),
    /// returning its path.
    pub fn store_write_derivation(&self, drv: &Derivation) -> Result<StorePath> {
        let store_dir = self.config.store_dir().clone();
        let text = drv.to_aterm(&store_dir)?;
        let name = format!("{}.drv", drv.name);
        self.with_daemon("adding a derivation", |_, client| {
            let info = client.add_text(&name, &text, &drv.references())?;
            Ok(info.path)
        })
    }

    /// The derivation at `path` from the daemon's store, for paths brix
    /// did not evaluate itself (`Store::readDerivation`).
    pub fn store_read_derivation(&self, path: &StorePath) -> Result<Derivation> {
        self.read_store_derivation(path, true)
    }

    /// `readDerivationCommon`: a path the store does not have is Nix's
    /// `InvalidPath`, worded as `requireStoreObjectAccessor` words it for
    /// `readDerivation` (`require_valid`) or `readInvalidDerivation`.
    ///
    /// A store on this machine (Nix's `LocalFSStore`, which the daemon's
    /// is) reads an invalid derivation from the store directory whenever
    /// the file is there (`getFSAccessor` without `requireValidPath`):
    /// one a build left behind, or one written before it was registered.
    /// Other stores have nothing but valid paths to give.
    pub(crate) fn read_store_derivation(
        &self,
        path: &StorePath,
        require_valid: bool,
    ) -> Result<Derivation> {
        let store_dir = self.config.store_dir().clone();
        if !require_valid && self.store_is_local() {
            use brix_store::SourceAccessor as _;
            let shown = store_dir.display(path);
            let file = brix_store::FsAccessor::new(&shown);
            let root = brix_store::CanonPath::root();
            match file.maybe_lstat(&root)? {
                None => {
                    return Err(Error::Store(
                        format!("store path '{shown}' does not exist").into(),
                    ))
                }
                Some(stat) if stat.file_type != brix_store::FileType::Regular => {
                    return Err(Error::Store(
                        format!("'{shown}' is not a regular file").into(),
                    ))
                }
                Some(_) => {}
            }
            let text = file.read_file(&root)?;
            let name = path.derivation_name()?.to_owned();
            return Derivation::parse(&store_dir, &text, &name, self.xp_settings())
                .map_err(Error::from);
        }
        self.with_daemon("reading a derivation", |_, client| {
            use brix_store::SourceAccessor as _;
            if !client.is_valid_path(path)? {
                let shown = store_dir.display(path);
                return Err(Error::Store(
                    if require_valid {
                        format!("path '{shown}' is not a valid store path")
                    } else {
                        format!("store path '{shown}' does not exist")
                    }
                    .into(),
                ));
            }
            let mem = client.read_path_into_memory(path)?;
            let root = brix_store::CanonPath::root();
            let read_error =
                |e| Error::Store(format!("reading '{}': {e}", store_dir.display(path)).into());
            // The NAR accessor's `readFile`.
            match mem.maybe_lstat(&root).map_err(read_error)? {
                Some(stat) if stat.file_type == brix_store::FileType::Regular => {}
                _ => {
                    return Err(Error::Store(
                        format!("'{}' is not a regular file", store_dir.display(path)).into(),
                    ))
                }
            }
            let text = mem.read_file(&root).map_err(read_error)?;
            let name = path.derivation_name()?.to_owned();
            Derivation::parse(&store_dir, &text, &name, self.xp_settings()).map_err(Error::from)
        })
    }

    /// Adds `contents` to the daemon's store as a text file named `name`
    /// (`addToStoreFromDump` with the `text` method and SHA-256), returning
    /// its path.
    pub fn store_add_text(
        &self,
        name: &str,
        contents: &[u8],
        references: &BTreeSet<StorePath>,
    ) -> Result<StorePath> {
        self.with_daemon("adding a file", |_, client| {
            Ok(client.add_text(name, contents, references)?.path)
        })
    }

    /// Makes the inputs of `drv` valid in the daemon's store, uploading
    /// the derivations and sources evaluation produced, so that a
    /// derivation brix made from another can be written there.
    pub fn store_upload_inputs(&self, drv: &Derivation) -> Result<()> {
        self.with_daemon("adding a derivation", |ctx, client| {
            daemon_build::upload_closures(
                client,
                self.config.store_dir(),
                ctx.ev.services(),
                drv.references(),
            )
        })
    }

    /// `Derivation::fillInOutputPaths` for a derivation made from another
    /// one: the hashes of its input derivations come from evaluation, or
    /// from the daemon's copies of derivations brix did not evaluate.
    pub fn fill_in_output_paths(&self, drv: &mut Derivation) -> Result<()> {
        use brix_store::{hash_derivation_modulo, DerivationError, DrvHashModulo, DrvHashes};
        let store_dir = self.config.store_dir().clone();
        let xp = self.xp_settings();
        // Gather every input derivation whose hash is not known yet.
        let mut hashes = DrvHashes::new();
        let mut drvs: BTreeMap<StorePath, Derivation> = BTreeMap::new();
        let mut todo: Vec<StorePath> = drv.input_drvs.keys().cloned().collect();
        while let Some(path) = todo.pop() {
            if hashes.contains_key(&path) || drvs.contains_key(&path) {
                continue;
            }
            let known = self.with(|ctx| ctx.ev.services().virtual_store().drv_hash(&path).cloned());
            if let Some(hash) = known {
                hashes.insert(path, hash);
                continue;
            }
            let input = self.read_invalid_derivation(&path)?;
            todo.extend(input.input_drvs.keys().cloned());
            drvs.insert(path, input);
        }
        fn hash_of(
            store_dir: &brix_store::StoreDir,
            xp: brix_store::XpSettings,
            drvs: &BTreeMap<StorePath, Derivation>,
            hashes: &mut DrvHashes,
            path: &StorePath,
        ) -> std::result::Result<DrvHashModulo, DerivationError> {
            if let Some(hash) = hashes.get(path) {
                return Ok(hash.clone());
            }
            let drv = drvs
                .get(path)
                .ok_or_else(|| DerivationError::UnknownInputDerivation(store_dir.display(path)))?;
            let hash = hash_derivation_modulo(store_dir, drv, false, xp, &mut |p| {
                hash_of(store_dir, xp, drvs, hashes, p)
            })?;
            hashes.insert(path.clone(), hash.clone());
            Ok(hash)
        }
        drv.fill_in_output_paths(&store_dir, xp, &mut |p| {
            hash_of(&store_dir, xp, &drvs, &mut hashes, p)
        })
        .map_err(Error::from)
    }

    /// `Derivation::parseJsonAndValidate` (`nix derivation add`): the
    /// derivation in `json` (`nix derivation show`'s format),
    /// its output paths filled in (`fillInOutputPaths`) and its invariants
    /// checked, as Nix does before writing it. The hashes of its input
    /// derivations come from evaluation or from the daemon's store.
    pub fn derivation_from_json(&self, json: &serde_json::Value) -> Result<Derivation> {
        let mut drv = self
            .with(|ctx| crate::derivation_json::derivation_from_json(ctx.ev.services(), json))
            .map_err(|e| self.eval_error(e))?;
        self.fill_in_output_paths(&mut drv)?;
        // `checkInvariants`: the same computation without filling in.
        // After a fill-in it can still fail: an output path filled into
        // `env` changes the derivation's hash.
        if let Err(e) = self.fill_in_output_paths(&mut drv.clone()) {
            let traced = brix_eval::EvalError::new(brix_eval::ErrorKind::Eval, e.to_string());
            traced.add_trace(
                None,
                format!(
                    "while checking derivation from JSON with name '{}'",
                    drv.name
                ),
            );
            return Err(self.eval_error(traced));
        }
        Ok(drv)
    }

    /// The contents of the store path `path` when it is a regular file,
    /// `None` when it is a directory or a symlink; read through the daemon.
    pub fn store_read_regular_file(&self, path: &StorePath) -> Result<Option<Vec<u8>>> {
        let store_dir = self.config.store_dir().clone();
        self.with_daemon("reading a store path", |_, client| {
            use brix_store::SourceAccessor as _;
            let mem = client.read_path_into_memory(path)?;
            let root = brix_store::CanonPath::root();
            let read_error = |e: brix_store::AccessorError| {
                Error::Store(format!("reading '{}': {e}", store_dir.display(path)).into())
            };
            match mem.maybe_lstat(&root).map_err(read_error)? {
                Some(stat) if stat.file_type == brix_store::FileType::Regular => {
                    Ok(Some(mem.read_file(&root).map_err(read_error)?))
                }
                _ => Ok(None),
            }
        })
    }

    /// Whether the store's paths are readable on this machine at their
    /// logical location (a unix-socket daemon sharing our file system).
    pub fn store_is_local(&self) -> bool {
        match &self.config.daemon {
            Some(uri) => {
                (uri == "daemon" || uri.starts_with("unix://") || uri.starts_with("local"))
                    && std::path::Path::new(self.config.store_dir().as_str()).is_dir()
            }
            None => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;

    fn options(settings: &[(&str, &str)]) -> Result<brix_daemon::ClientSettings> {
        let config = Config {
            daemon: Some("unix:///nonexistent/brix-store-test".into()),
            daemon_settings: settings
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
            ..Config::default()
        };
        match DaemonState::new(&config, config.store_dir())? {
            DaemonState::Lazy {
                options: Some(options),
                ..
            } => Ok(options.settings),
            _ => panic!("a configured store starts unconnected"),
        }
    }

    /// The settings sent in `setOptions`' fixed fields are parsed as Nix
    /// parses them, errors included.
    #[test]
    fn daemon_settings_parse_like_nix() {
        let s = options(&[
            ("cores", "2K"),
            ("max-silent-time", "1m"),
            ("max-jobs", "3"),
            ("keep-going", "yes"),
            ("builders", ""),
        ])
        .unwrap();
        assert_eq!(s.build_cores, 2048);
        assert_eq!(s.max_silent_time, 1 << 20);
        assert_eq!(s.max_build_jobs, 3);
        assert!(s.keep_going);
        assert_eq!(s.overrides.get("builders").map(String::as_str), Some(""));
        for (name, value, message) in [
            ("cores", "x", "setting 'cores' has invalid value 'x'"),
            (
                "cores",
                "4294967296",
                "setting 'cores' has invalid value '4294967296'",
            ),
            (
                "max-jobs",
                "2K",
                "configuration setting 'max-jobs' should be 'auto' or an integer",
            ),
            (
                "fallback",
                "maybe",
                "Boolean setting 'fallback' has invalid value 'maybe'",
            ),
        ] {
            let err = options(&[(name, value)]).unwrap_err();
            assert_eq!(err.to_string(), message);
            assert!(matches!(err, Error::Usage(_)), "{err:?}");
        }
    }

    /// `try_client`: no store and a closed one are `None`; a store that
    /// cannot be reached is its connection error, then, as in Nix's
    /// `openConnectionWrapper`, the error that it failed before.
    #[test]
    fn try_client_reports_an_unreachable_store() {
        let config = Config {
            daemon: Some("unix:///nonexistent/brix-try-client?priority=1".into()),
            ..Config::default()
        };
        let mut state = DaemonState::new(&config, config.store_dir()).unwrap();
        let mut err = || {
            state
                .try_client()
                .map(|c| c.is_some())
                .unwrap_err()
                .to_string()
        };
        assert_eq!(
            err(),
            "cannot connect to socket at '/nonexistent/brix-try-client': No such file or directory"
        );
        for _ in 0..2 {
            assert_eq!(
                err(),
                "opening a connection to remote store 'unix:///nonexistent/brix-try-client' previously failed"
            );
        }
        state.close();
        assert!(state.try_client().unwrap().is_none());
        assert!(DaemonState::None.try_client().unwrap().is_none());
    }

    /// The messages a [`BuildLogger`] in `mode` emits for a build of
    /// `hello-2.12.2` with a post-build hook.
    fn build_log(mode: BuildLogMode) -> Vec<BString> {
        use brix_daemon::logger::{Activity, ActivityKind, ActivityType, Field, ResultType};
        use brix_daemon::Logger as _;
        let out = Arc::new(Mutex::new(Vec::new()));
        let sink = out.clone();
        let mut logger =
            BuildLogger::with_output(mode, move |m| sink.lock().unwrap().push(BString::from(m)));
        let drv = "/nix/store/5v2nn7qarcn9l81hvbphg27gvqy4zjma-hello-2.12.2.drv";
        let activity = |id, kind, text: &str, level| Activity {
            id,
            level,
            kind: ActivityKind::Typed {
                kind,
                fields: vec![Field::Str(drv.into()), Field::Str(BString::default())],
            },
            text: text.into(),
            parent: 0,
        };
        let info = brix_daemon::Verbosity::Info;
        logger.start_activity(&activity(1, ActivityType::BuildWaiting, "waiting", info));
        logger.start_activity(&activity(
            2,
            ActivityType::Build,
            &format!("building '{drv}'"),
            info,
        ));
        logger.result(
            2,
            ResultType::BuildLogLine,
            &[Field::Str("compiling\r\n".into())],
        );
        logger.log(b"warning: something");
        logger.start_activity(&activity(3, ActivityType::PostBuildHook, "", info));
        logger.result(
            3,
            ResultType::PostBuildLogLine,
            &[Field::Str("uploading".into())],
        );
        logger.result(9, ResultType::BuildLogLine, &[Field::Str("whose?".into())]);
        logger.stop_activity(2);
        logger.result(2, ResultType::BuildLogLine, &[Field::Str("late".into())]);
        let out = out.lock().unwrap().clone();
        out
    }

    /// `nix -L`: lines prefixed with the derivation's `DrvName` (faint),
    /// ` (post)> ` for a post-build hook; activities shown with `...`.
    #[test]
    fn build_logs_render_like_nix() {
        let drv = "/nix/store/5v2nn7qarcn9l81hvbphg27gvqy4zjma-hello-2.12.2.drv";
        assert_eq!(
            build_log(BuildLogMode::Logs),
            [
                format!("building '{drv}'..."),
                "\u{1b}[2mhello> \u{1b}[0mcompiling".to_owned(),
                "warning: something".to_owned(),
                "\u{1b}[2mhello (post)> \u{1b}[0muploading".to_owned(),
                "\u{1b}[2munnamed> \u{1b}[0mwhose?".to_owned(),
                "\u{1b}[2munnamed> \u{1b}[0mlate".to_owned(),
            ]
        );
        assert_eq!(
            build_log(BuildLogMode::Normal),
            [
                format!("building '{drv}'..."),
                "warning: something".to_owned()
            ]
        );
        assert_eq!(build_log(BuildLogMode::Quiet), ["warning: something"]);
    }

    /// What the daemon sends is written as it is, bytes that are not
    /// UTF-8 included: `nix build -L` of a builder that prints `first
    /// \377 line` shows `<name>> first \377 line`, the name taken from
    /// the derivation's bytes too.
    #[test]
    fn build_logs_keep_their_bytes() {
        use brix_daemon::logger::{Activity, ActivityKind, ActivityType, Field, ResultType};
        use brix_daemon::Logger as _;
        let out = Arc::new(Mutex::new(Vec::new()));
        let sink = out.clone();
        let mut logger = BuildLogger::with_output(BuildLogMode::Logs, move |m| {
            sink.lock().unwrap().push(BString::from(m))
        });
        logger.start_activity(&Activity {
            id: 1,
            level: brix_daemon::Verbosity::Info,
            kind: ActivityKind::Typed {
                kind: ActivityType::Build,
                fields: vec![Field::Str(b"/nix/store/x-caf\xe9-1.0.drv".into())],
            },
            text: b"building '\xff'".into(),
            parent: 0,
        });
        logger.result(
            1,
            ResultType::BuildLogLine,
            &[Field::Str(b"first \xff line \r\n".into())],
        );
        logger.log(b"warning: \xe9");
        assert_eq!(
            *out.lock().unwrap(),
            [
                BString::from(b"building '\xff'..."),
                BString::from(b"\x1b[2mcaf\xe9> \x1b[0mfirst \xff line"),
                BString::from(b"warning: \xe9"),
            ]
        );
    }
}
