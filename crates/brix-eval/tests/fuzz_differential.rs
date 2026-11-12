//! Differential fuzzing: random expressions (see `support/gen.rs`) are
//! evaluated by brix and by the reference `nix-instantiate`, and stdout,
//! stderr and the exit status must agree byte for byte, in the plain,
//! `--json` and `--xml` formats, with and without `--show-trace`.
//!
//! Both sides run in child processes with an address-space limit (2 GiB
//! for brix, 8 GiB for Nix, whose collector reserves a large arena), so
//! a pathological case (a huge `genList`, runaway recursion) costs one
//! child, not the machine; a child that dies of memory exhaustion counts
//! as `error: out of memory` on either side, which is also what Nix
//! prints for a failed allocation. The brix child is this test binary
//! re-executed on the hidden `fuzz_child` test.
//!
//! The default run uses fixed seeds, so it is deterministic. For a longer
//! run, in batches with the parent's memory checked between them:
//!
//! ```text
//! BRIX_FUZZ_CASES=20000 BRIX_FUZZ_SEED=42 cargo test -p brix-eval --release \
//!     --test fuzz_differential -- --ignored long_run
//! ```
//!
//! `BRIX_FUZZ_JOBS` sets the number of concurrent cases (default 4) and
//! `BRIX_FUZZ_VERBOSE=1` prints each seed before it runs. Every mismatch is
//! shrunk to a minimal expression and written under `target/fuzz-failures/`
//! as `seed-<n>.nix` with the two outputs beside it.

mod common;
mod support;

use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use brix_eval::Config;
use bstr::{BString, ByteSlice};
use common::{nix_instantiate, oracle_outcome, run_brix_expr, Format, Outcome, TestHost};
use support::gen::{self, Expr, Gen};

/// Address-space limit for the brix child.
const BRIX_MEMORY_LIMIT: u64 = 2 << 30;

/// Address-space limit for the oracle. Nix's garbage collector maps a
/// large arena at start-up, so it needs far more address space than it
/// uses; below about 4 GiB it fails before evaluating anything.
const NIX_MEMORY_LIMIT: u64 = 8 << 30;

/// Cases per batch in the long run; the parent's RSS is checked after
/// each.
const BATCH: u64 = 2000;

/// How one case is run.
#[derive(Clone, Copy, Debug)]
struct Mode {
    format: Format,
    show_trace: bool,
}

impl Mode {
    fn format_name(self) -> &'static str {
        match self.format {
            Format::Plain => "plain",
            Format::Json => "json",
            Format::Xml => "xml",
        }
    }

    fn parse(format: &str, show_trace: &str) -> Mode {
        let format = match format {
            "plain" => Format::Plain,
            "json" => Format::Json,
            "xml" => Format::Xml,
            other => panic!("unknown format {other}"),
        };
        Mode {
            format,
            show_trace: show_trace == "true",
        }
    }
}

struct Mismatch {
    seed: u64,
    mode: Mode,
    expr: Expr,
    ours: Outcome,
    theirs: Outcome,
}

/// Caps the child's address space.
fn limit_memory(cmd: &mut Command, bytes: u64) {
    // SAFETY: `setrlimit` is async-signal-safe and touches no memory
    // shared with the parent, which is all `pre_exec` requires.
    unsafe {
        cmd.pre_exec(move || {
            let limit = rustix::process::Rlimit {
                current: Some(bytes),
                maximum: Some(bytes),
            };
            rustix::process::setrlimit(rustix::process::Resource::As, limit)
                .map_err(std::io::Error::from)
        });
    }
}

