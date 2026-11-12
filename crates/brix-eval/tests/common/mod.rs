//! Shared helpers for the integration tests: the test host (a root file
//! system with trees mounted at absolute paths, trace and warning
//! capture), the oracle `nix-instantiate`, running both and comparing
//! what they printed.

#![allow(dead_code)]

pub mod cases;

use std::cell::RefCell;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::Arc;

use brix_eval::image::{Image, ImageCompileContext};
use brix_eval::{Arena, Config, DisplayPrefixAccessor, Evaluator, Host, SrcPos};
use brix_store::{CanonPath, FsAccessor, MemoryAccessor, MountedAccessor, SourceAccessor};
use brix_testutil::Oracle;
use bstr::{BString, ByteSlice};

/// A directory on disk shown at a logical mount point (`/pwd/…`), the
/// way the test suite's `sed s!$(pwd)!/pwd!` makes Nix's output look.
pub fn shown_at(at: impl AsRef<[u8]>, dir: &Path) -> Arc<dyn SourceAccessor> {
    shown_as(at, Arc::new(FsAccessor::new(dir)))
}

/// `inner` shown at a logical mount point, like [`shown_at`].
pub fn shown_as(at: impl AsRef<[u8]>, inner: Arc<dyn SourceAccessor>) -> Arc<dyn SourceAccessor> {
    Arc::new(DisplayPrefixAccessor::new(
        inner,
        BString::from(at.as_ref()),
    ))
}

/// A host whose root file system is empty except for its mounts, and
/// which records traces and warnings the way `nix` prints them on
/// stderr.
pub struct TestHost {
    root: Arc<dyn SourceAccessor>,
    allowed: Vec<CanonPath>,
    pub stderr: RefCell<BString>,
}

impl TestHost {
    /// Mounts each directory at its path (see [`shown_at`]) and allows
    /// reading all of them.
    pub fn new(mounts: &[(&str, &Path)]) -> TestHost {
        let allowed: Vec<&str> = mounts.iter().map(|(at, _)| *at).collect();
        TestHost::with_allowed(mounts, &allowed)
    }

    /// Mounts each directory at its path (see [`shown_at`]); only
    /// `allowed` may be read (the store directory always may), as in
    /// Nix's pure mode.
    pub fn with_allowed(mounts: &[(&str, &Path)], allowed: &[&str]) -> TestHost {
        let accessors: Vec<(&str, Arc<dyn SourceAccessor>)> = mounts
            .iter()
            .map(|(at, dir)| (*at, shown_at(at, dir)))
            .collect();
        TestHost::with_accessors(&accessors, allowed)
    }

    /// Mounts each accessor, as it is, at its path; only `allowed` may be
    /// read.
    pub fn with_accessors(
        mounts: &[(&str, Arc<dyn SourceAccessor>)],
        allowed: &[&str],
    ) -> TestHost {
        let mut mounted = MountedAccessor::new(Arc::new(MemoryAccessor::new()));
        for (at, accessor) in mounts {
            mounted.mount(
                CanonPath::new(at).expect("mount points are absolute"),
                accessor.clone(),
            );
        }
        TestHost {
            root: Arc::new(mounted),
            allowed: allowed
                .iter()
                .map(|s| CanonPath::new(s).expect("allowed paths are absolute"))
                .collect(),
            stderr: RefCell::new(BString::default()),
        }
    }

    pub fn take_stderr(&self) -> BString {
        std::mem::take(&mut *self.stderr.borrow_mut())
    }
}

impl Host for TestHost {
    fn root_accessor(&self) -> Arc<dyn SourceAccessor> {
        self.root.clone()
    }

    fn allowed_paths(&self) -> Vec<CanonPath> {
        self.allowed.clone()
    }

