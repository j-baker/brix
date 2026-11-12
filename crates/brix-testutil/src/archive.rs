//! Archives written byte by byte, for tests of what an unpacker makes of
//! member names that are not UTF-8, of pax headers and of zip flags: the
//! `tar` and `zip` crates' writers normalise such input or refuse it, and
//! a test must control every byte the unpacker reads.

/// The modification time of every member: `lastModified` 1700000000.
pub const ARCHIVE_MTIME: u64 = 1_700_000_000;

/// A tar archive built member by member, in the order of the calls.
/// Members are ustar headers (`ustar\0` `00`), the GNU extension headers
/// (`L`, `K`) are GNU ones (`ustar  \0`); every member has uid and gid 0,
/// user and group `root` and [`ARCHIVE_MTIME`].
#[derive(Clone, Debug, Default)]
pub struct TarBuilder {
    out: Vec<u8>,
}

impl TarBuilder {
    pub fn new() -> TarBuilder {
        TarBuilder::default()
    }

    /// A directory (type `5`, mode 0755). `name` usually ends in `/`.
    pub fn dir(&mut self, name: &[u8]) -> &mut TarBuilder {
        self.member(name, b'5', 0o755, b"", b"")
    }

    /// A regular file (type `0`, mode 0644, or 0755 when `executable`).
    pub fn file(&mut self, name: &[u8], contents: &[u8], executable: bool) -> &mut TarBuilder {
        let mode = if executable { 0o755 } else { 0o644 };
        self.member(name, b'0', mode, b"", contents)
    }

    /// A symlink (type `2`) to `target`.
    pub fn symlink(&mut self, name: &[u8], target: &[u8]) -> &mut TarBuilder {
        self.member(name, b'2', 0o777, target, b"")
    }

    /// A hard link (type `1`) to the member named `target`.
    pub fn hardlink(&mut self, name: &[u8], target: &[u8]) -> &mut TarBuilder {
        self.member(name, b'1', 0o644, target, b"")
    }

    /// A pax extended header (type `x`) for the next member, holding
    /// `key=value` for each of `records`, in order.
    pub fn pax(&mut self, records: &[(&str, &[u8])]) -> &mut TarBuilder {
        let mut data = Vec::new();
        for (key, value) in records {
            data.extend_from_slice(&pax_record(key, value));
        }
        self.member(b"PaxHeader", b'x', 0o644, b"", &data)
    }

    /// A GNU long name (type `L`) for the next member, whose own header
    /// holds the name's first 100 bytes.
    pub fn gnu_long_name(&mut self, name: &[u8]) -> &mut TarBuilder {
        self.gnu_long(b'L', name)
    }

    /// A GNU long link target (type `K`) for the next member.
    pub fn gnu_long_link(&mut self, target: &[u8]) -> &mut TarBuilder {
        self.gnu_long(b'K', target)
    }

    /// The archive, with the two zero blocks that end it.
    pub fn finish(&self) -> Vec<u8> {
        let mut out = self.out.clone();
        out.resize(out.len() + 1024, 0);
        out
    }

    fn gnu_long(&mut self, kind: u8, text: &[u8]) -> &mut TarBuilder {
        let mut data = text.to_vec();
        data.push(0);
        let header = tar_header(b"././@LongLink", kind, 0o644, data.len(), b"", true);
        self.push(&header, &data)
    }

    fn member(
        &mut self,
        name: &[u8],
        kind: u8,
        mode: u32,
        link: &[u8],
        contents: &[u8],
    ) -> &mut TarBuilder {
        let header = tar_header(name, kind, mode, contents.len(), link, false);
        self.push(&header, contents)
    }

    fn push(&mut self, header: &[u8; 512], contents: &[u8]) -> &mut TarBuilder {
        self.out.extend_from_slice(header);
        self.out.extend_from_slice(contents);
        let padding = (512 - contents.len() % 512) % 512;
        self.out.resize(self.out.len() + padding, 0);
        self
    }
}

