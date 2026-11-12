//! Cryptographic hashes and the encodings Nix uses for them.
//!
//! Mirrors `src/libutil/hash.cc`, `base-n.cc` and `base-nix-32.cc`. The
//! important peculiarity is Nix's base-32 ("nix32"): a custom alphabet
//! (`0123456789abcdfghijklmnpqrsvwxyz`, no `e`, `o`, `u`, `t`) emitted
//! least-significant quintet *last*, i.e. the string is built from the end
//! of the bit stream backwards. A hash's nix32 form is what appears in
//! store paths.

use std::fmt;

use bstr::BString;
use digest::Digest;

use crate::experimental::{MissingExperimentalFeature, Xp, XpSettings};

/// The hash algorithms Nix knows. `blake3` is behind the experimental
/// feature `blake3-hashes`: every parser here takes the [`XpSettings`]
/// and refuses the name (or a BLAKE3 hash) without it, as Nix does.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum HashAlgo {
    Md5,
    Sha1,
    Sha256,
    Sha512,
    Blake3,
}

impl HashAlgo {
    /// All algorithms, in the order Nix's error messages list them.
    pub const ALL: [HashAlgo; 5] = [
        HashAlgo::Blake3,
        HashAlgo::Md5,
        HashAlgo::Sha1,
        HashAlgo::Sha256,
        HashAlgo::Sha512,
    ];

    /// The digest size in bytes (`regularHashSize`).
    pub const fn size(self) -> usize {
        match self {
            HashAlgo::Md5 => 16,
            HashAlgo::Sha1 => 20,
            HashAlgo::Sha256 | HashAlgo::Blake3 => 32,
            HashAlgo::Sha512 => 64,
        }
    }

    /// The name Nix prints (`printHashAlgo`).
    pub const fn name(self) -> &'static str {
        match self {
            HashAlgo::Md5 => "md5",
            HashAlgo::Sha1 => "sha1",
            HashAlgo::Sha256 => "sha256",
            HashAlgo::Sha512 => "sha512",
            HashAlgo::Blake3 => "blake3",
        }
    }

    /// `parseHashAlgoOpt`: the algorithm, or `None` if the name is unknown.
    /// `blake3` needs the `blake3-hashes` feature. The name is bytes, as
    /// Nix's strings are.
    pub fn parse_opt<S: AsRef<[u8]> + ?Sized>(
        s: &S,
        xp: XpSettings,
    ) -> Result<Option<HashAlgo>, HashError> {
        let s = s.as_ref();
        if s == b"blake3" {
            xp.require(Xp::Blake3Hashes, None)?;
        }
        Ok(match s {
            b"blake3" => Some(HashAlgo::Blake3),
            b"md5" => Some(HashAlgo::Md5),
            b"sha1" => Some(HashAlgo::Sha1),
            b"sha256" => Some(HashAlgo::Sha256),
            b"sha512" => Some(HashAlgo::Sha512),
            _ => None,
        })
    }

    /// `parseHashAlgo`, with Nix's error message for unknown names, which
    /// quotes the name's bytes as they are.
    pub fn parse<S: AsRef<[u8]> + ?Sized>(s: &S, xp: XpSettings) -> Result<HashAlgo, HashError> {
        let s = s.as_ref();
        Self::parse_opt(s, xp)?.ok_or_else(|| HashError::UnknownAlgo(s.into()))
    }
}

impl fmt::Display for HashAlgo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// The textual encodings of a hash (`HashFormat`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum HashFormat {
    /// RFC 4648 base64, with padding.
    Base64,
    /// Nix's base-32 variant, as used in store paths.
    Nix32,
    /// Lower-case hexadecimal.
    Base16,
    /// Subresource integrity: `<algo>-<base64>`.
    Sri,
}

impl HashFormat {
    /// `printHashFormat`.
    pub const fn name(self) -> &'static str {
        match self {
            HashFormat::Base64 => "base64",
            HashFormat::Nix32 => "nix32",
            HashFormat::Base16 => "base16",
            HashFormat::Sri => "sri",
        }
    }

    /// The warning Nix prints when `base32` names nix32.
    pub const BASE32_DEPRECATION: &'static str =
        "\"base32\" is a deprecated alias for hash format \"nix32\".";

    /// `parseHashFormatOpt`: the format, and the warning Nix prints when
    /// it is given by the deprecated alias `base32`
    /// ([`HashFormat::BASE32_DEPRECATION`]), which the caller must show.
    pub fn parse_opt<S: AsRef<[u8]> + ?Sized>(s: &S) -> Option<(HashFormat, Option<&'static str>)> {
        match s.as_ref() {
            b"base16" => Some((HashFormat::Base16, None)),
            b"nix32" => Some((HashFormat::Nix32, None)),
            b"base32" => Some((HashFormat::Nix32, Some(Self::BASE32_DEPRECATION))),
            b"base64" => Some((HashFormat::Base64, None)),
            b"sri" => Some((HashFormat::Sri, None)),
            _ => None,
        }
    }

    /// `parseHashFormat`, with Nix's error message (the name's bytes as
    /// they are); the warning as for [`HashFormat::parse_opt`].
    pub fn parse<S: AsRef<[u8]> + ?Sized>(
        s: &S,
    ) -> Result<(HashFormat, Option<&'static str>), HashError> {
        let s = s.as_ref();
        Self::parse_opt(s).ok_or_else(|| HashError::UnknownFormat(s.into()))
    }
}

