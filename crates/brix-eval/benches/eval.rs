//! Evaluation benchmarks. Run with `cargo bench -p brix-eval`.
//!
//! The nixpkgs benchmark evaluates the checkout `$BRIX_NIXPKGS` points at
//! (the devshell sets it). For comparison, the reference is
//! `nix eval --raw --expr '(import (builtins.fetchTree { … }) { system =
//! "x86_64-linux"; }).hello.drvPath'`.

use std::path::Path;
use std::sync::Arc;

use brix_eval::{Arena, Config, Evaluator, Host, NullHost};
use brix_store::{CanonPath, FsAccessor, MemoryAccessor, MountedAccessor, SourceAccessor};
use criterion::{criterion_group, criterion_main, Criterion};

fn eval_with(host: &dyn Host, src: &str) {
    let arena = Arena::new();
    let ev = Evaluator::new(&arena, Config::default(), host);
    let v = ev
        .eval_str(src.as_bytes(), "/")
        .expect("benchmark expression evaluates");
    let cell = ev.alloc_cell(v);
    ev.force_deep(cell).expect("benchmark value forces");
}

fn eval(src: &str) {
    eval_with(&NullHost, src)
}

fn synthetic(c: &mut Criterion) {
    c.bench_function("deep recursion", |b| {
        b.iter(|| eval("let f = n: if n == 0 then 0 else 1 + f (n - 1); in f 5000"))
    });
    c.bench_function("attrset merges", |b| {
        b.iter(|| {
            eval(
                "builtins.foldl' (acc: i: acc // { \"k${toString i}\" = i; }) {} (builtins.genList (i: i) 2000)",
            )
        })
    });
    c.bench_function("string building", |b| {
        b.iter(|| eval("builtins.concatStringsSep \",\" (builtins.genList (i: \"item-${toString i}\") 20000)"))
    });
    c.bench_function("list sort", |b| {
        b.iter(|| eval("builtins.sort builtins.lessThan (builtins.genList (i: (i * 7919) - (i * 7919 / 1000) * 1000) 20000)"))
    });
}

/// A host whose root file system has the nixpkgs checkout at its path.
struct NixpkgsHost {
    root: Arc<dyn SourceAccessor>,
    nixpkgs: String,
}

impl Host for NixpkgsHost {
    fn root_accessor(&self) -> Arc<dyn SourceAccessor> {
        self.root.clone()
    }

    fn allowed_paths(&self) -> Vec<CanonPath> {
        vec![CanonPath::new(&self.nixpkgs).expect("the nixpkgs checkout is an absolute path")]
    }
}

fn nixpkgs(c: &mut Criterion) {
    let nixpkgs = std::env::var("BRIX_NIXPKGS").expect(
        "BRIX_NIXPKGS must point at the nixpkgs checkout to benchmark (run inside `nix develop`)",
    );
    assert!(
        Path::new(&nixpkgs).exists(),
        "nixpkgs checkout {nixpkgs} (from BRIX_NIXPKGS) is not present"
    );
    let mut mounted = MountedAccessor::new(Arc::new(MemoryAccessor::new()));
    mounted.mount(
        CanonPath::new(&nixpkgs).expect("BRIX_NIXPKGS is an absolute path"),
        Arc::new(FsAccessor::new(&nixpkgs)),
    );
    let host = NixpkgsHost {
        root: Arc::new(mounted),
        nixpkgs: nixpkgs.clone(),
    };
    let mut group = c.benchmark_group("nixpkgs");
    group.sample_size(10);
    group.bench_function("hello.drvPath", |b| {
        b.iter(|| {
            eval_with(
                &host,
                &format!("(import {nixpkgs} {{ system = \"x86_64-linux\"; }}).hello.drvPath"),
            )
        })
    });
    group.bench_function("lib.version", |b| {
        b.iter(|| {
            eval_with(
                &host,
                &format!("(import {nixpkgs} {{ system = \"x86_64-linux\"; }}).lib.version"),
            )
        })
    });
    group.finish();
}

criterion_group!(benches, synthetic, nixpkgs);
criterion_main!(benches);
