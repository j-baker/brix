//! Checksums: what catches damage to an image that leaves its structure
//! intact.
//!
//! Opening an image validates its structure (the archive, the directory
//! tree), and linking a module validates its code, so damage that breaks
//! either is refused. A flipped bit that leaves a valid image behind (in
//! an integer literal, a string or a name, a directory's NAR hash, a
//! file's size) would instead evaluate to a different value or store
//! path. So every part of the archive the loader reads has an XXH3-64
//! checksum in the *checksum table* that follows the archive, and is
//! compared with it before it is first used:
//!
//! ```text
//! slot         covers                                   compared
//! header       the header, and the length of every      when the image is
//!              table                                    opened
//! dirs[d]      directory d: its NAR hash and size, its  when a lookup first
//!              entries (names, kinds, indices, flags,   reaches it
//!              symlink targets)
//! names[c]     names c * 32 to c * 32 + 31: their end   when one of them is
//!              offsets and bytes                        first interned
//! paths[c]     module paths, in chunks of 32 too        when a module lookup
//!                                                       misses (a hit is
//!                                                       covered by the
//!                                                       module's own slot)
//! modules[m]   module m: its path, record and error,    when it is first
//!              its words, its bytes (line table and     linked
//!              string literals)
//! blobs[b]     blob b: its length, encoding and bytes   when it is first read,
//!                                                       or `stat` first
//!                                                       reports its length
//! ```
//!
//! The preamble has a checksum of its own (see
//! [`super::format::PREAMBLE_LEN`]). Each checksum is taken over a part
//! as the loader reads it out of the archive rather than over a range of
//! the file fixed in advance, so a damaged offset or pointer that leads
//! elsewhere reads different contents and fails like damaged contents;
//! and the writer computes the table with these same functions, from the
//! archive it has just serialised. The bytes no part covers (alignment
//! padding between parts) are never read.
//!
//! What it costs: opening an image adds two checksums of a few dozen
//! bytes and a bitset with one bit per slot. Everything else is paid
//! once per part, for the parts an evaluation uses, as one or two bulk
//! XXH3 passes over contiguous bytes (never an update per item): linking
//! a module hashes its words and bytes, beside linking's per-word
//! decoding; reaching a directory hashes the span of its entries;
//! interning a name hashes its chunk of the name table once. After that
//! a check is one bit test. An image that is not used, and evaluation
//! without images, cost nothing.

use std::sync::atomic::{AtomicU64, Ordering};

use xxhash_rust::xxh3::Xxh3Default;

use super::format::STRINGS_PER_CHUNK;
use super::format::{
    ArchivedBlob, ArchivedDir, ArchivedEntryKind, ArchivedHeader, ArchivedImage, ArchivedModule,
    ArchivedModuleError, ArchivedStringTable,
};
use super::loader::ModuleParts;

/// The number of chunks of a string table of `len` strings.
fn chunks(len: usize) -> usize {
    len.div_ceil(STRINGS_PER_CHUNK)
}

/// The header's slot.
pub(crate) const HEADER: usize = 0;

/// Where each part's checksum is in the checksum table: little-endian
/// `u64` slots, the header's first, then the directories', the name
/// chunks', the path chunks', the modules' and the blobs', each in
/// index order.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Layout {
    pub dirs: usize,
    pub names: usize,
    pub paths: usize,
    pub modules: usize,
    pub blobs: usize,
    /// The number of slots.
    pub len: usize,
}

impl Layout {
    pub fn of(image: &ArchivedImage) -> Layout {
        let dirs = HEADER + 1;
        let names = dirs + image.dirs.len();
        let paths = names + chunks(image.names.len());
        let modules = paths + chunks(image.modules.paths.len());
        let blobs = modules + image.modules.entries.len();
        Layout {
            dirs,
            names,
            paths,
            modules,
            blobs,
            len: blobs + image.blobs.len(),
        }
    }

    /// The checksum table's length in bytes.
    pub fn bytes(&self) -> usize {
        8 * self.len
    }
}