/// Errors from parsing hashes. The messages are Nix's (`BadHash`,
/// `UsageError`, `FormatError`) so that `builtins.hashString` and friends
/// report the same text.
///
/// What they quote is bytes, as Nix's strings are: a hash or an algorithm
/// name need not be UTF-8. [`HashError::message_bytes`] is the message as
/// Nix writes it; `Display` shows the same text with the bytes that are
/// not UTF-8 replaced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HashError {
    UnknownAlgo(BString),
    UnknownFormat(BString),
    NotSri(BString),
    MissingAlgo(BString),
    MissingAlgoNoContext(BString),
    WrongAlgo {
        hash: BString,
        algo: HashAlgo,
    },
    WrongLength {
        hash: BString,
        algo: HashAlgo,
    },
    InvalidLength {
        encoding: &'static str,
        hash: BString,
        actual: usize,
        expected: usize,
    },
    MissingFeature(MissingExperimentalFeature),
}

impl HashError {
    /// The message as Nix writes it, the quoted hash or name byte for byte.
    pub fn message_bytes(&self) -> Vec<u8> {
        let quoted =
            |before: &str, s: &[u8], after: &str| [before.as_bytes(), s, after.as_bytes()].concat();
        match self {
            HashError::UnknownAlgo(s) => quoted(
                "unknown hash algorithm '",
                s,
                "', expect 'blake3', 'md5', 'sha1', 'sha256', or 'sha512'",
            ),
            HashError::UnknownFormat(s) => quoted(
                "unknown hash format '",
                s,
                "', expect 'base16', 'base32', 'base64', or 'sri'",
            ),
            HashError::NotSri(s) => quoted("hash '", s, "' is not SRI"),
            HashError::MissingAlgo(s) => quoted("hash '", s, "' does not include a type"),
            HashError::MissingAlgoNoContext(s) => quoted(
                "hash '",
                s,
                "' does not include a type, nor is the type otherwise known from context",
            ),
            HashError::WrongAlgo { hash, algo } => {
                quoted("hash '", hash, &format!("' should have type '{algo}'"))
            }
            HashError::WrongLength { hash, algo } => quoted(
                "hash '",
                hash,
                &format!("' has wrong length for hash algorithm '{algo}'"),
            ),
            HashError::InvalidLength {
                encoding,
                hash,
                actual,
                expected,
            } => quoted(
                &format!("invalid {encoding} hash '"),
                hash,
                &format!("', length {actual} != expected length {expected}"),
            ),
            HashError::MissingFeature(e) => e.to_string().into_bytes(),
        }
    }
}

impl fmt::Display for HashError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&String::from_utf8_lossy(&self.message_bytes()))
    }
}

impl std::error::Error for HashError {}

impl From<MissingExperimentalFeature> for HashError {
    fn from(e: MissingExperimentalFeature) -> HashError {
        HashError::MissingFeature(e)
    }
}

/// A hash digest together with its algorithm.
///
/// `len` is normally `algo.size()`; it is shorter only for the output of
/// [`Hash::compress`], which Nix uses to shrink a sha256 to the 20 bytes a
/// store path encodes.
#[derive(Clone, Copy)]
pub struct Hash {
    algo: HashAlgo,
    len: u8,
    bytes: [u8; Hash::MAX_SIZE],
}

impl Hash {
    /// `Hash::maxHashSize`.
    pub const MAX_SIZE: usize = 64;

    /// A zero hash of the given algorithm (`Hash(algo)` in Nix).
    pub fn zero(algo: HashAlgo) -> Hash {
        Hash {
            algo,
            len: algo.size() as u8,
            bytes: [0; Hash::MAX_SIZE],
        }
    }

    /// Wrap raw digest bytes; the slice must be exactly `algo.size()`
    /// bytes long.
    pub fn from_bytes(algo: HashAlgo, digest: &[u8]) -> Hash {
        assert_eq!(digest.len(), algo.size(), "digest length for {algo}");
        let mut h = Hash::zero(algo);
        h.bytes[..digest.len()].copy_from_slice(digest);
        h
    }

    /// Hash `data` in one go (`hashString`).
    pub fn of(algo: HashAlgo, data: &[u8]) -> Hash {
        let mut hasher = Hasher::new(algo);
        hasher.update(data);
        hasher.finish().0
    }

    pub fn algo(&self) -> HashAlgo {
        self.algo
    }

