//! The on-disk layout of a compiled flake-input image.
//!
//! An image file is a fixed [`Preamble`], one `rkyv` archive whose root
//! is [`Image`], and the checksums of the archive's parts:
//!
//! ```text
//! preamble        magic, format version, fingerprint, the archive's
//!                 length, and a checksum of all that (64 bytes, read
//!                 before anything else so a stale image is rejected
//!                 without looking further)
//! Image (rkyv)
//! ├── header      base symbol count, tree NAR hash, whether the tree's
//! │               contents are stored
//! ├── blobs       file contents, deduplicated by SHA-256, each stored
//! │               raw or deflate-compressed, or absent (see Blob)
//! ├── dirs        directories: sorted entries; dir 0 is the tree's root
//! ├── names       every symbol name the modules create, once, as one
//! │               byte string plus end offsets
//! ├── modules     every `*.nix` file, sorted by path: fixed-size records
//! │               pointing into the areas below, plus the errors
//! │               importing a broken file raises
//! └── code        the modules' compiled IR as fixed-width 32-bit words
//!                 (see `codec`), and their string literals and line
//!                 tables as bytes
//! checksums       one XXH3 checksum per part of the archive, each
//!                 checked when the part is first used (see `integrity`)
//! ```
//!
//! The archive is validated once when the image is opened (`rkyv::access`),
//! which for everything but the directory tree is a handful of range
//! checks: the large tables are flat vectors of integers or bytes. A
//! directory is checked to be part of a tree with valid, sorted names
//! when a lookup first reaches it (see `loader`); a module's own words
//! are validated when it is linked, and only then; linking reads them
//! in place from the mapped file and turns them into the evaluator's
//! tables with plain loads and stores (see `codec`).
//! Damage that leaves the structure intact (a flipped bit in an integer
//! literal, a name, a directory's NAR hash) is caught by the checksums.
//!
//! Each directory records the SHA-256 NAR hash and NAR size of its
//! subtree, so `src = ./.` and `builtins.path` on any directory of the
//! input are a lookup rather than a traversal.
//!
//! By default no file contents are stored at all: the compiled IR stands
//! in for the `.nix` sources (each module keeps its line table, so
//! positions resolve without the text), evaluation reads other files
//! rarely, and the input's source tree is normally at hand to serve
//! anything that is needed, error excerpts included (the image accessor
//! falls back to it). An image compiled with the whole tree is
//! self-contained.
//!
//! Compiled IR carries two kinds of symbol reference. Symbols the
//! evaluator creates while registering its builtins (the first
//! [`Header::base_symbols`] of its table) are referenced by their id,
//! because every evaluator with the same fingerprint creates them in the
//! same order. Every other name a module uses is listed by the module in
//! the order the compiler first met it (as indices into [`Image::names`])
//! and referenced as `base_symbols + index`. Interning that list in order
//! at load time creates exactly the symbols that compiling the source
//! would have created, in the same order, which keeps symbol-id order
//! (observable in Nix through `rec` evaluation order) identical.
//!
//! Path literals are compiled relative to a marker root ([`ROOT_MARKER`])
//! and stored without it; linking puts the mount point in front, so one
//! image serves any store directory or mount path.

use rkyv::{Archive, Deserialize, Serialize};

/// The first bytes of every image.
pub const MAGIC: [u8; 8] = *b"BRIXIMG\0";

/// Bumped whenever the layout in this file or the module encoding in
/// `codec` changes. Version 4: fixed-width module words in one code area,
/// flat name and module tables, the preamble. Version 5: no stored name
/// orders (attribute sets are kept in symbol-id order). Version 6: a
/// parse error keeps the position of an earlier definition apart. Version 7: no
/// length prefix in deflate blobs (the length is [`Blob::raw_len`]). Version 8:
/// a module error's message and name are bytes. Version 9: checksums (a
/// 64-byte preamble with the archive's length and its own checksum, and
/// the checksums of the archive's parts after it). Version 10: a
/// directory's checksum covers the span of its entries and no file
/// lengths, a module's covers its path, string tables are checksummed in
/// chunks of [`STRINGS_PER_CHUNK`]. Version 11: directory entry names and
/// symlink targets are bytes, as Nix's are (they were UTF-8 strings).
pub const FORMAT_VERSION: u32 = 11;