#[test]
fn memory_limit_is_applied_to_the_child() {
    const CHILD: &str = "BRIX_TEST_MEMORY_LIMIT_CHILD";
    if std::env::var_os(CHILD).is_some() {
        let limit = rustix::process::getrlimit(rustix::process::Resource::As);
        assert_eq!(limit.current, Some(BRIX_MEMORY_LIMIT));
        assert_eq!(limit.maximum, Some(BRIX_MEMORY_LIMIT));
        return;
    }
    let mut child = Command::new(std::env::current_exe().unwrap());
    child
        .args(["--exact", "memory_limit_is_applied_to_the_child"])
        .env(CHILD, "1");
    limit_memory(&mut child, BRIX_MEMORY_LIMIT);
    let output = child.output().unwrap();
    assert!(output.status.success(), "{output:?}");
}

/// What a child that produced no result stands for: memory exhaustion
/// (reported like Nix's `std::bad_alloc` handler does) or a genuine crash.
fn died(status: ExitStatus, stderr: &str) -> Outcome {
    let memory = [
        "memory allocation of",
        "out of memory",
        "capacity overflow",
        "bad_alloc",
        "GC Warning: Out of Memory",
        "Cannot allocate memory",
    ];
    if memory.iter().any(|m| stderr.contains(m)) {
        return oom();
    }
    let tail: String = stderr
        .lines()
        .rev()
        .take(5)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .map(|l| format!("{l}\n"))
        .collect();
    Outcome {
        stdout: BString::default(),
        stderr: format!("PROCESS DIED: {status}\n{tail}").into(),
        ok: false,
    }
}

/// `nix-instantiate --eval --strict --pure-eval` on the dummy store (see
/// `common::nix_instantiate`), with the memory limit applied.
fn run_oracle(expr: &str, cwd: &Path, mode: Mode) -> Outcome {
    let mut oracle = nix_instantiate(cwd, mode.format, mode.show_trace, false);
    oracle.arg("--expr").arg(expr);
    let mut cmd = oracle.command();
    limit_memory(&mut cmd, NIX_MEMORY_LIMIT);
    let out = cmd.output().expect("run nix-instantiate");
    if out.status.code().is_none() {
        return died(out.status, &out.stderr.to_str_lossy());
    }
    oracle_outcome(&out, cwd)
}

/// Evaluates in this process. The evaluator and its arena live for one
/// case only.
fn run_ours_in_process(expr: &str, pwd: &Path, mode: Mode) -> Outcome {
    let empty_store = tempfile::tempdir().expect("temporary directory");
    // The working directory sits at its real path, as for the oracle, so
    // that `../up` resolves to the same place; the path is rewritten to
    // `/pwd` in the output afterwards, like `run_oracle` does.
    let pwd_s = pwd.to_str().expect("utf-8 temp dir");
    let host = TestHost::with_allowed(&[(pwd_s, pwd), ("/nix/store", empty_store.path())], &[]);
    let config = Config {
        // The reference runs the parallel evaluator (see `run_oracle`).
        eval_cores: 0,
        show_trace: mode.show_trace,
        ..Config::default()
    };
    // A panic in the evaluator is a bug worth a repro of its own: report
    // it as the outcome rather than tearing the whole run down.
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        run_brix_expr(&host, config, expr, pwd_s, mode.format)
    })) {
        Ok(outcome) => Outcome {
            stdout: outcome.stdout.replace(pwd_s, "/pwd").into(),
            stderr: outcome.stderr.replace(pwd_s, "/pwd").into(),
            ok: outcome.ok,
        },
        Err(payload) => {
            let msg = payload
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| payload.downcast_ref::<&str>().map(|s| (*s).to_owned()))
                .unwrap_or_else(|| "non-string panic payload".to_owned());
            Outcome {
                stdout: BString::default(),
                stderr: format!("PANIC: {msg}\n").into(),
                ok: false,
            }
        }
    }
}

/// The file format the parent and the `fuzz_child` test exchange: four
/// header lines (format, show-trace, pwd, stdout length) and the bodies.
fn encode_request(expr: &str, pwd: &Path, mode: Mode) -> String {
    format!(
        "{}\n{}\n{}\n{}",
        mode.format_name(),
        mode.show_trace,
        pwd.display(),
        expr
    )
}

