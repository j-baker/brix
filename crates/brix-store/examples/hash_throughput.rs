//! Measures SHA-256 throughput through `brix_store::Hash::of` on a
//! synthetic buffer, to confirm the hardware (SHA-NI) code path is in use:
//! `cargo run --release -p brix-store --example hash_throughput`.
//! Expect well over 1 GB/s on a CPU with SHA extensions; a few hundred
//! MB/s means the software fallback.

use std::time::Instant;

use brix_store::{Hash, HashAlgo};

fn main() {
    let mib: usize = std::env::args()
        .nth(1)
        .map(|s| s.parse().expect("size in MiB"))
        .unwrap_or(256);
    // Pseudo-random so nothing can shortcut on repetition.
    let mut buf = vec![0u8; mib << 20];
    let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
    for chunk in buf.chunks_mut(8) {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        let b = x.to_le_bytes();
        chunk.copy_from_slice(&b[..chunk.len()]);
    }
    let mut best = f64::MAX;
    let mut digest = None;
    for _ in 0..5 {
        let start = Instant::now();
        let h = Hash::of(HashAlgo::Sha256, &buf);
        let secs = start.elapsed().as_secs_f64();
        best = best.min(secs);
        digest = Some(h);
    }
    let gbps = (mib as f64 / 1024.0) / best;
    println!(
        "sha256 of {mib} MiB: best {:.0} ms = {gbps:.2} GiB/s ({})",
        best * 1000.0,
        digest
            .expect("one run")
            .to_string(brix_store::HashFormat::Base16, true)
    );
}