    /// Appends one logged message the way Nix's `SimpleLogger::log`
    /// writes it: filtered (tabs expanded, escapes stripped), then a
    /// newline.
    fn log(&self, text: &[u8]) {
        let mut stderr = self.stderr.borrow_mut();
        stderr.extend_from_slice(&brix_eval::filter_ansi_escapes(text, true, usize::MAX));
        stderr.push(b'\n');
    }

    fn fetch_tree(
        &self,
        request: &brix_eval::FetchRequest,
    ) -> Result<brix_eval::FetchedTree, brix_eval::FetchError> {
        match request.attrs.get("type") {
            Some(brix_eval::FetchAttr::String(t)) if t == "path" => self
                .fetch_path_tree(request)
                .map_err(brix_eval::FetchError::from),
            _ => Err(brix_eval::FetchError::new(format!(
                "'{}' is not supported by this host (no fetcher configured)",
                request.who
            ))),
        }
    }
}

/// What a run produced: stdout, stderr, and whether it failed. The
/// output is kept as bytes, so comparing two outcomes is byte for byte;
/// it prints lossily.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub stdout: BString,
    pub stderr: BString,
    pub ok: bool,
}

/// Output format, as for `nix-instantiate --eval`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Format {
    Plain,
    Json,
    Xml,
}

fn finish<'a>(
    ev: &Evaluator<'a>,
    host: &TestHost,
    r: brix_eval::Result<brix_eval::Value<'a>>,
    format: Format,
) -> Outcome {
    let r = r.and_then(|v| {
        let cell = ev.alloc_cell(v);
        match format {
            Format::Plain => {
                ev.force_deep(cell)?;
                let mut out = ev.print_ambiguous(cell)?;
                out.push(b'\n');
                Ok(out)
            }
            Format::Json => {
                // `nix-instantiate --json` copies paths to the store.
                let mut ctx = brix_eval::string_context::StringContext::new();
                Ok((ev.to_json(cell, SrcPos::NONE, &mut ctx, true, true)? + "\n").into())
            }
            Format::Xml => {
                let mut ctx = brix_eval::string_context::StringContext::new();
                ev.to_xml(cell, SrcPos::NONE, &mut ctx, true, false)
                    .map(BString::from)
            }
        }
    });
    match r {
        Ok(out) => Outcome {
            stdout: out,
            stderr: host.take_stderr(),
            ok: true,
        },
        Err(e) => {
            let mut stderr = host.take_stderr();
            // The whole rendered error is one logged message.
            stderr.extend_from_slice(&brix_eval::filter_ansi_escapes(
                &e.render(ev.config.show_trace),
                true,
                usize::MAX,
            ));
            stderr.push(b'\n');
            if e.kind == brix_eval::ErrorKind::Usage {
                // What `nix-instantiate` prints after a usage error.
                stderr.extend_from_slice(b"\nTry 'nix-instantiate --help' for more information.\n");
            }
            Outcome {
                stdout: BString::default(),
                stderr,
                ok: false,
            }
        }
    }
}

/// The file at `path` (absolute on the root accessor, bytes that need
/// not be UTF-8) as a path value.
fn main_file(arena: &Arena, path: impl AsRef<[u8]>) -> brix_eval::value::PathVal<'_> {
    let canon = CanonPath::new(path).expect("absolute path");
    brix_eval::value::PathVal {
        accessor: brix_store::AccessorId(0),
        path: arena.alloc_slice_copy(canon.as_bytes()),
    }
}

/// Evaluates a file (`path` is absolute on the test host, bytes that
/// need not be UTF-8) the way `nix-instantiate --eval --strict` does.
pub fn run_brix_file(
    host: &TestHost,
    config: Config,
    path: impl AsRef<[u8]>,
    format: Format,
) -> Outcome {
    let arena = Arena::new();
    let ev = Evaluator::new(&arena, config, host);
    let r = ev.eval_main_file(&main_file(&arena, path));
    finish(&ev, host, r, format)
}

