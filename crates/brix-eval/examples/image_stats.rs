//! Where the bytes of a compiled-input image go.
//!
//! Compiles the tree given on the command line (default: `$BRIX_NIXPKGS`)
//! and serialises each section of the image separately, so the report
//! says how much the modules, the names, the file tree and the blobs
//! cost, how the module words split into tables, and how long opening
//! the image and checking all of its checksums take.
//!
//! ```text
//! cargo run --release -p brix-eval --example image_stats -- [TREE] [--with-tree]
//! ```

use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Instant;

use brix_eval::image::format as fmt;
use brix_eval::image::writer::{build_image, to_bytes, CompileOptions};
use brix_eval::image::Image;
use brix_eval::{Arena, Config, Evaluator, NullHost};
use brix_store::{CanonPath, FsAccessor};

fn archived_len<T>(value: &T) -> usize
where
    T: for<'a> rkyv::Serialize<
        rkyv::rancor::Strategy<
            rkyv::ser::Serializer<
                rkyv::util::AlignedVec,
                rkyv::ser::allocator::ArenaHandle<'a>,
                rkyv::ser::sharing::Share,
            >,
            rkyv::rancor::Error,
        >,
    >,
{
    rkyv::to_bytes::<rkyv::rancor::Error>(value)
        .expect("serialises")
        .len()
}

fn mib(n: usize) -> String {
    format!("{:>8.2} MiB", n as f64 / (1024.0 * 1024.0))
}

fn main() {
    // The tree's path is bytes, which need not be UTF-8.
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    let with_tree = args.iter().any(|a| a == "--with-tree");
    let tree: PathBuf = args
        .iter()
        .find(|a| !a.as_encoded_bytes().starts_with(b"--"))
        .cloned()
        .or_else(|| std::env::var_os("BRIX_NIXPKGS"))
        .expect("pass a tree or set BRIX_NIXPKGS")
        .into();
    let arena = Arena::new();
    let host = NullHost;
    let ev = Evaluator::new(&arena, Config::default(), &host);
    let ctx = ev.image_compile_context().clone();
    let fs = FsAccessor::new(&tree);
    let options = CompileOptions {
        threads: 0,
        include_tree: with_tree,
        notes: true,
    };
    let started = Instant::now();
    let image = build_image(&fs, &CanonPath::root(), &ctx, &options).expect("compiles");
    println!("built in {:?}", started.elapsed());
    let started = Instant::now();
    let bytes = to_bytes(&image, &ctx.fingerprint).expect("serialises");
    println!("serialised in {:?}", started.elapsed());
    println!("total image      {}", mib(bytes.len()));

    let (mut raw, mut absent_raw, mut n_deflate, mut n_raw, mut n_absent) =
        (0usize, 0usize, 0, 0, 0);
    for b in &image.blobs {
        match b.encoding {
            fmt::BLOB_DEFLATE => {
                n_deflate += 1;
                raw += b.raw_len as usize;
            }
            fmt::BLOB_RAW => {
                n_raw += 1;
                raw += b.raw_len as usize;
            }
            _ => {
                n_absent += 1;
                absent_raw += b.raw_len as usize;
            }
        }
    }
    println!(
        "blobs            {}  ({} deflate + {} raw, {} unpacked; {} absent, {} unpacked)",
        mib(archived_len(&image.blobs)),
        n_deflate,
        n_raw,
        mib(raw),
        n_absent,
        mib(absent_raw)
    );
    let files: usize = image
        .dirs
        .iter()
        .map(|d| {
            d.entries
                .iter()
                .filter(|e| matches!(e.kind, fmt::EntryKind::File { .. }))
                .count()
        })
        .sum();
    println!(
        "dirs             {}  ({} dirs, {} file entries)",
        mib(archived_len(&image.dirs)),
        image.dirs.len(),
        files
    );
    // Entry names and symlink targets are byte vectors, stored out of
    // line however short (format version 11; a string of 8 bytes or
    // fewer used to be stored in the entry itself).
    let (mut entries, mut short_names, mut name_bytes, mut target_bytes) = (0, 0, 0, 0);
    for e in image.dirs.iter().flat_map(|d| &d.entries) {
        entries += 1;
        name_bytes += e.name.len();
        if e.name.len() <= 8 {
            short_names += 1;
        }
        if let fmt::EntryKind::Symlink { target } = &e.kind {
            target_bytes += target.len();
        }
    }
    println!(
        "  entries        {entries} ({short_names} names of 8 bytes or fewer), names {}, \
         symlink targets {}",
        mib(name_bytes),
        mib(target_bytes)
    );
    println!(
        "names            {}  ({} distinct names)",
        mib(archived_len(&image.names)),
        image.names.len()
    );
    let entries = &image.modules.entries;
    let n_code = entries
        .iter()
        .filter(|m| m.kind == fmt::MODULE_CODE)
        .count();
    println!(
        "module table     {}  ({} modules: {} compiled, {} errors, {} unsupported)",
        mib(archived_len(&image.modules)),
        entries.len(),
        n_code,
        entries
            .iter()
            .filter(|m| m.kind == fmt::MODULE_ERROR)
            .count(),
        entries
            .iter()
            .filter(|m| m.kind == fmt::MODULE_UNSUPPORTED)
            .count(),
    );
    let lines: usize = entries.iter().map(|m| m.lines_len as usize).sum();
    println!(
        "code words       {}  ({} words)",
        mib(image.code.words.len() * 4),
        image.code.words.len()
    );
    println!(
        "code bytes       {}  ({} line tables, {} string literals)",
        mib(image.code.bytes.len()),
        mib(lines),
        mib(image.code.bytes.len() - lines)
    );
    let mut sections: Vec<(&str, usize)> = Vec::new();
    for m in entries.iter().filter(|m| m.kind == fmt::MODULE_CODE) {
        let start = m.words_start as usize;
        let words = &image.code.words[start..start + m.words_len as usize];
        for (k, (name, len)) in brix_eval::image::codec::section_sizes(words)
            .expect("well-formed")
            .into_iter()
            .enumerate()
        {
            if sections.len() <= k {
                sections.push((name, 0));
            }
            sections[k].1 += len;
        }
    }
    for (name, words) in &sections {
        println!("  {name:<14} {}", mib(words * 4));
    }

    let started = Instant::now();
    let opened = Image::open(bytes, Some(&ctx.fingerprint)).expect("opens");
    println!(
        "opened in {:?}: {} modules, full tree: {}",
        started.elapsed(),
        opened.module_count(),
        opened.has_full_tree()
    );
    // What checking every checksum at once costs: an upper bound on what
    // an evaluation pays, which checks only the parts it uses.
    let started = Instant::now();
    opened.verify().expect("an image just compiled verifies");
    println!("verified every checksum in {:?}", started.elapsed());
}
