//! Counters describing what an evaluation did, printed when
//! [`crate::Config::timings`] is set (`BRIX_TIMINGS=1` in the brix CLI).
//!
//! Per-file and per-phase figures (files parsed and how long that took,
//! image modules, store additions) are always collected, the times with
//! [`crate::platform::Instant`] (zero on targets without a clock).
//! Per-operation work counters (thunks, environments, calls, attribute
//! sets and the histograms) are updated millions of times per nixpkgs
//! evaluation and measurably slowed it (2-4%), so they are compiled in
//! only with the `work-counters` cargo feature (implied by
//! `profiling`).

use crate::runtime::cell::Cell;
use std::time::Duration;

#[derive(Default)]
pub struct Stats {
    /// Background sparks (`parallel::sparks`): run, run to completion, and
    /// stopped early (an ordered effect, an error, a busy thunk, cancellation).
    #[cfg(brix_parallel)]
    pub sparks_started: Cell<u64>,
    #[cfg(brix_parallel)]
    pub sparks_completed: Cell<u64>,
    #[cfg(brix_parallel)]
    pub sparks_deferred: Cell<u64>,
    /// Sparks whose value was already evaluated, or being evaluated, when a
    /// worker took them, and sparks put back after giving way at a busy thunk.
    #[cfg(brix_parallel)]
    pub sparks_stale: Cell<u64>,
    #[cfg(brix_parallel)]
    pub sparks_retried: Cell<u64>,
    #[cfg(feature = "work-counters")]
    pub(crate) derivation_probe_types: [Cell<u64>; 7],
    #[cfg(feature = "work-counters")]
    pub(crate) derivation_probe_shapes: [Cell<u64>; 2],
    #[cfg(feature = "work-counters")]
    pub(crate) derivation_probe_sizes: Histogram,
    #[cfg(feature = "work-counters")]
    pub(crate) derivation_probe_reuse: [Cell<u64>; 2],
    #[cfg(feature = "work-counters")]
    pub(crate) derivation_cache_hits: Cell<u64>,
    #[cfg(feature = "work-counters")]
    pub(crate) attribute_probe_total: Cell<u64>,
    #[cfg(feature = "work-counters")]
    pub(crate) attribute_probe_missing: Cell<u64>,
    #[cfg(feature = "work-counters")]
    pub(crate) attribute_probe_shapes: [Cell<u64>; 2],
    #[cfg(feature = "work-counters")]
    pub(crate) attribute_probe_reuse: [Cell<u64>; 2],
    /// Static selection, general selection, and hasAttr: total, 4K hits, 16K hits.
    #[cfg(feature = "work-counters")]
    pub(crate) attribute_probe_sites: [[Cell<u64>; 3]; 3],
    /// Files parsed (`import`, `evalFile`); the import cache makes this the
    /// number of distinct files.
    pub files_parsed: Cell<u64>,
    /// Bytes of source handed to the parser.
    pub source_bytes: Cell<u64>,
    pub parse_time: Cell<Duration>,
    /// Compiling (scope resolution) and linking into the arena.
    pub compile_time: Cell<Duration>,
    /// Thunks allocated (`mk_thunk`), as opposed to cells that
    /// `maybe_thunk` could share.
    pub thunks: Cell<u64>,
    /// Thunks actually forced.
    pub thunk_forces: Cell<u64>,
    /// Environment frames allocated.
    pub envs: Cell<u64>,
    /// Attribute sets built (not counting the shared empty set).
    pub attr_sets: Cell<u64>,
    /// Attributes in those sets.
    pub attrs: Cell<u64>,
    /// Lists built.
    pub lists: Cell<u64>,
    /// Lambda calls (one per argument applied).
    pub lambda_calls: Cell<u64>,
    /// Primop calls.
    pub primop_calls: Cell<u64>,
    /// Strings allocated.
    pub strings: Cell<u64>,
    /// Paths added to the virtual store (NAR hashing).
    pub store_adds: Cell<u64>,
    pub store_time: Cell<Duration>,
    /// `copyPathToStore` calls answered from the source-path cache
    /// (`srcToStore` in Nix) without hashing.
    pub store_cache_hits: Cell<u64>,
    /// Formal parameters matched against argument sets, over all calls.
    pub formal_lookups: Cell<u64>,
    /// Files linked from compiled-input images instead of being parsed,
    /// and the time spent linking them (also counted in `compile_time`).
    pub image_modules: Cell<u64>,
    pub image_load_time: Cell<Duration>,
    /// Time spent decoding modules' tables and linking their names (part
    /// of `image_load_time`).
    pub image_decode_time: Cell<Duration>,
    /// Closures compiled from IR nodes (`crate::closure`), their captured
    /// data in bytes, and the time spent building them.
    pub closures_built: Cell<u64>,
    pub closure_bytes: Cell<u64>,
    pub closure_build_time: Cell<Duration>,
    /// Sizes of attribute sets at construction, bucketed by
    /// `Histogram::bucket`.
    pub attr_set_sizes: Histogram,
    /// Sizes of the left and right operands of `//` merges, and the
    /// attributes copied by those merges in total.
    pub update_left_sizes: Histogram,
    pub update_right_sizes: Histogram,
    pub update_attrs_copied: Cell<u64>,
    /// Thunks created, by the kind of expression suspended.
    pub thunk_kinds: [Cell<u64>; ThunkKind::COUNT],
    /// Thunks created, by the lazy position they were created for.
    pub thunk_sites: [Cell<u64>; LazySite::COUNT],
}

