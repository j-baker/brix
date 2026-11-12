//! Store paths and the functions that compute them.
//!
//! Mirrors `src/libstore/path.cc` (validation) and
//! `src/libstore/store-dir-config.cc` (`makeStorePath` and friends). The
//! store directory is a parameter everywhere: brix never assumes
//! `/nix/store`, although that is the default.
//!
//! A store path's base name is `<hash part>-<name>` where the hash part is
//! 32 nix32 characters (160 bits) and the name is at most 211 characters
//! from `[a-zA-Z0-9+\-._?=]`, not starting with `.`/`..` as a dash
//! separated component.

use std::borrow::Cow;
use std::collections::BTreeSet;
use std::fmt;

use bstr::BString;

use crate::canon_path::{canonicalise, CanonPath, CanonPathError};
use crate::content_address::{ContentAddressWithReferences, FileIngestionMethod, FixedOutputInfo};
use crate::hash::{base16, nix32, Hash, HashAlgo, HashFormat, Hasher};

/// Errors from parsing or constructing store paths (`BadStorePath` and
/// `BadStorePathName` in Nix, same messages).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StorePathError {
    #[error("name must not be empty")]
    NameEmpty,
    #[error("name '{0}' must be no longer than 211 characters")]
    NameTooLong(BString),
    #[error("name '{0}' is not valid")]
    NameReserved(BString),
    #[error(
        "name '{name}' is not valid: first dash-separated component must not be '{component}'"
    )]
    NameReservedComponent {
        name: BString,
        component: &'static str,
    },
    /// `byte` is the first byte of the offending character, which Nix
    /// formats on its own; see [`StorePathError::message_bytes`].
    #[error("name '{name}' contains illegal character '{}'", shown_byte(*.byte))]
    NameIllegalCharacter { name: BString, byte: u8 },
    /// The base name, bytes as it was given.
    #[error("'{0}' is too short to be a valid store path")]
    TooShort(BString),
    /// The base name, bytes as it was given.
    #[error("'{0}' can't name a store path because the hash part is not followed by a '-'")]
    NoDashAfterHash(BString),
    /// The base name and the first byte of the hash part that is not a
    /// nix32 digit, which Nix formats on its own (as for
    /// [`StorePathError::NameIllegalCharacter`]).
    #[error("store path '{path}' contains illegal base-32 character '{}'", shown_byte(*.byte))]
    IllegalHashCharacter { path: BString, byte: u8 },
    /// A name error found while parsing a whole base name or path, or
    /// while making one from a name: bytes, as the name's are.
    #[error("path '{path}' is not a valid store path: {reason}")]
    BadName {
        path: BString,
        reason: Box<StorePathError>,
    },
    #[error("empty path is not a valid store path")]
    EmptyPath,
    /// The path with its quotes, as each message has them: `'…'` from
    /// `toStorePath`, `"…"` (canonical, `PathFmt`) from `parseStorePath`.
    #[error("path {0} is not in the Nix store")]
    NotInStore(BString),
    /// `canonPath`'s error for a relative path.
    #[error("not an absolute path: \"{0}\"")]
    NotAbsolute(BString),
    #[error("store path '{0}' is not a valid derivation path")]
    NotADerivation(String),
    #[error("Git file ingestion must use SHA-1 or SHA-256 hash, but instead using: {0}")]
    GitHashAlgo(HashAlgo),
    #[error("fixed output derivation '{0}' is not allowed to refer to other store paths.\nYou may need to use the 'unsafeDiscardReferences' derivation attribute, see the manual for more details.")]
    FixedOutputWithReferences(BString),
    /// `canonStoreDir`'s `UsageError`.
    #[error("store directory path \"{0}\" is not an absolute path")]
    StoreDirNotAbsolute(String),
    #[error(transparent)]
    CanonPath(#[from] CanonPathError),
}

impl StorePathError {
    /// The message as Nix writes it. It is the `Display` text but for a
    /// name's or a path's bytes, which are shown as they are (`Display`
    /// replaces those that are not UTF-8), and an illegal character in a
    /// name or a hash part: Nix formats that character's first byte on
    /// its own (`%s` of a `char`), which is not UTF-8 when the character
    /// is not ASCII.
    pub fn message_bytes(&self) -> Vec<u8> {
        let quoted = |before: &str, name: &[u8], after: &str| {
            [before.as_bytes(), name, after.as_bytes()].concat()
        };
        match self {
            StorePathError::NameTooLong(name) => {
                quoted("name '", name, "' must be no longer than 211 characters")
            }
            StorePathError::NameReserved(name) => quoted("name '", name, "' is not valid"),
            StorePathError::NameReservedComponent { name, component } => quoted(
                "name '",
                name,
                &format!(
                    "' is not valid: first dash-separated component must not be '{component}'"
                ),
            ),
            StorePathError::NameIllegalCharacter { name, byte } => [
                b"name '".as_slice(),
                name,
                b"' contains illegal character '",
                &[*byte],
                b"'",
            ]
            .concat(),
            StorePathError::BadName { path, reason } => {
                let mut out = quoted("path '", path, "' is not a valid store path: ");
                out.extend_from_slice(&reason.message_bytes());
                out
            }
            StorePathError::FixedOutputWithReferences(name) => quoted(
                "fixed output derivation '",
                name,
                "' is not allowed to refer to other store paths.\nYou may need to use the 'unsafeDiscardReferences' derivation attribute, see the manual for more details.",
            ),
            StorePathError::TooShort(base_name) => {
                quoted("'", base_name, "' is too short to be a valid store path")
            }
            StorePathError::NoDashAfterHash(base_name) => quoted(
                "'",
                base_name,
                "' can't name a store path because the hash part is not followed by a '-'",
            ),
            StorePathError::IllegalHashCharacter { path, byte } => [
                b"store path '".as_slice(),
                path,
                b"' contains illegal base-32 character '",
                &[*byte],
                b"'",
            ]
            .concat(),
            StorePathError::NotInStore(path) => quoted("path ", path, " is not in the Nix store"),
            StorePathError::NotAbsolute(path) => quoted("not an absolute path: \"", path, "\""),
            StorePathError::CanonPath(e) => e.message_bytes(),
            StorePathError::NameEmpty
            | StorePathError::EmptyPath
            | StorePathError::NotADerivation(_)
            | StorePathError::GitHashAlgo(_)
            | StorePathError::StoreDirNotAbsolute(_) => self.to_string().into_bytes(),
        }
    }
}

