//! Compiled flake-input images on disk: the cache under
//! `<cache dir>/compiled/`, explicitly supplied images, and compiling a
//! tree into either.
//!
//! An image is keyed by the tree's `narHash` and the evaluator's
//! fingerprint (`<nix32 narHash>-<16 hex of fingerprint>.brix`), so a
//! brix whose compiler output changed never picks up a stale file. Files
//! are written to a temporary name and renamed into place; two brix
//! processes compiling the same input race harmlessly.

#[cfg(brix_parallel)]
use brix_eval::parallel_support::cell::RefCell;
#[cfg(not(brix_parallel))]
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use brix_eval::image::writer::CompileOptions;
use brix_eval::image::{compile_tree, Image, ImageCompileContext, ImageError};
use brix_eval::{FetchAttr, FetchedTree};
use brix_flake::{Activity, ActivityKind, Progress, Waiting};
use brix_store::{CanonPath, FileType, Hash, HashAlgo, HashFormat, SourceAccessor, StorePath};

use brix_flake::FlakeRef;

use crate::config::CompiledInputsConfig;
use crate::error::{Error, Result};
use crate::flake;
use crate::session::{store_hash, Flake, Session};

/// What compiling an input produced.
#[derive(Clone, Debug)]
pub struct CompiledImage {
    /// Where the image was written.
    pub path: PathBuf,
    pub bytes: u64,
    pub modules: usize,
    pub nar_hash: Hash,
    pub duration: std::time::Duration,
}

/// Images available to a session.
pub(crate) struct ImageStore {
    enabled: bool,
    dir: PathBuf,
    /// Explicit images by `narHash` (nix32) and by name; names are
    /// resolved to hashes when a flake's lock is known.
    explicit_by_hash: RefCell<HashMap<String, PathBuf>>,
    explicit_by_name: Vec<(String, PathBuf)>,
    min_nix_files: usize,
    /// Images already opened (or found absent) in this session.
    loaded: RefCell<HashMap<StorePath, Option<Arc<Image>>>>,
    /// The images this session has served from cache files, with their
    /// files.
    served: RefCell<Vec<(PathBuf, Arc<Image>)>>,
    /// Where compiling an image is reported: it takes seconds for nixpkgs.
    progress: Arc<dyn Progress>,
    /// The experimental features `narHash` keys are read with.
    xp: brix_store::XpSettings,
    /// [`brix_eval::Config::timings`]: say which images are used,
    /// compiled and replaced.
    timings: bool,
}

impl ImageStore {
    pub(crate) fn new(
        config: &CompiledInputsConfig,
        default_cache_dir: &Path,
        progress: Arc<dyn Progress>,
        xp: brix_store::XpSettings,
        timings: bool,
    ) -> ImageStore {
        let mut by_hash = HashMap::new();
        let mut by_name = Vec::new();
        for (key, path) in &config.explicit {
            match Hash::parse_any(key, Some(HashAlgo::Sha256), xp) {
                Ok(h) => {
                    by_hash.insert(h.to_string(HashFormat::Nix32, false), path.clone());
                }
                Err(_) => by_name.push((key.clone(), path.clone())),
            }
        }
        ImageStore {
            enabled: config.enabled,
            dir: config
                .cache_dir
                .clone()
                .unwrap_or_else(|| default_cache_dir.to_path_buf())
                .join("compiled"),
            explicit_by_hash: RefCell::new(by_hash),
            explicit_by_name: by_name,
            min_nix_files: config.min_nix_files,
            loaded: RefCell::new(HashMap::new()),
            served: RefCell::new(Vec::new()),
            progress,
            xp,
            timings,
        }
    }