    /// The digest bytes.
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes[..self.len as usize]
    }

    /// `compressHash`: fold the digest down to `new_size` bytes by xor-ing
    /// byte `i` into position `i % new_size`. Store paths use 20 bytes.
    pub fn compress(&self, new_size: usize) -> Hash {
        assert!(new_size > 0 && new_size <= Hash::MAX_SIZE);
        let mut out = Hash {
            algo: self.algo,
            len: new_size as u8,
            bytes: [0; Hash::MAX_SIZE],
        };
        for (i, b) in self.as_bytes().iter().enumerate() {
            out.bytes[i % new_size] ^= b;
        }
        out
    }

    /// `Hash::to_string`. With `include_algo` (or always for SRI) the
    /// algorithm is prefixed: `sha256:…` or `sha256-…`.
    pub fn to_string(&self, format: HashFormat, include_algo: bool) -> String {
        let n = self.as_bytes().len();
        let digits = match format {
            HashFormat::Base16 => base16::encoded_len(n),
            HashFormat::Nix32 => nix32::encoded_len(n),
            HashFormat::Base64 | HashFormat::Sri => base64::encoded_len(n),
        };
        let mut s = String::with_capacity(self.algo.name().len() + 1 + digits);
        self.write_to(&mut s, format, include_algo);
        s
    }

    /// [`Hash::to_string`] appended to `out`, for callers that render a
    /// hash into a larger string.
    pub fn write_to(&self, out: &mut String, format: HashFormat, include_algo: bool) {
        if format == HashFormat::Sri || include_algo {
            out.push_str(self.algo.name());
            out.push(if format == HashFormat::Sri { '-' } else { ':' });
        }
        match format {
            HashFormat::Base16 => base16::encode_into(self.as_bytes(), out),
            HashFormat::Nix32 => nix32::encode_into(self.as_bytes(), out),
            HashFormat::Base64 | HashFormat::Sri => base64::encode_into(self.as_bytes(), out),
        }
    }

    // The parsers take bytes (anything that is `AsRef<[u8]>`: `&str`,
    // `&[u8]`, `String`, …), as Nix's take its strings: a hash that is not
    // UTF-8 fails with Nix's message for those bytes, and their length is
    // what decides the encoding.

    /// `Hash::parseSRI`: `<algo>-<base64>`.
    pub fn parse_sri<S: AsRef<[u8]> + ?Sized>(s: &S, xp: XpSettings) -> Result<Hash, HashError> {
        let s = s.as_ref();
        let (algo_name, rest) = split_prefix(s, b'-').ok_or_else(|| HashError::NotSri(s.into()))?;
        let algo = HashAlgo::parse(algo_name, xp)?;
        parse_low_level(rest, algo, Decoder::Sri, xp)
    }

    /// `Hash::parseAnyPrefixed`: the hash must carry its algorithm, as
    /// `<algo>:<hash>` in any encoding or as SRI.
    pub fn parse_any_prefixed<S: AsRef<[u8]> + ?Sized>(
        s: &S,
        xp: XpSettings,
    ) -> Result<Hash, HashError> {
        let s = s.as_ref();
        parse_any_helper(s, xp, |parsed| {
            parsed.ok_or_else(|| HashError::MissingAlgo(s.into()))
        })
        .map(|(h, _)| h)
    }

    /// `Hash::parseAny`: like [`Hash::parse_any_prefixed`] but the algorithm
    /// may instead (or also, if consistent) come from `algo`.
    pub fn parse_any<S: AsRef<[u8]> + ?Sized>(
        s: &S,
        algo: Option<HashAlgo>,
        xp: XpSettings,
    ) -> Result<Hash, HashError> {
        Self::parse_any_returning_format(s, algo, xp).map(|(h, _)| h)
    }

    /// `Hash::parseAnyReturningFormat`.
    pub fn parse_any_returning_format<S: AsRef<[u8]> + ?Sized>(
        s: &S,
        algo: Option<HashAlgo>,
        xp: XpSettings,
    ) -> Result<(Hash, HashFormat), HashError> {
        let s = s.as_ref();
        parse_any_helper(s, xp, |parsed| match (parsed, algo) {
            (None, None) => Err(HashError::MissingAlgoNoContext(s.into())),
            (Some(p), Some(a)) if p != a => Err(HashError::WrongAlgo {
                hash: s.into(),
                algo: a,
            }),
            (Some(p), _) => Ok(p),
            (None, Some(a)) => Ok(a),
        })
    }

    /// `Hash::parseNonSRIUnprefixed`: no algorithm prefix, encoding chosen
    /// by length.
    pub fn parse_non_sri_unprefixed<S: AsRef<[u8]> + ?Sized>(
        s: &S,
        algo: HashAlgo,
        xp: XpSettings,
    ) -> Result<Hash, HashError> {
        let s = s.as_ref();
        let format = format_from_size(s, algo)?;
        Self::parse_explicit_format_unprefixed(s, algo, format, xp)
    }

    /// `Hash::parseExplicitFormatUnprefixed`.
    pub fn parse_explicit_format_unprefixed<S: AsRef<[u8]> + ?Sized>(
        s: &S,
        algo: HashAlgo,
        format: HashFormat,
        xp: XpSettings,
    ) -> Result<Hash, HashError> {
        parse_low_level(s.as_ref(), algo, Decoder::explicit(format), xp)
    }
}

/// `splitPrefixTo`: the bytes before the first `sep` and those after it,
/// or `None` without one.
fn split_prefix(s: &[u8], sep: u8) -> Option<(&[u8], &[u8])> {
    let i = s.iter().position(|&b| b == sep)?;
    Some((&s[..i], &s[i + 1..]))
}

impl PartialEq for Hash {
    fn eq(&self, other: &Hash) -> bool {
        // Like Nix, equality ignores the algorithm: only size and bytes.
        self.as_bytes() == other.as_bytes()
    }
}

