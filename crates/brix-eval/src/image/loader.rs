//! Reading images: validation, the file-tree accessor, and linking a
//! precompiled module into an evaluator.

use crate::runtime::cell::RefCell;
use std::borrow::Cow;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, OnceLock};

use brix_store::{
    AccessorError, CanonPath, DirEntries, FileType, Hash, HashAlgo, SourceAccessor, Stat,
};
use bstr::{BStr, BString, ByteSlice};
use rkyv::util::AlignedVec;

use super::format::STRINGS_PER_CHUNK;
use super::format::{
    self as fmt, ArchivedEntryKind, ArchivedImage, ArchivedModule, ArchivedModuleError,
    ArchivedStringTable,
};
use super::integrity::{self, Checked, Layout};
use super::{codec, ImageError};
use crate::code::LinkedCode;
use crate::error::{ErrorKind, SourceInfo};
use crate::evaluator::Evaluator;
use crate::ir::Origin;
use crate::symbol::Symbol;
use crate::value::SrcPos;
use crate::Result;

/// Something that owns image bytes: a memory map, an aligned buffer.
pub trait ImageBytes: AsRef<[u8]> + Send + Sync + 'static {}
impl<T: AsRef<[u8]> + Send + Sync + 'static> ImageBytes for T {}

self_cell::self_cell! {
    struct Inner {
        owner: Box<dyn ImageBytes>,
        #[covariant]
        dependent: Archived,
    }
}

type Archived<'a> = &'a ArchivedImage;

/// A validated image, used in place.
pub struct Image {
    inner: Inner,
    fingerprint: [u8; 32],
    /// Where the image came from (its file), for messages.
    label: Option<String>,
    /// The first damage found after the image was opened, if any.
    damage: OnceLock<ImageError>,
    /// The archive's length: it lies at `PREAMBLE_LEN..` this, and the
    /// checksum table follows it.
    archive_len: usize,
    /// Where each part's checksum is in the checksum table.
    layout: Layout,
    /// Which parts have matched their checksums (and, for a directory,
    /// passed [`Image::dir_is_sound`]).
    checked: Checked,
    /// Each directory's parent as `index + 1`, recorded when the parent
    /// is first reached (0: not yet): a directory a second parent lists
    /// is a shared subtree, found before it is expanded a second time.
    parents: Box<[AtomicU32]>,
}

/// What is wrong with a directory, found when it is first reached.
#[derive(Clone, Debug)]
enum DirDamage {
    /// It does not match its checksum.
    Checksum,
    /// It matches its checksum, but its entries do not make a tree.
    Structure(String),
}

/// One of the image's string tables, which are checksummed in chunks.
#[derive(Clone, Copy, Debug)]
enum Strings {
    Names,
    Paths,
}

impl std::fmt::Debug for Image {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Image")
            .field("modules", &self.module_count())
            .field("dirs", &self.archived().dirs.len())
            .field("full_tree", &self.has_full_tree())
            .finish()
    }
}

/// The alignment `rkyv` archives are written with.
const ALIGN: usize = 16;

impl Image {
    /// Opens an image from bytes that are already suitably aligned (a
    /// memory map, an [`AlignedVec`]). The preamble is checked first, so
    /// an image of another format version or (with
    /// `expected_fingerprint`) for another evaluator is rejected without
    /// reading further; then the archive, its header and the root
    /// directory are validated, once. Every other part is checked when it
    /// is first used: a directory when a lookup reaches it (see
    /// [`Image::dir_is_sound`]), module code when it is linked, and each
    /// part against its checksum (see [`super::integrity`]).
    pub fn open(
        bytes: impl ImageBytes,
        expected_fingerprint: Option<&[u8; 32]>,
    ) -> std::result::Result<Image, ImageError> {
        let owner: Box<dyn ImageBytes> = Box::new(bytes);
        let slice: &[u8] = owner.as_ref().as_ref();
        let Some(preamble) = fmt::read_preamble(slice) else {
            return Err(match slice.starts_with(&fmt::MAGIC) {
                true => ImageError::Corrupt("truncated in the preamble".into()),
                false => ImageError::NotAnImage,
            });
        };
        if preamble.magic != fmt::MAGIC {
            return Err(ImageError::NotAnImage);
        }
        if preamble.format_version != fmt::FORMAT_VERSION {
            return Err(ImageError::FormatVersion {
                found: preamble.format_version,
                supported: fmt::FORMAT_VERSION,
            });
        }
        if !preamble.is_intact(slice) {
            return Err(ImageError::Corrupt("preamble: checksum mismatch".into()));
        }
        if let Some(fp) = expected_fingerprint {
            if preamble.fingerprint != *fp {
                return Err(ImageError::Fingerprint);
            }
        }
        if (slice.as_ptr() as usize + fmt::PREAMBLE_LEN) % ALIGN != 0 {
            return Err(ImageError::Corrupt(
                "image bytes are not 16-byte aligned; use Image::from_vec".into(),
            ));
        }
        let room = slice.len() - fmt::PREAMBLE_LEN;
        let archive_len = usize::try_from(preamble.archive_len)
            .ok()
            .filter(|&len| len <= room)
            .ok_or_else(|| {
                ImageError::Corrupt(format!(
                    "truncated: the archive is {} bytes long, but {room} bytes follow the preamble",
                    preamble.archive_len
                ))
            })?;
        let inner = Inner::try_new(owner, |owner| {
            let archive =
                &owner.as_ref().as_ref()[fmt::PREAMBLE_LEN..fmt::PREAMBLE_LEN + archive_len];
            rkyv::access::<ArchivedImage, rkyv::rancor::Error>(archive)
                .map_err(|e| ImageError::Corrupt(e.to_string()))
        })?;
        let layout = Layout::of(inner.borrow_dependent());
        if room - archive_len != layout.bytes() {
            return Err(ImageError::Corrupt(format!(
                "the checksum table is {} bytes long, expected {}",
                room - archive_len,
                layout.bytes()
            )));
        }
        let dirs = inner.borrow_dependent().dirs.len();
        let image = Image {
            inner,
            fingerprint: preamble.fingerprint,
            label: None,
            damage: OnceLock::new(),
            archive_len,
            layout,
            checked: Checked::new(layout.len),
            parents: (0..dirs).map(|_| AtomicU32::new(0)).collect(),
        };
        let archived = image.archived();
        if integrity::header(archived) != image.stored_checksum(integrity::HEADER) {
            return Err(ImageError::Corrupt("header: checksum mismatch".into()));
        }
        let modules = &archived.modules;
        if modules.paths.len() != modules.entries.len() {
            return Err(ImageError::Corrupt(
                "module table and module paths differ in length".into(),
            ));
        }
        // The rest of the tree is checked a directory at a time, when a
        // lookup first reaches it (see `Image::dir_is_sound`).
        let tree = |why: &str| Err(ImageError::Corrupt(format!("directory tree: {why}")));
        let Some(root) = archived.dirs.first() else {
            return tree("no root directory");
        };
        if root.nar_hash != archived.header.nar_hash || root.nar_size != archived.header.nar_size {
            return tree("the root's NAR hash or size differs from the header's");
        }
        Ok(image)
    }

