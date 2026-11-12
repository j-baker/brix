//! Content addressing: how a store object's hash relates to its content.
//!
//! Mirrors `src/libstore/content-address.cc` and the `FileIngestionMethod`
//! part of `src/libutil/file-content-address.cc`. The string forms matter
//! because they are hashed into store paths and written into `.drv` files.

use std::collections::BTreeSet;
use std::fmt;

use crate::experimental::{MissingExperimentalFeature, Xp, XpSettings};
use crate::hash::{Hash, HashAlgo, HashError, HashFormat};
use crate::store_path::StorePath;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ContentAddressError {
    #[error("Unknown file ingestion method '{0}', expect `flat`, `nar`, or `git`")]
    UnknownIngestionMethod(String),
    #[error("not a content address because it is not in the form '<prefix>:<rest>': {0}")]
    NoPrefix(String),
    #[error("content address hash must be in form '<algo>:<hash>', but found: {0}")]
    NoHashAlgo(String),
    #[error(
        "content address prefix '{0}' is unrecognized. Recogonized prefixes are 'text' or 'fixed'"
    )]
    UnknownPrefix(String),
    #[error("self-reference not allowed with text hashing")]
    TextSelfReference,
    #[error(transparent)]
    Hash(#[from] HashError),
    #[error(transparent)]
    MissingFeature(#[from] MissingExperimentalFeature),
}

impl ContentAddressError {
    /// The message as Nix writes it: a hash's error quotes the hash's
    /// bytes as they are ([`HashError::message_bytes`]).
    pub fn message_bytes(&self) -> Vec<u8> {
        match self {
            ContentAddressError::Hash(e) => e.message_bytes(),
            other => other.to_string().into_bytes(),
        }
    }
}

/// How a file system object is turned into bytes for hashing
/// (`FileIngestionMethod`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum FileIngestionMethod {
    /// A single regular file's contents, as is.
    Flat,
    /// The NAR serialisation of the object (`recursive` in old terms).
    NixArchive,
    /// Git tree/blob hashing (experimental in Nix).
    Git,
}

impl FileIngestionMethod {
    /// `renderFileIngestionMethod`: `flat`, `nar` or `git`.
    pub const fn render(self) -> &'static str {
        match self {
            FileIngestionMethod::Flat => "flat",
            FileIngestionMethod::NixArchive => "nar",
            FileIngestionMethod::Git => "git",
        }
    }

    /// `parseFileIngestionMethod`: `flat`, `nar` or `git`. The old spelling
    /// `recursive` is not accepted here; `derivationStrict` maps it to
    /// `nar` itself.
    pub fn parse(s: &str) -> Result<FileIngestionMethod, ContentAddressError> {
        match s {
            "flat" => Ok(FileIngestionMethod::Flat),
            "nar" => Ok(FileIngestionMethod::NixArchive),
            "git" => Ok(FileIngestionMethod::Git),
            _ => Err(ContentAddressError::UnknownIngestionMethod(s.to_owned())),
        }
    }

    /// `makeFileIngestionPrefix`: the prefix used in hash strings, `""`,
    /// `r:` or `git:`.
    pub const fn prefix(self) -> &'static str {
        match self {
            FileIngestionMethod::Flat => "",
            FileIngestionMethod::NixArchive => "r:",
            FileIngestionMethod::Git => "git:",
        }
    }
}

/// `ContentAddressMethod`: a file ingestion method, or `text` (a flat file
/// whose references are recorded, e.g. a `.drv`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ContentAddressMethod {
    Text,
    Flat,
    NixArchive,
    Git,
}

impl ContentAddressMethod {
    pub fn from_ingestion(m: FileIngestionMethod) -> ContentAddressMethod {
        match m {
            FileIngestionMethod::Flat => ContentAddressMethod::Flat,
            FileIngestionMethod::NixArchive => ContentAddressMethod::NixArchive,
            FileIngestionMethod::Git => ContentAddressMethod::Git,
        }
    }

    /// `getFileIngestionMethod`: `text` ingests as `flat`.
    pub const fn ingestion_method(self) -> FileIngestionMethod {
        match self {
            ContentAddressMethod::Text | ContentAddressMethod::Flat => FileIngestionMethod::Flat,
            ContentAddressMethod::NixArchive => FileIngestionMethod::NixArchive,
            ContentAddressMethod::Git => FileIngestionMethod::Git,
        }
    }

