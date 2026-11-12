//! Compiling a source tree into an image.
//!
//! [`compile_tree`] walks the tree once (files are deduplicated by
//! SHA-256), hashes every directory as a NAR, parses and compiles every
//! `.nix` file, and serialises the result. Parsing, compiling and
//! hashing are independent per file or directory and run on all cores.
//!
//! Each module is compiled with its own symbol table that starts out as
//! a copy of the evaluator's base symbols, so the ids it produces are
//! "base id" below [`ImageCompileContext::base_symbols`]`.len()` and
//! "base + n-th new name" above it; see [`super::format`] for why that
//! keeps symbol order identical at load time. The new names of all
//! modules are pooled into one name table (nixpkgs' 41k modules create
//! 1.4 million names, of which 131k are distinct).

use crate::runtime::Bump;
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use brix_store::{
    AccessorError, CanonPath, DirEntries, FileType, Hash, HashAlgo, SourceAccessor, Stat,
};
use bstr::{BStr, BString, ByteSlice};
use rkyv::util::AlignedVec;

use super::format::{self as fmt, ROOT_MARKER};
use super::{codec, integrity, ImageCompileContext, ImageError};
use crate::compile::{self, CompileError, StaticEnv};
use crate::symbol::{Symbol, SymbolKeys, SymbolTable};

/// Options for [`compile_tree`].
#[derive(Clone, Debug, Default)]
pub struct CompileOptions {
    /// Worker threads for parsing, compiling and hashing. `0` means one
    /// per available core.
    pub threads: usize,
    /// Store the contents of every file, not only of the `.nix` files,
    /// so the image can serve the tree without the source at hand.
    pub include_tree: bool,
    /// Note on stderr every module that is not stored as code
    /// ([`crate::Config::timings`]).
    pub notes: bool,
}

/// Blobs shorter than this are stored as they are: deflate cannot shrink them.
const MIN_COMPRESS: usize = 64;

/// A directory while the tree is being collected.
struct DirBuild {
    path: CanonPath,
    entries: Vec<fmt::Entry>,
}

/// The tree collected from the accessor, before hashing and compiling.
struct Walk<'t> {
    accessor: &'t dyn SourceAccessor,
    /// Where the tree is on `accessor`; paths in the image are relative
    /// to it.
    root: &'t CanonPath,
    blobs: Vec<Vec<u8>>,
    blob_index: HashMap<[u8; 32], u32>,
    dirs: Vec<DirBuild>,
    /// `(path relative to root, blob)` of every `.nix` file; paths are
    /// bytes, as the tree's names are.
    nix_files: Vec<(Vec<u8>, u32)>,
}

