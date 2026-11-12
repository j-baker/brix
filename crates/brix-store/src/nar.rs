//! The Nix ARchive format.
//!
//! Mirrors `src/libutil/archive.cc`. A NAR is a canonical serialisation of
//! a file system object: every string is a little-endian `u64` length,
//! the bytes, and zero padding to a multiple of eight. Directory entries
//! are emitted in byte order of their names; only regular files (with an
//! executable flag), directories and symlinks can appear.
//!
//! Names and symlink targets are bytes, as in Nix: whatever the accessor
//! lists is written as it is, and parsing takes any name and target Nix
//! takes (`parseDump` checks only for `/`, NUL, `.`, `..` and the empty
//! name, and for an empty target).
//!
//! ```text
//! nix-archive-1 ( type regular [executable ""] contents <bytes> )
//! nix-archive-1 ( type directory [entry ( name <n> node <nar> )]* )
//! nix-archive-1 ( type symlink target <t> )
//! ```

use std::collections::BTreeMap;
use std::io::{self, Read, Write};

use bstr::BString;

use crate::accessor::{AccessorError, FileType, MemoryAccessor, MemoryNode, SourceAccessor};
use crate::canon_path::CanonPath;
use crate::content_address::FileIngestionMethod;
use crate::hash::{Hash, HashAlgo, Hasher};

/// `narVersionMagic1`.
pub const NAR_VERSION_MAGIC_1: &str = "nix-archive-1";

/// `narMaxDepth`: Nix refuses deeper trees in both directions.
pub const MAX_DEPTH: usize = 64;