    /// `render`: `text`, `flat`, `nar` or `git`.
    pub const fn render(self) -> &'static str {
        match self {
            ContentAddressMethod::Text => "text",
            other => other.ingestion_method().render(),
        }
    }

    /// `parse`: the inverse of [`ContentAddressMethod::render`].
    pub fn parse(s: &str) -> Result<ContentAddressMethod, ContentAddressError> {
        if s == "text" {
            Ok(ContentAddressMethod::Text)
        } else {
            FileIngestionMethod::parse(s).map(Self::from_ingestion)
        }
    }

    /// `renderPrefix`: `text:`, `""`, `r:` or `git:`, the form used in
    /// derivation outputs (`r:sha256`).
    pub const fn prefix(self) -> &'static str {
        match self {
            ContentAddressMethod::Text => "text:",
            other => other.ingestion_method().prefix(),
        }
    }

    /// `parsePrefix`: strip a method prefix from the front of `s`,
    /// defaulting to `flat`. `git:` needs the `git-hashing` feature. The
    /// string is bytes, as a derivation's are.
    pub fn parse_prefix(
        s: &[u8],
        xp: XpSettings,
    ) -> Result<(ContentAddressMethod, &[u8]), MissingExperimentalFeature> {
        Ok(if let Some(rest) = s.strip_prefix(b"r:") {
            (ContentAddressMethod::NixArchive, rest)
        } else if let Some(rest) = s.strip_prefix(b"git:") {
            xp.require(Xp::GitHashing, None)?;
            (ContentAddressMethod::Git, rest)
        } else if let Some(rest) = s.strip_prefix(b"text:") {
            (ContentAddressMethod::Text, rest)
        } else {
            (ContentAddressMethod::Flat, s)
        })
    }

    /// `renderPrefixModern`: `text:` or `fixed:<ingestion prefix>`.
    fn prefix_modern(self) -> String {
        match self {
            ContentAddressMethod::Text => "text:".into(),
            other => format!("fixed:{}", other.ingestion_method().prefix()),
        }
    }

    /// `renderWithAlgo`: e.g. `fixed:r:sha256`.
    pub fn render_with_algo(self, algo: HashAlgo) -> String {
        format!("{}{}", self.prefix_modern(), algo.name())
    }
}

/// `parseContentAddressMethodPrefix`: parse `text:<algo>:` or
/// `fixed:[r:|git:]<algo>:` and return the rest. `git:` needs the
/// `git-hashing` feature.
fn parse_method_prefix(
    s: &str,
    xp: XpSettings,
) -> Result<(ContentAddressMethod, HashAlgo, &str), ContentAddressError> {
    let (prefix, rest) = s
        .split_once(':')
        .ok_or_else(|| ContentAddressError::NoPrefix(s.to_owned()))?;
    fn parse_algo<'a>(
        whole: &str,
        rest: &'a str,
        xp: XpSettings,
    ) -> Result<(HashAlgo, &'a str), ContentAddressError> {
        let (algo, rest) = rest
            .split_once(':')
            .ok_or_else(|| ContentAddressError::NoHashAlgo(whole.to_owned()))?;
        Ok((HashAlgo::parse(algo, xp)?, rest))
    }
    match prefix {
        "text" => {
            let (algo, rest) = parse_algo(s, rest, xp)?;
            Ok((ContentAddressMethod::Text, algo, rest))
        }
        "fixed" => {
            let (method, rest) = if let Some(r) = rest.strip_prefix("r:") {
                (ContentAddressMethod::NixArchive, r)
            } else if let Some(r) = rest.strip_prefix("git:") {
                xp.require(Xp::GitHashing, None)?;
                (ContentAddressMethod::Git, r)
            } else {
                (ContentAddressMethod::Flat, rest)
            };
            let (algo, rest) = parse_algo(s, rest, xp)?;
            Ok((method, algo, rest))
        }
        other => Err(ContentAddressError::UnknownPrefix(other.to_owned())),
    }
}

/// `ContentAddress`: a method and the hash it produced.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ContentAddress {
    pub method: ContentAddressMethod,
    pub hash: Hash,
}