/// An outcome as bytes: `ok` and the length of stdout on a line each,
/// then stdout and stderr as they are.
fn encode_outcome(o: &Outcome) -> Vec<u8> {
    let mut out = format!("{}\n{}\n", o.ok, o.stdout.len()).into_bytes();
    out.extend_from_slice(&o.stdout);
    out.extend_from_slice(&o.stderr);
    out
}

fn decode_outcome(s: &[u8]) -> Option<Outcome> {
    let (ok, rest) = s.split_once_str("\n")?;
    let (len, rest) = rest.split_once_str("\n")?;
    let len: usize = len.to_str().ok()?.parse().ok()?;
    Some(Outcome {
        ok: ok == b"true",
        stdout: rest.get(..len)?.into(),
        stderr: rest.get(len..)?.into(),
    })
}

/// Evaluates in a memory-limited child: this binary re-run on
/// `fuzz_child`.
fn run_ours(expr: &str, pwd: &Path, mode: Mode) -> Outcome {
    let request = tempfile::NamedTempFile::new().expect("temp file");
    std::fs::write(request.path(), encode_request(expr, pwd, mode)).unwrap();
    let response = tempfile::NamedTempFile::new().expect("temp file");
    let mut cmd = Command::new(std::env::current_exe().expect("test binary path"));
    limit_memory(&mut cmd, BRIX_MEMORY_LIMIT);
    cmd.args(["--exact", "fuzz_child", "--ignored", "--quiet"])
        .env("BRIX_FUZZ_CHILD_IN", request.path())
        .env("BRIX_FUZZ_CHILD_OUT", response.path());
    let out = cmd.output().expect("run fuzz child");
    // A child that died wrote nothing (or not all of it).
    let written = std::fs::read(response.path()).expect("read the child's response");
    match decode_outcome(&written) {
        Some(outcome) => outcome,
        None => died(out.status, &out.stderr.to_str_lossy()),
    }
}

/// The child side of [`run_ours`]: a no-op unless the parent set the
/// request variable.
#[test]
#[ignore = "internal: run by the fuzz harness in a child process"]
fn fuzz_child() {
    let Ok(input) = std::env::var("BRIX_FUZZ_CHILD_IN") else {
        return;
    };
    let output = std::env::var("BRIX_FUZZ_CHILD_OUT").expect("BRIX_FUZZ_CHILD_OUT");
    let request = std::fs::read_to_string(input).expect("read request");
    let mut lines = request.splitn(4, '\n');
    let format = lines.next().expect("format");
    let show_trace = lines.next().expect("show-trace");
    let pwd = PathBuf::from(lines.next().expect("pwd"));
    let expr = lines.next().expect("expression").to_owned();
    let mode = Mode::parse(format, show_trace);
    // Deep expressions need more than the default stack; the evaluator
    // grows its own on demand but the printers recurse on the plain one.
    let outcome = std::thread::Builder::new()
        .stack_size(256 << 20)
        .spawn(move || run_ours_in_process(&expr, &pwd, mode))
        .expect("spawn")
        .join()
        .expect("evaluation thread");
    std::fs::write(output, encode_outcome(&outcome)).expect("write response");
}

/// Memory exhaustion, in whichever words it came: Nix says `out of
/// memory` when `operator new` fails and `ran out of memory` (in colour,
/// from the collector's handler) when the garbage collector cannot grow;
/// which one depends on the size requested and the machine. brix says
/// `out of memory`. The three are the same outcome for this harness.
/// (Recognising the message lossily is safe: no other bytes decode to
/// it.)
fn normalise_oom(o: Outcome) -> Outcome {
    let plain = brix_eval::filter_ansi_escapes_str(&o.stderr.to_str_lossy(), true, usize::MAX);
    let trimmed = plain.trim();
    if !o.ok
        && o.stdout.is_empty()
        && (trimmed == "error: out of memory"
            || trimmed == "error: ran out of memory"
            // The arena allocator panics with this text when the child's
            // address-space limit is hit.
            || trimmed == "PANIC: out of memory")
    {
        return oom();
    }
    o
}

