//! `hg` inputs (`src/libfetchers/mercurial.cc`). Nix runs the `hg`
//! program for everything, and so does brix: the same commands with the
//! same arguments, `hg` looked up on `PATH`, `HGPLAIN` set (so that the
//! user's configuration does not change what `hg` prints), stdout read,
//! stdin and stderr left to `hg`. A failure is Nix's "hg failed with exit
//! code N" after whatever `hg` itself printed.
//!
//! How a revision is fetched (`MercurialInputScheme::fetchToStore`):
//!
//! - The input always ends up with a `ref`: the one it names, else
//!   `default` (whatever the working tree has checked out).
//! - Without a `rev`, the revision the `ref` named when it was last
//!   fetched is reused for `tarball-ttl` ([`hg_ref_key`]).
//! - A revision fetched before is taken from the fetch cache with its
//!   `revCount` ([`hg_rev_key`]), as long as its tree is still there.
//! - Otherwise the repository is cloned (`hg clone --noupdate`) into
//!   `<cache dir>/hg/<nix32 SHA-256 of the URL>`, or pulled into that
//!   clone (`hg pull`, after `hg recover` if an interrupted pull left a
//!   journal) unless the clone has the revision already. A local
//!   repository is cloned too, from its path.
//! - `hg log -r <rev or ref>` gives the revision, its number (which Nix
//!   reports as `revCount`) and its branch, which becomes the `ref`.
//! - `hg archive` exports the revision, without the `.hg_archival.txt`
//!   it adds.
//!
//! So, as in Nix, what a fetch reports depends on the cache: the first
//! fetch of a revision has the branch it is on as its `ref`, a later one
//! answered from the cache the `ref` it was asked for (`default` when it
//! named none).
//!
//! A local repository (`file:` URL) without `ref` or `rev` whose working
//! tree has uncommitted changes (`hg status --modified --added
//! --removed` lists something; a file merely missing from disk does not
//! count) is not cloned: its tracked files are read as they are on disk
//! (`hg status --clean --modified --added`), and the input gets the
//! working tree's branch as its `ref` but no revision. It is an error
//! under `allow-dirty = false` and warned about under `warn-dirty`.

use std::collections::BTreeSet;
use std::fmt;
use std::ops::Bound;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::time::Duration;

use brix_store::url::{parse_url, url_path_to_path};
use brix_store::{
    CanonPath, FileType, FsAccessor, Hash, HashAlgo, HashFormat, MemoryAccessor, SourceAccessor,
    Stat,
};

use super::cache::{hg_ref_key, hg_rev_key};
use super::NativeFetcher;
use crate::attrs::{get_int_attr, get_str_attr, Attr, Attrs};
use crate::fetch::{compute_store_path, tree_nar_hash, FetchedTree};
use crate::hashes::{GitRev, NarHash};
use crate::input::{FetchSettings, Input};
use crate::progress::{Activity, ActivityKind};
use crate::{err, Error, Result};

/// Where a repository is (`getActualUrl`).
enum Location {
    /// A `file:` URL: the repository's directory, as the URL spells it.
    Local(String),
    /// Anything else: the URL `hg` is given.
    Remote(String),
}

impl Location {
    fn of(input: &Input) -> Result<Location> {
        // The `url` attribute was parsed (and warned about) when the input
        // was made.
        let url = parse_url(get_str_attr(&input.attrs, "url")?, false, &|_| {})?;
        if url.scheme == "file" {
            Ok(Location::Local(url_path_to_path(&url.path)?))
        } else {
            Ok(Location::Remote(url.to_string()))
        }
    }

    /// What `hg` clones from, and what the clone and the cache entries
    /// are keyed by: a local repository's path, else the URL.
    fn url(&self) -> &str {
        match self {
            Location::Local(path) | Location::Remote(path) => path,
        }
    }
}

/// `MercurialInputScheme::getSourcePath`: the working tree of a `file:`
/// input without `ref` or `rev`.
pub fn source_path(input: &Input) -> Result<Option<PathBuf>> {
    if input.ref_()?.is_some() || input.rev_hash()?.is_some() {
        return Ok(None);
    }
    match Location::of(input)? {
        Location::Local(path) => Ok(Some(PathBuf::from(path))),
        Location::Remote(_) => Ok(None),
    }
}