/// [`run_brix_file`] with the tree mounted at `at` served from a
/// compiled image, which `image_for` provides for the evaluator's
/// compile context (an image fits only evaluators with its
/// fingerprint). Also returns how many modules were linked from the
/// image.
pub fn run_brix_file_with_image(
    host: &TestHost,
    config: Config,
    path: impl AsRef<[u8]>,
    format: Format,
    at: &CanonPath,
    image_for: &mut dyn FnMut(&ImageCompileContext) -> Arc<Image>,
) -> (Outcome, u64) {
    let arena = Arena::new();
    let ev = Evaluator::new(&arena, config, host);
    ev.register_image(at.clone(), image_for(ev.image_compile_context()))
        .expect("the image fits the evaluator it was compiled for");
    let r = ev.eval_main_file(&main_file(&arena, path));
    let outcome = finish(&ev, host, r, format);
    (outcome, ev.stats().image_modules.get())
}

/// Evaluates an expression string like `nix-instantiate --eval --strict
/// --expr`, with relative paths against `base_dir`.
pub fn run_brix_expr(
    host: &TestHost,
    config: Config,
    expr: &str,
    base_dir: &str,
    format: Format,
) -> Outcome {
    run_brix_expr_bytes(host, config, expr.as_bytes(), base_dir.as_bytes(), format)
}

/// [`run_brix_expr`] for an expression and a base directory given as
/// bytes, which need not be UTF-8 (`/a + "\xff"` with the raw byte, or
/// relative paths against `/pwd/d\xe9`): what Nix gets on its command
/// line as they are.
pub fn run_brix_expr_bytes(
    host: &TestHost,
    config: Config,
    expr: &[u8],
    base_dir: &[u8],
    format: Format,
) -> Outcome {
    let arena = Arena::new();
    let ev = Evaluator::new(&arena, config, host);
    let r = ev.eval_str(expr, base_dir);
    finish(&ev, host, r, format)
}

/// The reference `nix-instantiate --eval --strict --pure-eval`, run in
/// `cwd` with the oracle's pinned configuration, on the `dummy://` store
/// (which has no valid paths at all) or, with `real_store`, on the
/// user's store (store paths can be read, derivations get written). The
/// caller adds the expression.
pub fn nix_instantiate(cwd: &Path, format: Format, show_trace: bool, real_store: bool) -> Oracle {
    let mut cmd = Oracle::nix_instantiate();
    cmd.dir(cwd);
    if !real_store {
        cmd.dummy_store();
    }
    cmd.env("NIX_STORE_DIR", "/nix/store")
        .config(&format!("show-trace = {show_trace}"))
        .args(["--eval", "--strict", "--pure-eval"]);
    if show_trace {
        cmd.arg("--show-trace");
    }
    match format {
        Format::Plain => {}
        Format::Json => {
            cmd.arg("--json");
        }
        Format::Xml => {
            cmd.arg("--xml").arg("--no-location");
        }
    }
    cmd
}

/// What an oracle run in `cwd` printed, byte for byte, with `cwd`
/// rewritten to `/pwd` and the lines that are noise rather than
/// semantics ([`brix_testutil::is_noise`]) dropped. `cwd` is replaced as
/// the bytes it is, so it need not be UTF-8.
pub fn oracle_outcome(out: &std::process::Output, cwd: &Path) -> Outcome {
    let cwd = cwd.as_os_str().as_bytes();
    let mut stderr = Vec::with_capacity(out.stderr.len());
    for line in out.stderr.lines() {
        if brix_testutil::is_noise(line) {
            continue;
        }
        stderr.extend_from_slice(line);
        stderr.push(b'\n');
    }
    Outcome {
        stdout: out.stdout.replace(cwd, "/pwd").into(),
        stderr: stderr.replace(cwd, "/pwd").into(),
        ok: out.status.success(),
    }
}

/// Runs the reference `nix-instantiate --eval --strict --show-trace
/// --expr` (see [`nix_instantiate`]) in `cwd`, rewriting that directory
/// to `/pwd` in the output.
pub fn run_nix_expr(
    expr: &str,
    cwd: &Path,
    format: Format,
    extra: &[&str],
    real_store: bool,
) -> Outcome {
    run_nix_expr_bytes(expr.as_bytes(), cwd, format, extra, real_store)
}