/// Errors writing or reading a NAR. Paths and the parts of a damaged
/// archive quoted in a message are bytes: `Display` replaces those that
/// are not UTF-8, [`NarError::message_bytes`] keeps them.
#[derive(Debug, thiserror::Error)]
pub enum NarError {
    #[error(transparent)]
    Accessor(#[from] AccessorError),
    #[error("{0}")]
    Io(#[from] io::Error),
    /// The path as the accessor shows it.
    #[error("file '{0}' has an unsupported type")]
    UnsupportedType(BString),
    /// The path as the accessor shows it.
    #[error("path '{0}' exceeds maximum NAR directory depth of 64")]
    TooDeep(BString),
    /// `badArchive`: a malformed NAR while parsing; what it quotes of the
    /// archive (a tag, a name) is as the archive has it.
    #[error("bad archive: {0}")]
    Bad(BString),
    #[error("git ingestion is not supported by brix")]
    GitUnsupported,
}

impl NarError {
    /// The message as Nix writes it: the `Display` text, with the bytes
    /// of a path or of the archive as they are.
    pub fn message_bytes(&self) -> Vec<u8> {
        let quoted = |before: &str, path: &[u8], after: &str| {
            [before.as_bytes(), path, after.as_bytes()].concat()
        };
        match self {
            NarError::Accessor(e) => e.message_bytes(),
            NarError::UnsupportedType(path) => quoted("file '", path, "' has an unsupported type"),
            NarError::TooDeep(path) => quoted(
                "path '",
                path,
                "' exceeds maximum NAR directory depth of 64",
            ),
            NarError::Bad(what) => quoted("bad archive: ", what, ""),
            NarError::Io(_) | NarError::GitUnsupported => self.to_string().into_bytes(),
        }
    }
}

/// A path filter as `builtins.filterSource` / `builtins.path` need it: the
/// argument is the absolute path on the accessor being dumped.
pub type PathFilter<'a> = dyn FnMut(&CanonPath) -> Result<bool, NarError> + 'a;

/// `dumpPath`: write the NAR of `path` to `out`.
pub fn dump(
    accessor: &dyn SourceAccessor,
    path: &CanonPath,
    out: &mut dyn Write,
) -> Result<(), NarError> {
    dump_filtered(accessor, path, out, &mut |_| Ok(true))
}

/// `dumpPath` with a filter. The root itself is never filtered; each
/// directory entry is offered to `filter` before being descended into.
pub fn dump_filtered(
    accessor: &dyn SourceAccessor,
    path: &CanonPath,
    out: &mut dyn Write,
    filter: &mut PathFilter<'_>,
) -> Result<(), NarError> {
    write_str(out, NAR_VERSION_MAGIC_1.as_bytes())?;
    dump_node(accessor, path, out, filter, 0, true)
}

fn dump_node(
    accessor: &dyn SourceAccessor,
    path: &CanonPath,
    out: &mut dyn Write,
    filter: &mut PathFilter<'_>,
    depth: usize,
    // `false` to measure: a file whose size is known is not read.
    contents: bool,
) -> Result<(), NarError> {
    if depth >= MAX_DEPTH {
        return Err(NarError::TooDeep(accessor.show_path(path)));
    }
    let st = accessor.lstat(path)?;
    write_str(out, b"(")?;
    match st.file_type {
        FileType::Regular => {
            write_str(out, b"type")?;
            write_str(out, b"regular")?;
            if st.executable {
                write_str(out, b"executable")?;
                write_str(out, b"")?;
            }
            write_str(out, b"contents")?;
            // The size must precede the bytes, so when the accessor cannot
            // tell us the size up front, buffer the file.
            match st.size {
                // Measuring only: the bytes do not change the size.
                Some(size) if !contents => {
                    write_u64(out, size)?;
                    io::copy(&mut io::repeat(0).take(size), out)?;
                    write_padding(out, size)?;
                }
                Some(size) => {
                    write_u64(out, size)?;
                    let mut counter = CountingWriter {
                        inner: out,
                        written: 0,
                    };
                    accessor.read_file_to(path, &mut counter)?;
                    if counter.written != size {
                        let mut what = b"file '".to_vec();
                        what.extend_from_slice(&accessor.show_path(path));
                        what.extend_from_slice(b"' changed size while being read");
                        return Err(NarError::Bad(what.into()));
                    }
                    write_padding(out, size)?;
                }
                None => {
                    let data = accessor.read_file(path)?;
                    write_str(out, &data)?;
                }
            }
        }
        FileType::Directory => {
            write_str(out, b"type")?;
            write_str(out, b"directory")?;
            for name in accessor.read_dir(path)?.keys() {
                let child = path
                    .join_component(name)
                    .map_err(|e| NarError::Bad(e.message_bytes().into()))?;
                if !filter(&child)? {
                    continue;
                }
                write_str(out, b"entry")?;
                write_str(out, b"(")?;
                write_str(out, b"name")?;
                write_str(out, name)?;
                write_str(out, b"node")?;
                dump_node(accessor, &child, out, filter, depth + 1, contents)?;
                write_str(out, b")")?;
            }
        }
        FileType::Symlink => {
            write_str(out, b"type")?;
            write_str(out, b"symlink")?;
            write_str(out, b"target")?;
            write_str(out, &accessor.read_link(path)?)?;
        }
        _ => return Err(NarError::UnsupportedType(accessor.show_path(path))),
    }
    write_str(out, b")")?;
    Ok(())
}

/// The size of the NAR [`dump`] writes for `path`, without reading the
/// files whose size the accessor knows: what a store wants to be told
/// before it is sent the NAR.
pub fn size(accessor: &dyn SourceAccessor, path: &CanonPath) -> Result<u64, NarError> {
    struct Count(u64);
    impl Write for Count {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0 += buf.len() as u64;
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut count = Count(0);
    write_str(&mut count, NAR_VERSION_MAGIC_1.as_bytes())?;
    dump_node(accessor, path, &mut count, &mut |_| Ok(true), 0, false)?;
    Ok(count.0)
}

/// The NAR of the regular file at `path` as `Flat` ingestion stores it:
/// its bytes in a file that is not executable, whatever the source is
/// ([`dump_bytes`] of its contents, streamed).
pub fn dump_flat(
    accessor: &dyn SourceAccessor,
    path: &CanonPath,
    out: &mut dyn Write,
) -> Result<(), NarError> {
    let st = accessor.lstat(path)?;
    if st.file_type != FileType::Regular {
        return Err(AccessorError::NotARegularFile(accessor.show_path(path)).into());
    }
    let Some(size) = st.size else {
        return dump_bytes(&accessor.read_file(path)?, out);
    };
    write_str(out, NAR_VERSION_MAGIC_1.as_bytes())?;
    write_str(out, b"(")?;
    write_str(out, b"type")?;
    write_str(out, b"regular")?;
    write_str(out, b"contents")?;
    write_u64(out, size)?;
    let mut counter = CountingWriter {
        inner: out,
        written: 0,
    };
    accessor.read_file_to(path, &mut counter)?;
    if counter.written != size {
        let mut what = b"file '".to_vec();
        what.extend_from_slice(&accessor.show_path(path));
        what.extend_from_slice(b"' changed size while being read");
        return Err(NarError::Bad(what.into()));
    }
    write_padding(out, size)?;
    write_str(out, b")")?;
    Ok(())
}

/// `dumpString`: the NAR of a single non-executable regular file.
pub fn dump_bytes(contents: &[u8], out: &mut dyn Write) -> Result<(), NarError> {
    write_str(out, NAR_VERSION_MAGIC_1.as_bytes())?;
    write_str(out, b"(")?;
    write_str(out, b"type")?;
    write_str(out, b"regular")?;
    write_str(out, b"contents")?;
    write_str(out, contents)?;
    write_str(out, b")")?;
    Ok(())
}

/// `SourceAccessor::hashPath` + `HashSink`: the hash of the NAR of `path`
/// and the NAR's size.
pub fn hash(
    accessor: &dyn SourceAccessor,
    path: &CanonPath,
    algo: HashAlgo,
) -> Result<(Hash, u64), NarError> {
    hash_filtered(accessor, path, algo, &mut |_| Ok(true))
}

pub fn hash_filtered(
    accessor: &dyn SourceAccessor,
    path: &CanonPath,
    algo: HashAlgo,
    filter: &mut PathFilter<'_>,
) -> Result<(Hash, u64), NarError> {
    let mut hasher = Hasher::new(algo);
    dump_filtered(accessor, path, &mut hasher, filter)?;
    Ok(hasher.finish())
}

/// `hashPath(path, FileIngestionMethod, algo)`: the content hash that a
/// store object's path is derived from. For `Flat` that is the bytes of a
/// regular file, for `NixArchive` the NAR. Returns the hash and, for NAR,
/// its size.
pub fn hash_path(
    accessor: &dyn SourceAccessor,
    path: &CanonPath,
    method: FileIngestionMethod,
    algo: HashAlgo,
    filter: &mut PathFilter<'_>,
) -> Result<(Hash, Option<u64>), NarError> {
    match method {
        FileIngestionMethod::Flat => {
            let st = accessor.lstat(path)?;
            if st.file_type != FileType::Regular {
                return Err(AccessorError::NotARegularFile(accessor.show_path(path)).into());
            }
            let mut hasher = Hasher::new(algo);
            accessor.read_file_to(path, &mut hasher)?;
            Ok((hasher.finish().0, None))
        }
        FileIngestionMethod::NixArchive => {
            let (h, size) = hash_filtered(accessor, path, algo, filter)?;
            Ok((h, Some(size)))
        }
        FileIngestionMethod::Git => Err(NarError::GitUnsupported),
    }
}

/// `parseDump` into an in-memory tree.
pub fn parse(input: &mut dyn Read) -> Result<MemoryAccessor, NarError> {
    let mut r = Reader { input };
    let magic = r.read_str(32)?;
    if magic != NAR_VERSION_MAGIC_1.as_bytes() {
        return Err(NarError::Bad(
            "input doesn't look like a Nix archive".into(),
        ));
    }
    let root = parse_node(&mut r, 0)?;
    Ok(MemoryAccessor::from_root(root))
}

/// `badArchive("<before>'<quoted>'<after>")`: a message quoting bytes of
/// the archive as they are.
fn bad_quoting(before: &str, quoted: &[u8], after: &str) -> NarError {
    NarError::Bad(
        [before.as_bytes(), quoted, after.as_bytes()]
            .concat()
            .into(),
    )
}

fn parse_node(r: &mut Reader<'_>, depth: usize) -> Result<MemoryNode, NarError> {
    if depth >= MAX_DEPTH {
        return Err(NarError::Bad(
            "NAR directory nesting exceeds maximum depth of 64".into(),
        ));
    }
    r.expect_tag(b"(")?;
    r.expect_tag(b"type")?;
    let ty = r.read_tag()?;
    let node = match ty.as_slice() {
        b"regular" => {
            let mut tag = r.read_tag()?;
            let mut executable = false;
            if tag == b"executable" {
                let marker = r.read_str(0)?;
                if !marker.is_empty() {
                    return Err(NarError::Bad(
                        "executable marker has non-empty value".into(),
                    ));
                }
                executable = true;
                tag = r.read_tag()?;
            }
            if tag != b"contents" {
                return Err(bad_quoting("expected tag 'contents', got '", &tag, "'"));
            }
            let contents = r.read_str(u64::MAX)?;
            r.expect_tag(b")")?;
            MemoryNode::Regular {
                executable,
                contents,
            }
        }
        b"directory" => {
            let mut entries = BTreeMap::new();
            let mut prev_name: Vec<u8> = Vec::new();
            loop {
                let tag = r.read_tag()?;
                if tag == b")" {
                    break;
                }
                if tag != b"entry" {
                    return Err(bad_quoting("expected tag 'entry' or ')', got '", &tag, "'"));
                }
                r.expect_tag(b"(")?;
                r.expect_tag(b"name")?;
                // Any bytes but these, as in Nix, compared as `std::string`
                // compares: unsigned bytes.
                let name = r.read_str(255)?;
                if name.is_empty()
                    || name == b"."
                    || name == b".."
                    || name.contains(&b'/')
                    || name.contains(&0)
                {
                    return Err(bad_quoting("NAR contains invalid file name '", &name, "'"));
                }
                if name <= prev_name {
                    return Err(NarError::Bad("NAR directory is not sorted".into()));
                }
                prev_name.clone_from(&name);
                r.expect_tag(b"node")?;
                let child = parse_node(r, depth + 1)?;
                r.expect_tag(b")")?;
                entries.insert(BString::from(name), child);
            }
            MemoryNode::Directory(entries)
        }
        b"symlink" => {
            r.expect_tag(b"target")?;
            let target = r.read_str(4095)?;
            if target.is_empty() || target.contains(&0) {
                return Err(NarError::Bad("NAR contains invalid symlink target".into()));
            }
            r.expect_tag(b")")?;
            MemoryNode::Symlink {
                target: BString::from(target),
            }
        }
        other => return Err(bad_quoting("unknown file type '", other, "'")),
    };
    Ok(node)
}

struct Reader<'a> {
    input: &'a mut dyn Read,
}

impl Reader<'_> {
    fn read_u64(&mut self) -> Result<u64, NarError> {
        let mut b = [0u8; 8];
        self.input.read_exact(&mut b).map_err(|e| {
            if e.kind() == io::ErrorKind::UnexpectedEof {
                NarError::Bad("unexpected end-of-file".into())
            } else {
                NarError::Io(e)
            }
        })?;
        Ok(u64::from_le_bytes(b))
    }

    /// `readString(source, max)`: length, bytes, padding.
    fn read_str(&mut self, max: u64) -> Result<Vec<u8>, NarError> {
        let len = self.read_u64()?;
        if len > max {
            return Err(NarError::Bad("string is too long".into()));
        }
        let len_usize =
            usize::try_from(len).map_err(|_| NarError::Bad("string is too long".into()))?;
        let mut buf = vec![0u8; len_usize.min(1 << 20)];
        let mut data = Vec::with_capacity(len_usize.min(1 << 24));
        let mut remaining = len_usize;
        while remaining > 0 {
            let chunk = remaining.min(buf.len());
            self.input.read_exact(&mut buf[..chunk]).map_err(|e| {
                if e.kind() == io::ErrorKind::UnexpectedEof {
                    NarError::Bad("unexpected end-of-file".into())
                } else {
                    NarError::Io(e)
                }
            })?;
            data.extend_from_slice(&buf[..chunk]);
            remaining -= chunk;
        }
        let pad = (8 - len % 8) % 8;
        if pad > 0 {
            let mut z = [0u8; 8];
            self.input
                .read_exact(&mut z[..pad as usize])
                .map_err(|_| NarError::Bad("unexpected end-of-file".into()))?;
            if z[..pad as usize].iter().any(|&b| b != 0) {
                return Err(NarError::Bad("non-zero padding".into()));
            }
        }
        Ok(data)
    }

    /// A tag (`narMaxTag`): bytes, as Nix reads it.
    fn read_tag(&mut self) -> Result<Vec<u8>, NarError> {
        self.read_str(32)
    }

    fn expect_tag(&mut self, expected: &[u8]) -> Result<(), NarError> {
        let tag = self.read_tag()?;
        if tag != expected {
            let mut what = b"expected tag '".to_vec();
            what.extend_from_slice(expected);
            what.extend_from_slice(b"', got '");
            what.extend_from_slice(&tag);
            what.push(b'\'');
            return Err(NarError::Bad(what.into()));
        }
        Ok(())
    }
}

struct CountingWriter<'a> {
    inner: &'a mut dyn Write,
    written: u64,
}