/// `MercurialInputScheme::putFile` once the file is written: `hg add` of
/// the file at `path` (absolute) in its working tree, so that the next
/// fetch of the tree has it. Like Nix's, it runs whether or not the file
/// is tracked already (`hg` then says so, and succeeds). Nix commits only
/// for `--commit-lock-file`, which brix does not support.
pub fn add_file(path: &Path) -> Result<()> {
    run_hg(hg().arg("add").arg(path)).map(drop)
}

/// `MercurialInputScheme::getAccessor` (its `fetchToStore`): see the
/// module documentation.
pub fn fetch(
    fetcher: &NativeFetcher,
    settings: &FetchSettings,
    input: &Input,
) -> Result<FetchedTree> {
    let cache_dir = fetcher
        .cache_dir
        .as_deref()
        .ok_or_else(|| err!("Mercurial inputs are disabled (no cache directory)"))?;
    // `revInfoKey`, which a `rev` reaches before `hg` runs (Nix's input
    // takes a hash of any algorithm; brix's Git revisions do not).
    if let Some(rev) = input.rev_hash()? {
        if rev.algo() != HashAlgo::Sha1 {
            return Err(err!(
                "Hash '{}' is not supported by Mercurial. Only sha1 is supported.",
                rev.to_string(HashFormat::Base16, false)
            ));
        }
    }
    let mut input = input.clone();
    let orig_rev = input.rev()?;
    let name = input.name()?;
    let location = Location::of(&input)?;

    if let Location::Local(path) = &location {
        // Neither `hg` nor the file system is asked about a locked input.
        let unlocked = input.ref_()?.is_none() && orig_rev.is_none();
        if unlocked && path_exists(&Path::new(path).join(".hg")) && is_dirty(path)? {
            return fetch_dirty(settings, input, path, &*fetcher.warn);
        }
    }

    let url = location.url();
    if input.ref_()?.is_none() {
        input
            .attrs
            .insert("ref".into(), Attr::String("default".into()));
    }
    let ref_ = input
        .ref_()?
        .expect("invariant: the ref was set above")
        .to_owned();
    let ref_key = hg_ref_key(url, &ref_);

    // The revision the ref named when it was last fetched, unless that
    // is longer ago than `tarball-ttl`.
    if let (None, Some(cache)) = (input.rev()?, &fetcher.cache) {
        let ttl = Duration::from_secs(settings.tarball_ttl);
        if let Some(found) = cache.lookup(&ref_key, Some(ttl))? {
            let rev = GitRev::parse_sha1(get_str_attr(&found, "rev")?, settings.xp())?;
            input.attrs.insert("rev".into(), Attr::String(rev.to_hex()));
        }
    }
    if let Some(rev) = input.rev()? {
        if let Some(known) = cached_revision(fetcher, settings, &input, &name, &rev)? {
            return Ok(known.into_tree(input));
        }
    }

    let clone = cache_dir
        .join("hg")
        .join(Hash::of(HashAlgo::Sha256, url.as_bytes()).to_string(HashFormat::Nix32, false));
    let _clone_lock = lock_clone(&clone)?;
    // A revision the clone has needs no pull. (Nix ignores how this `hg
    // log` ends: one that fails has not printed `1`.)
    let have_rev = match input.rev()? {
        Some(rev) if path_exists(&clone) => {
            let (_, out) = run_program(hg().arg("log").arg("-R").arg(&clone).args([
                "-r",
                rev.to_hex().as_str(),
                "--template",
                "1",
            ]))?;
            out == "1"
        }
        _ => false,
    };
    if !have_rev {
        let _activity = Activity::new(
            &*fetcher.progress,
            ActivityKind::Other,
            &format!("fetching Mercurial repository '{url}'"),
        );
        if path_exists(&clone) {
            pull(&clone, url)?;
        } else {
            run_hg(
                hg().args(["clone", "--noupdate", "--"])
                    .arg(url)
                    .arg(&clone),
            )?;
        }
    }

    let spec = match input.rev()? {
        Some(rev) => rev.to_hex(),
        None => ref_,
    };
    let log = run_hg(hg().arg("log").arg("-R").arg(&clone).args([
        "-r",
        spec.as_str(),
        "--template",
        "{node} {rev} {branch}",
    ]))?;
    // `tokenizeString`, which Nix asserts gives three words.
    let words: Vec<&str> = log
        .split([' ', '\t', '\n', '\r'])
        .filter(|w| !w.is_empty())
        .collect();
    let [node, number, branch] = words[..] else {
        return Err(err!(
            "'hg log -r {}' printed '{}', which is not a revision, its number and its branch",
            spec,
            log
        ));
    };
    let rev = GitRev::parse_sha1(node, settings.xp())?;
    input.attrs.insert("rev".into(), Attr::String(rev.to_hex()));
    let rev_count = parse_rev_number(number)?;
    input
        .attrs
        .insert("ref".into(), Attr::String(branch.to_owned()));

    if let Some(known) = cached_revision(fetcher, settings, &input, &name, &rev)? {
        return Ok(known.into_tree(input));
    }

    let mut tree = archive(&clone, &rev)?;
    let nar_hash = tree_nar_hash(&tree)?;
    if let Some(cache) = &fetcher.cache {
        if orig_rev.is_none() {
            let mut value = Attrs::new();
            value.insert("rev".into(), Attr::String(rev.to_hex()));
            cache.insert(&ref_key, &value)?;
        }
        let mut info = Attrs::new();
        info.insert("revCount".into(), Attr::Int(rev_count));
        info.insert("narHash".into(), Attr::String(nar_hash.to_sri()));
        cache.insert(&hg_rev_key(&name, &rev), &info)?;
    }
    input.attrs.insert("revCount".into(), Attr::Int(rev_count));
    input
        .attrs
        .insert("narHash".into(), Attr::String(nar_hash.to_sri()));
    tree.set_display_prefix(format!("«{}»", input.to_url_text(true)?));
    Ok(FetchedTree {
        accessor: Arc::new(tree),
        input,
        verified: true,
    })
}