    /// Explicit images were given by input name: now that the lock says
    /// which `narHash` each name has, key them by hash.
    pub(crate) fn resolve_names(&self, names_to_hashes: &[(String, Hash)]) {
        if self.explicit_by_name.is_empty() {
            return;
        }
        let mut by_hash = self.explicit_by_hash.borrow_mut();
        for (name, path) in &self.explicit_by_name {
            for (lock_name, hash) in names_to_hashes {
                if lock_name == name {
                    by_hash.insert(hash.to_string(HashFormat::Nix32, false), path.clone());
                }
            }
        }
    }

    /// Where the cache keeps the image for a tree.
    pub(crate) fn cache_path(&self, nar_hash: &Hash, ctx: &ImageCompileContext) -> PathBuf {
        self.dir.join(format!(
            "{}-{}.brix",
            nar_hash.to_string(HashFormat::Nix32, false),
            &ctx.fingerprint_hex()[..16]
        ))
    }

    /// The image for a mounted tree: an explicit one (which must open,
    /// or the evaluation fails), a cached one, or one compiled now if the
    /// tree is big enough to be worth it. Cache trouble never fails an
    /// evaluation: the source is used and the file replaced.
    pub(crate) fn image_for(
        &self,
        tree: &FetchedTree,
        ctx: &ImageCompileContext,
    ) -> std::result::Result<Option<Arc<Image>>, String> {
        let nar_hash = match tree.attrs.get("narHash") {
            Some(FetchAttr::String(s)) => {
                match Hash::parse_any(s, Some(HashAlgo::Sha256), self.xp) {
                    Ok(h) => h,
                    Err(_) => return Ok(None),
                }
            }
            _ => return Ok(None),
        };
        let nix32 = nar_hash.to_string(HashFormat::Nix32, false);
        let explicit = self.explicit_by_hash.borrow().get(&nix32).cloned();
        if let Some(path) = explicit {
            if let Some(cached) = self.loaded.borrow().get(&tree.store_path) {
                return Ok(cached.clone());
            }
            let img =
                open_image(&path, Load::Read, Some(&ctx.fingerprint), &nar_hash).map_err(|e| {
                    format!(
                        "cannot use the compiled image '{}' for '{}': {e}",
                        path.display(),
                        tree.locked_url
                    )
                })?;
            self.note(&format!(
                "'{}' served from the compiled image {}",
                tree.locked_url,
                path.display()
            ));
            let img = Arc::new(img);
            self.loaded
                .borrow_mut()
                .insert(tree.store_path.clone(), Some(img.clone()));
            return Ok(Some(img));
        }
        if !self.enabled {
            return Ok(None);
        }
        if let Some(cached) = self.loaded.borrow().get(&tree.store_path) {
            return Ok(cached.clone());
        }
        let result = self.cached_or_compiled(tree, ctx, &nar_hash);
        self.loaded
            .borrow_mut()
            .insert(tree.store_path.clone(), result.clone());
        Ok(result)
    }

    fn cached_or_compiled(
        &self,
        tree: &FetchedTree,
        ctx: &ImageCompileContext,
        nar_hash: &Hash,
    ) -> Option<Arc<Image>> {
        let path = self.cache_path(nar_hash, ctx);
        match open_image(&path, Load::Map, Some(&ctx.fingerprint), nar_hash) {
            Ok(img) => {
                self.note(&format!(
                    "'{}' served from the compiled image {}",
                    tree.locked_url,
                    path.display()
                ));
                let img = Arc::new(img);
                self.served.borrow_mut().push((path, img.clone()));
                return Some(img);
            }
            Err(ImageError::Io(_)) => {}
            Err(e) => {
                // A stale or damaged cache file is replaced below.
                self.note(&format!("replacing compiled image {}: {e}", path.display()));
            }
        }
        if !worth_compiling(&*tree.accessor, self.min_nix_files) {
            return None;
        }
        // The cache serves trees that are at hand, so their other files
        // need not be stored.
        let compiled = {
            let _waiting = Waiting::new(&*self.progress);
            let _activity = Activity::new(
                &*self.progress,
                ActivityKind::Other,
                &format!("compiling '{}'", tree.locked_url),
            );
            compile_to(&*tree.accessor, ctx, &path, false, self.timings).and_then(|info| {
                Ok((
                    info,
                    open_image(&path, Load::Map, Some(&ctx.fingerprint), nar_hash)?,
                ))
            })
        };
        match compiled {
            Ok((info, image)) => {
                self.note(&format!(
                    "'{}' compiled to {} ({} modules, {} bytes) in {:?}",
                    tree.locked_url,
                    path.display(),
                    info.modules,
                    info.bytes,
                    info.duration
                ));
                let image = Arc::new(image);
                self.served.borrow_mut().push((path, image.clone()));
                Some(image)
            }
            Err(e) => {
                crate::log::stderr(&format!(
                    "warning: could not compile '{}' into an image: {e}",
                    tree.locked_url
                ));
                None
            }
        }
    }