    /// Names the image in messages, typically by its file's path; the
    /// opener sets it.
    pub fn with_label(mut self, label: impl Into<String>) -> Image {
        self.label = Some(label.into());
        self
    }

    /// The label set with [`Image::with_label`].
    pub fn label(&self) -> Option<&str> {
        self.label.as_deref()
    }

    /// The first damage found after the image was opened: in a module's
    /// code when it was linked, or in a part that did not match its
    /// checksum when it was first used. The operation that found it
    /// failed and named the image; whoever owns the file (a cache)
    /// should not serve it again.
    pub fn damage(&self) -> Option<&ImageError> {
        self.damage.get()
    }

    /// Keeps `damage` as the image's [`Image::damage`] unless one was
    /// found before, and returns it.
    fn record(&self, damage: ImageError) -> ImageError {
        self.damage.get_or_init(|| damage.clone());
        damage
    }

    /// The message for damage found while the image is in use: it names
    /// the image and says how to get past it.
    pub(crate) fn damage_message(&self, damage: &ImageError) -> String {
        match self.label() {
            Some(label) => format!(
                "compiled image '{label}': {damage}; delete the image or run with \
                 --no-compiled-inputs"
            ),
            None => format!(
                "compiled image: {damage}; run with --no-compiled-inputs to evaluate from source"
            ),
        }
    }

    /// Records `damage` (see [`Image::record`]) and returns its message.
    pub(crate) fn report(&self, damage: ImageError) -> String {
        self.damage_message(&self.record(damage))
    }

    /// The whole file.
    fn file(&self) -> &[u8] {
        self.inner.borrow_owner().as_ref().as_ref()
    }

    /// The archive's bytes, which the archived view borrows from.
    fn archive(&self) -> &[u8] {
        &self.file()[fmt::PREAMBLE_LEN..fmt::PREAMBLE_LEN + self.archive_len]
    }

    /// The checksum stored in `slot` (see [`Layout`]), which `open`
    /// checked is in the file.
    fn stored_checksum(&self, slot: usize) -> u64 {
        let at = fmt::PREAMBLE_LEN + self.archive_len + 8 * slot;
        u64::from_le_bytes(self.file()[at..at + 8].try_into().expect("8 bytes"))
    }

    /// Compares the part in `slot` with its checksum, unless it has
    /// matched before; `sum` computes the part's checksum, or says why
    /// it cannot.
    #[inline]
    fn check(
        &self,
        slot: usize,
        sum: impl FnOnce() -> std::result::Result<u64, String>,
    ) -> std::result::Result<(), String> {
        if self.checked.contains(slot) {
            return Ok(());
        }
        self.check_now(slot, sum)
    }

    #[cold]
    fn check_now(
        &self,
        slot: usize,
        sum: impl FnOnce() -> std::result::Result<u64, String>,
    ) -> std::result::Result<(), String> {
        if sum()? != self.stored_checksum(slot) {
            return Err("checksum mismatch".into());
        }
        self.checked.insert(slot);
        Ok(())
    }

    /// Checks directory `d` (in range) the first time it is reached, so
    /// that walking from it can neither fail nor run away: it must match
    /// its checksum, and its entries must make a tree:
    ///
    /// - names valid and strictly sorted, as Nix's NAR parser requires
    ///   (`parse` in `libutil/archive.cc`), which is also what the binary
    ///   search in [`Image::walk`] needs;
    /// - blob indices in range;
    /// - directory indices in range, after `d` (so there are no cycles)
    ///   and increasing (the writer emits directories in pre-order), and
    ///   listed by no other directory: `d` claims each one as its parent,
    ///   so a shared subtree fails when its second parent is reached,
    ///   before it is expanded a second time.
    ///
    /// After that, one bit test.
    #[inline]
    fn dir_is_sound(&self, d: usize) -> std::result::Result<(), DirDamage> {
        if self.checked.contains(self.layout.dirs + d) {
            return Ok(());
        }
        self.check_dir_now(d)
    }

