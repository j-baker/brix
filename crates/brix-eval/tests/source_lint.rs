//! Source-level containment checks for the managed heap's trust boundaries.
//!
//! The compiler already walls raw heap pointers inside the `managed_heap`
//! module subtree (the `heap_layout` module is a private child), and the
//! `identity` submodule is the only code that can read an address out of an
//! `Identity`. Two obligations cannot be expressed as visibility and are
//! checked here instead, so that a new call site fails CI with an
//! explanation rather than silently widening the audited surface:
//!
//! - Every `write_barrier` call must stay inside the audited publish/mutate
//!   funnels. A barriered write added anywhere else must be reviewed for the
//!   old-to-young edge it records (or fails to record) and then added to the
//!   allowlist together with that review.
//! - Heap addresses may become integers (`as usize` on a pointer, `.addr()`,
//!   `expose_provenance`) only in the relocation-following weak sets and the
//!   epoch-gated identity module. Anywhere else, an integer that outlives a
//!   collection is a use-after-move waiting for address reuse.
//!
//! The allowlists are exact file sets, not counts, so moving a function
//! between files is caught and renaming within a file is not. Update an
//! allowlist only together with the review described above.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

/// Files allowed to call `RawHeap::write_barrier` (via any receiver).
const BARRIER_FILES: &[&str] = &["src/managed_heap.rs", "src/parallel/heap.rs"];

/// Files allowed to turn a heap address into an integer.
const ADDRESS_FILES: &[&str] = &[
    "src/managed_heap.rs",           // mark_visited/mark_seen feeding WeakSeen
    "src/managed_heap/weak_seen.rs", // relocation-following weak address sets
    "src/managed_heap/identity.rs",  // epoch-gated AddressTag minting
    "src/parallel/heap.rs",          // promotion repair set over .addr()
    "src/heap_layout.rs",            // tests assert relocation by address
    "src/managed_language.rs",       // primop IDs keyed by permanent arena addresses
];

/// The moving-heap subtree. The legacy arena evaluator (eval.rs, print.rs,
/// bindings.rs, ...) also derives integers from addresses, legitimately: its
/// bump arena never moves or reuses an allocation within a session. Only code
/// that touches the relocating heap is scanned.
fn in_moving_heap(relative: &str) -> bool {
    relative.starts_with("src/managed_")
        || relative.starts_with("src/parallel/")
        || relative == "src/heap_layout.rs"
}

/// Patterns that count as deriving an integer from a pointer. Kept simple and
/// over-broad on purpose: a false positive is a one-line allowlist review; a
/// false negative is a latent use-after-move.
const ADDRESS_PATTERNS: &[&str] = &[
    "as_ptr() as usize",
    "as *const _ as usize",
    "as *mut _ as usize",
    ".addr()",
    "expose_provenance",
];

fn src_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
}

fn rust_sources(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).expect("source tree is readable") {
        let path = entry.expect("source tree is readable").path();
        if path.is_dir() {
            rust_sources(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// Strip line comments so documentation may mention the patterns freely.
/// String literals and block comments are rare enough in this crate's source
/// that the line-based scan stays honest; a match inside one still only asks
/// for a review, never hides one.
fn code_lines(source: &str) -> impl Iterator<Item = (usize, &str)> {
    source.lines().enumerate().map(|(index, line)| {
        let code = line.split("//").next().unwrap_or(line);
        (index + 1, code)
    })
}

fn scan(patterns: &[&str], allowed: &[&str]) -> (BTreeSet<String>, Vec<String>) {
    let root = src_root();
    let mut files = Vec::new();
    rust_sources(&root, &mut files);
    let mut hits = BTreeSet::new();
    let mut violations = Vec::new();
    for path in files {
        let relative = format!(
            "src/{}",
            path.strip_prefix(&root)
                .unwrap()
                .display()
                .to_string()
                .replace('\\', "/")
        );
        if !in_moving_heap(&relative) {
            continue;
        }
        let source = fs::read_to_string(&path).expect("source file is readable");
        for (line, code) in code_lines(&source) {
            if patterns.iter().any(|pattern| code.contains(pattern)) {
                hits.insert(relative.clone());
                if !allowed.contains(&relative.as_str()) {
                    violations.push(format!("{relative}:{line}: {}", code.trim()));
                }
            }
        }
    }
    (hits, violations)
}

#[test]
fn write_barriers_stay_in_the_audited_funnels() {
    let (hits, violations) = scan(&["write_barrier"], BARRIER_FILES);
    assert!(
        violations.is_empty(),
        "write_barrier outside the audited funnels; review the old-to-young \
         edge it must record, then extend BARRIER_FILES:\n{}",
        violations.join("\n")
    );
    // The allowlist must not go stale: every allowed file still barriers.
    for file in BARRIER_FILES {
        assert!(
            hits.contains(*file),
            "{file} no longer calls write_barrier; shrink BARRIER_FILES"
        );
    }
}

#[test]
fn heap_addresses_become_integers_only_in_blessed_modules() {
    let (_, violations) = scan(ADDRESS_PATTERNS, ADDRESS_FILES);
    assert!(
        violations.is_empty(),
        "address-derived integer outside the weak sets and the epoch-gated \
         identity module; such an integer is stale after any collection. \
         Route it through Identity/AddressTag or WeakSeen instead:\n{}",
        violations.join("\n")
    );
}