fn oom() -> Outcome {
    Outcome {
        stdout: BString::default(),
        stderr: "error: out of memory\n".into(),
        ok: false,
    }
}

/// Whether the brix outcome is Nix's `max-call-depth` error, in any
/// output mode.
fn is_call_depth_error(o: &Outcome) -> bool {
    !o.ok
        && o.stderr
            .contains_str("error: stack overflow; max-call-depth exceeded")
}

/// Whether the outcome ends in Nix's out-of-memory report (possibly after
/// trace output).
fn ends_in_oom(o: &Outcome) -> bool {
    let plain = brix_eval::filter_ansi_escapes_str(&o.stderr.to_str_lossy(), true, usize::MAX);
    let last = plain.trim_end().lines().last().unwrap_or("");
    !o.ok && (last == "error: out of memory" || last == "error: ran out of memory")
}

/// The stderr text before the `error:` block (trace output, warnings),
/// or `None` if stderr is not UTF-8 (and so cannot be compared exactly
/// once filtered).
fn before_error(o: &Outcome) -> Option<String> {
    let plain = brix_eval::filter_ansi_escapes_str(o.stderr.to_str().ok()?, true, usize::MAX);
    let cut = plain
        .lines()
        .take_while(|l| !l.starts_with("error:"))
        .map(|l| format!("{l}\n"))
        .collect::<String>();
    Some(cut.trim_end().to_owned())
}

/// Runs one expression both ways; `Some` on a mismatch.
fn compare(expr: &Expr, pwd: &Path, mode: Mode) -> Option<(Outcome, Outcome)> {
    let mut src = expr.render();
    if src.starts_with('-') {
        // `nix-instantiate --expr -1` reads `-1` as a flag.
        src = format!("({src})");
    }
    let ours = normalise_oom(run_ours(&src, pwd, mode));
    let theirs = normalise_oom(run_oracle(&src, pwd, mode));
    // Determinate Nix's parallel deep force does not count call depth, so
    // an infinitely nested value (`rec { a = { b = a; }; }` under
    // `--strict`) exhausts its memory where brix, like upstream Nix,
    // reports `max-call-depth exceeded`. Either is a correct refusal.
    if ends_in_oom(&theirs)
        && is_call_depth_error(&ours)
        && ours.stdout == theirs.stdout
        && before_error(&ours).is_some_and(|b| Some(b) == before_error(&theirs))
    {
        return None;
    }
    (ours != theirs).then_some((ours, theirs))
}

fn mode_for(seed: u64) -> Mode {
    // Derived from the seed, not from the generator's RNG, so changing
    // the grammar does not change which modes a seed exercises.
    let format = match seed % 5 {
        0 => Format::Json,
        1 => Format::Xml,
        _ => Format::Plain,
    };
    Mode {
        format,
        show_trace: seed % 3 != 0,
    }
}

fn budget_for(seed: u64) -> usize {
    5 + (seed.wrapping_mul(2654435761) % 36) as usize
}

fn failures_dir() -> PathBuf {
    let target = std::env::var("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target"));
    let dir = target.join("fuzz-failures");
    if std::fs::create_dir_all(&dir).is_ok() {
        return dir;
    }
    // The manifest directory is read-only when the tests run inside a
    // Nix build; keep the repros somewhere writable instead.
    let dir = std::env::temp_dir().join("brix-fuzz-failures");
    std::fs::create_dir_all(&dir).expect("create fuzz failures dir");
    dir
}

fn clear_failures_dir() {
    let dir = failures_dir();
    for entry in std::fs::read_dir(&dir).expect("list the fuzz failures dir") {
        let path = entry.expect("list the fuzz failures dir").path();
        std::fs::remove_file(&path)
            .unwrap_or_else(|e| panic!("removing the old repro {}: {e}", path.display()));
    }
}