/// `std::stoull` of the revision number `hg log` printed, which wraps a
/// negative one (the null revision's `-1`) around.
fn parse_rev_number(number: &str) -> Result<u64> {
    number
        .parse::<u64>()
        .or_else(|_| number.parse::<i64>().map(|n| n as u64))
        .map_err(|_| err!("'hg log' printed '{}' as a revision number", number))
}

/// What the fetch cache remembers about a revision ([`hg_rev_key`]).
struct Known {
    rev_count: u64,
    nar_hash: NarHash,
    root: PathBuf,
}

impl Known {
    /// `makeResult`: the input with the remembered `revCount`, and the
    /// tree. The `ref` is the one the fetch was asked for.
    fn into_tree(self, mut input: Input) -> FetchedTree {
        input
            .attrs
            .insert("revCount".into(), Attr::Int(self.rev_count));
        input
            .attrs
            .insert("narHash".into(), Attr::String(self.nar_hash.to_sri()));
        FetchedTree {
            accessor: Arc::new(FsAccessor::new(self.root)),
            input,
            verified: true,
        }
    }
}

/// `lookupStorePath(revInfoKey(rev))`: the remembered `revCount` of `rev`
/// with its tree, which must still be in the fetch cache or in the store
/// whose trees the cache leaves out (Nix's entry, likewise, counts only
/// while its store path is valid).
fn cached_revision(
    fetcher: &NativeFetcher,
    settings: &FetchSettings,
    input: &Input,
    name: &str,
    rev: &GitRev,
) -> Result<Option<Known>> {
    let Some(cache) = &fetcher.cache else {
        return Ok(None);
    };
    let Some(info) = cache.lookup(&hg_rev_key(name, rev), None)? else {
        return Ok(None);
    };
    let rev_count = get_int_attr(&info, "revCount")?;
    let nar_hash = NarHash::parse_sri(get_str_attr(&info, "narHash")?, settings.xp())?;
    let known = |root| {
        Some(Known {
            rev_count,
            nar_hash,
            root,
        })
    };
    if let Some(root) = cache.tree_dir(&nar_hash)? {
        return Ok(known(root));
    }
    if let Some(store) = &fetcher.store_with_trees {
        let mut probe = input.clone();
        probe
            .attrs
            .insert("narHash".into(), Attr::String(nar_hash.to_sri()));
        let at = PathBuf::from(store.display(&compute_store_path(store, &probe)?));
        if path_exists(&at) {
            return Ok(known(at));
        }
    }
    Ok(None)
}

