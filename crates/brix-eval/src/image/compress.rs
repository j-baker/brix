//! Compression of the file contents stored in a full-tree image.
//!
//! Deflate, through `flate2`'s pure-Rust backend, so the loader builds
//! for wasm. The unpacked length is not part of the stream: it is the
//! blob's `raw_len`, and unpacking is bounds-checked against it, so a
//! damaged image is reported rather than trusted. Module code is not
//! compressed: it is read in place (see `codec`).

use std::io::{Read, Write};

use flate2::read::DeflateDecoder;
use flate2::write::DeflateEncoder;
use flate2::Compression;

use super::ImageError;

/// The most a deflate stream can expand: a stream of length-258 matches
/// at distance 1 costs a little under two bits per 258 bytes. A
/// declared length beyond this many times the compressed size cannot be
/// right, so no more than that is reserved up front.
const MAX_DEFLATE_RATIO: usize = 1032;

/// Compresses `data` into a raw deflate stream.
pub(crate) fn pack(data: &[u8]) -> Vec<u8> {
    // Level 9 reads an image's lifetime: it is written once and mapped
    // many times, and the extra encoder effort is spread over all cores.
    let mut enc = DeflateEncoder::new(Vec::with_capacity(data.len() / 3), Compression::new(9));
    enc.write_all(data).expect("writing to a Vec cannot fail");
    enc.finish().expect("writing to a Vec cannot fail")
}

/// Reverses [`pack`], checking that exactly `raw_len` bytes come out.
pub(crate) fn unpack(packed: &[u8], raw_len: usize, what: &str) -> Result<Vec<u8>, ImageError> {
    let corrupt = |msg: String| ImageError::Corrupt(format!("{what}: {msg}"));
    // `raw_len` comes from the image: trust it for the reservation only
    // as far as the compressed size could possibly justify.
    let mut out = Vec::with_capacity(raw_len.min(packed.len().saturating_mul(MAX_DEFLATE_RATIO)));
    // One byte past the declared length tells a longer stream from a
    // truncated declaration.
    DeflateDecoder::new(packed)
        .take(raw_len as u64 + 1)
        .read_to_end(&mut out)
        .map_err(|e| corrupt(format!("decompression failed: {e}")))?;
    if out.len() != raw_len {
        return Err(corrupt(format!(
            "decompressed to {} bytes, expected {raw_len}",
            out.len()
        )));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_damage() {
        let data: Vec<u8> = (0..10_000u32).map(|i| (i % 251) as u8).collect();
        let packed = pack(&data);
        assert!(packed.len() < data.len());
        assert_eq!(unpack(&packed, data.len(), "test").unwrap(), data);
        assert!(unpack(&packed[..packed.len() / 2], data.len(), "test").is_err());
        assert!(unpack(b"junk", 5, "test").is_err());
        for wrong_len in [data.len() - 1, data.len() + 1] {
            let err = unpack(&packed, wrong_len, "test").unwrap_err().to_string();
            assert!(err.contains("decompressed to"), "{err}");
        }
        assert_eq!(unpack(&pack(b""), 0, "test").unwrap(), b"");
    }

    /// A declared length far beyond what the stream could hold neither
    /// reserves it nor is believed.
    #[test]
    fn huge_declared_length_is_not_reserved() {
        let packed = pack(b"tiny");
        let err = unpack(&packed, u32::MAX as usize, "test")
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("decompressed to 4 bytes, expected 4294967295"),
            "{err}"
        );
    }

    /// The ratio bound holds for the most compressible input there is.
    #[test]
    fn max_ratio_bounds_real_streams() {
        let data = vec![0u8; 1 << 20];
        let packed = pack(&data);
        assert!(packed.len() * MAX_DEFLATE_RATIO >= data.len());
        assert_eq!(unpack(&packed, data.len(), "test").unwrap(), data);
    }
}