impl ContentAddress {
    /// `render`: `fixed:r:sha256:<nix32>` or `text:sha256:<nix32>`. This
    /// is the `ca` field of path info and the `ca` in `narinfo`.
    pub fn render(&self) -> String {
        format!(
            "{}{}",
            self.method.prefix_modern(),
            self.hash.to_string(HashFormat::Nix32, true)
        )
    }

    /// `parse`: the inverse of [`ContentAddress::render`]; the hash may
    /// be in any non-SRI encoding.
    pub fn parse(s: &str, xp: XpSettings) -> Result<ContentAddress, ContentAddressError> {
        let (method, algo, rest) = parse_method_prefix(s, xp)?;
        Ok(ContentAddress {
            method,
            hash: Hash::parse_non_sri_unprefixed(rest, algo, xp)?,
        })
    }

    /// `parseOpt`: the empty string means no content address.
    pub fn parse_opt(
        s: &str,
        xp: XpSettings,
    ) -> Result<Option<ContentAddress>, ContentAddressError> {
        if s.is_empty() {
            Ok(None)
        } else {
            Self::parse(s, xp).map(Some)
        }
    }

    /// `printMethodAlgo`: `r:sha256`, the form in a `.drv` output.
    pub fn print_method_algo(&self) -> String {
        format!("{}{}", self.method.prefix(), self.hash.algo().name())
    }
}

impl fmt::Display for ContentAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.render())
    }
}

/// `StoreReferences`: what a store object refers to, with the self
/// reference kept separate because the path is not known yet when it is
/// computed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StoreReferences {
    pub others: BTreeSet<StorePath>,
    pub self_reference: bool,
}

impl StoreReferences {
    pub fn is_empty(&self) -> bool {
        !self.self_reference && self.others.is_empty()
    }

    pub fn len(&self) -> usize {
        self.others.len() + usize::from(self.self_reference)
    }
}

/// `FixedOutputInfo`: a file-ingested object with references.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FixedOutputInfo {
    pub method: FileIngestionMethod,
    pub hash: Hash,
    pub references: StoreReferences,
}

/// `TextInfo`: a `text` object (sha256 of flat contents) with references
/// but never a self reference.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextInfo {
    pub hash: Hash,
    pub references: BTreeSet<StorePath>,
}

/// `ContentAddressWithReferences`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ContentAddressWithReferences {
    Text(TextInfo),
    Fixed(FixedOutputInfo),
}

impl ContentAddressWithReferences {
    /// `withoutRefs`.
    pub fn without_refs(ca: &ContentAddress) -> ContentAddressWithReferences {
        match ca.method {
            ContentAddressMethod::Text => ContentAddressWithReferences::Text(TextInfo {
                hash: ca.hash,
                references: BTreeSet::new(),
            }),
            m => ContentAddressWithReferences::Fixed(FixedOutputInfo {
                method: m.ingestion_method(),
                hash: ca.hash,
                references: StoreReferences::default(),
            }),
        }
    }

    /// `fromParts`.
    pub fn from_parts(
        method: ContentAddressMethod,
        hash: Hash,
        references: StoreReferences,
    ) -> Result<ContentAddressWithReferences, ContentAddressError> {
        match method {
            ContentAddressMethod::Text => {
                if references.self_reference {
                    return Err(ContentAddressError::TextSelfReference);
                }
                Ok(ContentAddressWithReferences::Text(TextInfo {
                    hash,
                    references: references.others,
                }))
            }
            m => Ok(ContentAddressWithReferences::Fixed(FixedOutputInfo {
                method: m.ingestion_method(),
                hash,
                references,
            })),
        }
    }

    /// `getMethod`.
    pub fn method(&self) -> ContentAddressMethod {
        match self {
            ContentAddressWithReferences::Text(_) => ContentAddressMethod::Text,
            ContentAddressWithReferences::Fixed(f) => {
                ContentAddressMethod::from_ingestion(f.method)
            }
        }
    }