/// `hg pull` into the clone. A pull that was interrupted leaves a journal
/// behind, and `hg` refuses to pull again ("abandoned transaction found")
/// until `hg recover` has rolled it back.
fn pull(clone: &Path, url: &str) -> Result<()> {
    let pull_command = || {
        let mut cmd = hg();
        cmd.arg("pull").arg("-R").arg(clone).arg("--").arg(url);
        cmd
    };
    let (status, _) = run_program(&mut pull_command())?;
    if status.ok() {
        return Ok(());
    }
    if path_exists(&clone.join(".hg").join("store").join("journal")) {
        run_hg(hg().arg("recover").arg("-R").arg(clone))?;
        run_hg(&mut pull_command())?;
        return Ok(());
    }
    Err(err!("'hg pull' {}", status))
}

/// `hg archive` of `rev` into a temporary directory, read into memory
/// without the `.hg_archival.txt` that `hg` adds there (Nix deletes it
/// before it adds the directory to the store).
fn archive(clone: &Path, rev: &GitRev) -> Result<MemoryAccessor> {
    let tmp = tempfile::Builder::new()
        .prefix("nix-")
        .tempdir()
        .map_err(|e| Error::Io {
            context: "creating a temporary directory".into(),
            source: e,
        })?;
    run_hg(
        hg().arg("archive")
            .arg("-R")
            .arg(clone)
            .args(["-r", rev.to_hex().as_str()])
            .arg(tmp.path()),
    )?;
    let meta = tmp.path().join(".hg_archival.txt");
    match std::fs::remove_file(&meta) {
        Ok(()) => {}
        // `ui.archivemeta = false` leaves it out.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(Error::Io {
                context: format!("deleting '{}'", meta.display()),
                source: e,
            })
        }
    }
    copy_into_memory(tmp.path(), &|_, _| true)
}

/// Whether the working tree at `path` has uncommitted changes.
fn is_dirty(path: &str) -> Result<bool> {
    let status = run_hg(hg().arg("status").arg("-R").arg(path).args([
        "--modified",
        "--added",
        "--removed",
    ]))?;
    Ok(!status.is_empty())
}

/// The dirty branch of `fetchToStore`: the files `hg` tracks in the
/// working tree at `path`, as they are on disk, and the working tree's
/// branch as the input's `ref`.
fn fetch_dirty(
    settings: &FetchSettings,
    mut input: Input,
    path: &str,
    warn: &dyn Fn(&str),
) -> Result<FetchedTree> {
    // `PathFmt` puts the path in double quotes.
    let shown = format!("\"{path}\"");
    if !settings.allow_dirty {
        return Err(err!("Mercurial tree '{}' is unclean", shown));
    }
    if settings.warn_dirty {
        warn(&format!("Mercurial tree '{shown}' is unclean"));
    }
    let branch = run_hg(hg().arg("branch").arg("-R").arg(path))?;
    // `chomp`.
    let branch = branch.trim_end_matches([' ', '\n', '\r', '\t']);
    input
        .attrs
        .insert("ref".into(), Attr::String(branch.to_owned()));

    let listing = run_hg(hg().arg("status").arg("-R").arg(path).args([
        "--clean",
        "--modified",
        "--added",
        "--no-status",
        "--print0",
    ]))?;
    // Relative to the repository's root.
    let files: BTreeSet<String> = listing
        .split('\0')
        .filter(|f| !f.is_empty())
        .map(str::to_owned)
        .collect();
    // Nix's `PathFilter`: a file if `hg` lists it, a directory if it
    // lists a file below it (so `.hg` and directories holding only
    // untracked files are left out).
    let tracked = |entry: &CanonPath, stat: &Stat| -> bool {
        let rel = entry.rel();
        if stat.file_type == FileType::Directory {
            let prefix = format!("{rel}/");
            files
                .range::<str, _>((Bound::Included(prefix.as_str()), Bound::Unbounded))
                .next()
                .is_some_and(|f| f.starts_with(&prefix))
        } else {
            files.contains(rel)
        }
    };
    // `absPath`.
    let root = match Path::new(path) {
        p if p.is_absolute() => p.to_path_buf(),
        p => std::env::current_dir()
            .map_err(|e| Error::Io {
                context: "getting the current directory".into(),
                source: e,
            })?
            .join(p),
    };
    let mut tree = copy_into_memory(&root, &tracked)?;
    tree.set_display_prefix(format!("«{}»", input.to_url_text(true)?));
    Ok(FetchedTree {
        accessor: Arc::new(tree),
        input,
        verified: false,
    })
}

