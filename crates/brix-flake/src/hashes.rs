//! The two hashes fetcher inputs carry: a Git revision (`rev`) and a NAR
//! hash (`narHash`).
//!
//! These are thin wrappers over [`brix_store::Hash`] that fix the
//! algorithm and the textual forms Nix uses for each: `rev` is printed as
//! 40 lower-case hex digits (`Hash::gitRev`), `narHash` as an SRI string
//! (`sha256-<base64>`).

use std::fmt;

use brix_store::{Hash, HashAlgo, HashError, HashFormat, XpSettings};

use crate::{err, Error, Result};

/// A Git commit hash (SHA-1), as the `rev` attribute carries it.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GitRev([u8; 20]);

/// `Input::getRev`'s reading of a `rev`, whatever algorithm it names:
/// `Hash::parseAnyPrefixed`, falling back to `Hash::parseAny(s, SHA1)`. A
/// `blake3` prefix needs the `blake3-hashes` feature in `xp`.
pub fn parse_rev_hash(s: &str, xp: XpSettings) -> Result<Hash> {
    match Hash::parse_any_prefixed(s, xp) {
        Ok(h) => Ok(h),
        Err(HashError::MissingFeature(e)) => Err(err!("{}", e)),
        Err(_) => Hash::parse_any(s, Some(HashAlgo::Sha1), xp).map_err(|e| err!("{}", e)),
    }
}

impl GitRev {
    /// The all-zero revision Nix uses for a repository without commits.
    pub const NULL: GitRev = GitRev([0; 20]);

    /// `Input::getRev`: `Hash::parseAnyPrefixed`, falling back to
    /// `Hash::parseAny(s, SHA1)`, so `rev` may be given in base-16,
    /// Nix base-32 or base-64, with or without a `sha1:` prefix. A
    /// `blake3` prefix needs the `blake3-hashes` feature in `xp`, and is
    /// then refused as a hash Git does not use.
    pub fn parse(s: &str, xp: XpSettings) -> Result<GitRev> {
        let hash = parse_rev_hash(s, xp)?;
        if hash.algo() != HashAlgo::Sha1 {
            return Err(err!(
                "Hash '{}' is not supported by Git. Supported types are sha1 and sha256.",
                hash.to_string(HashFormat::Base16, true)
            ));
        }
        let bytes: [u8; 20] = hash
            .as_bytes()
            .try_into()
            .expect("sha1 digests are 20 bytes");
        Ok(GitRev(bytes))
    }

    /// The revision with these SHA-1 digest bytes.
    pub fn from_bytes(bytes: [u8; 20]) -> GitRev {
        GitRev(bytes)
    }

    /// `Hash::parseAny(s, SHA1)`: a revision that must be SHA-1, as the
    /// forges' schemes and APIs require ("hash '…' should have type
    /// 'sha1'").
    pub fn parse_sha1(s: &str, xp: XpSettings) -> Result<GitRev> {
        let hash = Hash::parse_any(s, Some(HashAlgo::Sha1), xp).map_err(|e| err!("{}", e))?;
        Ok(GitRev(
            hash.as_bytes()
                .try_into()
                .expect("sha1 digests are 20 bytes"),
        ))
    }

    pub fn as_bytes(&self) -> &[u8; 20] {
        &self.0
    }

    /// `Hash::gitRev`: 40 lower-case hex digits.
    pub fn to_hex(&self) -> String {
        brix_store::hash::base16::encode(&self.0)
    }

    /// `Hash::gitShortRev`: the first 7 hex digits.
    pub fn short(&self) -> String {
        self.to_hex()[..7].to_string()
    }

    pub fn is_null(&self) -> bool {
        *self == GitRev::NULL
    }
}

impl fmt::Debug for GitRev {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "GitRev({})", self.to_hex())
    }
}

impl fmt::Display for GitRev {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

/// A SHA-256 NAR hash, as the `narHash` attribute carries it.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NarHash([u8; 32]);

impl NarHash {
    pub fn from_bytes(bytes: [u8; 32]) -> NarHash {
        NarHash(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// `Input::getNarHash`: `Hash::parseSRI` followed by the
    /// `narHash must use SHA-256` check. An empty string is the zero hash
    /// (Nix's `Hash(HashAlgorithm::SHA256)`). A BLAKE3 hash needs the
    /// `blake3-hashes` feature in `xp`, and then fails the check.
    pub fn parse_sri(s: &str, xp: XpSettings) -> Result<NarHash> {
        if s.is_empty() {
            return Ok(NarHash([0; 32]));
        }
        let hash = Hash::parse_sri(s, xp).map_err(|e| Error::Message(e.to_string()))?;
        if hash.algo() != HashAlgo::Sha256 {
            return Err(err!("narHash must use SHA-256"));
        }
        let bytes: [u8; 32] = hash
            .as_bytes()
            .try_into()
            .expect("sha256 digests are 32 bytes");
        Ok(NarHash(bytes))
    }

    /// `Hash::to_string(HashFormat::SRI, true)`.
    pub fn to_sri(&self) -> String {
        Hash::from_bytes(HashAlgo::Sha256, &self.0).to_string(HashFormat::Sri, true)
    }
}

impl fmt::Debug for NarHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "NarHash({})", self.to_sri())
    }
}