impl Eq for Hash {}

impl std::hash::Hash for Hash {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.as_bytes().hash(state);
    }
}

impl PartialOrd for Hash {
    fn partial_cmp(&self, other: &Hash) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Hash {
    /// Nix's `operator<=>`: size, then bytes, then algorithm.
    fn cmp(&self, other: &Hash) -> std::cmp::Ordering {
        self.len
            .cmp(&other.len)
            .then_with(|| self.as_bytes().cmp(other.as_bytes()))
            .then_with(|| self.algo.cmp(&other.algo))
    }
}

impl fmt::Debug for Hash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Hash({})", self.to_string(HashFormat::Base16, true))
    }
}

impl fmt::Display for Hash {
    /// The SRI form, which is what Nix prints by default nowadays.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_string(HashFormat::Sri, true))
    }
}

/// Which decoder `parseLowLevel` was handed, and the name it uses in
/// error messages.
#[derive(Clone, Copy)]
enum Decoder {
    Base16,
    Nix32,
    Base64,
    Sri,
}

impl Decoder {
    fn explicit(format: HashFormat) -> Decoder {
        match format {
            HashFormat::Base16 => Decoder::Base16,
            HashFormat::Nix32 => Decoder::Nix32,
            HashFormat::Base64 => Decoder::Base64,
            HashFormat::Sri => Decoder::Sri,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Decoder::Base16 => "base16",
            Decoder::Nix32 => "nix32",
            Decoder::Base64 => "Base64",
            Decoder::Sri => "SRI",
        }
    }

    /// The decoded bytes, or `None` for a character outside the
    /// encoding's alphabet.
    fn decode(self, s: &[u8]) -> Option<Vec<u8>> {
        match self {
            Decoder::Base16 => base16::decode(s),
            Decoder::Nix32 => nix32::decode(s),
            Decoder::Base64 | Decoder::Sri => base64::decode(s),
        }
    }
}

/// `parseLowLevel`. Note the Nix quirk reproduced here: a decoding error
/// is *swallowed* (Nix adds a trace to it and carries on with an empty
/// buffer), so an invalid character surfaces as a length mismatch with
/// `length 0`. Making a BLAKE3 hash (`Hash::Hash`) needs the feature.
fn parse_low_level(
    rest: &[u8],
    algo: HashAlgo,
    decoder: Decoder,
    xp: XpSettings,
) -> Result<Hash, HashError> {
    if algo == HashAlgo::Blake3 {
        xp.require(Xp::Blake3Hashes, None)?;
    }
    // Nix's decoding error only gains a trace; the length check below
    // is what reports it.
    let decoded = decoder.decode(rest).unwrap_or_default();
    if decoded.len() != algo.size() {
        return Err(HashError::InvalidLength {
            encoding: decoder.name(),
            hash: rest.into(),
            actual: decoded.len(),
            expected: algo.size(),
        });
    }
    Ok(Hash::from_bytes(algo, &decoded))
}

/// `baseFromSize`: pick the encoding by the encoded length in bytes.
fn format_from_size(rest: &[u8], algo: HashAlgo) -> Result<HashFormat, HashError> {
    let n = algo.size();
    if rest.len() == base16::encoded_len(n) {
        Ok(HashFormat::Base16)
    } else if rest.len() == nix32::encoded_len(n) {
        Ok(HashFormat::Nix32)
    } else if rest.len() == base64::encoded_len(n) {
        Ok(HashFormat::Base64)
    } else {
        Err(HashError::WrongLength {
            hash: rest.into(),
            algo,
        })
    }
}

/// `parseAnyHelper`: split off an `algo:` or `algo-` prefix, let the
/// caller reconcile it with context, then decode by length (or as SRI).
fn parse_any_helper(
    original: &[u8],
    xp: XpSettings,
    resolve_algo: impl FnOnce(Option<HashAlgo>) -> Result<HashAlgo, HashError>,
) -> Result<(Hash, HashFormat), HashError> {
    let mut rest = original;
    let mut is_sri = false;
    let mut parsed_algo = None;
    if let Some((prefix, r)) = split_prefix(rest, b':') {
        parsed_algo = Some(HashAlgo::parse(prefix, xp)?);
        rest = r;
    } else if let Some((prefix, r)) = split_prefix(rest, b'-') {
        parsed_algo = Some(HashAlgo::parse(prefix, xp)?);
        rest = r;
        is_sri = true;
    }
    let algo = resolve_algo(parsed_algo)?;
    let (decoder, format) = if is_sri {
        (Decoder::Sri, HashFormat::Sri)
    } else {
        let format = format_from_size(rest, algo)?;
        (Decoder::explicit(format), format)
    };
    Ok((parse_low_level(rest, algo, decoder, xp)?, format))
}

/// Incremental hashing (`HashSink`): feed bytes, then take the digest and
/// the number of bytes hashed.
pub struct Hasher {
    algo: HashAlgo,
    bytes: u64,
    state: HasherState,
}

enum HasherState {
    Md5(md5::Md5),
    Sha1(sha1::Sha1),
    Sha256(sha2::Sha256),
    Sha512(sha2::Sha512),
    Blake3(Box<blake3::Hasher>),
}