    /// After an evaluation error: the cache files of the images in which
    /// damage was found in use ([`Image::damage`]: damage the checks made
    /// when an image is opened do not see, since module code is validated
    /// as it is linked and every other part checked against its checksum
    /// when first used) are deleted, so that the next evaluation compiles
    /// them again rather than fail the same way. Only the damaged images'
    /// own files go; images the user named are never deleted (they are
    /// not served from the cache), the error says what is wrong with them.
    pub(crate) fn forget_damaged(&self) {
        let mut served = self.served.borrow_mut();
        let (damaged, sound): (Vec<_>, Vec<_>) = std::mem::take(&mut *served)
            .into_iter()
            .partition(|(_, image)| image.damage().is_some());
        *served = sound;
        drop(served);
        for (path, _) in damaged {
            match std::fs::remove_file(&path) {
                Ok(()) => crate::log::stderr(&format!(
                    "warning: deleted the damaged compiled image '{}'; it is compiled again when next needed",
                    path.display()
                )),
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => crate::log::stderr(&format!(
                    "warning: could not delete the damaged compiled image '{}' (delete it, or run with --no-compiled-inputs): {err}",
                    path.display()
                )),
            }
        }
    }
}

/// Whether the tree has at least `min` `.nix` files (stops counting at
/// `min`).
fn worth_compiling(accessor: &dyn SourceAccessor, min: usize) -> bool {
    fn count(acc: &dyn SourceAccessor, dir: &CanonPath, min: usize, n: &mut usize) {
        let Ok(entries) = acc.read_dir(dir) else {
            return;
        };
        for (name, ty) in entries {
            if *n >= min {
                return;
            }
            let Ok(child) = dir.join_component(&name) else {
                continue;
            };
            let ty = match ty {
                Some(t) => t,
                None => match acc.maybe_lstat(&child) {
                    Ok(Some(st)) => st.file_type,
                    _ => continue,
                },
            };
            match ty {
                FileType::Directory => count(acc, &child, min, n),
                FileType::Regular if name.ends_with(b".nix") => *n += 1,
                _ => {}
            }
        }
    }
    let mut n = 0;
    count(accessor, &CanonPath::root(), min, &mut n);
    n >= min
}

/// How an image file is brought into memory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Load {
    /// Memory-mapped, so that only the modules imported are read: for
    /// brix's own cache files, which are never modified in place.
    Map,
    /// Read whole: for a file the user named, which brix does not own
    /// and which may change while it runs.
    Read,
}

