//! The global allocator for brix binaries: mimalloc, tuned for the
//! evaluator's allocation pattern, under [`Prefaulting`], which populates
//! the evaluator's large arena chunks off the critical path.
//!
//! Library crates never install an allocator; the `brix` binary and the
//! measurement examples do, with one macro call at module level and one
//! function call at the top of `main`:
//!
//! ```ignore
//! brix_alloc::install!();
//!
//! fn main() {
//!     if let Err(e) = brix_alloc::init() {
//!         eprintln!("error: {e}");
//!         std::process::exit(1);
//!     }
//! }
//! ```
//!
//! A binary that uses [`GLOBAL`] in an allocator of its own (the
//! profiling example counts allocations around it) calls [`init`] all the
//! same: `init` is what links the mimalloc tuning in and checks it, and
//! what starts pre-faulting.

mod prefault;

pub use prefault::{policy, set_policy, Policy, Prefaulting, THRESHOLD, WINDOW};

/// The allocator type brix binaries install.
pub type Global = Prefaulting<mimalloc::MiMalloc>;

/// The allocator value brix binaries install.
pub const GLOBAL: Global = Prefaulting::new(mimalloc::MiMalloc);

/// Installs the allocator in the calling binary.
#[macro_export]
macro_rules! install {
    () => {
        #[global_allocator]
        static BRIX_GLOBAL_ALLOC: $crate::Global = $crate::GLOBAL;
    };
}

/// Sets the allocator up at start-up; call it at the top of `main`.
///
/// - Checks that mimalloc's eager arena commit is off (see `tune.c`),
///   unless the user turned it on with `MIMALLOC_ARENA_EAGER_COMMIT`.
/// - Selects the pre-faulting policy `BRIX_PREFAULT` names (`off`, `sync`
///   or `background`), starting the helper thread for `background`. A
///   value that is not one of those, or a policy the kernel cannot do, is
///   an error naming the variable. Unset, the policy is `background`
///   where the kernel supports it; on a Linux kernel that does not, it is
///   `off`, with a warning on stderr that says so.
pub fn init() -> Result<(), String> {
    check_tuning()?;
    let policy = match std::env::var_os("BRIX_PREFAULT") {
        Some(value) => {
            let parsed = value.to_str().and_then(Policy::parse);
            parsed.ok_or_else(|| {
                format!("BRIX_PREFAULT must be 'off', 'sync' or 'background', not {value:?}")
            })?
        }
        None => match prefault::populate_support() {
            Ok(_) => Policy::Background,
            Err(why) => {
                if cfg!(target_os = "linux") {
                    eprintln!(
                        "warning: {why}; large allocations are not pre-faulted (set BRIX_PREFAULT=off to silence this warning)"
                    );
                }
                Policy::Off
            }
        },
    };
    set_policy(policy).map_err(|e| format!("BRIX_PREFAULT={}: {e}", policy.name()))
}

/// The value of mimalloc's `arena_eager_commit` option in force: 0 once
/// the tuning constructor has run, unless the user set
/// `MIMALLOC_ARENA_EAGER_COMMIT`; 2 (mimalloc's default, eager on
/// overcommitting systems) otherwise. Always 0 on other targets than
/// Linux, where brix does not tune mimalloc.
pub fn arena_eager_commit() -> i64 {
    #[cfg(target_os = "linux")]
    {
        // SAFETY: reads a mimalloc option; no preconditions.
        let value = unsafe { brix_alloc_arena_eager_commit() };
        // `c_long` is `i64` here but 32 bits wide on 32-bit targets.
        #[allow(clippy::useless_conversion)]
        i64::from(value)
    }
    #[cfg(not(target_os = "linux"))]
    {
        0
    }
}

/// Fails unless eager arena commit is off or the user asked for it.
#[cfg(target_os = "linux")]
fn check_tuning() -> Result<(), String> {
    let eager = arena_eager_commit();
    // mimalloc matches its variables case-insensitively, and still reads
    // the option's legacy name.
    let user_set = std::env::vars_os().any(|(name, _)| {
        name.to_str().is_some_and(|name| {
            name.eq_ignore_ascii_case("MIMALLOC_ARENA_EAGER_COMMIT")
                || name.eq_ignore_ascii_case("MIMALLOC_EAGER_REGION_COMMIT")
        })
    });
    if eager == 0 || user_set {
        return Ok(());
    }
    Err(format!(
        "mimalloc's arena_eager_commit option is {eager}, not 0: the constructor in brix-alloc/tune.c that turns it off did not run"
    ))
}

#[cfg(not(target_os = "linux"))]
fn check_tuning() -> Result<(), String> {
    Ok(())
}

// Defined in `tune.c`, whose constructor turns off mimalloc's eager
// arena commit before mimalloc reserves its first arena.
//
// With eager commit on (mimalloc's default on 64-bit Linux), a parallel
// parse of nixpkgs spent 36 s of system time committing and purging
// 4.6 GiB of arena memory across 20 threads (wall 3.8 s against 2.0 s
// with glibc malloc); with it off the same job takes 2.0 s and 9 s of
// CPU, and the single-threaded evaluation workloads are unchanged within
// noise. The setting has to be in place before mimalloc reserves its
// first arena, so it is made from a C constructor, before any Rust code
// runs (made from `main` or from the allocator's first call, back when it
// was an environment variable, it was measured to come too late), and not
// from a Rust `#[used]` static (thin LTO discards those).
#[cfg(target_os = "linux")]
extern "C" {
    fn brix_alloc_arena_eager_commit() -> std::os::raw::c_long;
}