/// Strings per checksummed chunk of a string table (the names, the
/// module paths): small, since an evaluation uses names from all over the
/// name table and checks every chunk it uses in full (32 halves what
/// hello over nixpkgs hashes, against 1024).
pub const STRINGS_PER_CHUNK: usize = 32;

/// Bumped whenever the compiler's output changes shape or meaning
/// (`ir.rs`, `compile.rs`, the desugarings in `brix-syntax`), so that
/// images compiled by an older brix are rejected rather than misread.
pub const IR_FINGERPRINT: &str = "brix-ir-2026-11-09-b";

/// SHA-256 (hex) of the code the compiler produces for the image tests'
/// fixture (`compiler_output_is_pinned` in `tests/image.rs`). A change
/// that alters it fails that test until [`IR_FINGERPRINT`] is bumped and
/// this is updated with it.
#[doc(hidden)]
pub const IR_FINGERPRINT_FIXTURE_HASH: &str =
    "e5ab1919ad6bac3305a31b41ff7abe425abadf40d82f4c1aba62f99c10c5f54a";

/// The directory relative path literals are resolved against when a
/// module is compiled; [`crate::image::loader`] replaces it with the
/// mount point.
pub const ROOT_MARKER: &str = "/__brix_image_root__";

/// The fixed bytes in front of the archive: [`MAGIC`], the format version
/// (little-endian, like every number below), four zero bytes, the
/// fingerprint (see [`crate::image::ImageCompileContext::fingerprint`]),
/// the archive's length in bytes (a `u64`; the checksums follow the
/// archive), and the XXH3-64 checksum of the 56 bytes before it. Its
/// length is a multiple of the archive's alignment, so the archive that
/// follows is as aligned as the file.
pub const PREAMBLE_LEN: usize = 64;

/// The preamble of an image, as [`write_preamble`] lays it out.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Preamble {
    pub magic: [u8; 8],
    pub format_version: u32,
    pub fingerprint: [u8; 32],
    pub archive_len: u64,
    /// The checksum as stored; [`Preamble::is_intact`] compares it.
    pub checksum: u64,
}

/// Where the preamble's checksum is: it covers the bytes before it.
const PREAMBLE_CHECKSUM_AT: usize = PREAMBLE_LEN - 8;

/// Writes the preamble for an image with `fingerprint` whose archive is
/// `archive_len` bytes long.
pub fn write_preamble(fingerprint: &[u8; 32], archive_len: u64) -> [u8; PREAMBLE_LEN] {
    let mut out = [0u8; PREAMBLE_LEN];
    out[..8].copy_from_slice(&MAGIC);
    out[8..12].copy_from_slice(&FORMAT_VERSION.to_le_bytes());
    out[16..48].copy_from_slice(fingerprint);
    out[48..56].copy_from_slice(&archive_len.to_le_bytes());
    let checksum = xxhash_rust::xxh3::xxh3_64(&out[..PREAMBLE_CHECKSUM_AT]);
    out[PREAMBLE_CHECKSUM_AT..].copy_from_slice(&checksum.to_le_bytes());
    out
}

/// Reads the preamble at the start of `bytes`, if there is room for one.
/// The fields are read as they are; check the magic and the version
/// before trusting the rest, and [`Preamble::is_intact`] before using it.
pub fn read_preamble(bytes: &[u8]) -> Option<Preamble> {
    let p = bytes.get(..PREAMBLE_LEN)?;
    Some(Preamble {
        magic: p[..8].try_into().expect("8 bytes"),
        format_version: u32::from_le_bytes(p[8..12].try_into().expect("4 bytes")),
        fingerprint: p[16..48].try_into().expect("32 bytes"),
        archive_len: u64::from_le_bytes(p[48..56].try_into().expect("8 bytes")),
        checksum: u64::from_le_bytes(p[PREAMBLE_CHECKSUM_AT..].try_into().expect("8 bytes")),
    })
}