impl Walk<'_> {
    fn blob(&mut self, data: Vec<u8>) -> u32 {
        let h = Hash::of(HashAlgo::Sha256, &data);
        let mut key = [0u8; 32];
        key.copy_from_slice(h.as_bytes());
        if let Some(&i) = self.blob_index.get(&key) {
            return i;
        }
        let i = self.blobs.len() as u32;
        self.blobs.push(data);
        self.blob_index.insert(key, i);
        i
    }

    /// Collects the directory at `path` (relative to the root) and
    /// everything below it, in pre-order: a directory's index is lower
    /// than its subdirectories'.
    fn walk_dir(&mut self, path: &CanonPath) -> Result<u32, ImageError> {
        let index = self.dirs.len() as u32;
        self.dirs.push(DirBuild {
            path: path.clone(),
            entries: Vec::new(),
        });
        let io = |e: AccessorError| ImageError::Io(e.to_string());
        let listing = self.accessor.read_dir(&self.root.join(path)).map_err(io)?;
        let mut entries = Vec::with_capacity(listing.len());
        // Names and symlink targets are kept as the accessor gives them:
        // bytes, which need not be UTF-8.
        for (name, _) in listing {
            let child = path
                .join_component(&name)
                .map_err(|e| ImageError::Io(e.to_string()))?;
            let at = self.root.join(&child);
            let st = self.accessor.lstat(&at).map_err(io)?;
            let kind = match st.file_type {
                FileType::Directory => fmt::EntryKind::Dir(self.walk_dir(&child)?),
                FileType::Regular => {
                    // Lengths and offsets in an image are 32-bit: a file
                    // that does not fit is declined, before reading it
                    // when its size is known.
                    let too_large = |len: u64| len > u64::from(u32::MAX);
                    let declined = || {
                        ImageError::Compile(format!(
                            "'{}' is 4 GiB or larger, which an image cannot hold",
                            self.accessor.show_path(&at)
                        ))
                    };
                    if st.size.is_some_and(too_large) {
                        return Err(declined());
                    }
                    let data = self.accessor.read_file(&at).map_err(io)?;
                    if too_large(data.len() as u64) {
                        return Err(declined());
                    }
                    let blob = self.blob(data);
                    if name.ends_with(b".nix") {
                        self.nix_files.push((child.rel().to_vec(), blob));
                    }
                    fmt::EntryKind::File {
                        blob,
                        executable: st.executable,
                    }
                }
                FileType::Symlink => fmt::EntryKind::Symlink {
                    target: self.accessor.read_link(&at).map_err(io)?.into(),
                },
                other => {
                    return Err(ImageError::Io(format!(
                        "'{}' has file type {other:?}, which cannot be put in an image (or a NAR)",
                        self.accessor.show_path(&at)
                    )))
                }
            };
            entries.push(fmt::Entry {
                name: name.into(),
                kind,
            });
        }
        // `read_dir` returns a `BTreeMap`, so entries are already in byte
        // order, which is the order a NAR lists them in.
        self.dirs[index as usize].entries = entries;
        Ok(index)
    }
}

/// A file's length as an image stores it. [`Walk::walk_dir`] declines
/// files that do not fit.
fn file_len(data: &[u8]) -> u32 {
    u32::try_from(data.len()).expect("invariant: the walk declined files of 4 GiB or more")
}

/// The collected tree, readable as a [`SourceAccessor`] so that the NAR
/// hashes can be computed with the same code that hashes any other tree.
struct TreeView<'w> {
    blobs: &'w [Vec<u8>],
    dirs: &'w [DirBuild],
}

impl TreeView<'_> {
    fn lookup(&self, path: &CanonPath) -> Option<Located<'_>> {
        let mut dir = 0usize;
        let comps: Vec<&[u8]> = path.components().map(|c| c.as_bytes()).collect();
        if comps.is_empty() {
            return Some(Located::Dir(0));
        }
        for (k, comp) in comps.iter().enumerate() {
            let entries = &self.dirs[dir].entries;
            let i = entries
                .binary_search_by(|e| e.name.as_slice().cmp(comp))
                .ok()?;
            let last = k + 1 == comps.len();
            match &entries[i].kind {
                fmt::EntryKind::Dir(d) => {
                    dir = *d as usize;
                    if last {
                        return Some(Located::Dir(dir));
                    }
                }
                fmt::EntryKind::File { blob, executable } => {
                    return last.then_some(Located::File {
                        blob: *blob,
                        executable: *executable,
                    });
                }
                fmt::EntryKind::Symlink { target } => {
                    return last.then_some(Located::Symlink(target.as_slice()));
                }
            }
        }
        Some(Located::Dir(dir))
    }
}

/// Where a path leads in the collected tree.
enum Located<'a> {
    Dir(usize),
    File { blob: u32, executable: bool },
    Symlink(&'a [u8]),
}

