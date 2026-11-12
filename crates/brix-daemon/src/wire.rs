//! The primitive encoding shared by every Nix protocol.
//!
//! Everything on the wire is built from two shapes (`src/libutil/serialise.cc`
//! in Nix):
//!
//! - **Integers** are 8 bytes, little endian. Booleans, enums and `uint8_t`
//!   values are widened to this width; Nix's `readNum<T>` rejects a value that
//!   does not fit `T`, which [`read_u8`] reproduces.
//! - **Strings** are a length (as above), the bytes, then zero padding up to
//!   the next multiple of 8. Nix rejects non-zero padding, so do we.
//!
//! Collections are a count followed by the elements. On top of that, the
//! "framed" encoding used by `AddToStore` sends a byte stream as a sequence
//! of `(length, bytes)` chunks terminated by a zero length, so that the
//! receiver can always find the end of the stream even if the sender gave up
//! half way.

use std::collections::BTreeSet;
use std::io::{self, Read, Write};

use crate::error::DaemonError;

/// The most a string read reserves before its bytes arrive.
const MAX_PREALLOCATION: usize = 1024 * 1024;

/// Reading side of the wire encoding, implemented for every `Read`.
pub trait WireRead: Read {
    fn read_u64(&mut self) -> Result<u64, DaemonError> {
        let mut buf = [0u8; 8];
        self.read_exact(&mut buf).map_err(eof_as_daemon_gone)?;
        Ok(u64::from_le_bytes(buf))
    }

    /// An integer that Nix declared as `uint8_t`: still 8 bytes on the wire.
    fn read_u8(&mut self) -> Result<u8, DaemonError> {
        let n = self.read_u64()?;
        u8::try_from(n).map_err(|_| {
            DaemonError::Protocol(format!("serialised integer {n} is too large for type 'h'"))
        })
    }

    fn read_bool(&mut self) -> Result<bool, DaemonError> {
        Ok(self.read_u64()? != 0)
    }

    /// A length-prefixed, padded byte string (`readString`).
    ///
    /// The length comes from the peer, so the buffer grows with the data
    /// actually received rather than being allocated up front: a bogus
    /// length ends in [`DaemonError::Disconnected`] when the stream runs
    /// dry, not in an allocation failure.
    fn read_bytes(&mut self) -> Result<Vec<u8>, DaemonError> {
        let len = self.read_u64()?;
        let len =
            usize::try_from(len).map_err(|_| DaemonError::Protocol("string is too long".into()))?;
        let mut data = Vec::with_capacity(len.min(MAX_PREALLOCATION));
        let n = Read::take(&mut *self, len as u64).read_to_end(&mut data)?;
        if n != len {
            return Err(DaemonError::Disconnected);
        }
        self.read_padding(len)?;
        Ok(data)
    }

    /// A string that must be UTF-8 (store paths, names, messages).
    fn read_string(&mut self) -> Result<String, DaemonError> {
        let bytes = self.read_bytes()?;
        String::from_utf8(bytes).map_err(|e| {
            DaemonError::Protocol(format!("string from the Nix daemon is not UTF-8: {e}"))
        })
    }

    /// Consume the zero padding that follows `len` bytes of string data.
    fn read_padding(&mut self, len: usize) -> Result<(), DaemonError> {
        let rem = len % 8;
        if rem != 0 {
            let mut pad = [0u8; 8];
            let n = 8 - rem;
            self.read_exact(&mut pad[..n]).map_err(eof_as_daemon_gone)?;
            if pad.iter().any(|&b| b != 0) {
                return Err(DaemonError::Protocol("non-zero padding".into()));
            }
        }
        Ok(())
    }

    /// A count followed by that many strings, in order.
    fn read_strings(&mut self) -> Result<Vec<String>, DaemonError> {
        let count = self.read_count()?;
        let mut out = Vec::with_capacity(count.min(1024));
        for _ in 0..count {
            out.push(self.read_string()?);
        }
        Ok(out)
    }