/// Opens an image file, labelled with its path for the messages that name
/// it. `nar_hash` must be the tree's: an image of another tree at this
/// path is corrupt.
pub(crate) fn open_image(
    path: &Path,
    load: Load,
    fingerprint: Option<&[u8; 32]>,
    nar_hash: &Hash,
) -> std::result::Result<Image, ImageError> {
    let io = |e: std::io::Error| ImageError::Io(e.to_string());
    let image = match load {
        Load::Map => {
            let file = std::fs::File::open(path).map_err(io)?;
            // SAFETY: `Load::Map` is only used for cache entries, which
            // brix writes to a temporary name and renames into place and
            // never modifies afterwards: a replacement is renamed over the
            // entry and a damaged one is deleted, both of which leave this
            // mapping intact. Only a writer outside brix that truncates or
            // rewrites the file in place could change the mapped bytes
            // (or fault on access), the standard memory-map caveat.
            let map = unsafe { memmap2::Mmap::map(&file) }.map_err(io)?;
            Image::open(map, fingerprint)?
        }
        Load::Read => Image::open(std::fs::read(path).map_err(io)?, fingerprint)?,
    };
    if image.nar_hash() != *nar_hash {
        return Err(ImageError::Corrupt(format!(
            "image is of a tree with NAR hash {}, expected {}",
            image.nar_hash().to_string(HashFormat::Sri, true),
            nar_hash.to_string(HashFormat::Sri, true)
        )));
    }
    Ok(image.with_label(path.display().to_string()))
}

/// Compiling flake inputs: the trees a session fetches, into images.
impl Session {
    /// Compiles the tree a flake reference denotes into an image (see
    /// `brix_eval::image`), written to `out_dir` if given and to the
    /// image cache otherwise, and returns where it went.
    pub fn compile_input(
        &self,
        flake_ref: &FlakeRef,
        out_dir: Option<&std::path::Path>,
        include_tree: bool,
    ) -> Result<CompiledImage> {
        self.with(|ctx| {
            let (tree, _, _) = ctx.ev.with_context(|runtime| {
                let deps = flake::Deps::new(runtime, ctx.host);
                deps.fetch_and_mount(&flake_ref.input, brix_flake::UseRegistries::No, true)
            })?;
            let nar_hash = tree
                .input
                .nar_hash()
                .map_err(Error::Flake)?
                .ok_or_else(|| Error::Store("the fetched tree has no NAR hash".into()))?;
            let nar_hash = store_hash(&nar_hash);
            let source = ctx
                .host
                .tree(tree.root.lockable_store_path())
                .ok_or_else(|| {
                    Error::Store(
                        format!(
                            "the tree at {} was not fetched by this session",
                            tree.root.lockable_store_path()
                        )
                        .into(),
                    )
                })?;
            let image_ctx = ctx.ev.services().image_compile_context();
            let path = match out_dir {
                Some(dir) => dir.join(
                    ctx.host
                        .images()
                        .cache_path(&nar_hash, image_ctx)
                        .file_name()
                        .expect("cache paths have file names"),
                ),
                None => ctx.host.images().cache_path(&nar_hash, image_ctx),
            };
            Ok(crate::compiled::compile_to(
                &*source.accessor,
                image_ctx,
                &path,
                include_tree,
                ctx.ev.services().config.timings,
            )?)
        })
    }

    /// Compiles the locked inputs of a flake (all of them, or those named
    /// in `only` by their lock path such as `nixpkgs` or
    /// `crate2nix/nixpkgs`) into images. Returns each input's lock path
    /// with its image; inputs sharing a tree are compiled once.
    pub fn compile_flake_inputs(
        &self,
        flake: &Flake<'_>,
        only: Option<&[String]>,
        out_dir: Option<&std::path::Path>,
        include_tree: bool,
    ) -> Result<Vec<(String, CompiledImage)>> {
        let lock = &flake.locked().lock_file;
        let mut done: std::collections::HashMap<String, CompiledImage> =
            std::collections::HashMap::new();
        let mut out = Vec::new();
        for (path, edge) in lock.all_inputs() {
            let name = path.join("/");
            if let Some(only) = only {
                if !only.contains(&name) {
                    continue;
                }
            }
            let brix_flake::Edge::Node(id) = edge else {
                continue;
            };
            let Some(locked) = &lock.node(id).locked else {
                continue;
            };
            let key = locked.locked_ref.input.to_string();
            let image = match done.get(&key) {
                Some(img) => img.clone(),
                None => {
                    let img = self.compile_input(&locked.locked_ref, out_dir, include_tree)?;
                    done.insert(key, img.clone());
                    img
                }
            };
            out.push((name, image));
        }
        if let Some(only) = only {
            for n in only {
                if !out.iter().any(|(name, _)| name == n) {
                    return Err(Error::Usage(
                        format!("the flake has no locked input '{n}'").into(),
                    ));
                }
            }
        }
        Ok(out)
    }
}

