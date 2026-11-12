//! Compiled flake-input images.
//!
//! A flake input such as nixpkgs is parsed and scope-resolved afresh by
//! every evaluation that touches it, although its content is pinned by
//! the lock file's `narHash`. An image is that work done once: every
//! `.nix` file of the input compiled to fixed-width code words that
//! linking writes straight into the evaluator's tables (there is no
//! intermediate [`crate::ir::Code`]), the file tree's shape with the NAR
//! hash of every directory, and, only when compiled with
//! [`writer::CompileOptions::include_tree`], every file's contents, in a
//! single file that is memory-mapped and used in place. Evaluating
//! against an image is bit-identical to evaluating against the source
//! tree: same values, store paths, positions, error excerpts and
//! evaluation order. It is a cache keyed by content, never a semantic
//! change.
//!
//! The pieces:
//!
//! - [`format`](mod@format): the file layout (a preamble, an `rkyv` archive
//!   and its checksums), and
//!   [`codec`]: the fixed-width words a module's code is stored as, and
//!   how linking writes them into the evaluator's arena.
//! - [`writer`]: [`compile_tree`], which turns a [`SourceAccessor`](brix_store::SourceAccessor) tree
//!   into an image, parsing and compiling files in parallel.
//! - [`loader`]: [`Image`], a validated, zero-copy view of an image;
//!   [`ImageAccessor`], which serves the file tree through the
//!   [`SourceAccessor`](brix_store::SourceAccessor) trait (with precomputed NAR hashes); and the
//!   linking of a module into an evaluator.
//! - `integrity`: the checksums, which catch damage that leaves an image
//!   valid (a flipped bit in a literal, a name, a NAR hash), each
//!   compared when its part is first used.
//!
//! An image is only meaningful for an evaluator whose compiler output,
//! builtin symbols and global scope match the ones it was compiled with;
//! [`ImageCompileContext::fingerprint`] captures those, and the loader
//! rejects a mismatch rather than guessing.

pub mod codec;
pub(crate) mod compress;
pub mod format;
pub(crate) mod integrity;
pub mod loader;
pub mod writer;

use std::sync::Arc;

use brix_store::{CanonPath, Hash, HashAlgo, SourcePath};
use thiserror::Error;

use crate::code::LinkedCode;
use crate::error::ErrorKind;
use crate::evaluator::Evaluator;
use crate::Result;

pub use loader::{Image, ImageAccessor};
pub use writer::compile_tree;

#[derive(Clone, Debug, Error)]
pub enum ImageError {
    #[error(
        "not a brix image (bad magic; an image made before format version 4 must be recompiled)"
    )]
    NotAnImage,
    #[error("unsupported brix image format version {found} (this brix reads version {supported})")]
    FormatVersion { found: u32, supported: u32 },
    #[error("the image was compiled by a different brix or for a different set of builtins (fingerprint mismatch)")]
    Fingerprint,
    #[error("corrupt brix image: {0}")]
    Corrupt(String),
    /// Damage in one module, found when the module was linked (opening
    /// the image does not read module code). See [`Image::damage`].
    #[error("corrupt brix image: module '{module}': {why}")]
    CorruptModule { module: String, why: String },
    #[error("{0}")]
    Io(String),
    #[error("while compiling an image: {0}")]
    Compile(String),
}

/// What compiling an image depends on, taken from the evaluator the
/// image is meant for.
#[derive(Clone, Debug)]
pub struct ImageCompileContext {
    /// The names of the symbols the evaluator creates while it is
    /// constructed (Nix's static symbols, the builtins, the host's extra
    /// primops), in id order.
    pub base_symbols: Vec<Vec<u8>>,
    /// The global scope: name and environment slot of every builtin, in
    /// slot order.
    pub globals: Vec<(Vec<u8>, u32)>,
    /// What the parser accepts (`pipe-operators`, the `lint-*`
    /// settings): a lint at `fatal` makes a file an error, one at `warn`
    /// makes it a file the image does not hold (see `writer`).
    pub parse_options: brix_syntax::ParseOptions,
    /// SHA-256 over everything above plus the compiler fingerprint.
    pub fingerprint: [u8; 32],
}

impl ImageCompileContext {
    pub fn new(
        base_symbols: Vec<Vec<u8>>,
        globals: Vec<(Vec<u8>, u32)>,
        parse_options: brix_syntax::ParseOptions,
    ) -> Self {
        let mut data = Vec::new();
        data.extend_from_slice(&format::FORMAT_VERSION.to_le_bytes());
        data.extend_from_slice(format::IR_FINGERPRINT.as_bytes());
        data.extend_from_slice(&(std::mem::size_of::<crate::ir::Node>() as u32).to_le_bytes());
        // Every parse option, one by one.
        let brix_syntax::ParseOptions {
            pipe_operators,
            lints:
                brix_syntax::Lints {
                    url_literals,
                    short_path_literals,
                    absolute_path_literals,
                },
        } = parse_options;
        data.push(u8::from(pipe_operators));
        for lint in [url_literals, short_path_literals, absolute_path_literals] {
            data.push(lint as u8);
        }
        data.extend_from_slice(&(base_symbols.len() as u32).to_le_bytes());
        for name in &base_symbols {
            data.extend_from_slice(&(name.len() as u32).to_le_bytes());
            data.extend_from_slice(name);
        }
        data.extend_from_slice(&(globals.len() as u32).to_le_bytes());
        for (name, slot) in &globals {
            data.extend_from_slice(&(name.len() as u32).to_le_bytes());
            data.extend_from_slice(name);
            data.extend_from_slice(&slot.to_le_bytes());
        }
        let hash = Hash::of(HashAlgo::Sha256, &data);
        let mut fingerprint = [0u8; 32];
        fingerprint.copy_from_slice(hash.as_bytes());
        ImageCompileContext {
            base_symbols,
            globals,
            parse_options,
            fingerprint,
        }
    }