    #[cold]
    #[inline(never)]
    fn check_dir_now(&self, d: usize) -> std::result::Result<(), DirDamage> {
        let image = self.archived();
        let slot = self.layout.dirs + d;
        if integrity::dir(image, self.archive(), d) != self.stored_checksum(slot) {
            return Err(DirDamage::Checksum);
        }
        let damaged = |why: String| Err(DirDamage::Structure(why));
        let (dirs, blobs) = (image.dirs.len(), image.blobs.len());
        let entries = image.dirs[d].entries.as_slice();
        // Names are bytes, compared unsigned, as Nix's `std::string` keys
        // are; they are shown escaped (`"caf\xe9"`) in the messages.
        let mut prev_name: Option<&[u8]> = None;
        let mut prev_child = d;
        for entry in entries {
            let name = entry.name.as_slice();
            if name.is_empty()
                || name == b"."
                || name == b".."
                || name.iter().any(|&b| b == b'/' || b == 0)
            {
                return damaged(format!(
                    "directory {d} has an entry with the invalid name {:?}",
                    name.as_bstr()
                ));
            }
            if prev_name.is_some_and(|p| p >= name) {
                return damaged(format!(
                    "directory {d} is not sorted at {:?}",
                    name.as_bstr()
                ));
            }
            prev_name = Some(name);
            match &entry.kind {
                ArchivedEntryKind::Dir(child) => {
                    let child = child.to_native() as usize;
                    if child <= d || child >= dirs {
                        return damaged(format!(
                            "directory {d} lists directory {child}, which does not follow it"
                        ));
                    }
                    if child == prev_child {
                        return damaged(format!("directory {child} is listed twice"));
                    }
                    if child < prev_child {
                        return damaged(format!(
                            "directory {d} lists directory {child} out of order"
                        ));
                    }
                    prev_child = child;
                }
                ArchivedEntryKind::File { blob, .. } => {
                    if blob.to_native() as usize >= blobs {
                        return damaged(format!(
                            "directory {d} lists blob {blob}, which is out of range"
                        ));
                    }
                }
                ArchivedEntryKind::Symlink { .. } => {}
            }
        }
        // Sound: claim the subdirectories. One this directory claimed
        // already (a check of it cut short, or another thread's) is its.
        let me = u32::try_from(d + 1).expect("invariant: archived vectors have u32 lengths");
        for entry in entries {
            if let ArchivedEntryKind::Dir(child) = &entry.kind {
                let child = child.to_native() as usize;
                match self.parents[child].compare_exchange(
                    0,
                    me,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => {}
                    Err(parent) if parent == me => {}
                    Err(_) => return damaged(format!("directory {child} is listed twice")),
                }
            }
        }
        self.checked.insert(slot);
        Ok(())
    }

    /// The error for damage in directory `d`, whose path is `/{path}`
    /// (as far as known), kept as the image's [`Image::damage`]. The
    /// message is brix's own text: a name that is not UTF-8 shows as
    /// U+FFFD.
    fn dir_damage(&self, damage: DirDamage, path: &[u8]) -> ImageError {
        self.record(ImageError::Corrupt(match damage {
            DirDamage::Checksum => {
                format!("directory '/{}': checksum mismatch", path.as_bstr())
            }
            DirDamage::Structure(why) => format!("directory tree: {why}"),
        }))
    }

    /// String `i` of a string table, once its chunk matches its checksum.
    fn string(&self, strings: Strings, i: usize) -> std::result::Result<&[u8], String> {
        let (table, first_slot, one, what): (&ArchivedStringTable, usize, &str, &str) =
            match strings {
                Strings::Names => (&self.archived().names, self.layout.names, "name", "names"),
                Strings::Paths => (
                    &self.archived().modules.paths,
                    self.layout.paths,
                    "module path",
                    "module paths",
                ),
            };
        let s = table
            .get(i)
            .ok_or_else(|| format!("{one} {i} out of range"))?;
        let c = i / STRINGS_PER_CHUNK;
        self.check(first_slot + c, || {
            integrity::string_chunk(table, self.archive(), c)
        })
        .map_err(|why| {
            let first = c * STRINGS_PER_CHUNK;
            let last = table.len().min(first + STRINGS_PER_CHUNK) - 1;
            format!("{what} {first} to {last}: {why}")
        })?;
        Ok(s)
    }

    /// Compares module `index`, its parts resolved, with its checksum.
    fn check_module(
        &self,
        index: usize,
        parts: &ModuleParts<'_>,
    ) -> std::result::Result<(), String> {
        self.check(self.layout.modules + index, || {
            Ok(integrity::module(self.archive(), parts))
        })
    }

    /// Checks every part of the image now, rather than when each is first
    /// used: every directory (its checksum and that it is sound, see
    /// [`Image::dir_is_sound`], and that the tree reaches every
    /// directory), the name and module path tables, every module and
    /// every blob against their checksums. One pass over the whole image,
    /// for tools and tests (opening an image does not do this). The first
    /// damage found is returned, and kept as [`Image::damage`].
    pub fn verify(&self) -> std::result::Result<(), ImageError> {
        let image = self.archived();
        // Directories are named by their paths (bytes, shown lossily in
        // messages), which the tree's pre-order gives before they are
        // needed.
        let mut paths: Vec<Vec<u8>> = vec![Vec::new(); image.dirs.len()];
        for d in 0..image.dirs.len() {
            self.dir_is_sound(d)
                .map_err(|damage| self.dir_damage(damage, &paths[d]))?;
            for entry in image.dirs[d].entries.iter() {
                if let ArchivedEntryKind::Dir(child) = &entry.kind {
                    let name = entry.name.as_slice();
                    paths[child.to_native() as usize] = match d {
                        0 => name.to_vec(),
                        _ => [paths[d].as_slice(), b"/".as_slice(), name].concat(),
                    };
                }
            }
        }
        // Every directory but the root has been claimed by its parent,
        // unless the tree does not reach it.
        let unlisted = self.parents[1..]
            .iter()
            .filter(|p| p.load(Ordering::Relaxed) == 0)
            .count();
        if unlisted != 0 {
            return Err(self.record(ImageError::Corrupt(format!(
                "directory tree: {unlisted} directories are not in the tree"
            ))));
        }
        for (strings, len) in [
            (Strings::Names, image.names.len()),
            (Strings::Paths, image.modules.paths.len()),
        ] {
            for first in (0..len).step_by(STRINGS_PER_CHUNK) {
                self.string(strings, first)
                    .map_err(|why| self.record(ImageError::Corrupt(why)))?;
            }
        }
        for index in 0..self.module_count() {
            module_parts(image, index)
                .and_then(|parts| self.check_module(index, &parts))
                .map_err(|why| self.record(self.module_damage(index, why)))?;
        }
        for index in 0..image.blobs.len() {
            self.check_blob(index)?;
        }
        Ok(())
    }