/// The kind of expression a thunk suspends. `maybe_thunk` already avoids
/// thunks for literals and for variables whose cell exists, so those
/// kinds never appear; the rest shows where laziness is being paid for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(usize)]
pub enum ThunkKind {
    /// A variable resolved through `with` (Nix never shares those cells).
    WithVar,
    /// A variable whose slot is not filled in yet (`rec`, `let`).
    UninitVar,
    InheritFrom,
    Select,
    HasAttr,
    Attrs,
    List,
    Lambda,
    Call,
    Let,
    With,
    If,
    Assert,
    Not,
    BinOp,
    ConcatStrings,
    CurPos,
    Other,
}

impl ThunkKind {
    pub const COUNT: usize = 18;

    /// The kind of thunk suspending `node`.
    pub(crate) fn of(node: &crate::ir::Node) -> ThunkKind {
        use crate::ir::{Node, VarRef};
        match *node {
            Node::Var {
                var: VarRef::With { .. },
                ..
            } => ThunkKind::WithVar,
            Node::Var { .. } => ThunkKind::UninitVar,
            Node::InheritFrom { .. } => ThunkKind::InheritFrom,
            Node::Select { .. } => ThunkKind::Select,
            Node::HasAttr { .. } => ThunkKind::HasAttr,
            Node::Attrs(_) => ThunkKind::Attrs,
            Node::List { .. } => ThunkKind::List,
            Node::Lambda(_) => ThunkKind::Lambda,
            Node::Call { .. } => ThunkKind::Call,
            Node::Let { .. } => ThunkKind::Let,
            Node::With { .. } => ThunkKind::With,
            Node::If { .. } => ThunkKind::If,
            Node::Assert { .. } => ThunkKind::Assert,
            Node::Not(_) => ThunkKind::Not,
            Node::BinOp { .. } => ThunkKind::BinOp,
            Node::ConcatStrings { .. } => ThunkKind::ConcatStrings,
            Node::CurPos(_) => ThunkKind::CurPos,
            Node::Int(_) | Node::Float(_) | Node::Str(_) | Node::Path { .. } => ThunkKind::Other,
        }
    }
    const LABELS: [&'static str; Self::COUNT] = [
        "with-var",
        "uninit-var",
        "inherit-from",
        "select",
        "has-attr",
        "attrs",
        "list",
        "lambda",
        "call",
        "let",
        "with",
        "if",
        "assert",
        "not",
        "binop",
        "concat-strings",
        "cur-pos",
        "other",
    ];
}

/// The lazy position a thunk is created for (where Nix calls
/// `maybeThunk`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(usize)]
pub enum LazySite {
    /// A static attribute's value in `{ … }`, `rec { … }`.
    AttrValue,
    /// An attribute of a `rec` set with `__overrides` (always a thunk).
    RecOverrideValue,
    /// ` = value;`
    DynAttrValue,
    ListElem,
    CallArg,
    LetBinding,
    /// The set of a `with` expression.
    WithSet,
    /// The source of `inherit (source) …;`.
    InheritFromSource,
    /// A formal's default when the argument is absent.
    FormalDefault,
}