impl Preamble {
    /// Whether the preamble at the start of `bytes` (the one this was
    /// read from) matches its checksum.
    pub fn is_intact(&self, bytes: &[u8]) -> bool {
        bytes
            .get(..PREAMBLE_CHECKSUM_AT)
            .is_some_and(|p| xxhash_rust::xxh3::xxh3_64(p) == self.checksum)
    }
}

#[derive(Archive, Serialize, Deserialize, Debug, PartialEq, Eq)]
pub struct Header {
    /// How many symbols the compiling evaluator had before any file was
    /// compiled; symbol ids below this are shared with the loader.
    pub base_symbols: u32,
    /// The SHA-256 NAR hash of the whole tree (the input's `narHash`).
    pub nar_hash: [u8; 32],
    pub nar_size: u64,
    /// Whether every file's contents are stored, not just the `.nix`
    /// files'.
    pub full_tree: bool,
}

#[derive(Archive, Serialize, Deserialize, Debug, PartialEq, Eq)]
pub struct Image {
    pub header: Header,
    /// Unique file contents. Blob 0 is always the empty file.
    pub blobs: Vec<Blob>,
    /// Directories in pre-order: index 0 is the root of the tree, and a
    /// directory comes before its subdirectories.
    pub dirs: Vec<Dir>,
    /// The distinct symbol names the modules create beyond the base
    /// symbols, referenced by index from the modules' name lists.
    pub names: StringTable,
    /// Compiled `.nix` files, sorted by path.
    pub modules: ModuleTable,
    /// Every module's compiled code.
    pub code: CodeArea,
}

/// Byte strings stored back to back: string `i` is
/// `bytes[ends[i - 1]..ends[i]]` (from 0 for the first). One flat table
/// instead of a vector of vectors, so opening an image validates two
/// ranges rather than one per string.
#[derive(Archive, Serialize, Deserialize, Debug, PartialEq, Eq, Default)]
pub struct StringTable {
    pub bytes: Vec<u8>,
    pub ends: Vec<u32>,
}

impl StringTable {
    /// Appends `s`, returning its index.
    pub fn push(&mut self, s: &[u8]) -> u32 {
        self.bytes.extend_from_slice(s);
        self.ends
            .push(u32::try_from(self.bytes.len()).expect("more than 4 GiB of strings in an image"));
        (self.ends.len() - 1) as u32
    }

    pub fn len(&self) -> usize {
        self.ends.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ends.is_empty()
    }

    pub fn get(&self, i: usize) -> &[u8] {
        let start = if i == 0 { 0 } else { self.ends[i - 1] as usize };
        &self.bytes[start..self.ends[i] as usize]
    }
}

impl ArchivedStringTable {
    /// String `i`, or `None` if it is out of range or its offsets are
    /// not (a damaged image).
    pub fn get(&self, i: usize) -> Option<&[u8]> {
        let ends = self.ends.as_slice();
        let start = match i {
            0 => 0,
            _ => ends.get(i - 1)?.to_native() as usize,
        };
        let end = ends.get(i)?.to_native() as usize;
        self.bytes.as_slice().get(start..end)
    }

    pub fn len(&self) -> usize {
        self.ends.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ends.is_empty()
    }
}

/// How a blob's bytes are stored.
pub const BLOB_RAW: u8 = 0;
/// A raw deflate stream that inflates to [`Blob::raw_len`] bytes (see
/// `compress`).
pub const BLOB_DEFLATE: u8 = 1;
/// Not stored: the file exists in the tree (its size and hash are
/// known), but its contents must come from the source tree.
pub const BLOB_ABSENT: u8 = 2;

#[derive(Archive, Serialize, Deserialize, Debug, PartialEq, Eq)]
pub struct Blob {
    /// The file's length, whatever the encoding.
    pub raw_len: u32,
    /// One of [`BLOB_RAW`], [`BLOB_DEFLATE`], [`BLOB_ABSENT`].
    pub encoding: u8,
    pub data: Vec<u8>,
}

#[derive(Archive, Serialize, Deserialize, Debug, PartialEq, Eq)]
pub struct Dir {
    /// Sorted by name in unsigned byte order, as a NAR lists them (Nix's
    /// `DirEntries` is a `std::map<std::string, …>`).
    pub entries: Vec<Entry>,
    pub nar_hash: [u8; 32],
    pub nar_size: u64,
}