/// `$name` as a number, or `default` when it is unset.
fn env_number<T: std::str::FromStr>(name: &str, default: impl FnOnce() -> T) -> T
where
    T::Err: std::fmt::Display,
{
    match std::env::var(name) {
        Ok(s) => s
            .parse()
            .unwrap_or_else(|e| panic!("{name}={s:?} is not a valid number: {e}")),
        Err(std::env::VarError::NotPresent) => default(),
        Err(e) => panic!("{name}: {e}"),
    }
}

/// `$name` as a switch: `1` on, `0` or unset off; anything else is a
/// mistake.
fn env_flag(name: &str) -> bool {
    match std::env::var_os(name) {
        None => false,
        Some(v) if v == "1" => true,
        Some(v) if v == "0" => false,
        Some(v) => panic!("{name} must be 1 or 0, not {v:?}"),
    }
}

fn jobs() -> usize {
    env_number("BRIX_FUZZ_JOBS", || 4)
}

/// This process's resident set size in bytes, from `/proc`.
fn resident_bytes() -> u64 {
    let status = std::fs::read_to_string("/proc/self/status").expect("read /proc/self/status");
    let kb = status
        .lines()
        .find_map(|l| l.strip_prefix("VmRSS:"))
        .and_then(|v| v.trim().strip_suffix("kB"))
        .unwrap_or_else(|| panic!("no VmRSS line in /proc/self/status:\n{status}"));
    let kb: u64 = kb
        .trim()
        .parse()
        .unwrap_or_else(|e| panic!("VmRSS {kb:?} in /proc/self/status: {e}"));
    kb * 1024
}

/// Runs seeds `start..start+count` on [`jobs`] workers, with `pwd` as the
/// (empty) working directory, and returns the mismatches.
fn run_seeds(pwd: &Path, start: u64, count: u64) -> Vec<Mismatch> {
    let next = AtomicUsize::new(0);
    let mismatches = Mutex::new(Vec::new());
    let verbose = env_flag("BRIX_FUZZ_VERBOSE");
    std::thread::scope(|scope| {
        for _ in 0..jobs() {
            scope.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::Relaxed) as u64;
                if i >= count {
                    break;
                }
                let seed = start + i;
                let mode = mode_for(seed);
                let expr = Gen::new(seed, budget_for(seed)).expr();
                if verbose {
                    eprintln!("seed {seed}: {}", expr.render().replace('\n', "\\n"));
                }
                if let Some((ours, theirs)) = compare(&expr, pwd, mode) {
                    mismatches.lock().unwrap().push(Mismatch {
                        seed,
                        mode,
                        expr,
                        ours,
                        theirs,
                    });
                }
            });
        }
    });
    let mut found = mismatches.into_inner().unwrap();
    found.sort_by_key(|m| m.seed);
    found
}

/// An outcome as a repro file shows it: `ok=…`, then stdout and stderr
/// as they are.
fn outcome_file(o: &Outcome) -> Vec<u8> {
    let mut out = format!("ok={}\n", o.ok).into_bytes();
    out.extend_from_slice(&o.stdout);
    out.extend_from_slice(&o.stderr);
    out
}

/// Shrinks each mismatch and writes a repro; returns the report text.
fn report(pwd: &Path, mismatches: Vec<Mismatch>, shrink: bool) -> String {
    let dir = failures_dir();
    let mut report = String::new();
    for m in mismatches {
        let (expr, ours, theirs) = if shrink {
            let mut last = (m.ours.clone(), m.theirs.clone());
            let small = gen::shrink(m.expr.clone(), 400, |candidate| {
                match compare(candidate, pwd, m.mode) {
                    Some(outcomes) => {
                        last = outcomes;
                        true
                    }
                    None => false,
                }
            });
            (small, last.0, last.1)
        } else {
            (m.expr, m.ours, m.theirs)
        };
        let src = expr.render();
        let base = dir.join(format!("seed-{}", m.seed));
        std::fs::write(base.with_extension("nix"), format!("{src}\n")).unwrap();
        std::fs::write(base.with_extension("nix.out"), outcome_file(&theirs)).unwrap();
        std::fs::write(base.with_extension("brix.out"), outcome_file(&ours)).unwrap();
        report.push_str(&format!(
            "seed {} [{:?}, show_trace={}]\n{src}\n--- nix (ok={})\n{}{}--- brix (ok={})\n{}{}\n",
            m.seed,
            m.mode.format,
            m.mode.show_trace,
            theirs.ok,
            theirs.stdout,
            theirs.stderr,
            ours.ok,
            ours.stdout,
            ours.stderr
        ));
    }
    report
}