/// An illegal byte as text: itself if it is ASCII, else U+FFFD (what a
/// UTF-8 reader shows for Nix's message).
fn shown_byte(byte: u8) -> char {
    if byte.is_ascii() {
        char::from(byte)
    } else {
        char::REPLACEMENT_CHARACTER
    }
}

/// A store path: the base name `<hash>-<name>` of an entry in the store
/// directory. It does not know which store directory it belongs to; see
/// [`StoreDir::display`].
///
/// The name is reference counted: a path is cloned into every derivation
/// that refers to it (`input_srcs`, `input_drvs`, references), and the
/// evaluator interns them, so a clone must not allocate.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct StorePath {
    base_name: std::sync::Arc<str>,
}

impl StorePath {
    /// `StorePath::HashLen`: 32 nix32 characters, i.e. 160 bits.
    pub const HASH_LEN: usize = 32;
    /// `StorePath::MaxPathLen`: the longest allowed name.
    pub const MAX_NAME_LEN: usize = 211;
    /// `drvExtension`.
    pub const DRV_EXTENSION: &'static str = ".drv";

    /// `StorePath(std::string_view baseName)`: validate a base name. It is
    /// bytes, as in Nix, and any bytes get Nix's errors; a valid one is
    /// ASCII.
    pub fn new(base_name: impl AsRef<[u8]>) -> Result<StorePath, StorePathError> {
        StorePath::from_base_name(base_name.as_ref())
    }

    fn from_base_name(base_name: &[u8]) -> Result<StorePath, StorePathError> {
        validate_base_name(base_name)?;
        let base_name =
            std::str::from_utf8(base_name).expect("a valid store path base name is ASCII");
        Ok(StorePath {
            base_name: base_name.into(),
        })
    }

    /// `StorePath(const Hash &, std::string_view name)`: the hash is
    /// rendered in nix32 (it must be 20 bytes long, usually the compressed
    /// sha256 from [`StoreDir::make_store_path`]). The name is bytes, as in
    /// Nix; one that is not a valid name is refused, the path quoted with
    /// its bytes.
    pub fn from_hash_and_name<N: AsRef<[u8]> + ?Sized>(
        hash: &Hash,
        name: &N,
    ) -> Result<StorePath, StorePathError> {
        let name = name.as_ref();
        if let Err(reason) = validate_name(name) {
            let mut path = hash.to_string(HashFormat::Nix32, false).into_bytes();
            path.push(b'-');
            path.extend_from_slice(name);
            return Err(StorePathError::BadName {
                path: path.into(),
                reason: Box::new(reason),
            });
        }
        // A valid name is at most `MAX_NAME_LEN` bytes, so the base name is
        // assembled on the stack and copied once, into the shared string.
        let mut buf = [0u8; nix32::encoded_len(Hash::MAX_SIZE) + 1 + StorePath::MAX_NAME_LEN];
        let hash_len = nix32::encoded_len(hash.as_bytes().len());
        nix32::encode_to_slice(hash.as_bytes(), &mut buf[..hash_len]);
        buf[hash_len] = b'-';
        let len = hash_len + 1 + name.len();
        buf[hash_len + 1..len].copy_from_slice(name);
        let base_name = std::str::from_utf8(&buf[..len]).expect("store path names are ASCII");
        Ok(StorePath {
            base_name: base_name.into(),
        })
    }

    /// The nix32 hash part (32 characters).
    pub fn hash_part(&self) -> &str {
        &self.base_name[..Self::HASH_LEN]
    }

    /// The name after the dash.
    pub fn name(&self) -> &str {
        &self.base_name[Self::HASH_LEN + 1..]
    }

    /// `<hash>-<name>`.
    pub fn as_str(&self) -> &str {
        &self.base_name
    }

    /// Does the name end in `.drv`?
    pub fn is_derivation(&self) -> bool {
        self.name().ends_with(Self::DRV_EXTENSION)
    }

    /// `requireDerivation`.
    pub fn require_derivation(&self) -> Result<(), StorePathError> {
        if self.is_derivation() {
            Ok(())
        } else {
            Err(StorePathError::NotADerivation(self.base_name.to_string()))
        }
    }

    /// `BasicDerivation::nameFromPath`: the derivation name without `.drv`.
    pub fn derivation_name(&self) -> Result<&str, StorePathError> {
        self.require_derivation()?;
        Ok(&self.name()[..self.name().len() - Self::DRV_EXTENSION.len()])
    }
}

impl fmt::Debug for StorePath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "StorePath({})", self.base_name)
    }
}

impl fmt::Display for StorePath {
    /// The base name only. Use [`StoreDir::display`] for the full path.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.base_name)
    }
}

fn is_valid_name_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, b'+' | b'-' | b'.' | b'_' | b'?' | b'=')
}

fn is_valid_hash_char(c: u8) -> bool {
    c.is_ascii_digit() || (c.is_ascii_lowercase() && !matches!(c, b'e' | b'o' | b'u' | b't'))
}