/// The checksum table of `image`, the archive `archive` holds, as the
/// writer appends it.
///
/// A part whose offsets do not lie in the archive has no meaningful
/// checksum and gets 0: no image the writer builds has one (only tests
/// make them, to exercise the loader's range checks), and the loader
/// reports the offsets before it compares checksums.
pub(crate) fn table(image: &ArchivedImage, archive: &[u8]) -> Vec<u8> {
    let layout = Layout::of(image);
    let mut out = Vec::with_capacity(layout.bytes());
    let mut put = |sum: u64| out.extend_from_slice(&sum.to_le_bytes());
    put(header(image));
    for d in 0..image.dirs.len() {
        put(dir(image, archive, d));
    }
    for strings in [&image.names, &image.modules.paths] {
        for c in 0..chunks(strings.len()) {
            put(string_chunk(strings, archive, c).unwrap_or(0));
        }
    }
    for m in 0..image.modules.entries.len() {
        put(super::loader::module_parts(image, m).map_or(0, |parts| module(archive, &parts)));
    }
    for b in image.blobs.iter() {
        put(blob(b));
    }
    debug_assert_eq!(out.len(), layout.bytes());
    out
}

/// The header, and the length of every table: a damaged length is then
/// found when the image is opened.
pub(crate) fn header(image: &ArchivedImage) -> u64 {
    let ArchivedHeader {
        base_symbols,
        nar_hash,
        nar_size,
        full_tree,
    } = &image.header;
    let mut sum = Xxh3Default::new();
    sum.update(&base_symbols.to_native().to_le_bytes());
    sum.update(nar_hash);
    sum.update(&nar_size.to_native().to_le_bytes());
    sum.update(&[u8::from(*full_tree)]);
    for len in [
        image.blobs.len(),
        image.dirs.len(),
        image.names.bytes.len(),
        image.names.ends.len(),
        image.modules.paths.bytes.len(),
        image.modules.paths.ends.len(),
        image.modules.entries.len(),
        image.modules.errors.len(),
        image.code.words.len(),
        image.code.bytes.len(),
    ] {
        sum.update(&(len as u64).to_le_bytes());
    }
    sum.digest()
}

/// Directory `d`, which must be in range: its NAR hash and size, and
/// everything the loader reads of its entries (names, kinds, directory
/// and blob indices, executable flags, symlink targets).
///
/// The entries are hashed in one pass over the bytes they occupy in the
/// archive: the entry array and every name and target it points to.
/// That span is computed from the pointers themselves, so it covers
/// what is read whatever the layout; the writer puts a directory's
/// names and targets (byte vectors, always stored out of line) just
/// before its entry array, which keeps it tight. A file's length is not
/// here: `stat` checks the file's blob, which records it.
pub(crate) fn dir(image: &ArchivedImage, archive: &[u8], d: usize) -> u64 {
    let ArchivedDir {
        entries,
        nar_hash,
        nar_size,
    } = &image.dirs[d];
    let mut extent = match entries.is_empty() {
        true => 0..0,
        false => span_in(archive, entries.as_slice()),
    };
    for entry in entries.iter() {
        widen(&mut extent, archive, entry.name.as_slice());
        if let ArchivedEntryKind::Symlink { target } = &entry.kind {
            widen(&mut extent, archive, target.as_slice());
        }
    }
    let mut record = [0u8; 48];
    record[..32].copy_from_slice(nar_hash);
    record[32..40].copy_from_slice(&nar_size.to_native().to_le_bytes());
    record[40..].copy_from_slice(&(entries.len() as u64).to_le_bytes());
    let mut sum = Xxh3Default::new();
    sum.update(&record);
    sum.update(&archive[extent]);
    sum.digest()
}

/// Widens `extent` to cover `bytes`, which lie in `archive`.
#[inline]
fn widen(extent: &mut std::ops::Range<usize>, archive: &[u8], bytes: &[u8]) {
    if !bytes.is_empty() {
        let span = span_in(archive, bytes);
        extent.start = extent.start.min(span.start);
        extent.end = extent.end.max(span.end);
    }
}

/// Chunk `c` of a string table: its strings' end offsets and their
/// bytes. Fails when the offsets do not lie in the table.
pub(crate) fn string_chunk(
    strings: &ArchivedStringTable,
    archive: &[u8],
    c: usize,
) -> Result<u64, String> {
    let ends = strings.ends.as_slice();
    let first = c * STRINGS_PER_CHUNK;
    let chunk = ends
        .get(first..ends.len().min(first + STRINGS_PER_CHUNK))
        .filter(|chunk| !chunk.is_empty())
        .ok_or("out of range")?;
    let start = match first {
        0 => 0,
        _ => ends[first - 1].to_native() as usize,
    };
    let end = chunk[chunk.len() - 1].to_native() as usize;
    let bytes = strings
        .bytes
        .as_slice()
        .get(start..end)
        .ok_or("out of range")?;
    let mut sum = Xxh3Default::new();
    sum.update(bytes_in(archive, chunk));
    sum.update(bytes);
    Ok(sum.digest())
}