    /// `getHash`.
    pub fn hash(&self) -> &Hash {
        match self {
            ContentAddressWithReferences::Text(t) => &t.hash,
            ContentAddressWithReferences::Fixed(f) => &f.hash,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_and_parses() {
        let h = Hash::of(HashAlgo::Sha256, b"abc");
        let ca = ContentAddress {
            method: ContentAddressMethod::NixArchive,
            hash: h,
        };
        let s = ca.render();
        assert_eq!(
            s,
            format!("fixed:r:sha256:{}", h.to_string(HashFormat::Nix32, false))
        );
        assert_eq!(ContentAddress::parse(&s, XpSettings::NONE).unwrap(), ca);
        assert_eq!(ca.print_method_algo(), "r:sha256");
        let text = ContentAddress {
            method: ContentAddressMethod::Text,
            hash: h,
        };
        assert!(text.render().starts_with("text:sha256:"));
        assert_eq!(
            ContentAddress::parse(&text.render(), XpSettings::NONE).unwrap(),
            text
        );
        assert_eq!(text.print_method_algo(), "text:sha256");
        let flat = ContentAddress {
            method: ContentAddressMethod::Flat,
            hash: Hash::of(HashAlgo::Sha1, b"abc"),
        };
        assert!(flat.render().starts_with("fixed:sha1:"));
        assert_eq!(flat.print_method_algo(), "sha1");
        // Base16 hash bodies parse too.
        let s16 = format!("fixed:r:sha256:{}", h.to_string(HashFormat::Base16, false));
        assert_eq!(ContentAddress::parse(&s16, XpSettings::NONE).unwrap(), ca);
        assert_eq!(
            ContentAddress::parse_opt("", XpSettings::NONE).unwrap(),
            None
        );
    }

    #[test]
    fn method_strings() {
        let prefix =
            |s: &'static str| ContentAddressMethod::parse_prefix(s.as_bytes(), XpSettings::NONE);
        assert_eq!(
            prefix("r:sha256"),
            Ok((ContentAddressMethod::NixArchive, &b"sha256"[..]))
        );
        assert_eq!(
            prefix("sha256"),
            Ok((ContentAddressMethod::Flat, &b"sha256"[..]))
        );
        assert_eq!(
            prefix("text:sha256"),
            Ok((ContentAddressMethod::Text, &b"sha256"[..]))
        );
        // `git:` is `git-hashing`'s, in derivation outputs and in content
        // addresses alike.
        let missing = "experimental Nix feature 'git-hashing' is disabled; add '--extra-experimental-features git-hashing' to enable it";
        assert_eq!(prefix("git:sha1").unwrap_err().to_string(), missing);
        let git = XpSettings::NONE.with(Xp::GitHashing);
        assert_eq!(
            ContentAddressMethod::parse_prefix(b"git:sha1", git),
            Ok((ContentAddressMethod::Git, &b"sha1"[..]))
        );
        let ca = format!("fixed:git:sha1:{}", "0".repeat(40));
        assert_eq!(
            ContentAddress::parse(&ca, XpSettings::NONE)
                .unwrap_err()
                .to_string(),
            missing
        );
        assert_eq!(
            ContentAddress::parse(&ca, git).unwrap().method,
            ContentAddressMethod::Git
        );
        assert_eq!(
            ContentAddressMethod::NixArchive.render_with_algo(HashAlgo::Sha256),
            "fixed:r:sha256"
        );
        assert_eq!(
            ContentAddressMethod::Text.render_with_algo(HashAlgo::Sha256),
            "text:sha256"
        );
        // `parseFileSerialisationMethodOpt` knows only `flat` and `nar`.
        assert_eq!(
            FileIngestionMethod::parse("recursive")
                .unwrap_err()
                .to_string(),
            "Unknown file ingestion method 'recursive', expect `flat`, `nar`, or `git`"
        );
        assert_eq!(
            ContentAddressMethod::parse("recursive"),
            Err(ContentAddressError::UnknownIngestionMethod(
                "recursive".into()
            ))
        );
        assert_eq!(
            ContentAddressMethod::parse("text"),
            Ok(ContentAddressMethod::Text)
        );
        assert_eq!(
            FileIngestionMethod::parse("tar").unwrap_err().to_string(),
            "Unknown file ingestion method 'tar', expect `flat`, `nar`, or `git`"
        );
        assert_eq!(
            ContentAddress::parse("bogus:sha256:abc", XpSettings::NONE).unwrap_err().to_string(),
            "content address prefix 'bogus' is unrecognized. Recogonized prefixes are 'text' or 'fixed'"
        );
    }
}