impl fmt::Display for NarHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_sri())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use brix_store::Xp;

    fn parse_rev(s: &str) -> Result<GitRev> {
        GitRev::parse(s, XpSettings::NONE)
    }

    fn parse_nar(s: &str) -> Result<NarHash> {
        NarHash::parse_sri(s, XpSettings::NONE)
    }

    /// A BLAKE3 `rev` or `narHash` is refused for the missing feature,
    /// and with `--extra-experimental-features blake3-hashes` for its
    /// algorithm, as `builtins.fetchTree` refuses them (`type = "git"`,
    /// `"github"` and `"tarball"`).
    #[test]
    fn blake3_needs_the_feature_and_is_then_the_wrong_algorithm() {
        let b3 = format!("blake3:{}", "a".repeat(64));
        let b3_sri = format!("blake3-{}=", "A".repeat(43));
        let missing = "experimental Nix feature 'blake3-hashes' is disabled; add '--extra-experimental-features blake3-hashes' to enable it";
        let on = XpSettings::NONE.with(Xp::Blake3Hashes);
        assert_eq!(parse_rev(&b3).unwrap_err().to_string(), missing);
        assert_eq!(
            GitRev::parse(&b3, on).unwrap_err().to_string(),
            format!("Hash '{b3}' is not supported by Git. Supported types are sha1 and sha256.")
        );
        assert_eq!(
            GitRev::parse_sha1(&b3, XpSettings::NONE)
                .unwrap_err()
                .to_string(),
            missing
        );
        assert_eq!(
            GitRev::parse_sha1(&b3, on).unwrap_err().to_string(),
            format!("hash '{b3}' should have type 'sha1'")
        );
        assert_eq!(parse_nar(&b3_sri).unwrap_err().to_string(), missing);
        assert_eq!(
            NarHash::parse_sri(&b3_sri, on).unwrap_err().to_string(),
            "narHash must use SHA-256"
        );
    }

    #[test]
    fn git_rev_round_trip() {
        let hex = "2aae6c35c94fcfb415dbe95f408b9ce91ee846ed";
        let rev = parse_rev(hex).unwrap();
        assert_eq!(rev.to_hex(), hex);
        assert_eq!(rev.short(), "2aae6c3");
        assert_eq!(parse_rev(&format!("sha1:{hex}")).unwrap(), rev);
        // Nix base-32 and base-64 spellings are accepted too.
        let h = Hash::from_bytes(HashAlgo::Sha1, rev.as_bytes());
        assert_eq!(
            parse_rev(&h.to_string(HashFormat::Nix32, false)).unwrap(),
            rev
        );
        assert_eq!(
            parse_rev(&h.to_string(HashFormat::Base64, false)).unwrap(),
            rev
        );
        assert!(parse_rev("abc").is_err());
        assert!(parse_rev(&"zz".repeat(20)).is_err());
        assert!(parse_rev(&format!("sha256:{}", "a".repeat(64))).is_err());
        assert!(parse_rev(&"0".repeat(40)).unwrap().is_null());
        assert_eq!(GitRev::from_bytes(*rev.as_bytes()), rev);
        assert_eq!(GitRev::from_bytes([0xab; 20]).to_hex(), "ab".repeat(20));
    }

    #[test]
    fn nar_hash_sri_round_trip() {
        let sri = "sha256-hM20uyap1a0M9d344I692r+ik4gTMyj60cQWO+hAYP8=";
        let h = parse_nar(sri).unwrap();
        assert_eq!(h.to_sri(), sri);
        assert_eq!(
            parse_nar("sha1-2jmj7l5rSw0yVb/vlWAYkK/YBwk=")
                .unwrap_err()
                .to_string(),
            "narHash must use SHA-256"
        );
        assert!(parse_nar("nope").is_err());
        assert!(parse_nar("sha256-abc").is_err());
        assert_eq!(parse_nar("").unwrap(), NarHash([0; 32]));
    }
}