impl Hasher {
    pub fn new(algo: HashAlgo) -> Hasher {
        let state = match algo {
            HashAlgo::Md5 => HasherState::Md5(md5::Md5::new()),
            HashAlgo::Sha1 => HasherState::Sha1(sha1::Sha1::new()),
            HashAlgo::Sha256 => HasherState::Sha256(sha2::Sha256::new()),
            HashAlgo::Sha512 => HasherState::Sha512(sha2::Sha512::new()),
            HashAlgo::Blake3 => HasherState::Blake3(Box::new(blake3::Hasher::new())),
        };
        Hasher {
            algo,
            bytes: 0,
            state,
        }
    }

    pub fn algo(&self) -> HashAlgo {
        self.algo
    }

    pub fn update(&mut self, data: &[u8]) {
        self.bytes += data.len() as u64;
        match &mut self.state {
            HasherState::Md5(h) => h.update(data),
            HasherState::Sha1(h) => h.update(data),
            HasherState::Sha256(h) => h.update(data),
            HasherState::Sha512(h) => h.update(data),
            HasherState::Blake3(h) => {
                h.update(data);
            }
        }
    }

    /// The digest and the total number of bytes fed in (`HashResult`).
    pub fn finish(self) -> (Hash, u64) {
        let hash = match self.state {
            HasherState::Md5(h) => Hash::from_bytes(self.algo, &h.finalize()),
            HasherState::Sha1(h) => Hash::from_bytes(self.algo, &h.finalize()),
            HasherState::Sha256(h) => Hash::from_bytes(self.algo, &h.finalize()),
            HasherState::Sha512(h) => Hash::from_bytes(self.algo, &h.finalize()),
            HasherState::Blake3(h) => Hash::from_bytes(self.algo, h.finalize().as_bytes()),
        };
        (hash, self.bytes)
    }
}

impl std::io::Write for Hasher {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.update(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Lower-case hexadecimal (`base16` in Nix).
pub mod base16 {
    pub const fn encoded_len(n: usize) -> usize {
        n * 2
    }

    pub fn encode(bytes: &[u8]) -> String {
        let mut s = String::with_capacity(encoded_len(bytes.len()));
        encode_into(bytes, &mut s);
        s
    }

    /// [`encode`] appended to `out`: the digits are produced in a stack
    /// buffer, a digest's worth at a time, and appended in one piece.
    pub fn encode_into(bytes: &[u8], out: &mut String) {
        let mut buf = [0u8; 2 * super::Hash::MAX_SIZE];
        for chunk in bytes.chunks(super::Hash::MAX_SIZE) {
            out.push_str(encode_digest(chunk, &mut buf));
        }
    }

    /// The encoding of at most a digest's worth of bytes, in `buf`.
    pub fn encode_digest<'b>(
        bytes: &[u8],
        buf: &'b mut [u8; 2 * super::Hash::MAX_SIZE],
    ) -> &'b str {
        const CHARS: &[u8; 16] = b"0123456789abcdef";
        let digits = &mut buf[..2 * bytes.len()];
        for (pair, &b) in digits.chunks_exact_mut(2).zip(bytes) {
            pair[0] = CHARS[(b >> 4) as usize];
            pair[1] = CHARS[(b & 0x0f) as usize];
        }
        std::str::from_utf8(digits).expect("hex digits are ASCII")
    }

    /// `base16::decode`, upper-case digits included; `None` on any other
    /// byte. Nix asserts an even length; brix treats a dangling digit
    /// like an invalid character so that nothing aborts.
    pub fn decode<S: AsRef<[u8]> + ?Sized>(s: &S) -> Option<Vec<u8>> {
        let digit = |c: u8| -> Option<u8> {
            match c {
                b'0'..=b'9' => Some(c - b'0'),
                b'A'..=b'F' => Some(c - b'A' + 10),
                b'a'..=b'f' => Some(c - b'a' + 10),
                _ => None,
            }
        };
        let bytes = s.as_ref();
        let mut out = Vec::with_capacity(bytes.len() / 2);
        for pair in bytes.chunks(2) {
            let &[hi, lo] = pair else {
                return None;
            };
            out.push(digit(hi)? << 4 | digit(lo)?);
        }
        Some(out)
    }
}

/// Nix's base-32 variant (`BaseNix32`).
pub mod nix32 {
    /// The alphabet: digits and lower-case letters without `e`, `o`, `t`,
    /// `u` (so that store paths cannot spell most rude words).
    pub const CHARS: &[u8; 32] = b"0123456789abcdfghijklmnpqrsvwxyz";

    /// `BaseNix32::encodedLength`: ceil(bits / 5).
    pub const fn encoded_len(n: usize) -> usize {
        if n == 0 {
            0
        } else {
            (n * 8 - 1) / 5 + 1
        }
    }

    /// `BaseNix32::encode`. Character `n` (counting from the *end* of the
    /// output) holds bits `5n..5n+5` of the byte string read
    /// little-endian, so the output is produced from the last character
    /// to the first.
    pub fn encode(bytes: &[u8]) -> String {
        let mut s = String::with_capacity(encoded_len(bytes.len()));
        encode_into(bytes, &mut s);
        s
    }