impl SourceAccessor for TreeView<'_> {
    fn maybe_lstat(&self, path: &CanonPath) -> Result<Option<Stat>, AccessorError> {
        Ok(self.lookup(path).map(|l| match l {
            Located::Dir(_) => Stat {
                file_type: FileType::Directory,
                size: None,
                executable: false,
            },
            Located::File { blob, executable } => Stat {
                file_type: FileType::Regular,
                size: Some(self.blobs[blob as usize].len() as u64),
                executable,
            },
            Located::Symlink(_) => Stat {
                file_type: FileType::Symlink,
                size: None,
                executable: false,
            },
        }))
    }

    fn read_dir(&self, path: &CanonPath) -> Result<DirEntries, AccessorError> {
        match self.lookup(path) {
            Some(Located::Dir(d)) => Ok(self.dirs[d]
                .entries
                .iter()
                .map(|e| {
                    let ty = match e.kind {
                        fmt::EntryKind::Dir(_) => FileType::Directory,
                        fmt::EntryKind::File { .. } => FileType::Regular,
                        fmt::EntryKind::Symlink { .. } => FileType::Symlink,
                    };
                    (BString::from(e.name.as_slice()), Some(ty))
                })
                .collect::<BTreeMap<_, _>>()),
            Some(_) => Err(AccessorError::NotADirectory(path.abs().to_owned())),
            None => Err(AccessorError::NotFound(path.abs().to_owned())),
        }
    }

    fn read_file(&self, path: &CanonPath) -> Result<Vec<u8>, AccessorError> {
        match self.lookup(path) {
            Some(Located::File { blob, .. }) => Ok(self.blobs[blob as usize].clone()),
            Some(_) => Err(AccessorError::NotARegularFile(path.abs().to_owned())),
            None => Err(AccessorError::NotFound(path.abs().to_owned())),
        }
    }

    fn read_file_to(
        &self,
        path: &CanonPath,
        out: &mut dyn std::io::Write,
    ) -> Result<u64, AccessorError> {
        match self.lookup(path) {
            Some(Located::File { blob, .. }) => {
                let data = &self.blobs[blob as usize];
                out.write_all(data)
                    .map_err(|e| AccessorError::Io(e.to_string().into()))?;
                Ok(data.len() as u64)
            }
            Some(_) => Err(AccessorError::NotARegularFile(path.abs().to_owned())),
            None => Err(AccessorError::NotFound(path.abs().to_owned())),
        }
    }

    fn read_link(&self, path: &CanonPath) -> Result<BString, AccessorError> {
        match self.lookup(path) {
            Some(Located::Symlink(target)) => Ok(BString::from(target)),
            Some(_) => Err(AccessorError::NotASymlink(path.abs().to_owned())),
            None => Err(AccessorError::NotFound(path.abs().to_owned())),
        }
    }

    fn display_prefix(&self) -> &BStr {
        BStr::new("")
    }
}

/// Runs `job(i)` for every `i` in `0..n` on `threads` threads, collecting
/// the results in index order. Panics in jobs propagate. Where there are
/// no threads (see [`crate::platform`]) the jobs run here, in order.
#[allow(
    clippy::disallowed_methods,
    reason = "threads are spawned only where platform::HAS_THREADS says they can be"
)]
fn parallel_map<T: Send>(n: usize, threads: usize, job: impl Fn(usize) -> T + Sync) -> Vec<T> {
    if !crate::platform::HAS_THREADS {
        return (0..n).map(job).collect();
    }
    let threads = threads.clamp(1, n.max(1));
    let next = AtomicUsize::new(0);
    let results: Mutex<Vec<Option<T>>> = Mutex::new((0..n).map(|_| None).collect());
    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                if i >= n {
                    break;
                }
                let r = job(i);
                results.lock().expect("results poisoned")[i] = Some(r);
            });
        }
    });
    results
        .into_inner()
        .expect("results poisoned")
        .into_iter()
        .map(|r| r.expect("every job ran"))
        .collect()
}

/// A per-module symbol table: the evaluator's base symbols, then the
/// module's own in creation order.
struct ModuleSymbols<'b> {
    table: SymbolTable<'b>,
    base: u32,
}