/// `validateStorePathName` + `checkName`. The name is bytes, as in Nix:
/// a name that is not UTF-8 is refused like any other with a character
/// outside the set, and the error keeps its bytes.
pub fn validate_name<N: AsRef<[u8]> + ?Sized>(name: &N) -> Result<(), StorePathError> {
    let name = name.as_ref();
    if name.is_empty() {
        return Err(StorePathError::NameEmpty);
    }
    if name.len() > StorePath::MAX_NAME_LEN {
        return Err(StorePathError::NameTooLong(name.into()));
    }
    if name[0] == b'.' {
        if name.len() == 1 {
            return Err(StorePathError::NameReserved(name.into()));
        }
        if name[1] == b'-' {
            return Err(StorePathError::NameReservedComponent {
                name: name.into(),
                component: ".",
            });
        }
        if name[1] == b'.' {
            if name.len() == 2 {
                return Err(StorePathError::NameReserved(name.into()));
            }
            if name[2] == b'-' {
                return Err(StorePathError::NameReservedComponent {
                    name: name.into(),
                    component: "..",
                });
            }
        }
    }
    if let Some(&byte) = name.iter().find(|&&c| !is_valid_name_char(c)) {
        return Err(StorePathError::NameIllegalCharacter {
            name: name.into(),
            byte,
        });
    }
    Ok(())
}

/// `StorePath::StorePath(baseName)`'s checks, with its messages: the
/// length, the dash, the hash part's characters (the first that is not a
/// nix32 digit), then the name (`checkPathName`). Bytes, as in Nix.
fn validate_base_name(base_name: &[u8]) -> Result<(), StorePathError> {
    if base_name.len() < StorePath::HASH_LEN + 1 {
        return Err(StorePathError::TooShort(base_name.into()));
    }
    if base_name[StorePath::HASH_LEN] != b'-' {
        return Err(StorePathError::NoDashAfterHash(base_name.into()));
    }
    if let Some(&byte) = base_name[..StorePath::HASH_LEN]
        .iter()
        .find(|&&c| !is_valid_hash_char(c))
    {
        return Err(StorePathError::IllegalHashCharacter {
            path: base_name.into(),
            byte,
        });
    }
    validate_name(&base_name[StorePath::HASH_LEN + 1..]).map_err(|reason| StorePathError::BadName {
        path: base_name.into(),
        reason: Box::new(reason),
    })
}

/// A store directory such as `/nix/store`. Carries the methods of Nix's
/// `StoreDirConfig`: printing and parsing full paths, and computing new
/// store paths.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct StoreDir {
    dir: String,
}

impl Default for StoreDir {
    fn default() -> Self {
        StoreDir::new("/nix/store")
    }
}

impl StoreDir {
    /// `dir` must be an absolute path without a trailing slash; a store
    /// directory from user input goes through [`StoreDir::parse_setting`].
    pub fn new(dir: &str) -> StoreDir {
        assert!(
            dir.starts_with('/') && (dir.len() == 1 || !dir.ends_with('/')),
            "store directory must be absolute without a trailing slash: {dir:?}"
        );
        StoreDir {
            dir: dir.to_owned(),
        }
    }

    /// `canonStoreDir`: the store directory a setting or the environment
    /// names (`NIX_STORE_DIR=/nix/store/`), made canonical. It must be
    /// absolute.
    pub fn parse_setting(dir: &str) -> Result<StoreDir, StorePathError> {
        if !dir.starts_with('/') {
            return Err(StorePathError::StoreDirNotAbsolute(dir.to_owned()));
        }
        // Canonicalising removes whole components and slashes, never part
        // of a character, so text stays text.
        let canon = CanonPath::new(dir)?.into_bytes();
        Ok(StoreDir {
            dir: String::from_utf8(canon).expect("a canonicalised UTF-8 path is UTF-8"),
        })
    }

    pub fn as_str(&self) -> &str {
        &self.dir
    }

    /// `printStorePath`: `<store dir>/<base name>`.
    pub fn display(&self, path: &StorePath) -> String {
        let mut s = String::with_capacity(self.dir.len() + 1 + path.base_name.len());
        self.display_into(&mut s, path);
        s
    }

    /// [`StoreDir::display`] appended to `out`, for callers that render
    /// many paths into one buffer (ATerm serialisation, type strings).
    pub fn display_into(&self, out: &mut String, path: &StorePath) {
        out.push_str(&self.dir);
        out.push('/');
        out.push_str(&path.base_name);
    }

    /// `parseStorePath`: an absolute path directly below the store
    /// directory, canonicalised first if it is not already. A path that
    /// is not is named canonicalised and in double quotes (`PathFmt`), as
    /// in Nix. The path is bytes, and any bytes get Nix's errors.
    pub fn parse(&self, path: impl AsRef<[u8]>) -> Result<StorePath, StorePathError> {
        self.parse_bytes(path.as_ref())
    }

    fn parse_bytes(&self, path: &[u8]) -> Result<StorePath, StorePathError> {
        if path.is_empty() {
            return Err(StorePathError::EmptyPath);
        }
        if path[0] != b'/' {
            // `canonPath` refuses a relative path.
            return Err(StorePathError::NotAbsolute(path.into()));
        }
        match self.base_name_of(path) {
            Ok(base_name) => StorePath::new(base_name),
            Err(canon) => {
                let mut quoted = Vec::with_capacity(canon.len() + 2);
                quoted.push(b'"');
                quoted.extend_from_slice(&canon);
                quoted.push(b'"');
                Err(StorePathError::NotInStore(quoted.into()))
            }
        }
    }

