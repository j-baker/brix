//! Lexes and parses every `.nix` file in nixpkgs.
//!
//! Needs `BRIX_NIXPKGS` (a nixpkgs checkout). Reports the whole-tree time;
//! files per second and MB/s are printed once before the measurement.

use std::fs;
use std::path::PathBuf;

use criterion::{criterion_group, criterion_main, Criterion, Throughput};

use brix_syntax::{parse, ParseOptions};

fn load_nixpkgs() -> Vec<(String, Vec<u8>)> {
    let root =
        std::env::var("BRIX_NIXPKGS").expect("BRIX_NIXPKGS must point at a nixpkgs checkout");
    let mut files = Vec::new();
    for entry in walkdir::WalkDir::new(&root) {
        let entry = entry.expect("walk nixpkgs");
        if entry.file_type().is_file() && entry.path().extension().is_some_and(|e| e == "nix") {
            let path: PathBuf = entry.into_path();
            let dir = path
                .parent()
                .expect("file has a parent")
                .to_string_lossy()
                .into_owned();
            files.push((dir, fs::read(&path).expect("read file")));
        }
    }
    files
}

fn bench_parse(c: &mut Criterion) {
    let files = load_nixpkgs();
    let bytes: usize = files.iter().map(|(_, src)| src.len()).sum();
    println!(
        "nixpkgs: {} files, {:.1} MB",
        files.len(),
        bytes as f64 / 1e6
    );
    let options = ParseOptions::default();
    let mut group = c.benchmark_group("nixpkgs");
    group.sample_size(10);
    group.throughput(Throughput::Bytes(bytes as u64));
    group.bench_function("parse", |b| {
        b.iter(|| {
            let mut ok = 0usize;
            for (dir, src) in &files {
                let mut warnings = Vec::new();
                if parse(src, dir, &options, &mut warnings).is_ok() {
                    ok += 1;
                }
            }
            ok
        })
    });
    group.finish();
}

criterion_group!(benches, bench_parse);
criterion_main!(benches);