impl<'b> ModuleSymbols<'b> {
    fn new(bump: &'b Bump, ctx: &ImageCompileContext) -> Result<ModuleSymbols<'b>, ImageError> {
        let keys = SymbolKeys::new(bump);
        let mut table = SymbolTable::new(keys);
        for name in &ctx.base_symbols {
            table.intern(name);
        }
        if table.len() != ctx.base_symbols.len() {
            return Err(ImageError::Compile(format!(
                "the base symbol list is inconsistent: {} names interned to {} symbols",
                ctx.base_symbols.len(),
                table.len()
            )));
        }
        Ok(ModuleSymbols {
            table,
            base: ctx.base_symbols.len() as u32,
        })
    }

    fn globals(&mut self, ctx: &ImageCompileContext) -> StaticEnv<'static> {
        let vars: Vec<(Symbol, u32)> = ctx
            .globals
            .iter()
            .map(|(name, slot)| (self.table.intern(name), *slot))
            .collect();
        StaticEnv::new(None, vars)
    }

    /// The names created after the base, in creation order.
    fn new_symbols(&self) -> Vec<Vec<u8>> {
        (self.base..self.table.len() as u32)
            .map(|i| self.table.name(Symbol(i)).to_vec())
            .collect()
    }
}

/// A module compiled but not yet encoded: its code still refers to the
/// module's own new names, listed here in creation order.
enum Compiled {
    Code {
        code: Box<crate::ir::Code>,
        new_symbols: Vec<Vec<u8>>,
    },
    Error(fmt::ModuleError),
    /// Served from source: Nix logs a file's parse warnings every time it
    /// parses it, which an image would skip.
    Unsupported,
}

/// Compiles one `.nix` file, whose path relative to the tree's root is
/// `rel_path` (bytes, as the tree's names are).
fn compile_module(
    rel_path: &[u8],
    source: &[u8],
    ctx: &ImageCompileContext,
) -> Result<Compiled, ImageError> {
    let bump = Bump::new();
    let mut symbols = ModuleSymbols::new(&bump, ctx)?;
    let globals = symbols.globals(ctx);
    // Relative path literals resolve against the file's directory under
    // the marker root; the loader puts the real mount point in.
    let dir: Vec<u8> = match rel_path.iter().rposition(|&b| b == b'/') {
        Some(slash) => [ROOT_MARKER.as_bytes(), b"/".as_slice(), &rel_path[..slash]].concat(),
        None => ROOT_MARKER.as_bytes().to_vec(),
    };
    let mut warnings = Vec::new();
    let expr = match brix_syntax::parse(source, dir.as_slice(), &ctx.parse_options, &mut warnings) {
        // Nix logs a file's parse warnings each time it parses the file;
        // such a file is served from source, where they are.
        _ if !warnings.is_empty() => return Ok(Compiled::Unsupported),
        Ok(e) => e,
        Err(e) => {
            return Ok(Compiled::Error(fmt::ModuleError::ParseError {
                msg: e.msg.into(),
                pos: e.pos.map(|p| p.0),
                defined_at: e.defined_at.map(|p| p.0),
            }))
        }
    };
    let code = match compile::compile(&expr, &mut symbols.table, &globals) {
        Ok(c) => c,
        Err(CompileError::UndefinedVariable { name, pos }) => {
            return Ok(Compiled::Error(fmt::ModuleError::UndefinedVariable {
                name: name.into(),
                pos: pos.0,
            }))
        }
    };
    // A path literal that climbs out of the tree loses the marker; the
    // encoder declines such a module, which is then served from source,
    // where the real base directory gives Nix's answer.
    Ok(Compiled::Code {
        new_symbols: symbols.new_symbols(),
        code: Box::new(code),
    })
}

/// Stores `data` as a blob: compressed when that helps.
fn encode_blob(data: &[u8]) -> fmt::Blob {
    let raw_len = file_len(data);
    if data.len() >= MIN_COMPRESS {
        let packed = super::compress::pack(data);
        if packed.len() < data.len() {
            return fmt::Blob {
                raw_len,
                encoding: fmt::BLOB_DEFLATE,
                data: packed,
            };
        }
    }
    fmt::Blob {
        raw_len,
        encoding: fmt::BLOB_RAW,
        data: data.to_vec(),
    }
}