    /// `maybeParseStorePath`: `None` if `path` is not a valid store path.
    pub fn maybe_parse(&self, path: impl AsRef<[u8]>) -> Option<StorePath> {
        self.parse_bytes(path.as_ref()).ok()
    }

    /// `isInStore`: whether `path` lies below the store directory. Like
    /// `isInDir`, which asks `std::filesystem::path::lexically_relative`,
    /// this compares components without resolving `.` or `..`: the path
    /// must be absolute, begin with the store directory's components
    /// (repeated slashes do not count), and go on with a component that
    /// is neither `.` nor `..`. Only `/` bytes separate components; the
    /// others may be anything.
    pub fn is_in_store(&self, path: impl AsRef<[u8]>) -> bool {
        self.is_in_store_bytes(path.as_ref())
    }

    fn is_in_store_bytes(&self, path: &[u8]) -> bool {
        if path.first() != Some(&b'/') {
            return false;
        }
        let mut components = path.split(|&b| b == b'/').filter(|c| !c.is_empty());
        for dir_component in self
            .dir
            .as_bytes()
            .split(|&b| b == b'/')
            .filter(|c| !c.is_empty())
        {
            if components.next() != Some(dir_component) {
                return false;
            }
        }
        matches!(components.next(), Some(c) if c != b"." && c != b"..")
    }

    /// `toStorePath`: the store path `path` lies in, and the rest of
    /// `path` below it. As in Nix, the store path ends at the first `/`
    /// after the store directory's length plus one.
    pub fn to_store_path(
        &self,
        path: impl AsRef<[u8]>,
    ) -> Result<(StorePath, CanonPath), StorePathError> {
        let (store_path, rest) = self.split_store_path(path.as_ref())?;
        let rest = match rest {
            None => CanonPath::root(),
            Some(rest) => CanonPath::new(rest)?,
        };
        Ok((store_path, rest))
    }

    /// `toStorePath(path).first`: the store path `path` lies in.
    pub fn store_path_of(&self, path: impl AsRef<[u8]>) -> Result<StorePath, StorePathError> {
        Ok(self.split_store_path(path.as_ref())?.0)
    }