/// The tree at `root` on disk, read into memory: what `addToStore`
/// copies, given Nix's `PathFilter` (`keep`, asked about every path below
/// the root before it is read).
fn copy_into_memory(
    root: &Path,
    keep: &dyn Fn(&CanonPath, &Stat) -> bool,
) -> Result<MemoryAccessor> {
    let disk = FsAccessor::new(root);
    let mut tree = MemoryAccessor::new();
    tree.add_dir(&CanonPath::root())?;
    copy_children(&disk, &CanonPath::root(), keep, &mut tree)?;
    Ok(tree)
}

fn copy_children(
    disk: &FsAccessor,
    dir: &CanonPath,
    keep: &dyn Fn(&CanonPath, &Stat) -> bool,
    tree: &mut MemoryAccessor,
) -> Result<()> {
    for name in disk.read_dir(dir)?.into_keys() {
        let path = dir.join_component(&name).map_err(|e| err!("{}", e))?;
        let stat = disk.lstat(&path)?;
        if !keep(&path, &stat) {
            continue;
        }
        match stat.file_type {
            FileType::Regular => tree.add_file(&path, disk.read_file(&path)?, stat.executable)?,
            FileType::Symlink => tree.add_symlink(&path, &disk.read_link(&path)?)?,
            FileType::Directory => {
                tree.add_dir(&path)?;
                copy_children(disk, &path, keep, tree)?;
            }
            _ => {
                return Err(err!(
                    "file '{}' has an unsupported type",
                    disk.show_path(&path)
                ))
            }
        }
    }
    Ok(())
}

/// `pathExists`: whether anything is at `path`, a symlink not followed.
fn path_exists(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}

/// Holds the clone at `dir` for one fetch: brix processes sharing the
/// cache directory take turns on `<dir>.lock` (Nix takes a lock per
/// input instead).
fn lock_clone(dir: &Path) -> Result<super::lock::FileLock> {
    let parent = dir
        .parent()
        .expect("invariant: a clone lives in <cache dir>/hg");
    std::fs::create_dir_all(parent).map_err(|e| Error::Io {
        context: format!("creating '{}'", parent.display()),
        source: e,
    })?;
    let mut name = dir.as_os_str().to_owned();
    name.push(".lock");
    super::lock::FileLock::acquire(Path::new(&name))
}

/// `hgOptions`: `hg` from `PATH`, with `HGPLAIN` set.
fn hg() -> Command {
    let mut cmd = Command::new("hg");
    cmd.env("HGPLAIN", "");
    cmd
}

/// `runHg`: [`run_program`], failing when `hg` does.
fn run_hg(cmd: &mut Command) -> Result<String> {
    let (status, out) = run_program(cmd)?;
    if !status.ok() {
        return Err(err!("hg {}", status));
    }
    Ok(out)
}

/// `runProgram`: runs `hg` to completion, its stdout captured, its stdin
/// and stderr the caller's; how it ended, and what it printed.
///
/// Nix starts a program by forking and then `exec`ing it, so a program
/// that cannot be started (no `hg` on `PATH`) is a child that prints why
/// and exits with 1. The message bypasses Nix's logger, colours and all,
/// and so does brix's. Both it and `hg`'s stderr go where
/// [`super::stderr`] sends them.
fn run_program(cmd: &mut Command) -> Result<(Status, String)> {
    cmd.stdin(Stdio::inherit())
        .stdout(Stdio::piped())
        .stderr(super::stderr::for_program());
    let child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => {
            super::stderr::write(
                format!(
                    "\x1b[31;1merror:\x1b[0m executing \x1b[35;1m\"{}\"\x1b[0m: \x1b[35;1m{}\x1b[0m\n",
                    cmd.get_program().to_string_lossy(),
                    super::path::sys_error_text(&e)
                )
                .as_bytes(),
            );
            return Ok((Status::Exited(1), String::new()));
        }
    };
    let out = child.wait_with_output().map_err(|e| Error::Io {
        context: "waiting for 'hg'".into(),
        source: e,
    })?;
    super::stderr::pass_on(&out.stderr);
    let stdout = String::from_utf8(out.stdout).map_err(|_| {
        err!("'hg' printed output that is not valid UTF-8, which brix does not support")
    })?;
    Ok((Status::of(out.status), stdout))
}