/// Compiles the tree on `accessor` into an image at `path` (atomically:
/// temporary file, then rename).
pub(crate) fn compile_to(
    accessor: &dyn SourceAccessor,
    ctx: &ImageCompileContext,
    path: &Path,
    include_tree: bool,
    notes: bool,
) -> std::result::Result<CompiledImage, ImageError> {
    let started = Instant::now();
    let bytes = compile_tree(
        accessor,
        &CanonPath::root(),
        ctx,
        &CompileOptions {
            threads: 0,
            include_tree,
            notes,
        },
    )?;
    let (modules, nar_hash) = {
        let image = Image::open(bytes.clone(), Some(&ctx.fingerprint))?;
        (image.module_count(), image.nar_hash())
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| ImageError::Io(e.to_string()))?;
    }
    let tmp = path.with_extension(format!(
        "brix.tmp.{}.{}",
        std::process::id(),
        started.elapsed().as_nanos()
    ));
    std::fs::write(&tmp, &bytes[..]).map_err(|e| ImageError::Io(e.to_string()))?;
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        ImageError::Io(e.to_string())
    })?;
    Ok(CompiledImage {
        path: path.to_path_buf(),
        bytes: bytes.len() as u64,
        modules,
        nar_hash,
        duration: started.elapsed(),
    })
}

impl ImageStore {
    /// A progress note, printed when [`brix_eval::Config::timings`] is set.
    fn note(&self, msg: &str) {
        if self.timings {
            eprintln!("brix: {msg}");
        }
    }
}

