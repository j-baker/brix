//! Profiles the evaluation of an expression against a mounted tree (by
//! default the nixpkgs checkout `$BRIX_NIXPKGS`) and writes a flamegraph.
//!
//! ```text
//! cargo run --release -p brix-eval --features profiling --example profile -- \
//!   "(import $BRIX_NIXPKGS { system = \"x86_64-linux\"; }).hello.drvPath" \
//!   /tmp/hello.svg
//! ```
//!
//! Without `--features profiling` it only prints the phase statistics,
//! which is useful on its own. `BRIX_MOUNT` selects another tree and
//! `BRIX_PROFILE_REPEAT=n` evaluates `n` times in fresh evaluators so the
//! sampler sees more of the evaluation.
//!
//! Legacy evaluation remains the default. `BRIX_PROFILE_GC=1` selects the
//! managed evaluator with the facade's 4 GiB startup / 512 MiB steady nursery budgets.
//! `BRIX_PROFILE_GC_THRESHOLD_BYTES=n` overrides that threshold (for example,
//! `1073741824` helps isolate overhead before collection). GC statistics and
//! runtime heap usage are printed alongside the existing allocation counters.
//! Evaluation time excludes evaluator teardown; `run:` includes teardown.

use std::sync::Arc;
use std::time::Instant;

use brix_eval::managed_api::{GcConfig, GcEvaluator};
use brix_eval::{Arena, Evaluator, ValueType};
use brix_store::FsAccessor;
use bstr::BString;

#[path = "support/mount_host.rs"]
mod mount_host;

use mount_host::MountHost;

/// Counts allocations so that allocation pressure can be compared
/// between runs (the arena allocates in large chunks, so this mostly
/// counts `Vec`/`String` traffic outside it).
mod counting {
    use std::alloc::{GlobalAlloc, Layout};
    use std::sync::atomic::{AtomicU64, Ordering};

    pub static ALLOCS: AtomicU64 = AtomicU64::new(0);
    pub static BYTES: AtomicU64 = AtomicU64::new(0);
    /// When non-zero, every `SAMPLE_EVERY`th allocation records a backtrace
    /// (`BRIX_PROFILE_ALLOCS=n`).
    pub static SAMPLE_EVERY: AtomicU64 = AtomicU64::new(0);
    pub static SAMPLING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    pub static SAMPLES: std::sync::Mutex<Vec<(usize, String)>> = std::sync::Mutex::new(Vec::new());

    /// The allocator the brix binary uses, so the numbers match it.
    static INNER: brix_alloc::Global = brix_alloc::GLOBAL;

    pub struct Counting;

    // SAFETY: every allocation and deallocation is passed through to
    // `INNER`, a `GlobalAlloc`, with the caller's layout and pointer
    // unchanged, so `INNER`'s guarantees are this allocator's. The
    // counting is atomic and never allocates; the backtrace sampling
    // allocates (through this allocator), which the `SAMPLING` flag keeps
    // from recursing, and does not unwind out of `alloc`.
    unsafe impl GlobalAlloc for Counting {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            let n = ALLOCS.fetch_add(1, Ordering::Relaxed);
            BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
            let every = SAMPLE_EVERY.load(Ordering::Relaxed);
            // Re-entrancy guard: capturing a backtrace allocates.
            if every != 0
                && n % every == 0
                && SAMPLING
                    .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
                    .is_ok()
            {
                let bt = std::backtrace::Backtrace::force_capture().to_string();
                if let Ok(mut s) = SAMPLES.lock() {
                    s.push((layout.size(), bt));
                }
                SAMPLING.store(false, Ordering::Release);
            }
            unsafe { INNER.alloc(layout) }
        }

        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            unsafe { INNER.dealloc(ptr, layout) }
        }
    }
}

#[global_allocator]
static ALLOC: counting::Counting = counting::Counting;

fn run_legacy(expr: &str, host: &MountHost) -> Result<BString, BString> {
    let arena = Arena::new();
    let ev = Evaluator::new(&arena, mount_host::config_from_env(), host);
    let started = Instant::now();
    let result = ev.eval_str(expr.as_bytes(), "/").and_then(|v| {
        let cell = ev.alloc_cell(v);
        ev.force_deep(cell)?;
        ev.print_ambiguous(cell)
    });
    eprintln!("evaluation: {:?} (legacy)", started.elapsed());
    let stats = ev.stats().render();
    eprintln!("{stats}");
    eprintln!(
        "arena: {:.1} MiB allocated in chunks",
        arena.allocated_bytes() as f64 / (1024.0 * 1024.0)
    );
    result.map_err(|e| e.render(true))
}