#[derive(Archive, Serialize, Deserialize, Debug, PartialEq, Eq)]
pub struct Entry {
    /// The name as the file system or the archive has it: bytes, which
    /// need not be UTF-8. An archived byte vector is validated with a
    /// range check, where a string's bytes are checked to be UTF-8 too.
    pub name: Vec<u8>,
    pub kind: EntryKind,
}

#[derive(Archive, Serialize, Deserialize, Debug, PartialEq, Eq)]
pub enum EntryKind {
    Dir(u32),
    File {
        blob: u32,
        executable: bool,
    },
    /// The target verbatim, as bytes (`readLink`).
    Symlink {
        target: Vec<u8>,
    },
}

/// The modules: one fixed-size record per `.nix` file, their paths, and
/// the few that do not compile.
#[derive(Archive, Serialize, Deserialize, Debug, PartialEq, Eq, Default)]
pub struct ModuleTable {
    /// Paths relative to the tree's root, without a leading slash, in
    /// byte order; path `i` belongs to `entries[i]`.
    pub paths: StringTable,
    pub entries: Vec<Module>,
    /// What importing a file that does not compile raises.
    pub errors: Vec<ModuleError>,
}

/// A module compiled to code (see `codec` for its words).
pub const MODULE_CODE: u32 = 0;
/// Importing the module raises [`ModuleTable::errors`]`[error]`.
pub const MODULE_ERROR: u32 = 1;
/// Compiled fine but cannot be served from the image (a path literal
/// escaped the tree, or a table outgrew the encoding); the loader falls
/// back to the source.
pub const MODULE_UNSUPPORTED: u32 = 2;

/// One `.nix` file. All fields are integers, so validating the module
/// table when an image is opened costs nothing per module; the ranges
/// are checked when the module is linked.
#[derive(Archive, Serialize, Deserialize, Debug, PartialEq, Eq, Clone, Copy, Default)]
pub struct Module {
    /// One of [`MODULE_CODE`], [`MODULE_ERROR`], [`MODULE_UNSUPPORTED`].
    pub kind: u32,
    /// For [`MODULE_ERROR`], the index into [`ModuleTable::errors`].
    pub error: u32,
    /// The source, as an index into [`Image::blobs`]; its contents are
    /// stored only in a full-tree image, otherwise read from the source
    /// tree when an error excerpt needs it.
    pub blob: u32,
    /// The source's length in bytes.
    pub len: u32,
    /// The module's words: `CodeArea::words[words_start..][..words_len]`.
    pub words_start: u32,
    pub words_len: u32,
    /// The module's bytes: `CodeArea::bytes[bytes_start..][..bytes_len]`,
    /// the first `lines_len` of which are its line table (LEB128 deltas
    /// between successive line starts, see `codec::pack_line_starts`,
    /// decoded only when a position is shown), the rest its string
    /// literals.
    pub bytes_start: u32,
    pub bytes_len: u32,
    pub lines_len: u32,
}

#[derive(Archive, Serialize, Deserialize, Debug, PartialEq, Eq)]
pub enum ModuleError {
    /// Parsing failed: importing the file raises this error, exactly as
    /// it would from source. `defined_at` is `ParseError::defined_at`:
    /// the loader shows it with the origin the image is loaded from.
    /// The message is bytes: it can quote a name or a string from the
    /// source, which need not be UTF-8.
    ParseError {
        msg: Vec<u8>,
        pos: Option<u32>,
        defined_at: Option<u32>,
    },
    /// Scope resolution failed with `undefined variable`, at this offset.
    /// The name is bytes, as written.
    UndefinedVariable { name: Vec<u8>, pos: u32 },
}

/// The compiled code of all modules, back to back in module order.
#[derive(Archive, Serialize, Deserialize, Debug, PartialEq, Eq, Default)]
pub struct CodeArea {
    /// Every module's tables as little-endian 32-bit words, 4-byte
    /// aligned in the file (see `codec`).
    pub words: Vec<u32>,
    /// Every module's line table and string literals.
    pub bytes: Vec<u8>,
}
