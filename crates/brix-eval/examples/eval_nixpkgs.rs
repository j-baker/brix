//! Evaluates an expression with a directory tree mounted at a path (by
//! default the nixpkgs checkout `$BRIX_NIXPKGS` at its store path),
//! printing the value with `print_ambiguous` and the error with traces
//! otherwise.
//!
//! ```text
//! cargo run --release -p brix-eval --example eval_nixpkgs -- \
//!   "(import $BRIX_NIXPKGS { system = \"x86_64-linux\"; }).hello.drvPath"
//! ```
//!
//! `BRIX_MOUNT=/some/path` mounts another tree instead.

use std::time::Instant;

use brix_eval::{Arena, Evaluator};

#[path = "support/mount_host.rs"]
mod mount_host;

// The allocator the brix binary uses, so timings match it. `BRIX_PREFAULT`
// selects the pre-faulting policy (`off`, `sync`, `background`).
brix_alloc::install!();

fn main() {
    brix_alloc::init().expect("allocator setup");
    let expr = std::env::args()
        .nth(1)
        .expect("usage: eval_nixpkgs <expression>");
    let host = mount_host::MountHost::from_env();
    let arena = Arena::new();
    let ev = Evaluator::new(&arena, mount_host::config_from_env(), &host);
    let start = Instant::now();
    let result = ev.eval_str(expr.as_bytes(), "/").and_then(|v| {
        let cell = ev.alloc_cell(v);
        ev.force_deep(cell)?;
        ev.print_ambiguous(cell)
    });
    match result {
        Ok(text) => {
            println!("{text}");
            eprintln!("({:?})", start.elapsed());
        }
        Err(e) => {
            eprintln!("{}", e.render(true));
            std::process::exit(1);
        }
    }
    // The process ends here: leave the evaluator's memory to the operating
    // system, as the brix binary does (`brix::Teardown::Leak`).
    ev.leak();
    std::mem::forget(arena);
}