/// Compiles the tree at `root` on `accessor` into an image for
/// evaluators matching `ctx`. The result is 16-byte aligned, as the
/// loader needs; write it to a file as is.
pub fn compile_tree(
    accessor: &dyn SourceAccessor,
    root: &CanonPath,
    ctx: &ImageCompileContext,
    options: &CompileOptions,
) -> Result<AlignedVec, ImageError> {
    let image = build_image(accessor, root, ctx, options)?;
    to_bytes(&image, &ctx.fingerprint)
}

/// An image's file contents: the preamble, the archive, and the
/// checksums of the archive's parts. The checksums are computed from the
/// serialised archive, by the functions the loader compares them with
/// (see `integrity`); so they vouch for whatever `image` holds, and an
/// image value damaged on purpose (as the tests do) is caught by the
/// loader's other checks.
#[doc(hidden)]
pub fn to_bytes(image: &fmt::Image, fingerprint: &[u8; 32]) -> Result<AlignedVec, ImageError> {
    let archive = rkyv::to_bytes::<rkyv::rancor::Error>(image)
        .map_err(|e| ImageError::Compile(format!("serialising the image: {e}")))?;
    let archived = rkyv::access::<fmt::ArchivedImage, rkyv::rancor::Error>(&archive)
        .map_err(|e| ImageError::Compile(format!("reading the serialised image back: {e}")))?;
    let checksums = integrity::table(archived, &archive);
    let mut out = AlignedVec::with_capacity(fmt::PREAMBLE_LEN + archive.len() + checksums.len());
    out.extend_from_slice(&fmt::write_preamble(fingerprint, archive.len() as u64));
    out.extend_from_slice(&archive);
    out.extend_from_slice(&checksums);
    Ok(out)
}