    /// Like [`WireRead::read_strings`] but deduplicated and ordered, which is
    /// how Nix reads a `StringSet`.
    fn read_string_set(&mut self) -> Result<BTreeSet<String>, DaemonError> {
        Ok(self.read_strings()?.into_iter().collect())
    }

    /// A count followed by `(key, value)` string pairs (the fake daemon in
    /// tests reads the client's option overrides with it).
    #[cfg(test)]
    fn read_string_map(
        &mut self,
    ) -> Result<std::collections::BTreeMap<String, String>, DaemonError> {
        let count = self.read_count()?;
        let mut out = std::collections::BTreeMap::new();
        for _ in 0..count {
            let k = self.read_string()?;
            let v = self.read_string()?;
            out.insert(k, v);
        }
        Ok(out)
    }

    /// A collection length, checked to fit in memory addressing.
    fn read_count(&mut self) -> Result<usize, DaemonError> {
        let n = self.read_u64()?;
        usize::try_from(n)
            .map_err(|_| DaemonError::Protocol(format!("collection size {n} is too large")))
    }
}

impl<R: Read + ?Sized> WireRead for R {}

/// Writing side of the wire encoding, implemented for every `Write`.
pub trait WireWrite: Write {
    fn write_u64(&mut self, n: u64) -> Result<(), DaemonError> {
        self.write_all(&n.to_le_bytes())?;
        Ok(())
    }

    fn write_bool(&mut self, b: bool) -> Result<(), DaemonError> {
        self.write_u64(u64::from(b))
    }

    /// A length-prefixed, padded byte string.
    fn write_bytes(&mut self, data: &[u8]) -> Result<(), DaemonError> {
        self.write_u64(data.len() as u64)?;
        self.write_all(data)?;
        self.write_padding(data.len())
    }

    fn write_str(&mut self, s: &str) -> Result<(), DaemonError> {
        self.write_bytes(s.as_bytes())
    }

    fn write_padding(&mut self, len: usize) -> Result<(), DaemonError> {
        let rem = len % 8;
        if rem != 0 {
            self.write_all(&[0u8; 8][..8 - rem])?;
        }
        Ok(())
    }

    /// A count followed by the strings, which are bytes on the wire
    /// (a derivation's arguments need not be UTF-8).
    fn write_strings<I, S>(&mut self, items: I) -> Result<(), DaemonError>
    where
        I: IntoIterator<Item = S>,
        I::IntoIter: ExactSizeIterator,
        S: AsRef<[u8]>,
    {
        let iter = items.into_iter();
        self.write_u64(iter.len() as u64)?;
        for s in iter {
            self.write_bytes(s.as_ref())?;
        }
        Ok(())
    }

    /// A count followed by `(key, value)` string pairs, bytes as in
    /// [`WireWrite::write_strings`].
    fn write_string_map<I, K, V>(&mut self, items: I) -> Result<(), DaemonError>
    where
        I: IntoIterator<Item = (K, V)>,
        I::IntoIter: ExactSizeIterator,
        K: AsRef<[u8]>,
        V: AsRef<[u8]>,
    {
        let iter = items.into_iter();
        self.write_u64(iter.len() as u64)?;
        for (k, v) in iter {
            self.write_bytes(k.as_ref())?;
            self.write_bytes(v.as_ref())?;
        }
        Ok(())
    }
}

impl<W: Write + ?Sized> WireWrite for W {}

/// Nix reports a daemon that hangs up mid-message as "Nix daemon disconnected
/// unexpectedly (maybe it crashed?)"; everything else is a plain I/O error.
fn eof_as_daemon_gone(e: io::Error) -> DaemonError {
    if e.kind() == io::ErrorKind::UnexpectedEof {
        DaemonError::Disconnected
    } else {
        DaemonError::Io(e)
    }
}

/// Writer side of Nix's `FramedSink`: the data is sent as `(length, bytes)`
/// chunks and [`FramedWriter::finish`] sends the zero-length terminator.
///
/// Dropping the writer without calling `finish` leaves the connection in an
/// undefined state; the client treats that as fatal for the connection.
pub(crate) struct FramedWriter<'a, W: Write> {
    inner: &'a mut W,
    buf: Vec<u8>,
}