impl From<ImageError> for Error {
    fn from(e: ImageError) -> Self {
        Error::Config(e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use brix_eval::{Arena, Config, Evaluator, NullHost};
    use brix_store::FsAccessor;
    use std::collections::BTreeMap;

    fn context() -> ImageCompileContext {
        let arena = Arena::new();
        let ev = Evaluator::new(&arena, Config::default(), &NullHost);
        ev.image_compile_context().clone()
    }

    /// A small fetched tree, as the flake layer hands it over.
    fn fetched(dir: &Path) -> FetchedTree {
        std::fs::write(dir.join("default.nix"), "{ a = import ./b.nix; }\n").unwrap();
        std::fs::write(dir.join("b.nix"), "rec { x = 1; y = x; }\n").unwrap();
        let accessor: Arc<dyn SourceAccessor> = Arc::new(FsAccessor::new(dir));
        let (nar_hash, _) =
            brix_store::nar::hash(&*accessor, &CanonPath::root(), HashAlgo::Sha256).unwrap();
        let mut attrs = BTreeMap::new();
        attrs.insert(
            "narHash".to_owned(),
            FetchAttr::String(nar_hash.to_string(HashFormat::Sri, true)),
        );
        FetchedTree {
            accessor,
            store_path: StorePath::new("0000000000000000000000000000000z-source").unwrap(),
            attrs,
            locked_attrs: BTreeMap::new(),
            locked_url: "path:/fixture".to_owned(),
            display_prefix: None,
        }
    }

    fn store(cache: &Path, explicit: Vec<(String, PathBuf)>) -> ImageStore {
        ImageStore::new(
            &CompiledInputsConfig {
                enabled: true,
                cache_dir: Some(cache.to_path_buf()),
                explicit,
                min_nix_files: 1,
            },
            cache,
            Arc::new(brix_flake::NoProgress),
            brix_store::XpSettings::NONE,
            false,
        )
    }

    /// A damaged or stale cache file is replaced by a fresh compile, never
    /// misread; a damaged explicit image fails loudly.
    #[test]
    fn damaged_images_are_replaced_or_rejected() {
        let tree_dir = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let tree = fetched(tree_dir.path());
        let ctx = context();
        let first = store(cache.path(), Vec::new())
            .image_for(&tree, &ctx)
            .unwrap()
            .expect("compiled on first use");
        assert_eq!(first.module_count(), 2);
        let nar_hash = first.nar_hash();
        let path = store(cache.path(), Vec::new()).cache_path(&nar_hash, &ctx);
        let good = std::fs::read(&path).unwrap();
        type Damage = Box<dyn Fn(&mut Vec<u8>)>;
        let damages: [(&str, Damage); 4] = [
            (
                "truncated",
                Box::new(|b: &mut Vec<u8>| b.truncate(b.len() / 2)),
            ),
            (
                "another format version",
                Box::new(|b: &mut Vec<u8>| b[8..12].copy_from_slice(&3u32.to_le_bytes())),
            ),
            (
                "not an image",
                Box::new(|b: &mut Vec<u8>| *b = b"not an image".to_vec()),
            ),
            (
                "flipped bytes in the archive",
                Box::new(|b: &mut Vec<u8>| {
                    let mid = b.len() / 2;
                    for x in &mut b[mid..mid + 256] {
                        *x ^= 0xa5;
                    }
                }),
            ),
        ];
        for (what, damage) in &damages {
            let mut bytes = good.clone();
            damage(&mut bytes);
            std::fs::write(&path, &bytes).unwrap();
            // The cache opens what it can: an image whose damage the
            // archive validation does not see is used (module code is
            // checked when linked); anything else is recompiled.
            let image = store(cache.path(), Vec::new())
                .image_for(&tree, &ctx)
                .unwrap()
                .unwrap_or_else(|| panic!("{what}: no image"));
            assert_eq!(image.nar_hash(), nar_hash, "{what}");
            let on_disk = std::fs::read(&path).unwrap();
            if on_disk != bytes {
                assert_eq!(on_disk, good, "{what}: recompiled differently");
            } else {
                assert_eq!(*what, "flipped bytes in the archive");
            }
            // Named explicitly, the same file must open or fail the
            // evaluation.
            std::fs::write(&path, &bytes).unwrap();
            let explicit = store(
                cache.path(),
                vec![(nar_hash.to_string(HashFormat::Sri, true), path.clone())],
            )
            .image_for(&tree, &ctx);
            match explicit {
                Err(e) => assert!(e.contains("cannot use the compiled image"), "{what}: {e}"),
                Ok(img) => {
                    assert!(img.is_some(), "{what}");
                    assert_eq!(*what, "flipped bytes in the archive");
                }
            }
        }
    }

    /// An image the user named is read into memory, not mapped: changing
    /// the file in place afterwards (here: truncating it) cannot reach
    /// the image in use.
    #[test]
    fn explicit_images_are_read_not_mapped() {
        let tree_dir = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let tree = fetched(tree_dir.path());
        let ctx = context();
        let compiled = store(cache.path(), Vec::new())
            .image_for(&tree, &ctx)
            .unwrap()
            .expect("compiled on first use");
        let nar_hash = compiled.nar_hash();
        let named = cache.path().join("named.brix");
        std::fs::copy(
            store(cache.path(), Vec::new()).cache_path(&nar_hash, &ctx),
            &named,
        )
        .unwrap();
        let image = store(
            cache.path(),
            vec![(nar_hash.to_string(HashFormat::Sri, true), named.clone())],
        )
        .image_for(&tree, &ctx)
        .unwrap()
        .expect("the named image");
        std::fs::write(&named, b"").unwrap();
        assert_eq!(image.module_count(), 2);
        assert_eq!(image.nar_hash(), nar_hash);
    }
}
