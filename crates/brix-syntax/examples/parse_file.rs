//! Parses one file repeatedly and reports the time per parse, for comparing
//! against `nix-instantiate --parse`.
//!
//! ```text
//! cargo run --release --example parse_file -- FILE [ITERATIONS]
//! ```

use std::time::Instant;

use brix_syntax::{parse, show_bytes, Origin, ParseOptions};

fn main() {
    let mut args = std::env::args().skip(1);
    let file = args.next().expect("usage: parse_file FILE [ITERATIONS]");
    let iterations: u32 = args.next().map_or(20, |n| n.parse().expect("iterations"));
    let src = std::fs::read(&file).expect("read file");
    let dir = std::path::Path::new(&file)
        .parent()
        .expect("file has a directory")
        .to_string_lossy()
        .into_owned();
    let origin = Origin::Path(file.clone().into());
    let options = ParseOptions::default();

    let start = Instant::now();
    let mut expr = None;
    let mut warnings = Vec::new();
    for _ in 0..iterations {
        warnings.clear();
        expr = Some(
            parse(&src, &dir, &options, &mut warnings).unwrap_or_else(|e| {
                eprint!("{}", String::from_utf8_lossy(&e.render(&origin, &src)));
                std::process::exit(1)
            }),
        );
    }
    for w in &warnings {
        eprintln!("{}", String::from_utf8_lossy(&w.render(&origin, &src)));
    }
    let per_parse = start.elapsed() / iterations;
    let expr = expr.expect("at least one iteration");

    let start = Instant::now();
    let shown = show_bytes(&expr);
    let show_time = start.elapsed();
    println!(
        "{}: {} bytes, parse {:?} ({:.1} MB/s), show {:?} ({} bytes)",
        file,
        src.len(),
        per_parse,
        src.len() as f64 / per_parse.as_secs_f64() / 1e6,
        show_time,
        shown.len()
    );
}