    /// [`StoreDir::to_store_path`] with the rest of the path unparsed.
    fn split_store_path<'p>(
        &self,
        path: &'p [u8],
    ) -> Result<(StorePath, Option<&'p [u8]>), StorePathError> {
        if !self.is_in_store_bytes(path) {
            let mut quoted = Vec::with_capacity(path.len() + 2);
            quoted.push(b'\'');
            quoted.extend_from_slice(path);
            quoted.push(b'\'');
            return Err(StorePathError::NotInStore(quoted.into()));
        }
        // `is_in_store` makes `path` at least this long.
        let from = self.dir.len() + 1;
        match path[from..].iter().position(|&b| b == b'/') {
            None => Ok((self.parse_bytes(path)?, None)),
            Some(i) => {
                let slash = from + i;
                Ok((self.parse_bytes(&path[..slash])?, Some(&path[slash..])))
            }
        }
    }

    /// `getStoreBaseName`: the unvalidated base name of the absolute
    /// `path` if, canonicalised, it is a direct child of the store
    /// directory, and otherwise the canonical path. A canonical `path` is
    /// not copied.
    fn base_name_of<'p>(&self, path: &'p [u8]) -> Result<Cow<'p, [u8]>, Vec<u8>> {
        if let Some(rest) = path.strip_prefix(self.dir.as_bytes()) {
            if let Some(base) = rest.strip_prefix(b"/") {
                if !base.is_empty() && !base.contains(&b'/') && base != b"." && base != b".." {
                    return Ok(Cow::Borrowed(base));
                }
            }
        }
        let canon = canonicalise(path);
        let slash = canon
            .iter()
            .rposition(|&b| b == b'/')
            .expect("a canonical path starts with '/'");
        let (parent, base) = (&canon[..slash], &canon[slash + 1..]);
        let parent: &[u8] = if parent.is_empty() { b"/" } else { parent };
        if !base.is_empty() && parent == self.dir.as_bytes() {
            let base = base.to_vec();
            return Ok(Cow::Owned(base));
        }
        Err(canon)
    }

    /// `makeStorePath(type, hash, name)`: the fundamental store path
    /// construction, `sha256("<type>:<algo>:<base16 hash>:<store
    /// dir>:<name>")` compressed to 160 bits. The type is fed to the
    /// hasher by `feed_type`, so that types assembled from pieces
    /// (`output:<id>`, `text:<ref>:<ref>…`) and the rendered hash never
    /// become strings. The name is bytes, as Nix's (a derivation's output
    /// names, `builtins.path`'s `name`, need not be text); one that is not
    /// a valid store path name is refused once the path is computed.
    fn make_store_path_with(
        &self,
        feed_type: impl FnOnce(&mut Hasher),
        hash: &Hash,
        name: &[u8],
    ) -> Result<StorePath, StorePathError> {
        let mut hex = [0u8; 2 * Hash::MAX_SIZE];
        let hex = base16::encode_digest(hash.as_bytes(), &mut hex);
        let mut hasher = Hasher::new(HashAlgo::Sha256);
        feed_type(&mut hasher);
        for part in [
            ":",
            hash.algo().name(),
            ":",
            hex,
            ":",
            self.dir.as_str(),
            ":",
        ] {
            hasher.update(part.as_bytes());
        }
        hasher.update(name);
        let h = hasher.finish().0.compress(20);
        StorePath::from_hash_and_name(&h, name)
    }

    /// `makeOutputPath`: the path of output `id` of a derivation whose
    /// `hashDerivationModulo` is `hash`. The name is `<drv name>-<id>`,
    /// except that `out` adds no suffix. The output name is bytes, as in
    /// Nix: one that is not a valid store path name (not ASCII, say) is
    /// refused here, with the path computed from its bytes.
    pub fn make_output_path(
        &self,
        id: &[u8],
        hash: &Hash,
        drv_name: &str,
    ) -> Result<StorePath, StorePathError> {
        let suffixed;
        let name = if id == b"out" {
            drv_name.as_bytes()
        } else {
            suffixed = output_path_name(drv_name, id);
            suffixed.as_slice()
        };
        self.make_store_path_with(
            |h| {
                h.update(b"output:");
                h.update(id);
            },
            hash,
            name,
        )
    }

    /// `makeFixedOutputPath`: the path of content hashed with `info`.
    /// Recursive sha256 (the common NAR case) uses the `source` type with
    /// references folded into it; anything else is hashed a second time
    /// as `fixed:out:<prefix><algo>:<hash>:` and may not have references.
    /// The name is bytes (see [`StoreDir::make_output_path`]).
    pub fn make_fixed_output_path<N: AsRef<[u8]> + ?Sized>(
        &self,
        name: &N,
        info: &FixedOutputInfo,
    ) -> Result<StorePath, StorePathError> {
        self.make_fixed_output_path_bytes(name.as_ref(), info)
    }

    fn make_fixed_output_path_bytes(
        &self,
        name: &[u8],
        info: &FixedOutputInfo,
    ) -> Result<StorePath, StorePathError> {
        if info.method == FileIngestionMethod::Git
            && !matches!(info.hash.algo(), HashAlgo::Sha1 | HashAlgo::Sha256)
        {
            return Err(StorePathError::GitHashAlgo(info.hash.algo()));
        }
        if info.hash.algo() == HashAlgo::Sha256 && info.method == FileIngestionMethod::NixArchive {
            let refs = &info.references;
            self.make_store_path_with(
                |h| self.feed_type(h, "source", refs.others.iter(), refs.self_reference),
                &info.hash,
                name,
            )
        } else {
            if !info.references.is_empty() {
                return Err(StorePathError::FixedOutputWithReferences(name.into()));
            }
            // sha256("fixed:out:<prefix><algo>:<base16>:")
            let mut hex = [0u8; 2 * Hash::MAX_SIZE];
            let hex = base16::encode_digest(info.hash.as_bytes(), &mut hex);
            let mut inner = Hasher::new(HashAlgo::Sha256);
            for part in [
                "fixed:out:",
                info.method.prefix(),
                info.hash.algo().name(),
                ":",
                hex,
                ":",
            ] {
                inner.update(part.as_bytes());
            }
            let digest = inner.finish().0;
            self.make_store_path_with(|h| h.update(b"output:out"), &digest, name)
        }
    }

    /// `makeTextPath` (via `makeFixedOutputPathFromCA` with `TextInfo`):
    /// a `text` object, such as a `.drv` file, with references. The name
    /// is bytes (see [`StoreDir::make_output_path`]).
    pub fn make_text_path<N: AsRef<[u8]> + ?Sized>(
        &self,
        name: &N,
        hash: &Hash,
        references: &BTreeSet<StorePath>,
    ) -> Result<StorePath, StorePathError> {
        self.make_text_path_from(name, hash, references.iter())
    }

    /// [`StoreDir::make_text_path`] with the references given as a sorted
    /// iterator, so callers holding them in several sets (a derivation's
    /// input sources and input derivations) need not merge them first.
    pub fn make_text_path_from<'a, N: AsRef<[u8]> + ?Sized>(
        &self,
        name: &N,
        hash: &Hash,
        references: impl Iterator<Item = &'a StorePath>,
    ) -> Result<StorePath, StorePathError> {
        assert_eq!(
            hash.algo(),
            HashAlgo::Sha256,
            "text paths are sha256 hashed"
        );
        self.make_store_path_with(
            |h| self.feed_type(h, "text", references, false),
            hash,
            name.as_ref(),
        )
    }

    /// `makeFixedOutputPathFromCA`. The name is bytes (see
    /// [`StoreDir::make_output_path`]).
    pub fn make_fixed_output_path_from_ca<N: AsRef<[u8]> + ?Sized>(
        &self,
        name: &N,
        ca: &ContentAddressWithReferences,
    ) -> Result<StorePath, StorePathError> {
        let name = name.as_ref();
        match ca {
            ContentAddressWithReferences::Text(ti) => {
                self.make_text_path(name, &ti.hash, &ti.references)
            }
            ContentAddressWithReferences::Fixed(foi) => {
                self.make_fixed_output_path_bytes(name, foi)
            }
        }
    }

    /// `makeType`: the type string with references folded in,
    /// `<type>:<ref path>…[:self]`, fed to `hasher` piece by piece. The
    /// references must be sorted, as a `StorePathSet` iterates.
    fn feed_type<'a>(
        &self,
        hasher: &mut Hasher,
        type_: &str,
        references: impl Iterator<Item = &'a StorePath>,
        self_reference: bool,
    ) {
        hasher.update(type_.as_bytes());
        for r in references {
            hasher.update(b":");
            hasher.update(self.dir.as_bytes());
            hasher.update(b"/");
            hasher.update(r.as_str().as_bytes());
        }
        if self_reference {
            hasher.update(b":self");
        }
    }
}

impl fmt::Debug for StoreDir {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "StoreDir({})", self.dir)
    }
}

impl fmt::Display for StoreDir {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.dir)
    }
}

/// `outputPathName`: `<drv name>-<output>`, but just `<drv name>` for
/// `out`. Bytes, as the output name is.
pub fn output_path_name(drv_name: &str, output_name: &[u8]) -> Vec<u8> {
    let mut name = Vec::with_capacity(drv_name.len() + 1 + output_name.len());
    name.extend_from_slice(drv_name.as_bytes());
    if output_name != b"out" {
        name.push(b'-');
        name.extend_from_slice(output_name);
    }
    name
}