/// [`run_nix_expr`] for an expression given as bytes, which need not be
/// UTF-8: Nix gets them on its command line as they are.
pub fn run_nix_expr_bytes(
    expr: &[u8],
    cwd: &Path,
    format: Format,
    extra: &[&str],
    real_store: bool,
) -> Outcome {
    let mut cmd = nix_instantiate(cwd, format, true, real_store);
    cmd.args(extra).arg("--expr").arg(OsStr::from_bytes(expr));
    let out = cmd
        .command()
        .output()
        .unwrap_or_else(|e| panic!("running {}: {e}", cmd.describe()));
    oracle_outcome(&out, cwd)
}

/// The extra `nix-instantiate` arguments that let Nix read the real
/// directory it runs in, which brix's [`TestHost`] mounts and allows at
/// `/pwd` while it evaluates in pure mode: Nix reads it impurely (as
/// [`nix_instantiate`] asks for `--pure-eval`, this turns it off again).
pub const NIX_READS_CWD: [&str; 3] = ["--option", "pure-eval", "false"];

/// `Ok` when brix and Nix printed the same, byte for byte, and agree on
/// success; otherwise an error showing both, headed by `what`.
pub fn same_outcome(what: &str, ours: &Outcome, theirs: &Outcome) -> Result<(), String> {
    if ours == theirs {
        return Ok(());
    }
    Err(format!(
        "{what}\n--- nix (ok={})\n{}{}--- brix (ok={})\n{}{}",
        theirs.ok, theirs.stdout, theirs.stderr, ours.ok, ours.stdout, ours.stderr
    ))
}

/// `bytes` as text for a test's message, every byte visible: valid UTF-8
/// as it is (newlines included), each byte that is not as `\xNN`. Unlike
/// a lossy conversion, two outputs that differ only in such bytes are
/// shown differently.
pub fn escaped(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len());
    for chunk in bytes.utf8_chunks() {
        out.push_str(chunk.valid());
        for b in chunk.invalid() {
            write!(out, "\\x{b:02x}").expect("writing to a String cannot fail");
        }
    }
    out
}

/// [`same_outcome`] for outputs (and a case, `what`) that need not be
/// UTF-8: equal byte for byte, or an error showing both with the bytes
/// that are not UTF-8 escaped (see [`escaped`]).
pub fn same_bytes(what: &[u8], ours: &Outcome, theirs: &Outcome) -> Result<(), String> {
    if ours == theirs {
        return Ok(());
    }
    Err(format!(
        "{}\n--- nix (ok={})\n{}{}--- brix (ok={})\n{}{}",
        escaped(what),
        theirs.ok,
        escaped(&theirs.stdout),
        escaped(&theirs.stderr),
        ours.ok,
        escaped(&ours.stdout),
        escaped(&ours.stderr)
    ))
}

/// Panics, showing both outputs as [`same_bytes`] does, unless brix and
/// Nix printed the same bytes and agree on success.
pub fn assert_same_bytes(what: &[u8], ours: &Outcome, theirs: &Outcome) {
    if let Err(diff) = same_bytes(what, ours, theirs) {
        panic!("brix differs from nix:\n{diff}");
    }
}

/// [`compare_all`] for cases given as bytes, which need not be UTF-8.
pub fn compare_all_bytes(
    what: &str,
    cases: &[&[u8]],
    compare: impl Fn(&[u8]) -> Result<(), String>,
) {
    let failures: Vec<String> = cases
        .iter()
        .filter_map(|case| compare(case).err())
        .collect();
    eprintln!(
        "{what}: {} of {} expressions match",
        cases.len() - failures.len(),
        cases.len()
    );
    for failure in &failures {
        eprintln!("MISMATCH {failure}\n");
    }
    assert!(
        failures.is_empty(),
        "{} expressions differ from nix",
        failures.len()
    );
}