/// Chunk size for framed writes. Nix uses its `BufferedSink` buffer (64 KiB)
/// so every full buffer becomes one chunk.
const FRAME_CHUNK: usize = 64 * 1024;

impl<'a, W: Write> FramedWriter<'a, W> {
    pub(crate) fn new(inner: &'a mut W) -> Self {
        FramedWriter {
            inner,
            buf: Vec::with_capacity(FRAME_CHUNK),
        }
    }

    fn flush_chunk(&mut self) -> io::Result<()> {
        if !self.buf.is_empty() {
            self.inner
                .write_all(&(self.buf.len() as u64).to_le_bytes())?;
            self.inner.write_all(&self.buf)?;
            self.buf.clear();
        }
        Ok(())
    }

    /// Send any buffered data and the terminating empty chunk.
    pub(crate) fn finish(mut self) -> io::Result<()> {
        self.flush_chunk()?;
        self.inner.write_all(&0u64.to_le_bytes())?;
        self.inner.flush()
    }
}

impl<W: Write> Write for FramedWriter<'_, W> {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        if self.buf.len() + data.len() > FRAME_CHUNK {
            self.flush_chunk()?;
        }
        if data.len() >= FRAME_CHUNK {
            // Large writes bypass the buffer and become a chunk of their own.
            self.inner.write_all(&(data.len() as u64).to_le_bytes())?;
            self.inner.write_all(data)?;
        } else {
            self.buf.extend_from_slice(data);
        }
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.flush_chunk()?;
        self.inner.flush()
    }
}

/// Reader side of Nix's `FramedSource`: yields the logical byte stream from
/// `(length, bytes)` chunks until the zero-length terminator. The client
/// never receives framed data; the fake daemon in the tests does.
#[cfg(test)]
pub(crate) struct FramedReader<'a, R: Read> {
    inner: &'a mut R,
    remaining: u64,
    eof: bool,
}

#[cfg(test)]
impl<'a, R: Read> FramedReader<'a, R> {
    pub(crate) fn new(inner: &'a mut R) -> Self {
        FramedReader {
            inner,
            remaining: 0,
            eof: false,
        }
    }
}