    /// Damage `why` in module `index`, named by its path.
    fn module_damage(&self, index: usize, why: String) -> ImageError {
        let module = match self.archived().modules.paths.get(index) {
            Some(path) => String::from_utf8_lossy(path).into_owned(),
            None => format!("#{index}"),
        };
        ImageError::CorruptModule { module, why }
    }

    /// Opens an image from an arbitrary buffer, copying it into aligned
    /// memory when needed.
    pub fn from_vec(
        bytes: Vec<u8>,
        expected_fingerprint: Option<&[u8; 32]>,
    ) -> std::result::Result<Image, ImageError> {
        let mut aligned = AlignedVec::<ALIGN>::with_capacity(bytes.len());
        aligned.extend_from_slice(&bytes);
        Self::open(aligned, expected_fingerprint)
    }

    fn archived(&self) -> &ArchivedImage {
        self.inner.borrow_dependent()
    }

    fn header(&self) -> &fmt::ArchivedHeader {
        &self.archived().header
    }

    /// What the image was compiled for (see
    /// [`super::ImageCompileContext::fingerprint`]).
    pub fn fingerprint(&self) -> &[u8; 32] {
        &self.fingerprint
    }

    /// How many symbols the compiling evaluator had before compiling any
    /// file; module code refers to them by id.
    pub fn base_symbols(&self) -> u32 {
        self.header().base_symbols.to_native()
    }

    /// The whole tree's NAR hash.
    pub fn nar_hash(&self) -> Hash {
        Hash::from_bytes(HashAlgo::Sha256, &self.header().nar_hash)
    }

    pub fn nar_size(&self) -> u64 {
        self.header().nar_size.to_native()
    }

    /// Whether every file's contents are stored (see
    /// [`super::writer::CompileOptions::include_tree`]).
    pub fn has_full_tree(&self) -> bool {
        self.header().full_tree
    }

    pub fn module_count(&self) -> usize {
        self.archived().modules.entries.len()
    }

    /// How many distinct names the modules create.
    pub(crate) fn name_count(&self) -> usize {
        self.archived().names.len()
    }

    /// The length of a file in the tree, stored or not, once its blob
    /// matches its checksum (which covers the length). `index` comes from
    /// a checked directory, whose blob indices are in range.
    fn blob_len(&self, index: u32) -> std::result::Result<u64, ImageError> {
        self.check_blob(index as usize)?;
        Ok(u64::from(
            self.archived().blobs[index as usize].raw_len.to_native(),
        ))
    }

    /// Compares blob `index` with its checksum.
    fn check_blob(&self, index: usize) -> std::result::Result<(), ImageError> {
        let blob = self.archived().blobs.get(index).ok_or_else(|| {
            self.record(ImageError::Corrupt(format!("blob {index} out of range")))
        })?;
        self.check(self.layout.blobs + index, || Ok(integrity::blob(blob)))
            .map_err(|why| self.record(ImageError::Corrupt(format!("blob {index}: {why}"))))
    }