fn run_managed(expr: &str, host: &MountHost, config: GcConfig) -> Result<BString, BString> {
    let arena = Arena::new();
    let mut ev = GcEvaluator::new(
        Evaluator::new(&arena, mount_host::config_from_env(), host),
        config,
    )
    .map_err(|error| error.render(true))?;
    let started = Instant::now();
    let result = ev.eval_str(expr.as_bytes(), "/").and_then(|value| {
        // Scalar workloads such as hello.drvPath need no extra forcing.
        // Use the managed deepSeq builtin for compound results, preserving
        // laziness/identity through its own precise root protocol.
        if matches!(ev.value_type(&value)?, ValueType::Attrs | ValueType::List) {
            let deep = ev.eval_str(b"builtins.deepSeq", "/")?;
            let deep = ev.apply(&deep, &value)?;
            let unit = ev.null()?;
            ev.apply(&deep, &unit)?;
        }
        ev.print_ambiguous(&value)
    });
    eprintln!("evaluation: {:?} (managed)", started.elapsed());
    eprintln!("{}", ev.services().stats().render());
    let stats = ev.gc_stats();
    const MIB: f64 = 1024.0 * 1024.0;
    eprintln!(
        "gc: {} collections, {:?} collecting, {:.1} MiB copied, {:.1} MiB reclaimed, {:.1} MiB last live, {:.1} MiB peak collection capacity",
        stats.collections,
        stats.collection_time,
        stats.copied_bytes as f64 / MIB,
        stats.reclaimed_bytes as f64 / MIB,
        stats.last_live_bytes as f64 / MIB,
        stats.peak_collection_capacity as f64 / MIB,
    );
    eprintln!(
        "gc generations: {} minor ({:?}), {} major ({:?}), {:.1} MiB promoted, {:.1} MiB compacted",
        stats.minor_collections,
        stats.minor_collection_time,
        stats.major_collections,
        stats.major_collection_time,
        stats.promoted_bytes as f64 / MIB,
        stats.compacted_bytes as f64 / MIB,
    );
    eprintln!(
        "heap: {:.1} MiB used; arena: {:.1} MiB allocated in chunks",
        ev.heap_bytes() as f64 / MIB,
        arena.allocated_bytes() as f64 / MIB
    );
    result.map_err(|error| error.render(true))
}

fn managed_config() -> Option<GcConfig> {
    let enabled = match std::env::var_os("BRIX_PROFILE_GC") {
        None => false,
        Some(value) if value == "0" => false,
        Some(value) if value == "1" => true,
        Some(value) => panic!("BRIX_PROFILE_GC must be 1, 0 or unset, not {value:?}"),
    };
    enabled.then(|| GcConfig {
        threshold_bytes: env_number("BRIX_PROFILE_GC_THRESHOLD_BYTES")
            .unwrap_or(4usize.saturating_mul(1024 * 1024 * 1024)),
        steady_threshold_bytes: env_number("BRIX_PROFILE_GC_STEADY_THRESHOLD_BYTES").or_else(
            || {
                std::env::var_os("BRIX_PROFILE_GC_THRESHOLD_BYTES")
                    .is_none()
                    .then_some(512 * 1024 * 1024)
            },
        ),
        ..GcConfig::default()
    })
}

/// `$name` as a number, or `None` when it is unset.
fn env_number<T: std::str::FromStr>(name: &str) -> Option<T>
where
    T::Err: std::fmt::Display,
{
    match std::env::var(name) {
        Ok(s) => Some(
            s.parse()
                .unwrap_or_else(|e| panic!("{name}={s:?} is not a valid number: {e}")),
        ),
        Err(std::env::VarError::NotPresent) => None,
        Err(e) => panic!("{name}: {e}"),
    }
}