    /// [`encode`] appended to `out`, built in a stack buffer for anything
    /// up to a digest's size.
    pub fn encode_into(bytes: &[u8], out: &mut String) {
        const STACK: usize = encoded_len(super::Hash::MAX_SIZE);
        let len = encoded_len(bytes.len());
        let mut stack = [0u8; STACK];
        let mut heap = Vec::new();
        let digits = if len <= STACK {
            &mut stack[..len]
        } else {
            heap.resize(len, 0);
            &mut heap[..]
        };
        encode_to_slice(bytes, digits);
        out.push_str(std::str::from_utf8(digits).expect("nix32 digits are ASCII"));
    }

    /// Writes the encoding of `bytes` into `digits`, which must be exactly
    /// [`encoded_len`] long.
    pub fn encode_to_slice(bytes: &[u8], digits: &mut [u8]) {
        let len = digits.len();
        assert_eq!(len, encoded_len(bytes.len()), "nix32 output length");
        for (k, d) in digits.iter_mut().enumerate() {
            let n = len - 1 - k;
            let b = n * 5;
            let i = b / 8;
            let j = b % 8;
            // Done in u16 as C++ does it in int: shifting by 8 yields 0 here.
            let lo = (bytes[i] as u16) >> j;
            let hi = match bytes.get(i + 1) {
                Some(&next) => (next as u16) << (8 - j),
                None => 0,
            };
            *d = CHARS[((lo | hi) & 0x1f) as usize];
        }
    }

    /// `BaseNix32::decode`, bit for bit, including its behaviour on input
    /// whose top character carries bits beyond the byte length: the
    /// result grows by a byte, which the caller then rejects as a length
    /// mismatch. `None` on a byte outside the alphabet.
    pub fn decode<S: AsRef<[u8]> + ?Sized>(s: &S) -> Option<Vec<u8>> {
        let bytes = s.as_ref();
        let mut out: Vec<u8> = Vec::with_capacity((bytes.len() * 5).div_ceil(8));
        for n in 0..bytes.len() {
            let c = bytes[bytes.len() - n - 1];
            let digit = CHARS.iter().position(|&x| x == c)? as u8;
            let b = n * 5;
            let i = b / 8;
            let j = b % 8;
            if out.len() < i + 1 {
                out.resize(i + 1, 0);
            }
            out[i] |= digit << j;
            let carry = if j == 0 { 0 } else { digit >> (8 - j) };
            if carry != 0 {
                if out.len() < i + 2 {
                    out.resize(i + 2, 0);
                }
                out[i + 1] |= carry;
            }
        }
        Some(out)
    }
}

/// RFC 4648 base64 (`base64` in Nix).
pub mod base64 {
    const CHARS: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    pub const fn encoded_len(n: usize) -> usize {
        n.div_ceil(3) * 4
    }

    pub fn encode(bytes: &[u8]) -> String {
        let mut s = String::with_capacity(encoded_len(bytes.len()));
        encode_into(bytes, &mut s);
        s
    }

    /// [`encode`] appended to `s`.
    pub fn encode_into(bytes: &[u8], s: &mut String) {
        s.reserve(encoded_len(bytes.len()));
        let start = s.len();
        let mut data: u32 = 0;
        let mut nbits = 0;
        for &c in bytes {
            data = (data << 8) | c as u32;
            nbits += 8;
            while nbits >= 6 {
                nbits -= 6;
                s.push(CHARS[((data >> nbits) & 0x3f) as usize] as char);
            }
        }
        if nbits > 0 {
            s.push(CHARS[((data << (6 - nbits)) & 0x3f) as usize] as char);
        }
        while (s.len() - start) % 4 != 0 {
            s.push('=');
        }
    }