    /// A stored file's contents, decompressed when needed; `None` for a
    /// file whose contents the image does not hold. Damage found is kept
    /// as the image's [`Image::damage`].
    pub fn blob(&self, index: u32) -> std::result::Result<Option<Cow<'_, [u8]>>, ImageError> {
        self.check_blob(index as usize)?;
        self.decode_blob(index)
            .map_err(|damage| self.record(damage))
    }

    /// Blob `index`, which matched its checksum, decoded.
    fn decode_blob(&self, index: u32) -> std::result::Result<Option<Cow<'_, [u8]>>, ImageError> {
        let blob = &self.archived().blobs[index as usize];
        match blob.encoding {
            fmt::BLOB_RAW => Ok(Some(Cow::Borrowed(blob.data.as_slice()))),
            fmt::BLOB_DEFLATE => Ok(Some(Cow::Owned(super::compress::unpack(
                blob.data.as_slice(),
                blob.raw_len.to_native() as usize,
                &format!("blob {index}"),
            )?))),
            fmt::BLOB_ABSENT => Ok(None),
            other => Err(ImageError::Corrupt(format!(
                "blob {index} has unknown encoding {other}"
            ))),
        }
    }

    /// The index of the module for a path relative to the tree root (no
    /// leading `/`; bytes, which need not be UTF-8), if the image has
    /// one.
    ///
    /// A match is not checked here: linking the module checks it, since
    /// a module's checksum covers its path. A miss, which sends the
    /// import to the source, is confirmed by searching again with every
    /// path compared checked against its chunk's checksum; damage found
    /// is an error, and kept as the image's [`Image::damage`].
    pub fn find_module(
        &self,
        rel: impl AsRef<[u8]>,
    ) -> std::result::Result<Option<usize>, ImageError> {
        let rel = rel.as_ref();
        let paths = &self.archived().modules.paths;
        if let Some(index) = self.search_modules(rel, |mid| Ok(paths.get(mid)))? {
            return Ok(Some(index));
        }
        self.search_modules(rel, |mid| {
            self.string(Strings::Paths, mid)
                .map(Some)
                .map_err(|why| self.record(ImageError::Corrupt(why)))
        })
    }

    /// Binary search of the module paths for `rel`, reading path `i`
    /// with `path(i)` (`None` stops the search, finding nothing).
    fn search_modules<'s>(
        &'s self,
        rel: &[u8],
        path: impl Fn(usize) -> std::result::Result<Option<&'s [u8]>, ImageError>,
    ) -> std::result::Result<Option<usize>, ImageError> {
        let (mut lo, mut hi) = (0usize, self.module_count());
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let Some(path) = path(mid)? else {
                return Ok(None);
            };
            match path.cmp(rel) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => return Ok(Some(mid)),
            }
        }
        Ok(None)
    }

    /// Where module `index`'s words lie in the image file, as a byte
    /// range, for tests that damage them.
    #[doc(hidden)]
    pub fn module_words_in_file(&self, index: usize) -> Option<std::ops::Range<usize>> {
        let file = self.inner.borrow_owner().as_ref().as_ref().as_ptr() as usize;
        let module = self.archived().modules.entries.get(index)?;
        let words = self.archived().code.words.as_slice();
        let start = words.as_ptr() as usize - file + 4 * module.words_start.to_native() as usize;
        Some(start..start + 4 * module.words_len.to_native() as usize)
    }

    /// Where `path` leads in the tree, following symlinks in its
    /// directories the way the file system does for the source tree (and
    /// in its last component too with `follow_final`). Every directory
    /// on the way, and the one found, has been checked (see
    /// [`Image::dir_is_sound`]), so callers index with what this returns.
    fn lookup(&self, path: &CanonPath, follow_final: bool) -> Lookup<'_> {
        // The common case, no symlink on the way, allocates nothing.
        let mut step = self.walk(path.components().map(|c| c.as_bytes()), follow_final);
        // The path with the symlinks met so far replaced by their
        // targets: components borrowed from `path` and from the targets
        // in the image, so following a link copies no names.
        let mut resolved: Option<Vec<&[u8]>> = None;
        let mut links = 0;
        loop {
            let (index, target) = match step {
                Step::Found(located) => return Lookup::Found(located),
                Step::Missing => return Lookup::Missing,
                Step::Damaged { depth, damage } => {
                    // The directory the walk reached after `depth`
                    // components of what it walked.
                    let dir: Vec<&[u8]> = match &resolved {
                        Some(components) => components.iter().take(depth).copied().collect(),
                        None => path
                            .components()
                            .take(depth)
                            .map(|c| c.as_bytes())
                            .collect(),
                    };
                    return Lookup::Damaged(self.dir_damage(damage, &dir.join(&b'/')));
                }
                Step::Symlink { index, target } => (index, target),
            };
            links += 1;
            // An absolute target is on the real file system, not in the
            // tree; so is one that climbs above the root, or the end of a
            // chain too long to follow (which the file system reports).
            if links > MAX_SYMLINKS || target.first() == Some(&b'/') {
                return Lookup::Outside;
            }
            let components =
                resolved.get_or_insert_with(|| path.components().map(|c| c.as_bytes()).collect());
            // The link's directory has no symlinks in it (they were
            // followed on the way), so `..` in the target is lexical.
            let rest = components.split_off(index + 1);
            components.pop();
            for part in target.split(|&b| b == b'/') {
                match part {
                    b"" | b"." => {}
                    b".." => {
                        if components.pop().is_none() {
                            return Lookup::Outside;
                        }
                    }
                    name => components.push(name),
                }
            }
            components.extend(rest);
            step = self.walk(components.iter().copied(), follow_final);
        }
    }

    /// Walks `components` from the root, up to the first symlink that has
    /// to be followed. Each directory it reaches is checked before it is
    /// used, the first time (see [`Image::dir_is_sound`]). Names are
    /// compared as bytes, in the order the writer sorted them.
    fn walk<'p>(&self, components: impl Iterator<Item = &'p [u8]>, follow_final: bool) -> Step<'_> {
        let dirs = self.archived().dirs.as_slice();
        let mut located = Located::Dir(0);
        let mut walked = 0;
        for comp in components {
            let dir = match located {
                Located::Dir(dir) => dir,
                Located::Symlink(target) => {
                    return Step::Symlink {
                        index: walked - 1,
                        target,
                    }
                }
                // Nothing is below a file.
                Located::File { .. } => return Step::Missing,
            };
            if let Err(damage) = self.dir_is_sound(dir) {
                return Step::Damaged {
                    depth: walked,
                    damage,
                };
            }
            let entries = dirs[dir].entries.as_slice();
            let Ok(i) = entries.binary_search_by(|e| e.name.as_slice().cmp(comp)) else {
                return Step::Missing;
            };
            located = match &entries[i].kind {
                ArchivedEntryKind::Dir(d) => Located::Dir(d.to_native() as usize),
                ArchivedEntryKind::File { blob, executable } => Located::File {
                    blob: blob.to_native(),
                    executable: *executable,
                },
                ArchivedEntryKind::Symlink { target } => Located::Symlink(target.as_slice()),
            };
            walked += 1;
        }
        match located {
            Located::Symlink(target) if follow_final => Step::Symlink {
                index: walked - 1,
                target,
            },
            // Its listing and its NAR hash are what the caller wants.
            Located::Dir(dir) => match self.dir_is_sound(dir) {
                Ok(()) => Step::Found(located),
                Err(damage) => Step::Damaged {
                    depth: walked,
                    damage,
                },
            },
            located => Step::Found(located),
        }
    }
}

/// How many symlinks a lookup follows before giving up, as Linux does
/// (`MAXSYMLINKS`).
const MAX_SYMLINKS: usize = 40;

/// Where a path leads in an image's tree.
enum Lookup<'a> {
    Found(Located<'a>),
    Missing,
    /// Through a symlink, out of the tree: only the source tree can say.
    Outside,
    /// A directory on the way is damaged (see [`Image::dir_is_sound`]).
    Damaged(ImageError),
}

/// How far [`Image::walk`] got.
enum Step<'a> {
    Found(Located<'a>),
    Missing,
    /// Component `index` is a symlink to `target`, to be followed.
    Symlink {
        index: usize,
        target: &'a [u8],
    },
    /// The directory reached after `depth` components is damaged.
    Damaged {
        depth: usize,
        damage: DirDamage,
    },
}

#[derive(Clone, Copy)]
enum Located<'a> {
    Dir(usize),
    File {
        blob: u32,
        executable: bool,
    },
    /// The link's target, as the image stores it.
    Symlink(&'a [u8]),
}

/// What [`ImageAccessor::find`] found: a place in the image (or
/// nothing), or the source tree's answer for a path that leaves it.
enum Found<'a, T> {
    Image(Option<Located<'a>>),
    Source(T),
}