fn main() {
    // Links in and checks the mimalloc tuning, and starts pre-faulting
    // as `BRIX_PREFAULT` selects, so the numbers match the brix binary.
    brix_alloc::init().expect("allocator setup");
    let mut args = std::env::args().skip(1);
    let expr = args
        .next()
        .expect("usage: profile <expression> [flamegraph.svg]");
    let svg = args.next();
    let repeat: usize = env_number("BRIX_PROFILE_REPEAT").unwrap_or(1);
    let gc = managed_config();
    if let Some(config) = gc {
        eprintln!(
            "evaluator: managed, nursery GC threshold={} bytes, chunk={} bytes",
            config.threshold_bytes, config.chunk_bytes
        );
    } else {
        eprintln!("evaluator: legacy");
    }
    let mount = mount_host::mount_path();
    // `BRIX_PROFILE_LAYOUT=store` reproduces the CLI's layout: the whole
    // store directory mounted at its path, instead of just the one tree.
    let store_layout = match std::env::var_os("BRIX_PROFILE_LAYOUT") {
        None => false,
        Some(v) if v == "store" => true,
        Some(v) => panic!("BRIX_PROFILE_LAYOUT must be 'store' or unset, not {v:?}"),
    };
    let host = if store_layout {
        MountHost::new(
            "/nix/store",
            Arc::new(FsAccessor::new("/nix/store")),
            &mount,
        )
    } else {
        MountHost::from_env()
    };

    #[cfg(feature = "profiling")]
    let guard = svg.as_ref().map(|_| {
        pprof::ProfilerGuardBuilder::default()
            .frequency(2000)
            .blocklist(&["libc", "libpthread", "vdso"])
            .build()
            .expect("start profiler")
    });
    #[cfg(not(feature = "profiling"))]
    if svg.is_some() {
        eprintln!("note: build with --features profiling to write a flamegraph");
    }

    if let Some(n) = env_number("BRIX_PROFILE_ALLOCS") {
        counting::SAMPLE_EVERY.store(n, std::sync::atomic::Ordering::Relaxed);
    }
    let start = Instant::now();
    let mut last = Ok(BString::default());
    for _ in 0..repeat {
        counting::ALLOCS.store(0, std::sync::atomic::Ordering::Relaxed);
        counting::BYTES.store(0, std::sync::atomic::Ordering::Relaxed);
        let t = Instant::now();
        last = match gc {
            Some(config) => run_managed(&expr, &host, config),
            None => run_legacy(&expr, &host),
        };
        eprintln!(
            "run: {:?}, {} heap allocations ({:.1} MiB)",
            t.elapsed(),
            counting::ALLOCS.load(std::sync::atomic::Ordering::Relaxed),
            counting::BYTES.load(std::sync::atomic::Ordering::Relaxed) as f64 / (1024.0 * 1024.0)
        );
    }
    eprintln!("total: {:?} for {repeat} run(s)", start.elapsed());
    counting::SAMPLE_EVERY.store(0, std::sync::atomic::Ordering::Relaxed);
    report_alloc_samples();

    #[cfg(feature = "profiling")]
    if let (Some(guard), Some(svg)) = (guard, &svg) {
        let report = guard.report().build().expect("profiler report");
        let file = std::fs::File::create(svg).expect("create flamegraph file");
        report.flamegraph(file).expect("write flamegraph");
        eprintln!("flamegraph: {svg}");
    }

    match last {
        Ok(text) => println!("{text}"),
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    }
}

/// Aggregates sampled allocation backtraces by their innermost brix
/// frames and prints the heaviest sites.
fn report_alloc_samples() {
    let samples = counting::SAMPLES.lock().expect("samples");
    if samples.is_empty() {
        return;
    }
    let mut by_site: std::collections::HashMap<String, (usize, usize)> =
        std::collections::HashMap::new();
    for (size, bt) in samples.iter() {
        // Keep the first few frames inside brix crates, skipping the
        // allocator and std internals.
        let frames: Vec<&str> = bt
            .lines()
            .filter_map(|l| l.trim().split_once(": ").map(|(_, f)| f.trim()))
            .filter(|f| {
                f.starts_with("brix_")
                    || f.contains("brix_eval")
                    || f.contains("brix_syntax")
                    || f.contains("brix_store")
            })
            .filter(|f| !f.contains("examples/profile") && !f.contains("profile::counting"))
            .take(4)
            .collect();
        let key = frames.join(" < ");
        let e = by_site.entry(key).or_insert((0, 0));
        e.0 += 1;
        e.1 += size;
    }
    let mut sites: Vec<_> = by_site.into_iter().collect();
    sites.sort_by_key(|(_, (n, _))| std::cmp::Reverse(*n));
    eprintln!(
        "--- sampled allocation sites by count ({} samples) ---",
        samples.len()
    );
    for (site, (n, bytes)) in sites.iter().take(20) {
        eprintln!("{n:5} samples {:8} B avg  {site}", bytes / n.max(&1));
    }
    sites.sort_by_key(|(_, (_, bytes))| std::cmp::Reverse(*bytes));
    eprintln!("--- sampled allocation sites by bytes ---");
    for (site, (n, bytes)) in sites.iter().take(20) {
        eprintln!("{:8} KiB {n:5} samples  {site}", bytes / 1024);
    }
}