/// [`compile_tree`] up to, but not including, serialisation: the image
/// as a value, for tools that inspect or measure it.
#[doc(hidden)]
pub fn build_image(
    accessor: &dyn SourceAccessor,
    root: &CanonPath,
    ctx: &ImageCompileContext,
    options: &CompileOptions,
) -> Result<fmt::Image, ImageError> {
    let threads = if options.threads == 0 {
        std::thread::available_parallelism().map_or(4, |n| n.get())
    } else {
        options.threads
    };
    // The root must be a directory: an image is a tree.
    match accessor.lstat(root) {
        Ok(st) if st.file_type == FileType::Directory => {}
        Ok(_) => {
            return Err(ImageError::Io(format!(
                "'{}' is not a directory",
                accessor.show_path(root)
            )))
        }
        Err(e) => return Err(ImageError::Io(e.to_string())),
    }
    // Paths inside the image are relative to `root`.
    let mut walk = Walk {
        accessor,
        root,
        blobs: vec![Vec::new()],
        blob_index: HashMap::new(),
        dirs: Vec::new(),
        nix_files: Vec::new(),
    };
    walk.blob_index.insert(
        {
            let mut k = [0u8; 32];
            k.copy_from_slice(Hash::of(HashAlgo::Sha256, &[]).as_bytes());
            k
        },
        0,
    );
    walk.walk_dir(&CanonPath::root())?;
    let Walk {
        blobs,
        dirs,
        mut nix_files,
        ..
    } = walk;
    nix_files.sort();

    // NAR hashes of every directory.
    let view = TreeView {
        blobs: &blobs,
        dirs: &dirs,
    };
    let hashes = parallel_map(dirs.len(), threads, |i| {
        brix_store::nar::hash(&view, &dirs[i].path, HashAlgo::Sha256)
            .map_err(|e| ImageError::Io(e.to_string()))
    });
    let mut image_dirs = Vec::with_capacity(dirs.len());
    for (d, h) in dirs.into_iter().zip(hashes) {
        let (hash, size) = h?;
        let mut nar_hash = [0u8; 32];
        nar_hash.copy_from_slice(hash.as_bytes());
        image_dirs.push(fmt::Dir {
            entries: d.entries,
            nar_hash,
            nar_size: size,
        });
    }

    // Modules: compile in parallel, then pool the names and encode. The
    // name table is filled in module order (modules are sorted by path),
    // so the image does not depend on scheduling.
    let compiled = parallel_map(nix_files.len(), threads, |i| {
        let (path, blob) = &nix_files[i];
        compile_module(path, &blobs[*blob as usize], ctx)
    });
    let mut names = fmt::StringTable::default();
    let mut name_index: HashMap<Vec<u8>, u32> = HashMap::new();
    let mut compiled_ok = Vec::with_capacity(compiled.len());
    for c in compiled {
        compiled_ok.push(c?);
    }
    let indices: Vec<Vec<u32>> = compiled_ok
        .iter()
        .map(|c| match c {
            Compiled::Code { new_symbols, .. } => new_symbols
                .iter()
                .map(|name| {
                    *name_index
                        .entry(name.clone())
                        .or_insert_with_key(|name| names.push(name))
                })
                .collect(),
            _ => Vec::new(),
        })
        .collect();
    let encoded = parallel_map(compiled_ok.len(), threads, |i| {
        let (path, blob) = &nix_files[i];
        let lines =
            codec::pack_line_starts(&brix_syntax::source::line_starts(&blobs[*blob as usize]));
        let code = match &compiled_ok[i] {
            Compiled::Code { code, .. } => {
                // A module the encoding cannot hold is parsed from source.
                match codec::encode(code, &indices[i]) {
                    Ok(code) => Some(code),
                    Err(why) => {
                        if options.notes {
                            eprintln!("brix: {} is not stored as code: {}", path.as_bstr(), why.0);
                        }
                        None
                    }
                }
            }
            _ => None,
        };
        (lines, code)
    });
    let mut modules = fmt::ModuleTable::default();
    let mut code_area = fmt::CodeArea::default();
    let offset = |len: usize| {
        u32::try_from(len)
            .map_err(|_| ImageError::Compile("more than 4 GiB of module code in one image".into()))
    };
    for (((path, blob), compiled), (lines, code)) in
        nix_files.into_iter().zip(compiled_ok).zip(encoded)
    {
        let mut module = fmt::Module {
            blob,
            len: file_len(&blobs[blob as usize]),
            words_start: offset(code_area.words.len())?,
            bytes_start: offset(code_area.bytes.len())?,
            lines_len: offset(lines.len())?,
            ..Default::default()
        };
        code_area.bytes.extend_from_slice(&lines);
        match (compiled, code) {
            (Compiled::Code { .. }, Some(code)) => {
                module.kind = fmt::MODULE_CODE;
                module.words_len = offset(code.words.len())?;
                code_area.words.extend_from_slice(&code.words);
                code_area.bytes.extend_from_slice(&code.strings);
            }
            (Compiled::Error(e), _) => {
                module.kind = fmt::MODULE_ERROR;
                module.error = modules.errors.len() as u32;
                modules.errors.push(e);
            }
            (Compiled::Code { .. }, None) | (Compiled::Unsupported, _) => {
                module.kind = fmt::MODULE_UNSUPPORTED;
            }
        }
        module.bytes_len = offset(code_area.bytes.len() - module.bytes_start as usize)?;
        modules.paths.push(&path);
        modules.entries.push(module);
    }
    // The areas are addressed with 32-bit offsets.
    offset(code_area.words.len())?;
    offset(code_area.bytes.len())?;

    // Blobs: stored only when the image is to carry the tree.
    let image_blobs = parallel_map(blobs.len(), threads, |i| {
        if options.include_tree {
            encode_blob(&blobs[i])
        } else {
            fmt::Blob {
                raw_len: file_len(&blobs[i]),
                encoding: fmt::BLOB_ABSENT,
                data: Vec::new(),
            }
        }
    });

    Ok(fmt::Image {
        header: fmt::Header {
            base_symbols: ctx.base_symbols.len() as u32,
            nar_hash: image_dirs[0].nar_hash,
            nar_size: image_dirs[0].nar_size,
            full_tree: options.include_tree,
        },
        blobs: image_blobs,
        dirs: image_dirs,
        names,
        modules,
        code: code_area,
    })
}