impl LazySite {
    pub const COUNT: usize = 9;
    const LABELS: [&'static str; Self::COUNT] = [
        "attr-value",
        "rec-override-value",
        "dyn-attr-value",
        "list-elem",
        "call-arg",
        "let-binding",
        "with-set",
        "inherit-from-source",
        "formal-default",
    ];
}

/// Powers-of-two buckets: `0`, `1`, `2`, `3-4`, `5-8`, … up to `>32768`.
#[derive(Default)]
pub struct Histogram(pub [Cell<u64>; Histogram::BUCKETS]);

impl Histogram {
    pub const BUCKETS: usize = 18;

    /// The bucket for a size: 0 → 0, 1 → 1, 2 → 2, then one bucket per
    /// power of two.
    pub fn bucket(n: usize) -> usize {
        match n {
            0 => 0,
            1 => 1,
            2 => 2,
            _ => (usize::BITS - (n - 1).leading_zeros()) as usize + 1,
        }
        .min(Self::BUCKETS - 1)
    }

    #[inline]
    pub(crate) fn record(&self, n: usize) {
        if cfg!(feature = "work-counters") {
            let cell = &self.0[Self::bucket(n)];
            Stats::bump(cell);
        }
    }

    fn label(bucket: usize) -> String {
        match bucket {
            0 => "0".to_owned(),
            1 => "1".to_owned(),
            2 => "2".to_owned(),
            b if b == Self::BUCKETS - 1 => format!(">{}", 1usize << (b - 2)),
            b => format!("{}-{}", (1usize << (b - 2)) + 1, 1usize << (b - 1)),
        }
    }

    /// One line: `label:count` for every non-empty bucket.
    pub fn render(&self) -> String {
        self.0
            .iter()
            .enumerate()
            .filter(|(_, c)| c.get() > 0)
            .map(|(b, c)| format!("{}:{}", Self::label(b), c.get()))
            .collect::<Vec<_>>()
            .join(" ")
    }
}

impl Stats {
    /// Per-file and per-phase counters: always collected.
    #[inline]
    pub(crate) fn bump(counter: &Cell<u64>) {
        Self::add(counter, 1);
    }

    #[inline]
    pub(crate) fn add(counter: &Cell<u64>, n: u64) {
        #[cfg(not(brix_parallel))]
        counter.set(counter.get() + n);
        #[cfg(brix_parallel)]
        {
            counter.fetch_add(n);
        }
    }

    /// Per-operation counters: compiled out unless the `work-counters`
    /// feature is enabled.
    #[inline]
    pub(crate) fn bump_work(counter: &Cell<u64>) {
        if cfg!(feature = "work-counters") {
            Self::add(counter, 1);
        }
    }

    #[inline]
    pub(crate) fn add_work(counter: &Cell<u64>, n: u64) {
        if cfg!(feature = "work-counters") {
            Self::add(counter, n);
        }
    }

    /// Records a thunk creation in both histograms.
    #[inline]
    pub(crate) fn record_thunk(&self, kind: ThunkKind, site: LazySite) {
        Self::bump_work(&self.thunk_kinds[kind as usize]);
        Self::bump_work(&self.thunk_sites[site as usize]);
    }

    fn render_counts(labels: &[&str], counts: &[Cell<u64>]) -> String {
        labels
            .iter()
            .zip(counts)
            .filter(|(_, c)| c.get() > 0)
            .map(|(l, c)| format!("{l}:{}", c.get()))
            .collect::<Vec<_>>()
            .join(" ")
    }

    #[inline]
    pub(crate) fn add_time(counter: &Cell<Duration>, d: Duration) {
        #[cfg(not(brix_parallel))]
        counter.set(counter.get() + d);
        #[cfg(brix_parallel)]
        counter.update(|previous| previous + d);
    }