    /// Nix's decoder: stops at the first `=`, skips newlines, tolerates
    /// missing padding. `None` on a byte outside the alphabet.
    pub fn decode<S: AsRef<[u8]> + ?Sized>(s: &S) -> Option<Vec<u8>> {
        let s = s.as_ref();
        let mut out = Vec::with_capacity((s.len() + 2) / 4 * 3);
        let mut d: u32 = 0;
        let mut bits = 0;
        for &c in s {
            if c == b'=' {
                break;
            }
            if c == b'\n' {
                continue;
            }
            let digit = CHARS.iter().position(|&x| x == c)? as u32;
            bits += 6;
            d = (d << 6) | digit;
            if bits >= 8 {
                out.push(((d >> (bits - 8)) & 0xff) as u8);
                bits -= 8;
            }
        }
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nix32_round_trips_every_length() {
        for len in 0..70 {
            let bytes: Vec<u8> = (0..len).map(|i| (i * 37 + 11) as u8).collect();
            let enc = nix32::encode(&bytes);
            assert_eq!(enc.len(), nix32::encoded_len(len));
            assert_eq!(nix32::decode(&enc).unwrap(), bytes, "len {len}");
        }
    }

    #[test]
    fn known_nix_vectors() {
        // From Nix's own unit tests (tests/unit/libutil/hash.cc).
        let h = Hash::of(HashAlgo::Sha256, b"abc");
        assert_eq!(
            h.to_string(HashFormat::Base16, false),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            h.to_string(HashFormat::Nix32, false),
            "1b8m03r63zqhnjf7l5wnldhh7c134ap5vpj0850ymkq1iyzicy5s"
        );
        assert_eq!(
            h.to_string(HashFormat::Sri, false),
            "sha256-ungWv48Bz+pBQUDeXa4iI7ADYaOWF3qctBD/YfIAFa0="
        );
        let md5 = Hash::of(HashAlgo::Md5, b"");
        assert_eq!(
            md5.to_string(HashFormat::Base16, true),
            "md5:d41d8cd98f00b204e9800998ecf8427e"
        );
        let sha1 = Hash::of(HashAlgo::Sha1, b"abc");
        assert_eq!(
            sha1.to_string(HashFormat::Base16, false),
            "a9993e364706816aba3e25717850c26c9cd0d89d"
        );
        let sha512 = Hash::of(HashAlgo::Sha512, b"abc");
        assert_eq!(
            sha512.to_string(HashFormat::Base16, false),
            "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a\
             2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f"
        );
    }

    #[test]
    fn parse_any_in_every_format() {
        let h = Hash::of(HashAlgo::Sha256, b"abc");
        for f in [HashFormat::Base16, HashFormat::Nix32, HashFormat::Base64] {
            let s = h.to_string(f, false);
            assert_eq!(
                Hash::parse_any(&s, Some(HashAlgo::Sha256), XpSettings::NONE).unwrap(),
                h
            );
            let s = h.to_string(f, true);
            assert_eq!(Hash::parse_any_prefixed(&s, XpSettings::NONE).unwrap(), h);
            let (_, got) = Hash::parse_any_returning_format(&s, None, XpSettings::NONE).unwrap();
            assert_eq!(got, f);
        }
        let sri = h.to_string(HashFormat::Sri, true);
        assert_eq!(Hash::parse_sri(&sri, XpSettings::NONE).unwrap(), h);
        assert_eq!(Hash::parse_any_prefixed(&sri, XpSettings::NONE).unwrap(), h);
        assert_eq!(
            Hash::parse_any(&sri, Some(HashAlgo::Sha1), XpSettings::NONE).unwrap_err(),
            HashError::WrongAlgo {
                hash: sri.clone().into(),
                algo: HashAlgo::Sha1
            }
        );
    }

    /// Hashes and names are bytes: one that is not UTF-8 is quoted as it
    /// is, and its length in bytes decides the encoding (as `nix eval` of
    /// `builtins.convertHash` and `builtins.hashString` with a Latin-1
    /// byte shows).
    #[test]
    fn parse_errors_quote_bytes_that_are_not_utf8() {
        let off = XpSettings::NONE;
        let message = |e: HashError| e.message_bytes();
        assert_eq!(
            message(HashAlgo::parse(b"sha\xff".as_slice(), off).unwrap_err()),
            b"unknown hash algorithm 'sha\xff', expect 'blake3', 'md5', 'sha1', 'sha256', or 'sha512'"
        );
        assert_eq!(
            message(HashFormat::parse(b"\xff".as_slice()).unwrap_err()),
            b"unknown hash format '\xff', expect 'base16', 'base32', 'base64', or 'sri'"
        );
        // The algorithm prefix is checked first, whichever separator.
        for hash in [&b"\xff:abc"[..], b"\xff-abc"] {
            assert_eq!(
                message(Hash::parse_any(hash, None, off).unwrap_err()),
                b"unknown hash algorithm '\xff', expect 'blake3', 'md5', 'sha1', 'sha256', or 'sha512'"
            );
        }
        assert_eq!(
            message(Hash::parse_any(b"sha256:\xff".as_slice(), None, off).unwrap_err()),
            b"hash '\xff' has wrong length for hash algorithm 'sha256'"
        );
        assert_eq!(
            message(Hash::parse_any(b"sha256-\xff".as_slice(), None, off).unwrap_err()),
            b"invalid SRI hash '\xff', length 0 != expected length 32"
        );
        // 64 bytes, one of them not UTF-8: base16 by length, then an
        // invalid character (U+FFFD would have made it 66 long).
        let mut hex = vec![b'0'; 63];
        hex.push(0xc3);
        let mut expected = b"invalid base16 hash '".to_vec();
        expected.extend_from_slice(&hex);
        expected.extend_from_slice(b"', length 0 != expected length 32");
        assert_eq!(
            message(Hash::parse_any(&hex, Some(HashAlgo::Sha256), off).unwrap_err()),
            expected
        );
        // `Display` is the same text, the bytes replaced.
        assert_eq!(
            HashFormat::parse(b"a\xff".as_slice())
                .unwrap_err()
                .to_string(),
            "unknown hash format 'a\u{fffd}', expect 'base16', 'base32', 'base64', or 'sri'"
        );
    }

    #[test]
    fn parse_errors_match_nix() {
        assert_eq!(
            Hash::parse_any("abc", None, XpSettings::NONE)
                .unwrap_err()
                .to_string(),
            "hash 'abc' does not include a type, nor is the type otherwise known from context"
        );
        assert_eq!(
            Hash::parse_any_prefixed("abc", XpSettings::NONE)
                .unwrap_err()
                .to_string(),
            "hash 'abc' does not include a type"
        );
        assert_eq!(
            Hash::parse_any("abc", Some(HashAlgo::Sha256), XpSettings::NONE)
                .unwrap_err()
                .to_string(),
            "hash 'abc' has wrong length for hash algorithm 'sha256'"
        );
        // A bad character is reported as a length-0 mismatch, as in Nix.
        let bad = "z".repeat(64);
        assert_eq!(
            Hash::parse_any(&bad, Some(HashAlgo::Sha256), XpSettings::NONE)
                .unwrap_err()
                .to_string(),
            format!("invalid base16 hash '{bad}', length 0 != expected length 32")
        );
        assert_eq!(
            Hash::parse_sri("sha256:abc", XpSettings::NONE)
                .unwrap_err()
                .to_string(),
            "hash 'sha256:abc' is not SRI"
        );
        assert_eq!(
            HashAlgo::parse("sha3", XpSettings::NONE)
                .unwrap_err()
                .to_string(),
            "unknown hash algorithm 'sha3', expect 'blake3', 'md5', 'sha1', 'sha256', or 'sha512'"
        );
    }

    #[test]
    fn hash_format_names_report_the_deprecated_alias() {
        assert_eq!(HashFormat::parse("nix32"), Ok((HashFormat::Nix32, None)));
        assert_eq!(
            HashFormat::parse("base32"),
            Ok((
                HashFormat::Nix32,
                Some("\"base32\" is a deprecated alias for hash format \"nix32\".")
            ))
        );
        for f in [
            HashFormat::Base16,
            HashFormat::Nix32,
            HashFormat::Base64,
            HashFormat::Sri,
        ] {
            assert_eq!(HashFormat::parse_opt(f.name()), Some((f, None)));
        }
        assert_eq!(
            HashFormat::parse("hex").unwrap_err().to_string(),
            "unknown hash format 'hex', expect 'base16', 'base32', 'base64', or 'sri'"
        );
    }

    #[test]
    fn blake3_needs_the_experimental_feature() {
        // `parseHashAlgoOpt` and `Hash::Hash` both require
        // `blake3-hashes`, so every way of naming BLAKE3 is refused
        // without it, before anything else is checked.
        let h = Hash::of(HashAlgo::Blake3, b"abc");
        let on = XpSettings::NONE.with(Xp::Blake3Hashes);
        let missing = HashError::MissingFeature(MissingExperimentalFeature {
            feature: Xp::Blake3Hashes,
            reason: None,
        });
        let base16 = h.to_string(HashFormat::Base16, false);
        let prefixed = h.to_string(HashFormat::Base16, true);
        let sri = h.to_string(HashFormat::Sri, true);
        let off = XpSettings::NONE;
        assert_eq!(HashAlgo::parse("blake3", off), Err(missing.clone()));
        assert_eq!(HashAlgo::parse_opt("blake3", off), Err(missing.clone()));
        assert_eq!(Hash::parse_sri(&sri, off), Err(missing.clone()));
        assert_eq!(
            Hash::parse_any_prefixed(&prefixed, off),
            Err(missing.clone())
        );
        assert_eq!(Hash::parse_any(&sri, None, off), Err(missing.clone()));
        assert_eq!(
            Hash::parse_any(&base16, Some(HashAlgo::Blake3), off),
            Err(missing.clone())
        );
        assert_eq!(
            Hash::parse_any("blake3:x", Some(HashAlgo::Sha256), off),
            Err(missing.clone())
        );
        assert_eq!(
            Hash::parse_non_sri_unprefixed(&base16, HashAlgo::Blake3, off),
            Err(missing)
        );
        assert_eq!(HashAlgo::parse("blake3", on), Ok(HashAlgo::Blake3));
        assert_eq!(Hash::parse_sri(&sri, on), Ok(h));
        assert_eq!(Hash::parse_any_prefixed(&prefixed, on), Ok(h));
        assert_eq!(Hash::parse_any(&base16, Some(HashAlgo::Blake3), on), Ok(h));
        // The other algorithms do not care.
        assert_eq!(HashAlgo::parse("sha256", off), Ok(HashAlgo::Sha256));
    }

    #[test]
    fn compress_folds_bytes() {
        let h = Hash::of(HashAlgo::Sha256, b"source:sha256:x:/nix/store:foo");
        let c = h.compress(20);
        assert_eq!(c.as_bytes().len(), 20);
        let mut expected = [0u8; 20];
        for (i, b) in h.as_bytes().iter().enumerate() {
            expected[i % 20] ^= b;
        }
        assert_eq!(c.as_bytes(), &expected);
        assert_eq!(c.to_string(HashFormat::Nix32, false).len(), 32);
    }

    #[test]
    fn hasher_counts_bytes() {
        let mut h = Hasher::new(HashAlgo::Sha256);
        h.update(b"ab");
        h.update(b"c");
        let (hash, n) = h.finish();
        assert_eq!(n, 3);
        assert_eq!(hash, Hash::of(HashAlgo::Sha256, b"abc"));
    }

    #[test]
    fn base64_decoder_quirks() {
        assert_eq!(base64::decode("YWJj").unwrap(), b"abc");
        assert_eq!(base64::decode("YWI=").unwrap(), b"ab");
        assert_eq!(base64::decode("YWI").unwrap(), b"ab");
        assert_eq!(base64::decode("YW\nJj").unwrap(), b"abc");
        assert_eq!(base64::decode("YW!j"), None);
    }
}
