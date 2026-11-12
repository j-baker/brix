//! Nix store primitives for brix.
//!
//! Everything in this crate is pure data and algorithms: hashes and their
//! encodings, store paths and the functions that derive them, derivations
//! and their ATerm serialisation, the NAR archive format, and the
//! [`SourceAccessor`] abstraction through which files are read. Nothing here
//! touches a real store; the [`VirtualStore`] remembers what *would* have
//! been written so that a consumer can materialise it.
//!
//! The reference for every rule reproduced here is the Nix source
//! (`src/libutil` and `src/libstore`); each function documents the C++
//! function it mirrors.
//!
//! Path names are bytes, as Nix's `std::string`s are: [`CanonPath`],
//! directory entries ([`DirEntries`]), symlink targets, what an accessor
//! shows a path as, NAR entries and the paths a store path is parsed
//! from. Their `Display` is lossy and only for Rust-side text; a message
//! Nix shows is built from the bytes (the errors' `message_bytes`). The
//! byte-string types are re-exported so that crates using this one need
//! not depend on `bstr` themselves.

#![forbid(unsafe_code)]

pub mod accessor;
pub mod canon_path;
pub mod content_address;
pub mod derivation;
pub mod derived_path;
pub mod experimental;
pub mod hash;
pub mod json;
pub mod nar;
pub mod os_path;
pub mod setting;
pub mod shell;
pub mod store_path;
pub mod url;
pub mod virtual_store;

pub use bstr::{BStr, BString, ByteSlice};

#[cfg(feature = "std-fs")]
pub use accessor::FsAccessor;
pub use accessor::{
    AccessorError, AccessorId, AccessorRegistry, DirEntries, FileType, MemoryAccessor,
    MountedAccessor, SourceAccessor, SourcePath, Stat,
};
pub use canon_path::{CanonPath, CanonPathError};
pub use content_address::{
    ContentAddress, ContentAddressError, ContentAddressMethod, ContentAddressWithReferences,
    FileIngestionMethod, FixedOutputInfo, StoreReferences, TextInfo,
};
pub use derivation::{
    downstream_placeholder, hash_derivation_modulo, hash_placeholder, Derivation, DerivationError,
    DerivationOutput, DerivationType, DrvHashModulo, DrvHashes,
};
pub use derived_path::{
    DerivedPath, DerivedPathError, ExtendedOutputsSpec, InvalidExtendedOutputsSpec, OutputsSpec,
};
pub use experimental::{MissingExperimentalFeature, Xp, XpSettings};
pub use hash::{Hash, HashAlgo, HashError, HashFormat, Hasher};
pub use setting::SettingError;
pub use store_path::{StoreDir, StorePath, StorePathError};
pub use virtual_store::{
    HashedDerivation, InputHashes, PathIngestion, StoreObject, VirtualStore, VirtualStoreError,
};