/// A 512-byte tar header; `name` and `link` are cut to their fields' 100
/// bytes.
fn tar_header(name: &[u8], kind: u8, mode: u32, size: usize, link: &[u8], gnu: bool) -> [u8; 512] {
    fn put(header: &mut [u8; 512], at: usize, len: usize, bytes: &[u8]) {
        let n = bytes.len().min(len);
        header[at..at + n].copy_from_slice(&bytes[..n]);
    }
    let mut h = [0u8; 512];
    put(&mut h, 0, 100, name);
    put(&mut h, 100, 8, format!("{mode:07o}\0").as_bytes());
    put(&mut h, 108, 8, b"0000000\0");
    put(&mut h, 116, 8, b"0000000\0");
    put(&mut h, 124, 12, format!("{size:011o}\0").as_bytes());
    put(
        &mut h,
        136,
        12,
        format!("{ARCHIVE_MTIME:011o}\0").as_bytes(),
    );
    // The checksum is computed with its own field as spaces.
    put(&mut h, 148, 8, b"        ");
    h[156] = kind;
    put(&mut h, 157, 100, link);
    put(
        &mut h,
        257,
        8,
        if gnu { b"ustar  \0" } else { b"ustar\x0000" },
    );
    put(&mut h, 265, 32, b"root");
    put(&mut h, 297, 32, b"root");
    let sum: u32 = h.iter().map(|&b| u32::from(b)).sum();
    put(&mut h, 148, 8, format!("{sum:06o}\0 ").as_bytes());
    h
}

/// A pax record, `<length> <key>=<value>\n`, its length counting itself.
fn pax_record(key: &str, value: &[u8]) -> Vec<u8> {
    let body = key.len() + value.len() + 3; // ' ', '=', '\n'
    let mut len = body + 1;
    while len != body + len.to_string().len() {
        len = body + len.to_string().len();
    }
    let mut record = format!("{len} {key}=").into_bytes();
    record.extend_from_slice(value);
    record.push(b'\n');
    record
}

/// A zip archive of stored (uncompressed) members, built member by
/// member: a local header, the contents, and at the end the central
/// directory (made by Unix, the mode in the external attributes) and its
/// end record. Every member has the DOS time 2023-11-14 22:13:20.
#[derive(Clone, Debug, Default)]
pub struct ZipBuilder {
    out: Vec<u8>,
    central: Vec<u8>,
    count: u16,
}

/// General purpose flag bit 11: the name is UTF-8.
pub const ZIP_UTF8_FLAG: u16 = 1 << 11;

impl ZipBuilder {
    pub fn new() -> ZipBuilder {
        ZipBuilder::default()
    }

    /// A member: `mode` is its Unix mode with the file type
    /// (`0o100644`, `0o40755` for a directory, whose name ends in `/`,
    /// `0o120777` for a symlink, whose contents are its target); `flags`
    /// its general purpose flags ([`ZIP_UTF8_FLAG`] or 0).
    pub fn member(
        &mut self,
        name: &[u8],
        contents: &[u8],
        mode: u32,
        flags: u16,
    ) -> &mut ZipBuilder {
        self.member_with_extra(name, contents, mode, flags, b"", b"")
    }