    /// The fingerprint as lower-case hex, for cache file names.
    pub fn fingerprint_hex(&self) -> String {
        brix_store::hash::base16::encode(&self.fingerprint)
    }
}

/// An image mounted in an evaluator.
pub(crate) struct MountedImage {
    pub at: CanonPath,
    pub image: Arc<Image>,
    /// The evaluator's symbols for the image's names, as far as known.
    pub names: crate::runtime::rc::Rc<loader::NameSymbols>,
}

impl<'a> Evaluator<'a> {
    /// What an image for this evaluator must be compiled with. Captured
    /// from the state right after construction, so it is the same for
    /// every evaluator built the same way, whenever it is asked.
    pub fn image_compile_context(&self) -> &ImageCompileContext {
        self.image_ctx.get_or_init(|| {
            let n = self.base_symbol_count.get() as usize;
            let base_symbols: Vec<Vec<u8>> = (0..n)
                .map(|i| self.keys.name(crate::symbol::Symbol(i as u32)).to_vec())
                .collect();
            let senv = self.base_static_env.borrow();
            let mut globals: Vec<(Vec<u8>, u32)> = senv
                .vars
                .iter()
                .map(|&(s, slot)| (self.keys.name(s).to_vec(), slot))
                .collect();
            globals.sort_by_key(|(_, slot)| *slot);
            ImageCompileContext::new(base_symbols, globals, self.parse_options())
        })
    }

    /// Serves the tree mounted at `at` from `image`: imports of its
    /// `.nix` files link the precompiled code instead of parsing. The
    /// image must have been compiled for this evaluator (same
    /// fingerprint); the caller mounts the accessor itself.
    pub fn register_image(
        &self,
        at: CanonPath,
        image: Arc<Image>,
    ) -> std::result::Result<(), ImageError> {
        let ctx = self.image_compile_context();
        if *image.fingerprint() != ctx.fingerprint {
            return Err(ImageError::Fingerprint);
        }
        // The fingerprint covers the base symbols; a count that disagrees
        // is damage, and module code must not use ids the evaluator lacks.
        if image.base_symbols() as usize != ctx.base_symbols.len() {
            return Err(ImageError::Corrupt(
                "base symbol count does not match the fingerprint".into(),
            ));
        }
        let mut images = self.images.borrow_mut();
        images.retain(|m| m.at != at);
        images.push(MountedImage {
            at,
            image,
            names: Default::default(),
        });
        Ok(())
    }

    /// The precompiled code for a file on the root accessor, if an image
    /// covers it: `Ok(None)` means "parse the source" (no image, or the
    /// module cannot be served from one); `Err` is the error importing
    /// the file raises (a parse error, an undefined variable).
    pub(crate) fn image_code_for(&self, path: &SourcePath) -> Result<Option<&'a LinkedCode<'a>>> {
        if path.accessor != self.root_accessor {
            return Ok(None);
        }
        let found = {
            let images = self.images.borrow();
            images
                .iter()
                .filter(|m| path.path.is_within(&m.at))
                .max_by_key(|m| m.at.abs().len())
                .map(|m| (m.at.clone(), m.image.clone(), m.names.clone()))
        };
        let Some((at, image, names)) = found else {
            return Ok(None);
        };
        let rel = path.path.remove_prefix(&at);
        let found = image.find_module(rel.rel());
        let Some(module) =
            found.map_err(|damage| self.error(ErrorKind::Eval, image.report(damage)))?
        else {
            if self.timings {
                eprintln!("brix: {} is not in the image; parsing it", path.path.abs());
            }
            return Ok(None);
        };
        let started = crate::platform::Instant::now();
        let display = self.accessor(path.accessor).show_path(&path.path);
        let linked = loader::link_module(self, &image, &names, module, &at, display, &path.path);
        crate::stats::Stats::add_time(&self.stats.image_load_time, started.elapsed());
        match linked {
            Ok(Some(code)) => {
                crate::stats::Stats::bump(&self.stats.image_modules);
                Ok(Some(code))
            }
            Ok(None) => {
                if self.timings {
                    eprintln!(
                        "brix: {} cannot be served from the image; parsing it",
                        path.path.abs()
                    );
                }
                Ok(None)
            }
            Err(e) => Err(e),
        }
    }
}