    /// A multi-line human-readable summary.
    pub fn render(&self) -> String {
        let phases = format!(
            "files parsed: {} ({:.1} MiB) in {:?}; compiled+linked in {:?}; from images: {} in {:?} (decode {:?}); store adds: {} in {:?} ({} cached)\n\
             closures: {} built ({:.1} MiB captured) in {:?}",
            self.files_parsed.get(),
            self.source_bytes.get() as f64 / (1024.0 * 1024.0),
            self.parse_time.get(),
            self.compile_time.get(),
            self.image_modules.get(),
            self.image_load_time.get(),
            self.image_decode_time.get(),
            self.store_adds.get(),
            self.store_time.get(),
            self.store_cache_hits.get(),
            self.closures_built.get(),
            self.closure_bytes.get() as f64 / (1024.0 * 1024.0),
            self.closure_build_time.get(),
        );
        let phases = if crate::platform::HAS_CLOCK {
            phases
        } else {
            format!("{phases}\ntimes: this target has no clock, so every time above is 0")
        };
        #[cfg(brix_parallel)]
        let phases = format!(
            "{phases}\nbackground sparks: {} started, {} completed, {} deferred, {} retried, {} stale{}",
            self.sparks_started.get(),
            self.sparks_completed.get(),
            self.sparks_deferred.get(),
            self.sparks_retried.get(),
            self.sparks_stale.get(),
            crate::runtime::deferrals(),
        );
        #[cfg(feature = "work-counters")]
        let phases = format!(
            "{phases}\nequality derivation probes: missing={} nonstring={} derivation={} other_string={} lazy_hash={} suspended={} failed={} flat={} tree={} reuse_4k={} reuse_64k={} cache_hits={}\nprobe sizes: {}",
            self.derivation_probe_types[0].get(), self.derivation_probe_types[1].get(),
            self.derivation_probe_types[2].get(), self.derivation_probe_types[3].get(),
            self.derivation_probe_types[4].get(), self.derivation_probe_types[5].get(),
            self.derivation_probe_types[6].get(), self.derivation_probe_shapes[0].get(),
            self.derivation_probe_shapes[1].get(), self.derivation_probe_reuse[0].get(),
            self.derivation_probe_reuse[1].get(), self.derivation_cache_hits.get(), self.derivation_probe_sizes.render(),
        );
        #[cfg(feature = "work-counters")]
        let phases = format!(
            "{phases}\nattribute probes: total={} missing={} flat={} tree={} reuse_4k={} reuse_16k={}",
            self.attribute_probe_total.get(), self.attribute_probe_missing.get(),
            self.attribute_probe_shapes[0].get(), self.attribute_probe_shapes[1].get(),
            self.attribute_probe_reuse[0].get(), self.attribute_probe_reuse[1].get(),
        );
        #[cfg(feature = "work-counters")]
        let phases = format!(
            "{phases}\nattribute probe sites (total/4k/16k): static={}/{}/{} select={}/{}/{} has={}/{}/{}",
            self.attribute_probe_sites[0][0].get(), self.attribute_probe_sites[0][1].get(), self.attribute_probe_sites[0][2].get(),
            self.attribute_probe_sites[1][0].get(), self.attribute_probe_sites[1][1].get(), self.attribute_probe_sites[1][2].get(),
            self.attribute_probe_sites[2][0].get(), self.attribute_probe_sites[2][1].get(), self.attribute_probe_sites[2][2].get(),
        );
        if !cfg!(feature = "work-counters") {
            return format!(
                "{phases}\nwork counters: not compiled in (build with the brix-eval feature `work-counters`)"
            );
        }
        format!(
            "{phases}\n\
             thunks: {} created, {} forced; envs: {}; lambda calls: {}; primop calls: {}\n\
             attr sets: {} ({} attrs); lists: {}; strings: {}; formal lookups: {}\n\
             attr set sizes: {}\n\
             // left sizes: {}\n\
             // right sizes: {}\n\
             // attrs copied: {}\n\
             thunks by kind: {}\n\
             thunks by site: {}",
            self.thunks.get(),
            self.thunk_forces.get(),
            self.envs.get(),
            self.lambda_calls.get(),
            self.primop_calls.get(),
            self.attr_sets.get(),
            self.attrs.get(),
            self.lists.get(),
            self.strings.get(),
            self.formal_lookups.get(),
            self.attr_set_sizes.render(),
            self.update_left_sizes.render(),
            self.update_right_sizes.render(),
            self.update_attrs_copied.get(),
            Self::render_counts(&ThunkKind::LABELS, &self.thunk_kinds),
            Self::render_counts(&LazySite::LABELS, &self.thunk_sites),
        )
    }
}