/// The image's file tree as a [`SourceAccessor`], standing in for the
/// input's source tree.
///
/// Files whose contents the image does not store (everything but `.nix`
/// files, unless it was compiled with the tree) are read from the
/// `fallback` accessor, the source tree itself, at the same paths;
/// without one, reading them is an error that names the image. The
/// fallback also answers for paths that lead out of the tree through a
/// symlink, and shows paths in messages, so that they read as they would
/// from source. Wrap the accessor in a
/// [`crate::mount::DisplayPrefixAccessor`] to show paths under the input.
pub struct ImageAccessor {
    image: Arc<Image>,
    fallback: Option<Arc<dyn SourceAccessor>>,
}

impl ImageAccessor {
    pub fn new(image: Arc<Image>) -> ImageAccessor {
        ImageAccessor {
            image,
            fallback: None,
        }
    }

    /// Serves what the image cannot from `tree`, the source tree.
    pub fn with_fallback(mut self, tree: Arc<dyn SourceAccessor>) -> ImageAccessor {
        self.fallback = Some(tree);
        self
    }

    /// A stored blob, or the fallback's file for an absent one.
    fn file_contents(
        &self,
        path: &CanonPath,
        blob: u32,
    ) -> std::result::Result<Cow<'_, [u8]>, AccessorError> {
        match self.image.blob(blob) {
            Ok(Some(data)) => Ok(data),
            Ok(None) => match &self.fallback {
                Some(tree) => tree.read_file(path).map(Cow::Owned),
                None => Err(self.unavailable(
                    path,
                    "is not stored in the compiled image (compiled without the tree) and the \
                     source tree is not available",
                )),
            },
            Err(damage) => Err(AccessorError::Io(self.image.damage_message(&damage).into())),
        }
    }

    /// The error for `path`, which only the source tree could serve
    /// when there is none: `'<path>' <why>`, the path's bytes as they
    /// are.
    #[cold]
    fn unavailable(&self, path: &CanonPath, why: &str) -> AccessorError {
        let shown = self.show_path(path);
        let mut msg = BString::from(Vec::with_capacity(shown.len() + why.len() + 3));
        msg.push(b'\'');
        msg.extend_from_slice(&shown);
        msg.extend_from_slice(b"' ");
        msg.extend_from_slice(why.as_bytes());
        AccessorError::Io(msg)
    }

    /// Where `path` leads (see [`Image::lookup`]), or, for a path that
    /// leaves the tree, what `outside` makes of it on the source tree.
    fn find<T>(
        &self,
        path: &CanonPath,
        follow_final: bool,
        outside: impl FnOnce(&dyn SourceAccessor) -> std::result::Result<T, AccessorError>,
    ) -> std::result::Result<Found<'_, T>, AccessorError> {
        match self.image.lookup(path, follow_final) {
            Lookup::Found(located) => Ok(Found::Image(Some(located))),
            Lookup::Missing => Ok(Found::Image(None)),
            Lookup::Damaged(damage) => {
                Err(AccessorError::Io(self.image.damage_message(&damage).into()))
            }
            Lookup::Outside => match &self.fallback {
                Some(tree) => outside(&**tree).map(Found::Source),
                None => Err(self.unavailable(
                    path,
                    "leads out of the compiled image through a symlink, and the source tree is \
                     not available",
                )),
            },
        }
    }
}

impl SourceAccessor for ImageAccessor {
    fn maybe_lstat(&self, path: &CanonPath) -> std::result::Result<Option<Stat>, AccessorError> {
        let located = match self.find(path, false, |tree| tree.maybe_lstat(path))? {
            Found::Image(located) => located,
            Found::Source(from_source) => return Ok(from_source),
        };
        let Some(located) = located else {
            return Ok(None);
        };
        Ok(Some(match located {
            Located::Dir(_) => Stat {
                file_type: FileType::Directory,
                size: None,
                executable: false,
            },
            Located::File { blob, executable } => Stat {
                file_type: FileType::Regular,
                size: Some(self.image.blob_len(blob).map_err(|damage| {
                    AccessorError::Io(self.image.damage_message(&damage).into())
                })?),
                executable,
            },
            Located::Symlink(_) => Stat {
                file_type: FileType::Symlink,
                size: None,
                executable: false,
            },
        }))
    }

    /// Lists the directory at `path`, its names as the image has them
    /// (bytes). Symlinks among its ancestors are followed, as the file
    /// system follows them for the source tree; a final symlink is
    /// refused, as by every [`SourceAccessor`] (callers resolve symlinks
    /// first).
    fn read_dir(&self, path: &CanonPath) -> std::result::Result<DirEntries, AccessorError> {
        match self.find(path, false, |tree| tree.read_dir(path))? {
            Found::Image(Some(Located::Dir(d))) => Ok(self.image.archived().dirs[d]
                .entries
                .iter()
                .map(|e| {
                    let ty = match e.kind {
                        ArchivedEntryKind::Dir(_) => FileType::Directory,
                        ArchivedEntryKind::File { .. } => FileType::Regular,
                        ArchivedEntryKind::Symlink { .. } => FileType::Symlink,
                    };
                    (BString::from(e.name.as_slice()), Some(ty))
                })
                .collect()),
            Found::Image(Some(Located::Symlink(_))) => {
                Err(AccessorError::PathIsASymlink(self.show_path(path)))
            }
            Found::Image(Some(_)) => Err(AccessorError::NotADirectory(self.show_path(path))),
            Found::Image(None) => Err(AccessorError::NotFound(self.show_path(path))),
            Found::Source(from_source) => Ok(from_source),
        }
    }