#[cfg(test)]
mod tests {
    use super::*;
    use bstr::BStr;

    const HASH: &str = "ffffffffffffffffffffffffffffffff";

    #[test]
    fn parses_and_splits() {
        let p = StorePath::new(format!("{HASH}-foo-1.2.drv")).unwrap();
        assert_eq!(p.hash_part(), HASH);
        assert_eq!(p.name(), "foo-1.2.drv");
        assert!(p.is_derivation());
        assert_eq!(p.derivation_name().unwrap(), "foo-1.2");
        let store = StoreDir::default();
        assert_eq!(store.display(&p), format!("/nix/store/{HASH}-foo-1.2.drv"));
        assert_eq!(store.parse(store.display(&p)).unwrap(), p);
        assert_eq!(
            store
                .parse(format!("/nix/store//{HASH}-foo-1.2.drv/"))
                .unwrap(),
            p
        );
    }

    #[test]
    fn errors_match_nix() {
        let e = |s: &str| StorePath::new(s).unwrap_err().to_string();
        assert_eq!(e("abc"), "'abc' is too short to be a valid store path");
        assert_eq!(
            e(&format!("{HASH}xfoo")),
            format!("'{HASH}xfoo' can't name a store path because the hash part is not followed by a '-'")
        );
        assert_eq!(
            e(&format!("e{}-foo", &HASH[1..])),
            format!(
                "store path 'e{}-foo' contains illegal base-32 character 'e'",
                &HASH[1..]
            )
        );
        assert_eq!(
            e(&format!("{HASH}-")),
            format!("path '{HASH}-' is not a valid store path: name must not be empty")
        );
        assert_eq!(
            e(&format!("{HASH}-.-x")),
            format!("path '{HASH}-.-x' is not a valid store path: name '.-x' is not valid: first dash-separated component must not be '.'")
        );
        assert_eq!(
            e(&format!("{HASH}-..")),
            format!("path '{HASH}-..' is not a valid store path: name '..' is not valid")
        );
        assert_eq!(
            e(&format!("{HASH}-a b")),
            format!("path '{HASH}-a b' is not a valid store path: name 'a b' contains illegal character ' '")
        );
        // Nix quotes the first byte of a character that is not ASCII.
        let not_ascii = StorePath::new(format!("{HASH}-\u{fc}")).unwrap_err();
        assert_eq!(
            not_ascii.to_string(),
            format!("path '{HASH}-\u{fc}' is not a valid store path: name '\u{fc}' contains illegal character '\u{fffd}'")
        );
        assert_eq!(
            not_ascii.message_bytes(),
            [
                format!("path '{HASH}-\u{fc}' is not a valid store path: name '\u{fc}' contains illegal character '").as_bytes(),
                b"\xc3'",
            ]
            .concat()
        );
        // A name is bytes: one that is not UTF-8 is shown as it is.
        let message = |name: &[u8]| validate_name(name).unwrap_err().message_bytes();
        assert_eq!(
            message(b"x\xff"),
            b"name 'x\xff' contains illegal character '\xff'"
        );
        assert_eq!(
            message(b".-\xff"),
            b"name '.-\xff' is not valid: first dash-separated component must not be '.'"
        );
        let long = [&b"\xff"[..], &[b'x'; 211]].concat();
        assert_eq!(
            message(&long),
            [
                &b"name '"[..],
                &long,
                b"' must be no longer than 211 characters"
            ]
            .concat()
        );
        assert_eq!(
            e(&format!("{HASH}-{}", "x".repeat(212))),
            format!("path '{HASH}-{0}' is not a valid store path: name '{0}' must be no longer than 211 characters", "x".repeat(212))
        );
        let store = StoreDir::default();
        assert_eq!(
            store.parse("").unwrap_err().to_string(),
            "empty path is not a valid store path"
        );
        // `parseStorePath` names the canonical path with `PathFmt`, as a
        // derivation whose output is `/tmp//x/` shows in Nix:
        // `path "/tmp/x" is not in the Nix store`.
        assert_eq!(
            store.parse("/tmp//x/").unwrap_err().to_string(),
            "path \"/tmp/x\" is not in the Nix store"
        );
        assert_eq!(
            store
                .parse(format!("/nix/store/{HASH}-foo/bar"))
                .unwrap_err()
                .to_string(),
            format!("path \"/nix/store/{HASH}-foo/bar\" is not in the Nix store")
        );
        assert_eq!(
            store
                .parse("/nix/store/../tmp/./x")
                .unwrap_err()
                .to_string(),
            "path \"/nix/tmp/x\" is not in the Nix store"
        );
        assert_eq!(
            store.parse("x").unwrap_err().to_string(),
            "not an absolute path: \"x\""
        );
        // A non-canonical path directly below the store is accepted.
        assert_eq!(
            store.parse(format!("/nix//store/./{HASH}-foo/")).unwrap(),
            StorePath::new(format!("{HASH}-foo")).unwrap()
        );
        assert_eq!(
            StoreDir::new("/")
                .parse(format!("//{HASH}-foo"))
                .unwrap()
                .to_string(),
            format!("{HASH}-foo")
        );
        assert!(validate_name("..-x").is_err());
        assert!(validate_name("...x").is_ok());
        assert!(validate_name(".x").is_ok());
    }