#[cfg(test)]
impl<R: Read> Read for FramedReader<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.eof || buf.is_empty() {
            return Ok(0);
        }
        if self.remaining == 0 {
            let mut len = [0u8; 8];
            self.inner.read_exact(&mut len)?;
            self.remaining = u64::from_le_bytes(len);
            if self.remaining == 0 {
                self.eof = true;
                return Ok(0);
            }
        }
        let want = buf
            .len()
            .min(usize::try_from(self.remaining).unwrap_or(usize::MAX));
        let n = self.inner.read(&mut buf[..want])?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "framed stream ended inside a chunk",
            ));
        }
        self.remaining -= n as u64;
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn integers_are_little_endian_u64() {
        let mut buf = Vec::new();
        buf.write_u64(0x6e69_7863).unwrap(); // WORKER_MAGIC_1
        assert_eq!(buf, [0x63, 0x78, 0x69, 0x6e, 0, 0, 0, 0]);
        assert_eq!(Cursor::new(&buf).read_u64().unwrap(), 0x6e69_7863);
    }

    #[test]
    fn strings_are_padded_to_eight_bytes() {
        let mut buf = Vec::new();
        buf.write_str("hello").unwrap();
        assert_eq!(
            buf,
            [5, 0, 0, 0, 0, 0, 0, 0, b'h', b'e', b'l', b'l', b'o', 0, 0, 0]
        );
        assert_eq!(Cursor::new(&buf).read_string().unwrap(), "hello");

        let mut buf = Vec::new();
        buf.write_str("12345678").unwrap();
        assert_eq!(buf.len(), 16, "an exact multiple needs no padding");

        let mut buf = Vec::new();
        buf.write_str("").unwrap();
        assert_eq!(buf, [0u8; 8]);
        assert_eq!(Cursor::new(&buf).read_string().unwrap(), "");
    }

    #[test]
    fn non_zero_padding_is_rejected() {
        let mut buf = vec![1, 0, 0, 0, 0, 0, 0, 0, b'x', 0, 0, 0, 0, 0, 0, 7];
        let err = Cursor::new(&mut buf).read_string().unwrap_err();
        assert!(matches!(err, DaemonError::Protocol(m) if m == "non-zero padding"));
    }

    #[test]
    fn u8_values_must_fit() {
        let mut buf = Vec::new();
        buf.write_u64(300).unwrap();
        let err = Cursor::new(&buf).read_u8().unwrap_err();
        // `readNum<uint8_t>`, with the Itanium name of the type.
        assert_eq!(
            err.to_string(),
            "serialised integer 300 is too large for type 'h'"
        );
        let mut buf = Vec::new();
        buf.write_u64(2).unwrap();
        assert_eq!(Cursor::new(&buf).read_u8().unwrap(), 2);
    }

    #[test]
    fn string_collections_round_trip() {
        let mut buf = Vec::new();
        buf.write_strings(["b", "a", "b"]).unwrap();
        let mut cur = Cursor::new(&buf);
        assert_eq!(cur.read_strings().unwrap(), ["b", "a", "b"]);
        let set = Cursor::new(&buf).read_string_set().unwrap();
        assert_eq!(set.into_iter().collect::<Vec<_>>(), ["a", "b"]);

        let mut buf = Vec::new();
        buf.write_string_map([("k", "v"), ("x", "y")]).unwrap();
        let map = Cursor::new(&buf).read_string_map().unwrap();
        assert_eq!(map.get("k").unwrap(), "v");
        assert_eq!(map.len(), 2);
    }

    #[test]
    fn truncated_input_is_a_disconnect() {
        let buf = [1u8, 0, 0];
        let err = Cursor::new(&buf).read_u64().unwrap_err();
        assert!(matches!(err, DaemonError::Disconnected));

        let mut buf = Vec::new();
        buf.write_u64(10).unwrap();
        buf.extend_from_slice(b"short");
        let err = Cursor::new(&buf).read_bytes().unwrap_err();
        assert!(matches!(err, DaemonError::Disconnected), "{err:?}");
    }

    #[test]
    fn huge_length_prefix_is_not_allocated() {
        // A petabyte-long string followed by three bytes: allocating the
        // announced length would abort the process.
        let mut buf = Vec::new();
        buf.write_u64(1 << 50).unwrap();
        buf.extend_from_slice(b"abc");
        let err = Cursor::new(&buf).read_bytes().unwrap_err();
        assert!(matches!(err, DaemonError::Disconnected), "{err:?}");
    }

    #[test]
    fn framed_round_trip_and_terminator() {
        let mut out = Vec::new();
        {
            let mut w = FramedWriter::new(&mut out);
            w.write_all(b"abc").unwrap();
            w.write_all(&vec![7u8; FRAME_CHUNK + 5]).unwrap();
            w.finish().unwrap();
        }
        // Ends with the zero-length chunk.
        assert_eq!(&out[out.len() - 8..], &[0u8; 8]);
        let mut cur = Cursor::new(&out);
        let mut got = Vec::new();
        FramedReader::new(&mut cur).read_to_end(&mut got).unwrap();
        assert_eq!(got.len(), 3 + FRAME_CHUNK + 5);
        assert_eq!(&got[..3], b"abc");
        assert_eq!(
            cur.position() as usize,
            out.len(),
            "reader stops exactly at the terminator"
        );
    }

    #[test]
    fn framed_chunks_have_nix_layout() {
        let mut out = Vec::new();
        let mut w = FramedWriter::new(&mut out);
        w.write_all(b"xy").unwrap();
        w.finish().unwrap();
        assert_eq!(
            out,
            [2, 0, 0, 0, 0, 0, 0, 0, b'x', b'y', 0, 0, 0, 0, 0, 0, 0, 0]
        );
    }
}