/// A module, its parts resolved by [`super::loader::module_parts`]: its
/// path (so a module lookup that finds it needs no check of the path
/// table), its record, its error, its words and its bytes.
pub(crate) fn module(archive: &[u8], parts: &ModuleParts<'_>) -> u64 {
    let ArchivedModule {
        kind,
        error,
        blob,
        len,
        words_start,
        words_len,
        bytes_start,
        bytes_len,
        lines_len,
    } = parts.record;
    let mut record = [0u8; 36];
    for (field, at) in [
        kind,
        error,
        blob,
        len,
        words_start,
        words_len,
        bytes_start,
        bytes_len,
        lines_len,
    ]
    .into_iter()
    .zip(record.chunks_exact_mut(4))
    {
        at.copy_from_slice(&field.to_native().to_le_bytes());
    }
    let mut sum = Xxh3Default::new();
    sum.update(&record);
    bytes_with_len(&mut sum, parts.path);
    match parts.error {
        None => {}
        Some(ArchivedModuleError::ParseError {
            msg,
            pos,
            defined_at,
        }) => {
            sum.update(&[0]);
            bytes_with_len(&mut sum, msg.as_slice());
            option(&mut sum, pos.as_ref().map(|p| p.to_native()));
            option(&mut sum, defined_at.as_ref().map(|p| p.to_native()));
        }
        Some(ArchivedModuleError::UndefinedVariable { name, pos }) => {
            sum.update(&[1]);
            bytes_with_len(&mut sum, name.as_slice());
            sum.update(&pos.to_native().to_le_bytes());
        }
    }
    sum.update(bytes_in(archive, parts.words));
    sum.update(parts.bytes);
    sum.digest()
}

/// A blob: its length, its encoding and its stored bytes.
pub(crate) fn blob(blob: &ArchivedBlob) -> u64 {
    let ArchivedBlob {
        raw_len,
        encoding,
        data,
    } = blob;
    let mut sum = Xxh3Default::new();
    sum.update(&raw_len.to_native().to_le_bytes());
    sum.update(&[*encoding]);
    sum.update(data.as_slice());
    sum.digest()
}

fn bytes_with_len(sum: &mut Xxh3Default, bytes: &[u8]) {
    sum.update(&(bytes.len() as u64).to_le_bytes());
    sum.update(bytes);
}

fn option(sum: &mut Xxh3Default, value: Option<u32>) {
    let mut record = [0u8; 5];
    if let Some(v) = value {
        record[0] = 1;
        record[1..].copy_from_slice(&v.to_le_bytes());
    }
    sum.update(&record);
}

/// The bytes `slice`, which lies in `archive`, occupies there: its
/// integers as stored (little-endian), without copying them.
pub(crate) fn bytes_in<'a, T>(archive: &'a [u8], slice: &[T]) -> &'a [u8] {
    if slice.is_empty() {
        return &[];
    }
    &archive[span_in(archive, slice)]
}

/// Where `slice`, which lies in `archive`, is in it. An empty slice's
/// position is not meaningful.
fn span_in<T>(archive: &[u8], slice: &[T]) -> std::ops::Range<usize> {
    let start = (slice.as_ptr() as usize)
        .checked_sub(archive.as_ptr() as usize)
        .expect("invariant: archived data lies in the archive it was accessed from");
    let end = start + std::mem::size_of_val(slice);
    assert!(
        end <= archive.len(),
        "invariant: archived data lies in the archive it was accessed from"
    );
    start..end
}

/// Which slots have matched their checksums: one bit each, set once.
/// Shared by every thread using the image; a part checked twice is
/// harmless, so relaxed ordering suffices.
pub(crate) struct Checked(Box<[AtomicU64]>);

impl Checked {
    pub fn new(slots: usize) -> Checked {
        Checked((0..slots.div_ceil(64)).map(|_| AtomicU64::new(0)).collect())
    }

    /// Whether `slot` (in range) has matched.
    #[inline]
    pub fn contains(&self, slot: usize) -> bool {
        self.0[slot / 64].load(Ordering::Relaxed) & (1 << (slot % 64)) != 0
    }

    pub fn insert(&self, slot: usize) {
        self.0[slot / 64].fetch_or(1 << (slot % 64), Ordering::Relaxed);
    }
}