/// How a program ended (the `status` Nix's `runProgram` returns).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Status {
    Exited(i32),
    Signalled(i32),
    Abnormal,
}

impl Status {
    fn of(status: ExitStatus) -> Status {
        if let Some(code) = status.code() {
            return Status::Exited(code);
        }
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;
            if let Some(signal) = status.signal() {
                return Status::Signalled(signal);
            }
        }
        Status::Abnormal
    }

    /// `statusOk`.
    fn ok(self) -> bool {
        self == Status::Exited(0)
    }
}

impl fmt::Display for Status {
    /// `statusToString`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Status::Exited(0) => f.write_str("succeeded"),
            Status::Exited(code) => write!(f, "failed with exit code {code}"),
            Status::Signalled(signal) => write!(
                f,
                "failed due to signal {signal} ({})",
                signal_description(signal)
            ),
            Status::Abnormal => f.write_str("died abnormally"),
        }
    }
}

/// glibc's `strsignal` for Linux's signal numbers.
fn signal_description(signal: i32) -> String {
    const DESCRIPTIONS: [&str; 31] = [
        "Hangup",
        "Interrupt",
        "Quit",
        "Illegal instruction",
        "Trace/breakpoint trap",
        "Aborted",
        "Bus error",
        "Floating point exception",
        "Killed",
        "User defined signal 1",
        "Segmentation fault",
        "User defined signal 2",
        "Broken pipe",
        "Alarm clock",
        "Terminated",
        "Stack fault",
        "Child exited",
        "Continued",
        "Stopped (signal)",
        "Stopped",
        "Stopped (tty input)",
        "Stopped (tty output)",
        "Urgent I/O condition",
        "CPU time limit exceeded",
        "File size limit exceeded",
        "Virtual timer expired",
        "Profiling timer expired",
        "Window changed",
        "I/O possible",
        "Power failure",
        "Bad system call",
    ];
    match signal {
        1..=31 => DESCRIPTIONS[signal as usize - 1].to_owned(),
        // `SIGRTMIN` is 34 under glibc, which keeps 32 and 33 for itself.
        34..=64 => format!("Real-time signal {}", signal - 34),
        _ => format!("Unknown signal {signal}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fetch::fetch_input;
    use brix_testutil::HgRepo;
    use std::sync::Mutex;

    /// A fetcher with its caches next to `repo`, and the warnings it gave.
    fn fetcher(repo: &HgRepo, cache: &str) -> (NativeFetcher, Arc<Mutex<Vec<String>>>) {
        let warnings = Arc::new(Mutex::new(Vec::new()));
        let mut fetcher = NativeFetcher::new()
            .unwrap()
            .with_cache_dir(repo.scratch().join(cache));
        let sink = warnings.clone();
        fetcher.warn = Box::new(move |msg| sink.lock().unwrap().push(msg.to_owned()));
        (fetcher, warnings)
    }

    fn input(url: &str) -> Input {
        Input::from_url_str(&FetchSettings::default(), url, true).unwrap()
    }

    fn file_url(repo: &HgRepo) -> String {
        format!("hg+file://{}", repo.path().display())
    }

    fn read(tree: &FetchedTree, path: &str) -> Vec<u8> {
        tree.accessor
            .read_file(&CanonPath::new(path).unwrap())
            .unwrap()
    }

    fn exists(tree: &FetchedTree, path: &str) -> bool {
        tree.accessor
            .path_exists(&CanonPath::new(path).unwrap())
            .unwrap()
    }

    /// Two commits on `default`, then one on `stable`; the three nodes.
    fn three_commits(repo: &HgRepo) -> [String; 3] {
        repo.write("hello", "hello\n");
        repo.write("sub/x", "x\n");
        let first = repo.commit();
        repo.write("hello", "world\n");
        let second = repo.commit();
        repo.branch("stable");
        repo.write("s", "s\n");
        let third = repo.commit();
        [first, second, third]
    }

    #[test]
    fn a_revision_is_archived_with_its_branch_and_number() {
        let repo = HgRepo::new();
        let [_, second, third] = three_commits(&repo);
        let settings = FetchSettings::default();
        let (fetcher, _) = fetcher(&repo, "cache");
        let by_rev = input(&format!("{}?rev={third}", file_url(&repo)));

        let first = fetch_input(&fetcher, &settings, &by_rev).unwrap();
        assert_eq!(first.input.ref_().unwrap(), Some("stable"));
        assert_eq!(first.input.rev_count().unwrap(), Some(2));
        assert_eq!(read(&first, "/s"), b"s\n");
        assert!(!exists(&first, "/.hg_archival.txt"));
        assert!(!exists(&first, "/.hg"));

        // Answered from the cache: the `ref` asked for, `default`.
        let again = fetch_input(&fetcher, &settings, &by_rev).unwrap();
        assert_eq!(again.input.ref_().unwrap(), Some("default"));
        assert_eq!(again.input.rev_count().unwrap(), Some(2));
        assert_eq!(again.nar_hash().unwrap(), first.nar_hash().unwrap());

        let older = input(&format!("{}?rev={second}", file_url(&repo)));
        let older = fetch_input(&fetcher, &settings, &older).unwrap();
        assert_eq!(read(&older, "/hello"), b"world\n");
        assert!(!exists(&older, "/s"));
        assert_eq!(older.input.ref_().unwrap(), Some("default"));
        assert_eq!(older.input.rev_count().unwrap(), Some(1));
    }

    #[test]
    fn a_ref_is_resolved_and_remembered_for_tarball_ttl() {
        let repo = HgRepo::new();
        let [_, _, third] = three_commits(&repo);
        let mut settings = FetchSettings::default();
        let (fetcher, _) = fetcher(&repo, "cache");
        let stable = input(&format!("{}?ref=stable", file_url(&repo)));
        let fetched = fetch_input(&fetcher, &settings, &stable).unwrap();
        assert_eq!(fetched.input.rev().unwrap().unwrap().to_hex(), third);
        assert_eq!(fetched.input.ref_().unwrap(), Some("stable"));

        repo.write("s", "t\n");
        let fourth = repo.commit();
        let cached = fetch_input(&fetcher, &settings, &stable).unwrap();
        assert_eq!(cached.input.rev().unwrap().unwrap().to_hex(), third);

        settings.tarball_ttl = 0;
        let refreshed = fetch_input(&fetcher, &settings, &stable).unwrap();
        assert_eq!(refreshed.input.rev().unwrap().unwrap().to_hex(), fourth);
        assert_eq!(read(&refreshed, "/s"), b"t\n");
        assert_eq!(refreshed.input.rev_count().unwrap(), Some(3));
    }

    /// A clean working tree is cloned, and its `ref` is `default` whatever
    /// is checked out.
    #[test]
    fn a_clean_working_tree_is_fetched_at_default() {
        let repo = HgRepo::new();
        let [_, second, _] = three_commits(&repo);
        let settings = FetchSettings::default();
        let (fetcher, warnings) = fetcher(&repo, "cache");
        let fetched = fetch_input(&fetcher, &settings, &input(&file_url(&repo))).unwrap();
        assert_eq!(fetched.input.rev().unwrap().unwrap().to_hex(), second);
        assert_eq!(fetched.input.ref_().unwrap(), Some("default"));
        assert!(!exists(&fetched, "/s"));
        assert!(warnings.lock().unwrap().is_empty());
        assert_eq!(
            source_path(&input(&file_url(&repo))).unwrap(),
            Some(repo.path().to_path_buf())
        );
    }

    #[test]
    fn a_dirty_working_tree_is_read_with_its_tracked_files() {
        let repo = HgRepo::new();
        repo.write("a", "a\n");
        repo.write("b", "b\n");
        repo.write("c", "c\n");
        repo.write("s/t/x", "x\n");
        repo.write("s/y", "y\n");
        repo.commit();
        repo.write("a", "changed\n");
        repo.hg(&["rm", "-q", "b"]);
        std::fs::remove_file(repo.path().join("c")).unwrap();
        std::fs::remove_file(repo.path().join("s/t/x")).unwrap();
        repo.write("n/n", "n\n");
        repo.hg(&["add", "-q", "n/n"]);
        repo.write("untracked", "u\n");
        repo.write("ud/u", "u\n");

        let mut settings = FetchSettings::default();
        let (fetcher, warnings) = fetcher(&repo, "cache");
        let url = file_url(&repo);
        let fetched = fetch_input(&fetcher, &settings, &input(&url)).unwrap();
        let shown = format!("\"{}\"", repo.path().display());
        assert_eq!(
            *warnings.lock().unwrap(),
            [format!("Mercurial tree '{shown}' is unclean")]
        );
        assert_eq!(fetched.input.ref_().unwrap(), Some("default"));
        assert_eq!(fetched.input.rev().unwrap(), None);
        assert_eq!(fetched.input.rev_count().unwrap(), None);
        assert_eq!(read(&fetched, "/a"), b"changed\n");
        assert_eq!(read(&fetched, "/n/n"), b"n\n");
        assert_eq!(read(&fetched, "/s/y"), b"y\n");
        for gone in ["/b", "/c", "/s/t", "/untracked", "/ud", "/.hg"] {
            assert!(!exists(&fetched, gone), "{gone}");
        }

        settings.allow_dirty = false;
        let err = fetch_input(&fetcher, &settings, &input(&url)).unwrap_err();
        assert_eq!(
            err.message(),
            format!("Mercurial tree '{shown}' is unclean")
        );
    }

    #[test]
    fn an_unknown_revision_fails_as_in_nix() {
        let repo = HgRepo::new();
        three_commits(&repo);
        let (fetcher, _) = fetcher(&repo, "cache");
        let missing = input(&format!("{}?ref=nonesuch", file_url(&repo)));
        let err = fetch_input(&fetcher, &FetchSettings::default(), &missing).unwrap_err();
        assert_eq!(err.message(), "hg failed with exit code 255");
        assert_eq!(
            err.traces(),
            [format!(
                "while fetching the input '{}?ref=nonesuch'",
                file_url(&repo)
            )]
        );
    }

    /// A SHA-256 `rev` is an input's like any other (it is locked, and
    /// shown in hexadecimal) until the fetcher refuses it, before `hg`
    /// runs.
    #[test]
    fn a_sha256_revision_is_refused_by_the_fetcher_as_in_nix() {
        let repo = HgRepo::new();
        let (fetcher, _) = fetcher(&repo, "cache");
        let settings = FetchSettings::default();
        let rev = "a".repeat(64);
        let asked = input(&format!("{}?rev=sha256:{rev}", file_url(&repo)));
        assert!(asked.is_locked(&settings).unwrap());
        let shown = format!("{}?rev={rev}", file_url(&repo));
        assert_eq!(asked.to_url_text(false).unwrap(), shown);
        assert_eq!(asked.fingerprint().unwrap(), Some(format!("hg:{rev}")));
        let err = fetch_input(&fetcher, &settings, &asked).unwrap_err();
        assert_eq!(
            err.message(),
            format!("Hash '{rev}' is not supported by Mercurial. Only sha1 is supported.")
        );
        assert_eq!(
            err.traces(),
            [format!("while fetching the input '{shown}'")]
        );
        assert!(!repo.scratch().join("cache").join("hg").exists());
    }

    #[test]
    fn source_paths() {
        assert_eq!(
            source_path(&input("hg+file:///some/repo")).unwrap(),
            Some(PathBuf::from("/some/repo"))
        );
        assert_eq!(
            source_path(&input("hg+file:///some/repo?ref=stable")).unwrap(),
            None
        );
        assert_eq!(
            source_path(&input("hg+https://example.org/r")).unwrap(),
            None
        );
    }

    #[test]
    fn statuses_read_as_in_nix() {
        assert_eq!(Status::Exited(0).to_string(), "succeeded");
        assert_eq!(Status::Exited(255).to_string(), "failed with exit code 255");
        assert_eq!(
            Status::Signalled(9).to_string(),
            "failed due to signal 9 (Killed)"
        );
        assert_eq!(
            Status::Signalled(35).to_string(),
            "failed due to signal 35 (Real-time signal 1)"
        );
        assert_eq!(parse_rev_number("-1").unwrap(), u64::MAX);
        assert_eq!(parse_rev_number("12").unwrap(), 12);
    }
}