fn fail_on(pwd: &Path, mismatches: Vec<Mismatch>, total: u64) {
    let n = mismatches.len();
    eprintln!("fuzz: {} of {total} cases match", total as usize - n);
    if n > 0 {
        let text = report(pwd, mismatches, true);
        panic!(
            "{n} fuzz cases differ from nix (repros in {}):\n{text}",
            failures_dir().display()
        );
    }
}

/// An empty directory that stands for the working directory `/pwd`.
fn pwd_dir() -> tempfile::TempDir {
    tempfile::tempdir().expect("temporary directory")
}

/// The deterministic default run.
#[test]
fn fixed_seeds_match_nix() {
    clear_failures_dir();
    let pwd = pwd_dir();
    let count = 500;
    let mismatches = run_seeds(pwd.path(), 0, count);
    fail_on(pwd.path(), mismatches, count);
}

/// `BRIX_FUZZ_CASES` cases from `BRIX_FUZZ_SEED` (default: the clock), in
/// batches of [`BATCH`], checking that this process stays small between
/// them (all the work happens in children).
#[test]
#[ignore = "long run; set BRIX_FUZZ_CASES and BRIX_FUZZ_SEED"]
fn long_run() {
    clear_failures_dir();
    let pwd = pwd_dir();
    let count: u64 = env_number("BRIX_FUZZ_CASES", || 5000);
    let start: u64 = env_number("BRIX_FUZZ_SEED", || {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("the clock is after 1970")
            .as_secs()
    });
    eprintln!(
        "fuzz long run: {count} cases from seed {start}, {} jobs",
        jobs()
    );
    let started = std::time::Instant::now();
    let mut mismatches = Vec::new();
    let mut done = 0;
    while done < count {
        let n = BATCH.min(count - done);
        mismatches.extend(run_seeds(pwd.path(), start + done, n));
        done += n;
        let rss = resident_bytes();
        eprintln!(
            "fuzz: {done}/{count} cases, {} mismatches so far, parent RSS {} MiB, {:.0}s",
            mismatches.len(),
            rss >> 20,
            started.elapsed().as_secs_f64()
        );
        assert!(
            rss < 1 << 30,
            "the fuzz harness itself grew to {} MiB; the work should be in children",
            rss >> 20
        );
    }
    fail_on(pwd.path(), mismatches, count);
}

/// Renders a handful of generated expressions, as a smoke test of the
/// generator and a way to eyeball its output (`--nocapture`).
#[test]
fn generator_renders_parseable_expressions() {
    let mut bad = Vec::new();
    for seed in 0..200 {
        let expr = Gen::new(seed, budget_for(seed)).expr();
        let src = expr.render();
        if seed < 5 {
            eprintln!("seed {seed}: {src}");
        }
        // Only whether it parses matters here, not what it warns about.
        let mut warnings = Vec::new();
        if let Err(e) =
            brix_syntax::parse(src.as_bytes(), "/pwd", &Default::default(), &mut warnings)
        {
            // Kept on disk so it can be fed to `nix-instantiate --parse`.
            std::fs::write(failures_dir().join(format!("gen-{seed}.nix")), &src).unwrap();
            bad.push(format!("seed {seed}: {e}\n  {src}"));
        }
    }
    assert!(
        bad.is_empty(),
        "generated expressions failed to parse:\n{}",
        bad.join("\n")
    );
}