impl Write for CountingWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.written += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

fn write_u64(out: &mut dyn Write, n: u64) -> io::Result<()> {
    out.write_all(&n.to_le_bytes())
}

/// `writePadding`.
fn write_padding(out: &mut dyn Write, len: u64) -> io::Result<()> {
    let pad = (8 - len % 8) % 8;
    if pad > 0 {
        out.write_all(&[0u8; 8][..pad as usize])?;
    }
    Ok(())
}

/// `writeString`: length, bytes, padding.
pub fn write_str(out: &mut dyn Write, s: &[u8]) -> io::Result<()> {
    write_u64(out, s.len() as u64)?;
    out.write_all(s)?;
    write_padding(out, s.len() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hash::HashFormat;
    use bstr::{BStr, ByteSlice};

    fn p(s: impl AsRef<[u8]>) -> CanonPath {
        CanonPath::new(s).unwrap()
    }

    fn sample() -> MemoryAccessor {
        let mut m = MemoryAccessor::new();
        m.add_file(&p("/b.txt"), "hello", false).unwrap();
        m.add_file(&p("/sub/run.sh"), "#!/bin/sh\n", true).unwrap();
        m.add_symlink(&p("/link"), "b.txt").unwrap();
        m.add_dir(&p("/empty")).unwrap();
        m
    }

    #[test]
    fn size_and_flat_dump_agree_with_the_dumps_they_stand_for() {
        for tree in [sample(), byte_names()] {
            let mut nar = Vec::new();
            dump(&tree, &CanonPath::root(), &mut nar).unwrap();
            assert_eq!(size(&tree, &CanonPath::root()).unwrap(), nar.len() as u64);
        }
        // The executable bit is the source's, not the flat object's.
        let (mut flat, mut expected) = (Vec::new(), Vec::new());
        dump_flat(&sample(), &p("/sub/run.sh"), &mut flat).unwrap();
        dump_bytes(b"#!/bin/sh\n", &mut expected).unwrap();
        assert_eq!(flat, expected);
        assert!(dump_flat(&sample(), &p("/sub"), &mut Vec::new()).is_err());
    }

    /// A tree whose names and target are not UTF-8, and whose names sort
    /// differently as signed and as unsigned bytes.
    fn byte_names() -> MemoryAccessor {
        let mut m = MemoryAccessor::new();
        for name in [&b"z"[..], b"caf\xe9", b"\xff", b"a\x80b", b"d\xe9/f"] {
            m.add_file(&p([&b"/"[..], name].concat()), name, false)
                .unwrap();
        }
        m.add_symlink(&p(b"/l\xff"), b"t\xe9").unwrap();
        m
    }

    #[test]
    fn single_file_nar_matches_nix() {
        // A non-executable file containing "hello": its NAR is the
        // regular-file form of `dump_bytes`.
        let mut m = MemoryAccessor::new();
        m.add_file(&CanonPath::root(), "hello", false).unwrap();
        let mut a = Vec::new();
        dump(&m, &CanonPath::root(), &mut a).unwrap();
        let mut b = Vec::new();
        dump_bytes(b"hello", &mut b).unwrap();
        assert_eq!(a, b);
        assert_eq!(&a[..8], &13u64.to_le_bytes());
        assert_eq!(&a[8..21], b"nix-archive-1");
        // From the oracle: `nix hash path --type sha256 --sri` of that file,
        // and the size of its `nix-store --dump`.
        let (h, size) = hash(&m, &CanonPath::root(), HashAlgo::Sha256).unwrap();
        assert_eq!(
            h.to_string(HashFormat::Sri, true),
            "sha256-CkMIecJm+LV/QJKg+TXPP6zUi7zN5XYNR0jKQFFx6Wk="
        );
        assert_eq!(size, 120);
        assert_eq!(size, a.len() as u64);
    }

    #[test]
    fn round_trips_through_parse() {
        for m in [sample(), byte_names()] {
            let mut nar = Vec::new();
            dump(&m, &CanonPath::root(), &mut nar).unwrap();
            let back = parse(&mut nar.as_slice()).unwrap();
            assert_eq!(back.root(), m.root());
            let mut again = Vec::new();
            dump(&back, &CanonPath::root(), &mut again).unwrap();
            assert_eq!(nar, again);
        }
    }

    /// Entries are written in unsigned byte order of their names, the
    /// names and the target as they are. Oracle: `nix-store --dump` of
    /// the same tree on disk (`tests/oracle.rs` compares the whole NAR).
    #[test]
    fn byte_names_are_dumped_as_they_are() {
        let mut nar = Vec::new();
        dump(&byte_names(), &CanonPath::root(), &mut nar).unwrap();
        let names: Vec<&BStr> = [
            &b"a\x80b"[..],
            b"caf\xe9",
            b"d\xe9",
            b"l\xff",
            b"z",
            b"\xff",
        ]
        .into_iter()
        .map(BStr::new)
        .collect();
        let mut at = 0;
        for name in names {
            let mut entry = Vec::new();
            write_str(&mut entry, b"name").unwrap();
            write_str(&mut entry, name).unwrap();
            let found = nar[at..]
                .find(&entry)
                .unwrap_or_else(|| panic!("{name:?} after byte {at}"));
            at += found + entry.len();
        }
        let mut target = Vec::new();
        write_str(&mut target, b"target").unwrap();
        write_str(&mut target, b"t\xe9").unwrap();
        assert!(nar.find(&target).is_some());
    }

    #[test]
    fn filter_drops_entries() {
        let m = sample();
        let (full, _) = hash(&m, &CanonPath::root(), HashAlgo::Sha256).unwrap();
        let mut seen = Vec::new();
        let (filtered, _) = hash_filtered(&m, &CanonPath::root(), HashAlgo::Sha256, &mut |path| {
            seen.push(path.abs().to_owned());
            Ok(path.abs() != "/sub")
        })
        .unwrap();
        assert_ne!(full, filtered);
        assert_eq!(seen, ["/b.txt", "/empty", "/link", "/sub"]);
        let mut expected = MemoryAccessor::new();
        expected.add_file(&p("/b.txt"), "hello", false).unwrap();
        expected.add_symlink(&p("/link"), "b.txt").unwrap();
        expected.add_dir(&p("/empty")).unwrap();
        let (e, _) = hash(&expected, &CanonPath::root(), HashAlgo::Sha256).unwrap();
        assert_eq!(filtered, e);
    }

    #[test]
    fn rejects_bad_archives() {
        let mut nar = Vec::new();
        dump(&sample(), &CanonPath::root(), &mut nar).unwrap();
        assert!(parse(&mut &nar[..nar.len() - 8]).is_err());
        let mut garbage = Vec::new();
        write_str(&mut garbage, b"nix-archive-2").unwrap();
        assert_eq!(
            parse(&mut garbage.as_slice()).unwrap_err().to_string(),
            "bad archive: input doesn't look like a Nix archive"
        );
    }

    /// A NAR of the given strings, each written as `writeString` does.
    fn nar_of(strings: &[&[u8]]) -> Vec<u8> {
        let mut nar = Vec::new();
        for s in strings {
            write_str(&mut nar, s).unwrap();
        }
        nar
    }

    #[test]
    fn checks_symlink_targets_and_names_like_nix() {
        let symlink = |target: &[u8]| {
            let nar = nar_of(&[
                &b"nix-archive-1"[..],
                b"(",
                b"type",
                b"symlink",
                b"target",
                target,
                b")",
            ]);
            parse(&mut nar.as_slice()).map(|m| m.root().cloned())
        };
        // `nix-store --restore` says the same of these NARs.
        for target in [&b""[..], b"a\0b"] {
            assert_eq!(
                symlink(target).unwrap_err().to_string(),
                "bad archive: NAR contains invalid symlink target"
            );
        }
        // Any other bytes are a target, as in Nix.
        for target in [&b"ok"[..], b"t\xe9", b"\xff/../x"] {
            assert_eq!(
                symlink(target).unwrap(),
                Some(MemoryNode::Symlink {
                    target: target.into()
                })
            );
        }
        let entry = |name: &[u8]| {
            let nar = nar_of(&[
                &b"nix-archive-1"[..],
                b"(",
                b"type",
                b"directory",
                b"entry",
                b"(",
                b"name",
                name,
                b"node",
            ]);
            parse(&mut nar.as_slice()).unwrap_err().message_bytes()
        };
        // The message quotes the name as it is.
        assert_eq!(
            entry(b"a/\xff"),
            b"bad archive: NAR contains invalid file name 'a/\xff'"
        );
        assert_eq!(
            entry(b".."),
            b"bad archive: NAR contains invalid file name '..'"
        );
        // A name that is not UTF-8 is a name: the parse goes on to the
        // node, which is missing here.
        assert_eq!(entry(b"\xff"), b"bad archive: unexpected end-of-file");
        // Tags are compared as bytes and quoted as they are.
        let bad_tag = nar_of(&[&b"nix-archive-1"[..], b"(", b"type", b"\xe9"]);
        assert_eq!(
            parse(&mut bad_tag.as_slice()).unwrap_err().message_bytes(),
            b"bad archive: unknown file type '\xe9'"
        );
        let bad_tag = nar_of(&[&b"nix-archive-1"[..], b"(", b"\xe9"]);
        assert_eq!(
            parse(&mut bad_tag.as_slice()).unwrap_err().message_bytes(),
            b"bad archive: expected tag 'type', got '\xe9'"
        );
    }

    /// Entries must be in unsigned byte order, as `std::string` compares
    /// them: `z` (0x7a) before `\xe9`, not after.
    #[test]
    fn directory_order_is_unsigned_bytes() {
        let dir = |names: &[&[u8]]| {
            let mut strings: Vec<&[u8]> = vec![&b"nix-archive-1"[..], b"(", b"type", b"directory"];
            for &name in names {
                for s in [
                    &b"entry"[..],
                    b"(",
                    b"name",
                    name,
                    b"node",
                    b"(",
                    b"type",
                    b"regular",
                    b"contents",
                    b"",
                    b")",
                    b")",
                ] {
                    strings.push(s);
                }
            }
            strings.push(b")");
            parse(&mut nar_of(&strings).as_slice()).map(|m| m.root().cloned())
        };
        assert!(dir(&[&b"z"[..], b"\xe9"]).is_ok());
        assert_eq!(
            dir(&[&b"\xe9"[..], b"z"]).unwrap_err().to_string(),
            "bad archive: NAR directory is not sorted"
        );
    }

    #[test]
    fn errors_show_paths_as_bytes() {
        let shown = BString::from(&b"/a\xff"[..]);
        assert_eq!(
            NarError::UnsupportedType(shown.clone()).message_bytes(),
            b"file '/a\xff' has an unsupported type"
        );
        assert_eq!(
            NarError::TooDeep(shown.clone()).message_bytes(),
            b"path '/a\xff' exceeds maximum NAR directory depth of 64"
        );
        assert_eq!(
            NarError::from(AccessorError::NotFound(shown)).message_bytes(),
            b"path '/a\xff' does not exist"
        );
        assert_eq!(
            NarError::GitUnsupported.message_bytes(),
            b"git ingestion is not supported by brix"
        );
    }

    #[test]
    fn flat_hash_is_file_hash() {
        let m = sample();
        let (h, size) = hash_path(
            &m,
            &p("/b.txt"),
            FileIngestionMethod::Flat,
            HashAlgo::Sha256,
            &mut |_| Ok(true),
        )
        .unwrap();
        assert_eq!(h, Hash::of(HashAlgo::Sha256, b"hello"));
        assert_eq!(size, None);
        assert!(hash_path(
            &m,
            &p("/sub"),
            FileIngestionMethod::Flat,
            HashAlgo::Sha256,
            &mut |_| Ok(true)
        )
        .is_err());
        let (h2, size2) = hash_path(
            &m,
            &p("/sub"),
            FileIngestionMethod::NixArchive,
            HashAlgo::Sha256,
            &mut |_| Ok(true),
        )
        .unwrap();
        assert!(size2.is_some());
        assert_ne!(
            h2.to_string(HashFormat::Nix32, false),
            h.to_string(HashFormat::Nix32, false)
        );
    }
}