    /// A member with `local_extra` as the extra field of its local
    /// header and `central_extra` as that of its central directory entry
    /// (see [`zip_unicode_path_field`]).
    pub fn member_with_extra(
        &mut self,
        name: &[u8],
        contents: &[u8],
        mode: u32,
        flags: u16,
        local_extra: &[u8],
        central_extra: &[u8],
    ) -> &mut ZipBuilder {
        const DOS_TIME: u16 = (22 << 11) | (13 << 5) | 10;
        const DOS_DATE: u16 = ((2023 - 1980) << 9) | (11 << 5) | 14;
        let offset = u32::try_from(self.out.len()).expect("a small archive");
        let crc = crc32(contents);
        let size = u32::try_from(contents.len()).expect("a small member");
        let name_len = u16::try_from(name.len()).expect("a short name");
        let extra_len = |extra: &[u8]| u16::try_from(extra.len()).expect("a short extra field");
        // The fields both headers share, from "version needed" to the
        // name's length.
        let mut common = Vec::new();
        common.extend_from_slice(&20u16.to_le_bytes());
        common.extend_from_slice(&flags.to_le_bytes());
        common.extend_from_slice(&0u16.to_le_bytes()); // stored
        common.extend_from_slice(&DOS_TIME.to_le_bytes());
        common.extend_from_slice(&DOS_DATE.to_le_bytes());
        common.extend_from_slice(&crc.to_le_bytes());
        common.extend_from_slice(&size.to_le_bytes());
        common.extend_from_slice(&size.to_le_bytes());
        common.extend_from_slice(&name_len.to_le_bytes());

        self.out.extend_from_slice(b"PK\x03\x04");
        self.out.extend_from_slice(&common);
        self.out
            .extend_from_slice(&extra_len(local_extra).to_le_bytes());
        self.out.extend_from_slice(name);
        self.out.extend_from_slice(local_extra);
        self.out.extend_from_slice(contents);

        self.central.extend_from_slice(b"PK\x01\x02");
        // Made by Unix (3), version 3.0.
        self.central
            .extend_from_slice(&((3u16 << 8) | 30).to_le_bytes());
        self.central.extend_from_slice(&common);
        self.central
            .extend_from_slice(&extra_len(central_extra).to_le_bytes());
        self.central.extend_from_slice(&0u16.to_le_bytes()); // comment
        self.central.extend_from_slice(&0u16.to_le_bytes()); // disk
        self.central.extend_from_slice(&0u16.to_le_bytes()); // internal attributes
        self.central.extend_from_slice(&(mode << 16).to_le_bytes());
        self.central.extend_from_slice(&offset.to_le_bytes());
        self.central.extend_from_slice(name);
        self.central.extend_from_slice(central_extra);
        self.count += 1;
        self
    }

    /// The archive.
    pub fn finish(&self) -> Vec<u8> {
        let mut out = self.out.clone();
        let offset = u32::try_from(out.len()).expect("a small archive");
        let size = u32::try_from(self.central.len()).expect("a small archive");
        out.extend_from_slice(&self.central);
        out.extend_from_slice(b"PK\x05\x06");
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&self.count.to_le_bytes());
        out.extend_from_slice(&self.count.to_le_bytes());
        out.extend_from_slice(&size.to_le_bytes());
        out.extend_from_slice(&offset.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out
    }
}

/// An Info-ZIP Unicode Path extra field (0x7075) for a member whose name
/// field is `name`: version 1, the CRC-32 of `name`, then `unicode`.
pub fn zip_unicode_path_field(name: &[u8], unicode: &[u8]) -> Vec<u8> {
    let mut field = Vec::new();
    field.extend_from_slice(&0x7075u16.to_le_bytes());
    let len = u16::try_from(5 + unicode.len()).expect("a short name");
    field.extend_from_slice(&len.to_le_bytes());
    field.push(1);
    field.extend_from_slice(&crc32(name).to_le_bytes());
    field.extend_from_slice(unicode);
    field
}

/// The CRC-32 (IEEE 802.3, as zip uses it) of `bytes`, bit by bit:
/// fixtures are small.
pub fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32_is_zips() {
        assert_eq!(crc32(b""), 0);
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn pax_records_count_their_own_length() {
        assert_eq!(pax_record("path", b"a"), b"9 path=a\n");
        let long = vec![b'x'; 95];
        let record = pax_record("path", &long);
        assert_eq!(record.len(), 105);
        assert!(record.starts_with(b"105 path="));
    }

    #[test]
    fn tar_headers_are_checksummed_blocks() {
        let tar = TarBuilder::new().file(b"caf\xe9", b"1", false).finish();
        assert_eq!(tar.len(), 512 * 4);
        assert_eq!(&tar[..4], b"caf\xe9");
        assert_eq!(&tar[257..265], b"ustar\x0000");
        let stored: u32 =
            u32::from_str_radix(std::str::from_utf8(&tar[148..154]).unwrap(), 8).unwrap();
        let mut header = tar[..512].to_vec();
        header[148..156].copy_from_slice(b"        ");
        assert_eq!(stored, header.iter().map(|&b| u32::from(b)).sum::<u32>());
        assert_eq!(&tar[512..513], b"1");
    }
}