    #[test]
    fn make_store_path_matches_known_value() {
        // `nix eval --store dummy:// --expr '(derivation { name = "x";
        // builder = "/bin/sh"; system = "x86_64-linux"; }).drvPath'`
        // yields 97qlv6h78lxlm9zc8849ahsbcklhsi2y-x.drv; its text is below.
        let store = StoreDir::default();
        let text = r#"Derive([("out","/nix/store/s668cfx1vrqfjryf96jj8wfhg7f67dhj-x","","")],[],[],"x86_64-linux","/bin/sh",[],[("builder","/bin/sh"),("name","x"),("out","/nix/store/s668cfx1vrqfjryf96jj8wfhg7f67dhj-x"),("system","x86_64-linux")])"#;
        let h = Hash::of(HashAlgo::Sha256, text.as_bytes());
        let p = store.make_text_path("x.drv", &h, &BTreeSet::new()).unwrap();
        assert_eq!(p.as_str(), "97qlv6h78lxlm9zc8849ahsbcklhsi2y-x.drv");
    }

    #[test]
    fn store_dir_settings_are_canonicalised_like_nix() {
        // `canonStoreDir`; the oracle agrees (brix-cli/tests/store_dir.rs).
        let dir = |s: &str| StoreDir::parse_setting(s).map(|d| d.as_str().to_owned());
        assert_eq!(dir("/nix/store").unwrap(), "/nix/store");
        assert_eq!(dir("/nix/store/").unwrap(), "/nix/store");
        assert_eq!(dir("/nix//store/./").unwrap(), "/nix/store");
        assert_eq!(dir("/nix/store/..").unwrap(), "/nix");
        assert_eq!(dir("/").unwrap(), "/");
        assert_eq!(
            dir("foo").unwrap_err().to_string(),
            "store directory path \"foo\" is not an absolute path"
        );
        assert_eq!(
            dir("").unwrap_err().to_string(),
            "store directory path \"\" is not an absolute path"
        );
        assert_eq!(
            dir("/nix\0").unwrap_err().to_string(),
            "path segment '/nix␀' must not contain null (\\0) bytes"
        );
    }

    #[test]
    fn is_in_store_is_lexical_like_is_in_dir() {
        // libutil-tests/file-system.cc `isInDir`, with `/foo` as the
        // store directory.
        let foo = StoreDir::new("/foo");
        assert!(foo.is_in_store("/foo/bar"));
        assert!(!foo.is_in_store("/zes/foo/bar"));
        assert!(foo.is_in_store("/foo/.ssh"));
        assert!(foo.is_in_store("/foo/..."));
        assert!(!foo.is_in_store("/foo"));
        assert!(!foo.is_in_store("/foo/."));
        assert!(!foo.is_in_store("/foo/../bar"));
        // And what `lexically_relative` makes of the rest.
        let store = StoreDir::default();
        assert!(store.is_in_store(format!("/nix/store/{HASH}-x")));
        assert!(store.is_in_store("/nix//store//x/"));
        assert!(store.is_in_store("/nix/store/x/../y"));
        assert!(!store.is_in_store("/nix/store/"));
        assert!(!store.is_in_store("/nix/store//"));
        assert!(!store.is_in_store("/nix/store/./x"));
        assert!(!store.is_in_store("/nix/./store/x"));
        assert!(!store.is_in_store("/nix/storex/y"));
        assert!(!store.is_in_store("nix/store/x"));
        assert!(!store.is_in_store(""));
        let root = StoreDir::new("/");
        assert!(root.is_in_store("/x"));
        assert!(!root.is_in_store("/"));
        assert!(!root.is_in_store("/.."));
    }

    #[test]
    fn to_store_path_splits_like_nix() {
        let store = StoreDir::default();
        let p = StorePath::new(format!("{HASH}-foo")).unwrap();
        let split = |s: &str| {
            store
                .to_store_path(s)
                .map(|(sp, rest)| (sp, rest.abs().to_owned()))
                .map_err(|e| e.to_string())
        };
        assert_eq!(
            split(&format!("/nix/store/{HASH}-foo")),
            Ok((p.clone(), "/".into()))
        );
        assert_eq!(
            split(&format!("/nix/store/{HASH}-foo/")),
            Ok((p.clone(), "/".into()))
        );
        assert_eq!(
            split(&format!("/nix/store/{HASH}-foo/bar//baz/./")),
            Ok((p.clone(), "/bar/baz".into()))
        );
        assert_eq!(
            split(&format!("/nix/store/{HASH}-foo/a/../b")),
            Ok((p, "/b".into()))
        );
        assert_eq!(
            split("/tmp/x"),
            Err("path '/tmp/x' is not in the Nix store".into())
        );
        assert_eq!(
            split("/nix/store"),
            Err("path '/nix/store' is not in the Nix store".into())
        );
        assert_eq!(
            split("/nix/store/abc"),
            Err("'abc' is too short to be a valid store path".into())
        );
        assert_eq!(
            split(&format!("/nix/store/{HASH}-foo/a\0b")),
            Err("path segment '/a␀b' must not contain null (\\0) bytes".into())
        );
        // The store path ends at the first slash past the store
        // directory's length, so extra slashes before it are not
        // skipped (`toStorePath` assumes a canonical prefix).
        assert!(split(&format!("/nix//store/{HASH}-foo")).is_err());
        let root = StoreDir::new("/");
        assert_eq!(
            root.to_store_path(format!("/{HASH}-foo/x"))
                .map(|(sp, rest)| (sp.to_string(), rest.abs().to_owned())),
            Ok((format!("{HASH}-foo"), "/x".into()))
        );
        // `toStorePath(path).first`.
        assert_eq!(
            store
                .store_path_of(format!("/nix/store/{HASH}-foo/a/b"))
                .unwrap()
                .to_string(),
            format!("{HASH}-foo")
        );
        assert_eq!(
            store.store_path_of("/tmp/x").unwrap_err().to_string(),
            "path '/tmp/x' is not in the Nix store"
        );
        // The rest of the path is bytes.
        assert_eq!(
            store
                .to_store_path([format!("/nix/store/{HASH}-foo").as_bytes(), b"/d\xe9/x"].concat())
                .map(|(sp, rest)| (sp, rest.into_bytes())),
            Ok((
                StorePath::new(format!("{HASH}-foo")).unwrap(),
                b"/d\xe9/x".to_vec()
            ))
        );
    }