    /// Reads the file at `path`; symlinks are handled as in [`Self::read_dir`].
    fn read_file(&self, path: &CanonPath) -> std::result::Result<Vec<u8>, AccessorError> {
        match self.find(path, false, |tree| tree.read_file(path))? {
            Found::Image(Some(Located::File { blob, .. })) => {
                Ok(self.file_contents(path, blob)?.into_owned())
            }
            Found::Image(Some(Located::Symlink(_))) => {
                Err(AccessorError::PathIsASymlink(self.show_path(path)))
            }
            Found::Image(Some(_)) => Err(AccessorError::NotARegularFile(self.show_path(path))),
            Found::Image(None) => Err(AccessorError::NotFound(self.show_path(path))),
            Found::Source(from_source) => Ok(from_source),
        }
    }

    fn read_file_to(
        &self,
        path: &CanonPath,
        out: &mut dyn std::io::Write,
    ) -> std::result::Result<u64, AccessorError> {
        let data = match self.find(path, false, |tree| tree.read_file(path))? {
            Found::Image(Some(Located::File { blob, .. })) => self.file_contents(path, blob)?,
            Found::Image(Some(Located::Symlink(_))) => {
                return Err(AccessorError::PathIsASymlink(self.show_path(path)))
            }
            Found::Image(Some(_)) => {
                return Err(AccessorError::NotARegularFile(self.show_path(path)))
            }
            Found::Image(None) => return Err(AccessorError::NotFound(self.show_path(path))),
            Found::Source(from_source) => Cow::Owned(from_source),
        };
        out.write_all(&data)
            .map_err(|e| AccessorError::Io(e.to_string().into()))?;
        Ok(data.len() as u64)
    }

    /// The link's target verbatim, as bytes.
    fn read_link(&self, path: &CanonPath) -> std::result::Result<BString, AccessorError> {
        match self.find(path, false, |tree| tree.read_link(path))? {
            Found::Image(Some(Located::Symlink(target))) => Ok(BString::from(target)),
            Found::Image(Some(_)) => Err(AccessorError::NotASymlink(self.show_path(path))),
            Found::Image(None) => Err(AccessorError::NotFound(self.show_path(path))),
            Found::Source(from_source) => Ok(from_source),
        }
    }

    fn display_prefix(&self) -> &BStr {
        match &self.fallback {
            Some(tree) => tree.display_prefix(),
            None => BStr::new(""),
        }
    }

    fn show_path(&self, path: &CanonPath) -> BString {
        match &self.fallback {
            Some(tree) => tree.show_path(path),
            None => path.abs().to_owned(),
        }
    }

    /// Directories have their NAR hash stored. A file's or a symlink's
    /// is not: its NAR is small, and the caller hashes it. Nor is a
    /// damaged directory's: the caller then dumps the NAR itself, which
    /// fails on that directory with the damage.
    fn precomputed_nar_hash(&self, path: &CanonPath) -> Option<(Hash, u64)> {
        match self.image.lookup(path, false) {
            Lookup::Found(Located::Dir(d)) => {
                let dir = &self.image.archived().dirs[d];
                Some((
                    Hash::from_bytes(HashAlgo::Sha256, &dir.nar_hash),
                    dir.nar_size.to_native(),
                ))
            }
            Lookup::Found(Located::File { .. } | Located::Symlink(_))
            | Lookup::Missing
            | Lookup::Outside
            | Lookup::Damaged(_) => None,
        }
    }
}

/// This evaluator's symbol for each of an image's names, filled as
/// modules are linked: `id + 1`, or 0 for a name not interned yet. A
/// name seen before by any module of the image is then one array load
/// rather than a hash-table lookup. It never changes what interning
/// produces: a symbol, once created, stays.
#[derive(Default)]
pub(crate) struct NameSymbols {
    ids: RefCell<Vec<u32>>,
}

/// Links module `index` of `image`, mounted at `mount`, into the
/// evaluator. `display` is how the file is shown in messages and `path`
/// its absolute path on the root accessor (both bytes, as Nix shows
/// them). `Ok(None)` means the module is not servable from the image.
/// Damage found on the way fails the import with an error that names
/// the image and says what to do about it, and is kept as the image's
/// [`Image::damage`].
///
/// Nothing of the module is used before it has matched its checksum
/// (once per image): one XXH3 pass over its words and bytes.
///
/// The module's source text is not needed to run it: the image carries
/// the line table (decoded only when a position in the module is shown),
/// and the text is fetched through the root accessor (the image's own
/// blob in a full-tree image, otherwise the source tree) only if an
/// error excerpt is printed.
pub(crate) fn link_module<'a>(
    ev: &Evaluator<'a>,
    image: &Arc<Image>,
    names: &NameSymbols,
    index: usize,
    mount: &CanonPath,
    display: BString,
    path: &CanonPath,
) -> Result<Option<&'a LinkedCode<'a>>> {
    let corrupt = |why: String| damaged(ev, image, index, why);
    let parts = module_parts(image.archived(), index).map_err(corrupt)?;
    let module = parts.record;
    let origin = Origin::File {
        display,
        path: BString::from(path.as_bytes()),
    };
    let len = module.len.to_native();
    let decode_started = crate::platform::Instant::now();
    image.check_module(index, &parts).map_err(corrupt)?;
    // The module's bytes: its line table, then its string literals.
    let (bytes_start, lines_len) = (
        module.bytes_start.to_native() as usize,
        module.lines_len.to_native() as usize,
    );
    let bytes = parts.bytes;
    let lines: Box<crate::runtime::Loader<u32>> = {
        let image = image.clone();
        Box::new(move || {
            let packed = image
                .archived()
                .code
                .bytes
                .as_slice()
                .get(bytes_start..bytes_start + lines_len)?;
            codec::unpack_line_starts(packed, len).ok()
        })
    };
    let loader: Box<crate::runtime::Loader<u8>> = {
        let accessor = ev.accessor(ev.root_accessor);
        let at = path.clone();
        Box::new(move || accessor.read_file(&at).ok())
    };
    if let Some(error) = parts.error {
        return Err(module_error(ev, error, origin, len, lines, loader));
    }
    if module.kind.to_native() == fmt::MODULE_UNSUPPORTED {
        return Ok(None);
    }
    ev.seal();
    let started = crate::platform::Instant::now();
    let code = codec::ModuleCode::parse(parts.words, &bytes[lines_len..])
        .map_err(|e| corrupt(e.to_string()))?;
    let syms = intern_names(ev, image, names, code.new_names()).map_err(corrupt)?;
    let (id, base) = ev.register_source_lazy(origin, len, lines, loader);
    let tables = code
        .link(
            ev.code_bump,
            image.base_symbols(),
            &syms,
            len,
            mount.abs_or_empty(),
        )
        .map_err(|e| corrupt(e.to_string()))?;
    crate::stats::Stats::add_time(&ev.stats.image_decode_time, decode_started.elapsed());
    let linked = ev
        .link_image_module(id, base, tables)
        .map_err(|e| match e {
            ImageError::Corrupt(why) => corrupt(why),
            other => corrupt(other.to_string()),
        })?;
    crate::stats::Stats::add_time(&ev.stats.compile_time, started.elapsed());
    Ok(Some(linked))
}