/// Runs `compare` on every case and fails the test, after printing each
/// mismatch, unless all of them match; `what` names the cases in the
/// summary line.
pub fn compare_all<S: AsRef<str>>(
    what: &str,
    cases: &[S],
    compare: impl Fn(&str) -> Result<(), String>,
) {
    let failures: Vec<String> = cases
        .iter()
        .filter_map(|case| compare(case.as_ref()).err())
        .collect();
    eprintln!(
        "{what}: {} of {} expressions match",
        cases.len() - failures.len(),
        cases.len()
    );
    for failure in &failures {
        eprintln!("MISMATCH {failure}\n");
    }
    assert!(
        failures.is_empty(),
        "{} expressions differ from nix",
        failures.len()
    );
}

impl TestHost {
    /// `builtins.fetchTree { type = "path"; … }` for the tests: the
    /// directory is served as it is and gets the store path Nix would
    /// give it after copying (`makeFixedOutputPath(NixArchive, narHash,
    /// "source")`).
    fn fetch_path_tree(
        &self,
        request: &brix_eval::FetchRequest,
    ) -> Result<brix_eval::FetchedTree, String> {
        use brix_eval::FetchAttr;
        let Some(FetchAttr::String(path)) = request.attrs.get("path") else {
            return Err("path input lacks 'path'".into());
        };
        let accessor = Arc::new(FsAccessor::new(path));
        let (hash, _) =
            brix_store::nar::hash(&*accessor, &CanonPath::root(), brix_store::HashAlgo::Sha256)
                .map_err(|e| e.to_string())?;
        if let Some(FetchAttr::String(expected)) = request.attrs.get("narHash") {
            let expected =
                brix_store::Hash::parse_any(expected, None, brix_store::XpSettings::NONE)
                    .map_err(|e| e.to_string())?;
            if expected != hash {
                return Err(format!(
                    "NAR hash mismatch in input 'path:{path}', expected '{}' but got '{}'",
                    expected.to_string(brix_store::HashFormat::Sri, true),
                    hash.to_string(brix_store::HashFormat::Sri, true)
                ));
            }
        }
        let store_path = brix_store::StoreDir::default()
            .make_fixed_output_path(
                "source",
                &brix_store::FixedOutputInfo {
                    method: brix_store::FileIngestionMethod::NixArchive,
                    hash,
                    references: Default::default(),
                },
            )
            .map_err(|e| e.to_string())?;
        // What `emitTreeAttrs` gives for a `path` input.
        let mut attrs = std::collections::BTreeMap::new();
        attrs.insert(
            "narHash".to_owned(),
            FetchAttr::String(hash.to_string(brix_store::HashFormat::Sri, true)),
        );
        // Nix reports `lastModified` only for paths outside the store.
        if !path.starts_with("/nix/store/") {
            let mtime = std::fs::metadata(path)
                .and_then(|m| m.modified())
                .map_err(|e| format!("getting the modification time of '{path}': {e}"))?
                .duration_since(std::time::UNIX_EPOCH)
                .expect("the tree was modified after 1970")
                .as_secs();
            attrs.insert("lastModified".to_owned(), FetchAttr::Int(mtime));
            let date = jiff::Timestamp::from_second(mtime as i64)
                .expect("a file's modification time is a jiff timestamp")
                .strftime("%Y%m%d%H%M%S")
                .to_string();
            attrs.insert("lastModifiedDate".to_owned(), FetchAttr::String(date));
        }
        Ok(brix_eval::FetchedTree {
            accessor,
            store_path,
            attrs,
            locked_attrs: Default::default(),
            locked_url: format!("path:{path}"),
            // As Nix (and the facade) show a `path` input: at its real
            // directory, its accessor being the file system itself.
            display_prefix: Some(path.trim_end_matches('/').to_owned()),
        })
    }
}