    /// Paths and base names are bytes, and any bytes get Nix's messages,
    /// each quoting them as they are. Oracle: `nix-store --store dummy://
    /// -q <path>` with the raw bytes (`tests/oracle.rs` runs it), whose
    /// `toStorePath` and `parseStorePath` say these.
    #[test]
    fn parse_takes_bytes_like_nix() {
        let store = StoreDir::default();
        let message = |path: &[u8]| store.parse(path).unwrap_err().message_bytes();
        let base_name = |path: &[u8]| [HASH.as_bytes(), path].concat();
        let in_store = |name: &[u8]| [b"/nix/store/".as_slice(), name].concat();
        assert_eq!(
            BStr::new(&message(&in_store(&base_name(b"-caf\xe9")))),
            BStr::new(
                &[
                    b"path '".as_slice(),
                    &base_name(b"-caf\xe9"),
                    b"' is not a valid store path: name 'caf\xe9' contains illegal character '\xe9'",
                ]
                .concat()
            )
        );
        assert_eq!(
            message(&in_store(b"caf\xe9")),
            b"'caf\xe9' is too short to be a valid store path"
        );
        let bad_hash = [&b"\xe9"[..], &HASH.as_bytes()[1..], b"-x"].concat();
        assert_eq!(
            message(&in_store(&bad_hash)),
            [
                b"store path '".as_slice(),
                &bad_hash,
                b"' contains illegal base-32 character '\xe9'"
            ]
            .concat()
        );
        assert_eq!(
            message(&in_store(&base_name(b"\xe9x"))),
            [
                b"'".as_slice(),
                &base_name(b"\xe9x"),
                b"' can't name a store path because the hash part is not followed by a '-'"
            ]
            .concat()
        );
        assert_eq!(
            message(b"/tmp/caf\xe9"),
            b"path \"/tmp/caf\xe9\" is not in the Nix store"
        );
        assert_eq!(
            message(b"/nix/store/../tmp/caf\xe9"),
            b"path \"/nix/tmp/caf\xe9\" is not in the Nix store"
        );
        assert_eq!(message(b"caf\xe9"), b"not an absolute path: \"caf\xe9\"");
        assert_eq!(
            store
                .store_path_of(b"/tmp/caf\xe9")
                .unwrap_err()
                .message_bytes(),
            b"path '/tmp/caf\xe9' is not in the Nix store"
        );
        assert_eq!(
            store
                .to_store_path([in_store(&base_name(b"-x")), b"/a\0\xe9".to_vec()].concat())
                .unwrap_err()
                .message_bytes(),
            b"path segment '/a\xe2\x90\x80\xe9' must not contain null (\\0) bytes"
        );
        // `Display` is the same text, lossily.
        assert_eq!(
            store.parse(b"/tmp/caf\xe9").unwrap_err().to_string(),
            "path \"/tmp/caf\u{fffd}\" is not in the Nix store"
        );
        assert_eq!(
            StorePath::new(&bad_hash).unwrap_err().to_string(),
            format!(
                "store path '\u{fffd}{}-x' contains illegal base-32 character '\u{fffd}'",
                &HASH[1..]
            )
        );
        // A valid path given as bytes is the path given as text.
        assert_eq!(
            store.parse(in_store(&base_name(b"-foo"))).unwrap(),
            store.parse(format!("/nix/store/{HASH}-foo")).unwrap()
        );
        assert!(store.is_in_store(b"/nix/store/caf\xe9"));
        assert!(!store.is_in_store(b"/nix/store\xe9/x"));
        assert_eq!(store.maybe_parse(b"/nix/store/caf\xe9"), None);
    }

    #[test]
    fn output_path_names() {
        assert_eq!(output_path_name("foo", b"out"), b"foo");
        assert_eq!(output_path_name("foo", b"dev"), b"foo-dev");
        assert_eq!(output_path_name("foo", b"a\xff"), b"foo-a\xff");
    }

    /// A name is bytes: the path is computed from them, and an invalid
    /// name is refused with that path. Oracle: `nix-instantiate --eval
    /// --expr 'builtins.toFile ("a" + builtins.substring 0 1 "é") "x"'`.
    #[test]
    fn paths_are_computed_from_name_bytes() {
        let store = StoreDir::default();
        let hash = Hash::of(HashAlgo::Sha256, b"x");
        let err = store
            .make_text_path(b"a\xc3".as_slice(), &hash, &BTreeSet::new())
            .unwrap_err();
        assert_eq!(
            bstr::BStr::new(&err.message_bytes()),
            bstr::BStr::new(
                b"path 'zmkqmdy26br27qy5hn5z93l3v8d2jnm5-a\xc3' is not a valid store path: name 'a\xc3' contains illegal character '\xc3'"
            )
        );
        let fixed = FixedOutputInfo {
            method: FileIngestionMethod::Flat,
            hash,
            references: Default::default(),
        };
        let err = store
            .make_fixed_output_path(b"a b".as_slice(), &fixed)
            .unwrap_err();
        assert!(matches!(err, StorePathError::BadName { .. }), "{err}");
        // A valid name gives the same path as text or as bytes.
        assert_eq!(
            store.make_fixed_output_path("ab", &fixed).unwrap(),
            store
                .make_fixed_output_path(b"ab".as_slice(), &fixed)
                .unwrap()
        );
    }
}