/// A module's parts, resolved against the areas they lie in.
pub(crate) struct ModuleParts<'i> {
    /// Its path, as the module path table has it.
    pub path: &'i [u8],
    pub record: &'i ArchivedModule,
    /// Its line table, then its string literals.
    pub bytes: &'i [u8],
    /// Its code: empty unless it is a [`fmt::MODULE_CODE`].
    pub words: &'i [codec::Word],
    /// What importing it raises, for a [`fmt::MODULE_ERROR`].
    pub error: Option<&'i ArchivedModuleError>,
}

/// Resolves module `index`'s ranges, failing on one that does not lie
/// in its area or on an unknown kind. Its contents are not checked.
pub(crate) fn module_parts(
    image: &ArchivedImage,
    index: usize,
) -> std::result::Result<ModuleParts<'_>, String> {
    let record = image
        .modules
        .entries
        .get(index)
        .ok_or("module out of range")?;
    let path = image
        .modules
        .paths
        .get(index)
        .ok_or("module path out of range")?;
    let range = |start: &rkyv::rend::u32_le, len: &rkyv::rend::u32_le| {
        let start = start.to_native() as usize;
        start..start.saturating_add(len.to_native() as usize)
    };
    let bytes = image
        .code
        .bytes
        .as_slice()
        .get(range(&record.bytes_start, &record.bytes_len))
        .filter(|b| record.lines_len.to_native() as usize <= b.len())
        .ok_or("module bytes out of range")?;
    let (mut words, mut error): (&[codec::Word], _) = (&[], None);
    match record.kind.to_native() {
        fmt::MODULE_CODE => {
            words = image
                .code
                .words
                .as_slice()
                .get(range(&record.words_start, &record.words_len))
                .ok_or("module words out of range")?;
        }
        fmt::MODULE_ERROR => {
            error = Some(
                image
                    .modules
                    .errors
                    .get(record.error.to_native() as usize)
                    .ok_or("module error out of range")?,
            );
        }
        fmt::MODULE_UNSUPPORTED => {}
        other => return Err(format!("unknown module kind {other}")),
    }
    Ok(ModuleParts {
        path,
        record,
        bytes,
        words,
        error,
    })
}

/// The error importing module `index` raises when the image turns out
/// to be damaged: it names the image and says how to get past it. The
/// damage is kept on the image for its owner (see [`Image::damage`]).
#[cold]
#[inline(never)]
fn damaged(ev: &Evaluator<'_>, image: &Image, index: usize, why: String) -> crate::error::Error {
    ev.error(
        ErrorKind::Eval,
        image.report(image.module_damage(index, why)),
    )
}

/// Interns a module's new names in order, through the image's name
/// cache. Interning them in the order the compiler created them
/// reproduces the ids compiling the source would have produced. A name
/// is interned only once its chunk of the name table has matched its
/// checksum.
fn intern_names(
    ev: &Evaluator<'_>,
    image: &Image,
    cache: &NameSymbols,
    indices: impl ExactSizeIterator<Item = u32>,
) -> std::result::Result<Vec<Symbol>, String> {
    let mut ids = cache.ids.borrow_mut();
    if ids.is_empty() {
        *ids = vec![0; image.name_count()];
    }
    let mut out = Vec::with_capacity(indices.len());
    for i in indices {
        let slot = ids
            .get_mut(i as usize)
            .ok_or_else(|| format!("name index {i} out of range"))?;
        let sym = match *slot {
            0 => {
                let name = image.string(Strings::Names, i as usize)?;
                let sym = ev.intern(name);
                *slot = sym.0 + 1;
                sym
            }
            id => Symbol(id - 1),
        };
        out.push(sym);
    }
    Ok(out)
}

/// The error importing a module that does not compile raises, with its
/// position resolved like a parsed file's.
fn module_error(
    ev: &Evaluator<'_>,
    error: &ArchivedModuleError,
    origin: Origin,
    len: u32,
    lines: Box<crate::runtime::Loader<u32>>,
    loader: Box<crate::runtime::Loader<u8>>,
) -> crate::error::Error {
    match error {
        ArchivedModuleError::ParseError {
            msg,
            pos,
            defined_at,
        } => {
            let info = crate::runtime::rc::Rc::new(SourceInfo::lazy(origin, len, lines, loader));
            crate::import::parse_error(
                info,
                msg.as_slice(),
                pos.as_ref().map(|p| p.to_native()),
                defined_at.as_ref().map(|p| p.to_native()),
            )
        }
        ArchivedModuleError::UndefinedVariable { name, pos } => {
            // Registered like a parsed file, so the position resolves.
            let (_, base) = ev.register_source_lazy(origin, len, lines, loader);
            let p = pos.to_native();
            let at = if p == u32::MAX {
                SrcPos::NONE
            } else {
                SrcPos(base + p.min(len))
            };
            ev.error_at(
                ErrorKind::UndefinedVar,
                crate::compile::undefined_variable(name.as_slice()),
                at,
            )
        }
    }
}
