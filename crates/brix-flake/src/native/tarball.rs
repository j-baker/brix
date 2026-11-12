//! Tarball and single-file inputs (`src/libfetchers/tarball.cc`,
//! `src/libutil/tarfile.cc`).
//!
//! Nix unpacks archives with libarchive into a Git-style tree and then
//! "dereferences" a singleton top-level directory: a tarball containing
//! just `foo-1.0/…` yields the contents of `foo-1.0`. `lastModified` is
//! the newest mtime of any archive member.
//!
//! Member names and symlink targets are bytes, as libarchive hands them
//! to Nix. Nix never sets a locale, so libarchive converts what it
//! converts to the C locale's ASCII:
//!
//! - tar: ustar, GNU (`L`, `K`) and v7 names and link targets are taken
//!   as they are. A pax header's `path`, `uname`, `gname` and `linkpath`
//!   are UTF-8 unless the header says `hdrcharset=BINARY`; one that is
//!   not ASCII cannot be converted, so libarchive keeps its bytes and
//!   Nix warns "getting archive member '…': Pathname can't be converted
//!   from UTF-8 to current locale", naming the last of the four that
//!   failed ([`pax_unconvertible`]).
//! - zip: a member's name is its local header's ([`zip_member_name`]).
//!   Without the UTF-8 flag it is taken as it is (no CP437 decoding on
//!   POSIX); with the flag, a name that is not ASCII cannot be converted
//!   and the member has no name: "cannot get archive member name:
//!   Pathname cannot be converted from UTF-8 to current locale". An
//!   Info-ZIP Unicode Path field replaces the name. A symlink's target is
//!   its contents, as they are.

use std::collections::BTreeMap;
use std::io::{Cursor, Read};
use std::sync::Arc;

use brix_store::accessor::MemoryNode;
use brix_store::{BStr, CanonPath, MemoryAccessor};

use crate::attrs::{get_str_attr, Attr};
use crate::fetch::{ArchiveMembers, FetchedTree};
use crate::hashes::NarHash;
use crate::input::{FetchSettings, Input, Scheme};
use crate::native::http::HttpClient;
use crate::{err, Result};
use brix_store::url::{parse_url, url_path_to_path, ParsedUrl};
use jiff::tz::TimeZone;

/// An unpacked archive.
pub struct Unpacked {
    pub tree: MemoryAccessor,
    /// The newest member mtime, as `unpackTarfileToSink` returns it.
    pub last_modified: u64,
    /// The name of the archive's single top-level directory, when `tree`
    /// is that directory's contents (`dereferenceSingletonDirectory`).
    pub stripped: Option<Vec<u8>>,
}

/// `TarArchive` + `unpackTarfileToSink` + `dereferenceSingletonDirectory`:
/// unpack a (possibly compressed) tar or zip archive into memory. What
/// libarchive warns about (a name it could not convert) goes to `warn`.
pub fn unpack_archive(data: &[u8], name_hint: &str, warn: &dyn Fn(&str)) -> Result<Unpacked> {
    let decompressed = decompress(data)?;
    let mut unpacked = if is_zip(&decompressed) || name_hint.to_lowercase().ends_with(".zip") {
        unpack_zip(&decompressed, &TimeZone::system(), warn)?
    } else {
        unpack_tar(&decompressed, warn)?
    };
    unpacked.stripped = dereference_singleton_directory(&mut unpacked.tree);
    Ok(unpacked)
}

/// How an archive is compressed, by its magic bytes (libarchive's "filter
/// all").
#[derive(Clone, Copy, PartialEq, Eq)]
enum Compression {
    None,
    Gzip,
    Bzip2,
    Xz,
    Zstd,
}

impl Compression {
    fn of(data: &[u8]) -> Compression {
        if data.starts_with(&[0x1f, 0x8b]) {
            Compression::Gzip
        } else if data.starts_with(b"BZh") {
            Compression::Bzip2
        } else if data.starts_with(&[0xfd, 0x37, 0x7a, 0x58, 0x5a, 0x00]) {
            Compression::Xz
        } else if data.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]) {
            Compression::Zstd
        } else {
            Compression::None
        }
    }

    /// `data`, decompressed as it is read.
    fn reader<'r>(self, data: impl Read + 'r) -> std::io::Result<Box<dyn Read + 'r>> {
        Ok(match self {
            Compression::None => Box::new(data),
            Compression::Gzip => Box::new(flate2::read::MultiGzDecoder::new(data)),
            Compression::Bzip2 => Box::new(bzip2::read::MultiBzDecoder::new(data)),
            Compression::Xz => Box::new(xz2::read::XzDecoder::new_multi_decoder(data)),
            Compression::Zstd => Box::new(zstd::stream::read::Decoder::new(data)?),
        })
    }
}

/// [`unpack_archive`] of an archive that is still arriving: `data` is
/// decompressed and unpacked as it is read, and `members` shown each
/// regular file as it is unpacked. `None` when this cannot be done, or
/// goes wrong: [`unpack_archive`] of the whole archive then says what the
/// archive unpacks to, or why it does not. A zip archive is such a one
/// (its directory is at its end), and so is anything `data` fails to
/// deliver. The warnings of an archive unpacked this way go to `warn`
/// once it has unpacked, in the order [`unpack_archive`] gives them.
pub fn unpack_stream(
    mut data: &mut dyn Read,
    name_hint: &str,
    warn: &dyn Fn(&str),
    members: Option<&mut (dyn ArchiveMembers + '_)>,
) -> Option<Unpacked> {
    // The magic bytes say what the archive is.
    let mut magic = Vec::with_capacity(6);
    (&mut data).take(6).read_to_end(&mut magic).ok()?;
    if is_zip(&magic) || name_hint.to_lowercase().ends_with(".zip") {
        return None;
    }
    let mut decompressed = Compression::of(&magic)
        .reader(Cursor::new(magic).chain(data))
        .ok()?;
    let warnings = std::cell::RefCell::new(Vec::new());
    let mut unpacked = unpack_tar_from(
        &mut decompressed,
        &|warning| warnings.borrow_mut().push(warning.to_owned()),
        members,
    )
    .ok()?;
    // What follows the archive's end is decompressed too, as it is when
    // the whole archive is decompressed first: damage there is an error.
    std::io::copy(&mut decompressed, &mut std::io::sink()).ok()?;
    for warning in warnings.into_inner() {
        warn(&warning);
    }
    unpacked.stripped = dereference_singleton_directory(&mut unpacked.tree);
    Some(unpacked)
}

/// Downloads the archive at `url` and unpacks it, as it arrives when that
/// can be done ([`unpack_stream`]): the archive observer of `settings`,
/// if there is one, is shown its members meanwhile. `nar_hash` is the
/// hash the tree is expected to have, and `download_error` what becomes
/// of an error of the download itself. The members are returned for the
/// caller to finish, once it knows the tree's hash.
pub(super) fn download_archive(
    http: &HttpClient,
    url: &str,
    headers: &[(String, String)],
    settings: &FetchSettings,
    name_hint: &str,
    nar_hash: Option<&NarHash>,
    download_error: &dyn Fn(crate::Error) -> crate::Error,
) -> Result<DownloadedArchive> {
    let mut streamed = None;
    let download = http.download_streaming(url, headers, settings, &mut |body| {
        let mut members = settings
            .archive_observer
            .as_ref()
            .and_then(|watch| watch.0.begin(nar_hash));
        streamed = unpack_stream(body, name_hint, &*settings.warn, members.as_deref_mut())
            .map(|unpacked| (unpacked, members));
    });
    let download = download.map_err(download_error)?;
    let (unpacked, members) = match streamed {
        Some(streamed) => streamed,
        None => (
            unpack_archive(&download.data, name_hint, &*settings.warn)?,
            None,
        ),
    };
    Ok(DownloadedArchive {
        download,
        unpacked,
        members,
    })
}

/// What [`download_archive`] returns.
pub(super) struct DownloadedArchive {
    pub download: super::http::Download,
    pub unpacked: Unpacked,
    /// The archive's members as its observer was shown them, to finish.
    pub members: Option<Box<dyn ArchiveMembers>>,
}

fn is_zip(data: &[u8]) -> bool {
    data.starts_with(b"PK\x03\x04") || data.starts_with(b"PK\x05\x06")
}

/// Detect the compression from magic bytes (libarchive's "filter all")
/// and decompress.
fn decompress(data: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let io = |e: std::io::Error| err!("decompressing archive: {}", e);
    let compression = Compression::of(data);
    if compression == Compression::None {
        return Ok(data.to_vec());
    }
    compression
        .reader(data)
        .map_err(io)?
        .read_to_end(&mut out)
        .map_err(io)?;
    Ok(out)
}

/// `unpackTarfile` of an uncompressed tar archive, as it is (no single
/// top-level directory stripped). A member whose pax header has a name
/// libarchive cannot convert is warned about on `warn`, as Nix does
/// (`getting archive member '…': …`), and kept with its bytes.
pub(super) fn unpack_tar(data: &[u8], warn: &dyn Fn(&str)) -> Result<Unpacked> {
    unpack_tar_from(&mut Cursor::new(data), warn, None)
}

/// [`unpack_tar`] of an archive as it is read, showing `members` each
/// regular file as it is unpacked.
fn unpack_tar_from(
    data: &mut dyn Read,
    warn: &dyn Fn(&str),
    mut members: Option<&mut (dyn ArchiveMembers + '_)>,
) -> Result<Unpacked> {
    let mut archive = tar::Archive::new(data);
    archive.set_preserve_permissions(true);
    archive.set_unpack_xattrs(false);
    let mut tree = MemoryAccessor::new();
    let mut last_modified = 0u64;
    // Hard links refer to earlier members; remember their contents.
    let mut hardlink_sources: BTreeMap<CanonPath, MemoryNode> = BTreeMap::new();
    let entries = archive
        .entries()
        .map_err(|e| err!("reading tar archive: {}", e))?;
    for entry in entries {
        let mut entry = entry.map_err(|e| err!("reading tar archive: {}", e))?;
        let unconvertible = pax_unconvertible(&mut entry)?;
        // The pax `path`, else the GNU long name, else the header's.
        let raw_path = entry.path_bytes().into_owned();
        let shown = BStr::new(&raw_path);
        let path = CanonPath::new(&raw_path)
            .map_err(|e| err!("invalid archive member name '{}': {}", shown, e))?;
        if let Some(attribute) = unconvertible {
            warn(&format!(
                "getting archive member '{shown}': {attribute} can't be converted from UTF-8 to current locale"
            ));
        }
        let header = entry.header();
        let header_err = |e: std::io::Error| err!("reading the header of '{}': {}", shown, e);
        last_modified = last_modified.max(header.mtime().map_err(header_err)?);
        let mode = header.mode().map_err(header_err)?;
        let size = header.size().map_err(header_err)?;
        let kind = header.entry_type();
        match kind {
            tar::EntryType::Directory => {
                if path.is_root() {
                    continue;
                }
                tree.add_dir(&path)?;
            }
            tar::EntryType::Regular | tar::EntryType::Continuous | tar::EntryType::GNUSparse => {
                let mut contents = Vec::with_capacity(size as usize);
                entry
                    .read_to_end(&mut contents)
                    .map_err(|e| err!("cannot read file from tarball: {}", e))?;
                if let Some(members) = members.as_deref_mut() {
                    members.file(&path, &contents);
                }
                let node = MemoryNode::Regular {
                    executable: mode & 0o100 != 0,
                    contents,
                };
                hardlink_sources.insert(path.clone(), node.clone());
                tree.insert(&path, node)?;
            }
            tar::EntryType::Symlink => {
                // The pax `linkpath`, else the GNU long link, else the
                // header's.
                let target = entry
                    .link_name_bytes()
                    .ok_or_else(|| err!("symlink '{}' in tarball has no target", shown))?;
                tree.add_symlink(&path, &*target)?;
            }
            tar::EntryType::Link => {
                let target = entry
                    .link_name_bytes()
                    .ok_or_else(|| err!("hard link '{}' in tarball has no target", shown))?;
                let target_path = CanonPath::new(&*target).map_err(|e| {
                    err!("invalid hard link target '{}': {}", BStr::new(&target), e)
                })?;
                let node = hardlink_sources.get(&target_path).cloned().ok_or_else(|| {
                    err!(
                        "hard link target '{}' does not exist in tarball",
                        BStr::new(&target)
                    )
                })?;
                tree.insert(&path, node)?;
            }
            // Extended headers carry metadata for the next member; the
            // `tar` crate has already applied them.
            tar::EntryType::XGlobalHeader
            | tar::EntryType::XHeader
            | tar::EntryType::GNULongName
            | tar::EntryType::GNULongLink => {}
            other => {
                return Err(err!(
                    "file '{}' in tarball has unsupported file type {}",
                    shown,
                    other.as_byte()
                ))
            }
        }
    }
    if tree.root().is_none() {
        tree.insert(&CanonPath::root(), MemoryNode::Directory(BTreeMap::new()))?;
    }
    Ok(Unpacked {
        tree,
        last_modified,
        stripped: None,
    })
}

/// What libarchive's tar reader says it could not convert in the pax
/// header of `entry` (`header_pax_extension`): it converts the `path`,
/// `uname`, `gname` and `linkpath` attributes, in that order, from UTF-8
/// to the C locale's ASCII, unless the header says `hdrcharset=BINARY`.
/// A value that is not ASCII fails, is kept as its bytes, and sets the
/// archive's error, so the warning Nix prints names the last of them
/// (`Pathname`, `Uname`, `Gname` or `Linkpath`). `None` when every
/// conversion succeeds or there is no pax header; a global header (`g`)
/// is not converted. A `hdrcharset` other than `BINARY` and
/// `ISO-IR 10646 2000 UTF-8` leaves the charset as it was (libarchive
/// returns a warning with no message of its own for it, which brix does
/// not repeat).
fn pax_unconvertible<R: Read>(entry: &mut tar::Entry<'_, R>) -> Result<Option<&'static str>> {
    if entry.header().entry_type().is_pax_global_extensions() {
        return Ok(None);
    }
    let pax_err = |e: std::io::Error| err!("reading tar archive: {}", e);
    let Some(extensions) = entry.pax_extensions().map_err(pax_err)? else {
        return Ok(None);
    };
    let mut binary = false;
    // Whether each attribute fails, in libarchive's order; a later value
    // for the same key replaces an earlier one.
    let mut failed = [false; 4];
    for extension in extensions {
        let extension = extension.map_err(pax_err)?;
        let value = extension.value_bytes();
        let slot = match extension.key_bytes() {
            b"hdrcharset" => {
                if value == b"BINARY" {
                    binary = true;
                } else if value == b"ISO-IR 10646 2000 UTF-8" {
                    binary = false;
                }
                continue;
            }
            b"path" => 0,
            b"uname" => 1,
            b"gname" => 2,
            b"linkpath" => 3,
            _ => continue,
        };
        failed[slot] = !value.is_ascii();
    }
    if binary {
        return Ok(None);
    }
    Ok(["Pathname", "Uname", "Gname", "Linkpath"]
        .into_iter()
        .zip(failed)
        .filter(|&(_, fails)| fails)
        .map(|(attribute, _)| attribute)
        .next_back())
}

/// A zip archive, its DOS times read in `zone` (libarchive's local time).
fn unpack_zip(data: &[u8], zone: &TimeZone, warn: &dyn Fn(&str)) -> Result<Unpacked> {
    let mut archive =
        zip::ZipArchive::new(Cursor::new(data)).map_err(|e| err!("reading zip archive: {}", e))?;
    let mut tree = MemoryAccessor::new();
    let mut last_modified = 0u64;
    for i in 0..archive.len() {
        let mut file = archive
            .by_index(i)
            .map_err(|e| err!("reading zip archive: {}", e))?;
        let raw_name = zip_member_name(data, file.header_start(), warn)?;
        let shown = BStr::new(&raw_name);
        let path = CanonPath::new(&raw_name)
            .map_err(|e| err!("invalid archive member name '{}': {}", shown, e))?;
        if let Some(mtime) = zip_mtime(&file, zone) {
            last_modified = last_modified.max(mtime);
        }
        let mode = file.unix_mode().unwrap_or(0o644);
        if file.is_dir() {
            if !path.is_root() {
                tree.add_dir(&path)?;
            }
            continue;
        }
        let mut contents = Vec::with_capacity(file.size() as usize);
        file.read_to_end(&mut contents)
            .map_err(|e| err!("cannot read file from zip archive: {}", e))?;
        if mode & 0o170000 == 0o120000 {
            tree.add_symlink(&path, &contents)?;
        } else {
            tree.add_file(&path, contents, mode & 0o100 != 0)?;
        }
    }
    if tree.root().is_none() {
        tree.insert(&CanonPath::root(), MemoryNode::Directory(BTreeMap::new()))?;
    }
    Ok(Unpacked {
        tree,
        last_modified,
        stripped: None,
    })
}

/// General purpose flag bit 11: the member's name is UTF-8.
const ZIP_UTF8_NAME: u16 = 1 << 11;

/// The name libarchive gives the zip member whose local header is at
/// `offset` in `data` (`zip_read_local_file_header`, then
/// `process_extra`), or Nix's error when it gives none.
///
/// The name is the local header's name field, up to a NUL (the zip
/// crate's is the central directory's). With the UTF-8 flag, libarchive
/// converts it to the C locale, which fails for anything not ASCII: the
/// entry then has no name, and the error says why. An Info-ZIP Unicode
/// Path field (0x7075) whose CRC-32 is that of the name so far (or that
/// comes after a failed conversion, when there is no name to check) is
/// converted the same way: when ASCII it becomes the name, otherwise
/// the entry has no name (and no new error message). A name that the
/// field recovers after a failed conversion keeps the warning, which
/// goes to `warn`.
fn zip_member_name(data: &[u8], offset: u64, warn: &dyn Fn(&str)) -> Result<Vec<u8>> {
    let truncated = || err!("failed to extract archive (Truncated ZIP file header)");
    let header = usize::try_from(offset)
        .ok()
        .and_then(|offset| data.get(offset..))
        .filter(|header| header.len() >= 30)
        .ok_or_else(truncated)?;
    if !header.starts_with(b"PK\x03\x04") {
        return Err(err!("failed to extract archive (Damaged Zip archive)"));
    }
    let le16 = |at: usize| u16::from_le_bytes([header[at], header[at + 1]]);
    let flags = le16(6);
    let name_end = 30 + usize::from(le16(26));
    let extra_end = name_end + usize::from(le16(28));
    let name = header.get(30..name_end).ok_or_else(truncated)?;
    let extra = header.get(name_end..extra_end).ok_or_else(truncated)?;

    let mut error = "";
    let mut name = if flags & ZIP_UTF8_NAME != 0 && !name.is_ascii() {
        error = "Pathname cannot be converted from UTF-8 to current locale";
        None
    } else {
        Some(until_nul(name).to_vec())
    };
    for (id, field) in zip_extra_fields(extra)? {
        // `datasize < 5` is skipped, as is a field whose CRC-32 is not
        // the name's.
        if id != 0x7075 || field.len() < 5 {
            continue;
        }
        let crc = u32::from_le_bytes([field[1], field[2], field[3], field[4]]);
        if name.as_ref().is_some_and(|name| crc32(name) != crc) {
            continue;
        }
        let unicode = &field[5..];
        name = unicode.is_ascii().then(|| until_nul(unicode).to_vec());
    }
    match name {
        None => Err(err!("cannot get archive member name: {}", error)),
        Some(name) => {
            if !error.is_empty() {
                warn(&format!(
                    "getting archive member '{}': {}",
                    BStr::new(&name),
                    error
                ));
            }
            Ok(name)
        }
    }
}

/// The fields of a zip extra field block, as libarchive's
/// `process_extra` walks them: an id and a length each; a block too short
/// for a field header must be zeros, and a field longer than what is left
/// is an error (both Nix's `failed to extract archive (…)`).
fn zip_extra_fields(mut extra: &[u8]) -> Result<Vec<(u16, &[u8])>> {
    if !extra.is_empty() && extra.len() < 4 {
        if extra.iter().any(|&b| b != 0) {
            return Err(err!(
                "failed to extract archive (Too-small extra data: Need at least 4 bytes, but only found {} bytes)",
                extra.len()
            ));
        }
        return Ok(Vec::new());
    }
    let mut fields = Vec::new();
    while extra.len() >= 4 {
        let id = u16::from_le_bytes([extra[0], extra[1]]);
        let len = usize::from(u16::from_le_bytes([extra[2], extra[3]]));
        let Some(field) = extra.get(4..4 + len) else {
            return Err(err!(
                "failed to extract archive (Extra data field contents exceed extra data field length)"
            ));
        };
        fields.push((id, field));
        extra = &extra[4 + len..];
    }
    Ok(fields)
}

/// `bytes` up to its first NUL: what libarchive copies of a C string.
fn until_nul(bytes: &[u8]) -> &[u8] {
    match bytes.iter().position(|&b| b == 0) {
        Some(nul) => &bytes[..nul],
        None => bytes,
    }
}

/// The CRC-32 zip uses.
fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = flate2::Crc::new();
    crc.update(bytes);
    crc.sum()
}

/// A zip member's modification time as libarchive reads it: the DOS time
/// of its header, overwritten by each NTFS or extended (`UT`) timestamp in
/// its extra fields in turn. The DOS time has no time zone; libarchive
/// converts it as local time (`dos_to_unix`, which calls `mktime`), and
/// brix as time in `zone`, the system's.
fn zip_mtime<R: Read>(file: &zip::read::ZipFile<'_, R>, zone: &TimeZone) -> Option<u64> {
    use zip::extra_fields::ExtraField;
    let mut mtime = file.last_modified().map(|dt| dos_time_to_unix(&dt, zone));
    for field in file.extra_data_fields() {
        match field {
            ExtraField::ExtendedTimestamp(ts) => {
                if let Some(t) = ts.mod_time() {
                    mtime = Some(u64::from(t));
                }
            }
            // `ntfs_to_unix`: 100 ns ticks since 1601.
            ExtraField::Ntfs(ntfs) => {
                let secs = (ntfs.mtime() as i64 - 116_444_736_000_000_000) / 10_000_000;
                mtime = Some(secs.max(0) as u64);
            }
        }
    }
    mtime
}

/// `dos_to_unix`: `mktime` of the DOS date and time in `zone`. (A DOS
/// time that is no valid date, which `mktime` would normalise, never gets
/// here: the zip crate drops it.)
fn dos_time_to_unix(dt: &zip::DateTime, zone: &TimeZone) -> u64 {
    let local = jiff::civil::DateTime::new(
        dt.year() as i16,
        dt.month() as i8,
        dt.day() as i8,
        dt.hour() as i8,
        dt.minute() as i8,
        dt.second() as i8,
        0,
    )
    .expect("the zip crate reads only valid DOS dates and times");
    let seconds = local
        .to_zoned(zone.clone())
        .expect("DOS times (1980 to 2107) are within jiff's range")
        .timestamp()
        .as_second();
    u64::try_from(seconds).expect("DOS times are after 1980")
}

/// `TarballCache::dereferenceSingletonDirectory`: if the root holds
/// exactly one entry and it is a directory, that directory becomes the
/// root.
pub fn dereference_singleton_directory(tree: &mut MemoryAccessor) -> Option<Vec<u8>> {
    let singleton = match tree.root() {
        Some(MemoryNode::Directory(entries)) if entries.len() == 1 => match entries.iter().next() {
            Some((name, MemoryNode::Directory(inner))) => {
                Some((AsRef::<[u8]>::as_ref(name).to_vec(), inner.clone()))
            }
            _ => None,
        },
        _ => None,
    };
    let (name, inner) = singleton?;
    *tree = MemoryAccessor::from_root(MemoryNode::Directory(inner));
    Some(name)
}

/// `TarballInputScheme::getAccessor`.
pub fn fetch_tarball(
    http: &HttpClient,
    settings: &FetchSettings,
    input: &Input,
) -> Result<FetchedTree> {
    let mut input = input.clone();
    let url = get_str_attr(&input.attrs, "url")?.to_string();
    let parsed = check_local_tarball_path(&url, settings)?;
    // `downloadTarball_`'s activity, which covers the download too.
    (settings.info)(&format!("unpacking '{parsed}' into the Git cache..."));
    let name_hint = parsed.path.last().map_or("", String::as_str);
    let expected = input.nar_hash()?;
    let DownloadedArchive {
        download,
        unpacked,
        members,
    } = download_archive(
        http,
        &url,
        &[],
        settings,
        name_hint,
        expected.as_ref(),
        &|e| e,
    )?;

    if let Some(immutable) = &download.immutable_url {
        let immutable_input = Input::from_url_str(settings, immutable, true)?;
        if immutable_input.scheme != Some(Scheme::Tarball) {
            return Err(err!(
                "tarball 'Link' headers that redirect to non-tarball URLs are not supported"
            ));
        }
        input = immutable_input;
    }

    if unpacked.last_modified != 0 && !input.attrs.contains_key("lastModified") {
        input
            .attrs
            .insert("lastModified".into(), Attr::Int(unpacked.last_modified));
    }
    let mut tree = unpacked.tree;
    tree.set_display_prefix(format!("«{}»", input.to_url_text(true)?));
    let nar_hash = crate::fetch::tree_nar_hash(&tree)?;
    input
        .attrs
        .insert("narHash".into(), Attr::String(nar_hash.to_sri()));
    let accessor: Arc<dyn brix_store::SourceAccessor> = Arc::new(tree);
    if let Some(members) = members {
        members.finish(&nar_hash, unpacked.stripped.as_deref(), accessor.clone());
    }
    Ok(FetchedTree {
        accessor,
        input,
        verified: false,
    })
}

/// `downloadTarball_`: the URL, parsed (strictly, as Nix does), and the
/// friendly errors it gives for local paths that are not tarballs.
fn check_local_tarball_path(url: &str, settings: &FetchSettings) -> Result<ParsedUrl> {
    let parsed = parse_url(url, false, &*settings.warn)?;
    if parsed.scheme != "file" {
        return Ok(parsed);
    }
    let path = url_path_to_path(&parsed.path)?;
    if !path.starts_with('/') {
        return Err(err!(
            "tarball '{}' must use an absolute path. The 'file' scheme does not support relative paths.",
            parsed
        ));
    }
    // `PathFmt` quotes with double quotes.
    let p = std::path::Path::new(&path);
    if !p.exists() {
        return Err(err!("tarball \"{}\" does not exist.", path));
    }
    if p.is_dir() {
        if p.join(".git").exists() {
            return Err(err!(
                "tarball \"{}\" is a git repository, not a tarball. Please use `git+file` as the scheme.",
                path
            ));
        }
        return Err(err!("tarball \"{}\" is a directory, not a file.", path));
    }
    Ok(parsed)
}

/// `FileInputScheme::getAccessor`: a single file, whose tree is just that
/// file at the root.
pub fn fetch_file(
    http: &HttpClient,
    settings: &FetchSettings,
    input: &Input,
) -> Result<FetchedTree> {
    let mut input = input.clone();
    let url = get_str_attr(&input.attrs, "url")?.to_string();
    let download = http.download(&url, &[], settings)?;
    let mut tree = MemoryAccessor::from_root(MemoryNode::Regular {
        executable: false,
        contents: download.data,
    });
    tree.set_display_prefix(format!("«{}»", input.to_url_text(true)?));
    let nar_hash = crate::fetch::tree_nar_hash(&tree)?;
    input
        .attrs
        .insert("narHash".into(), Attr::String(nar_hash.to_sri()));
    Ok(FetchedTree {
        accessor: Arc::new(tree),
        input,
        verified: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use brix_store::{BString, SourceAccessor};
    use brix_testutil::{zip_unicode_path_field, TarBuilder, ZipBuilder, ZIP_UTF8_FLAG};
    use std::cell::RefCell;
    use std::io::Write;

    fn no_warnings(msg: &str) {
        panic!("unexpected warning: {msg}");
    }

    /// `unpack_archive` with the warnings it gave.
    fn unpack_warning(data: &[u8], name_hint: &str) -> (Result<Unpacked>, Vec<String>) {
        let warnings = RefCell::new(Vec::new());
        let unpacked = unpack_archive(data, name_hint, &|msg: &str| {
            warnings.borrow_mut().push(msg.to_owned())
        });
        (unpacked, warnings.into_inner())
    }

    fn p(path: &[u8]) -> CanonPath {
        CanonPath::new(path).unwrap()
    }

    /// The names at the root, in the tree's (unsigned byte) order.
    fn root_names(tree: &MemoryAccessor) -> Vec<BString> {
        tree.read_dir(&CanonPath::root())
            .unwrap()
            .into_keys()
            .collect()
    }

    fn make_tar(entries: &[(&str, &[u8], u32, u64)]) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        for (name, contents, mode, mtime) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_size(contents.len() as u64);
            header.set_mode(*mode);
            header.set_mtime(*mtime);
            header.set_entry_type(if name.ends_with('/') {
                tar::EntryType::Directory
            } else {
                tar::EntryType::Regular
            });
            header.set_cksum();
            builder.append_data(&mut header, name, *contents).unwrap();
        }
        builder.into_inner().unwrap()
    }

    /// The regular files an archive's members were shown as.
    #[derive(Default)]
    struct Seen(Vec<(Vec<u8>, Vec<u8>)>);

    impl ArchiveMembers for Seen {
        fn file(&mut self, path: &CanonPath, contents: &[u8]) {
            self.0.push((path.rel().to_vec(), contents.to_vec()));
        }

        fn finish(
            self: Box<Self>,
            _: &NarHash,
            _: Option<&[u8]>,
            _: Arc<dyn brix_store::SourceAccessor>,
        ) {
        }
    }

    /// A reader that hands out one byte at a time, as a slow connection
    /// might.
    struct Trickle<'a>(&'a [u8]);

    impl Read for Trickle<'_> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let n = self.0.len().min(buf.len()).min(1);
            buf[..n].copy_from_slice(&self.0[..n]);
            self.0 = &self.0[n..];
            Ok(n)
        }
    }

    fn gzip(data: &[u8]) -> Vec<u8> {
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(data).unwrap();
        gz.finish().unwrap()
    }

    /// An archive unpacked as it arrives is the tree the whole archive
    /// unpacks to, and its regular files are shown in the archive's order.
    #[test]
    fn streamed_unpacking_matches_buffered() {
        let tar = make_tar(&[
            ("pkg-1.0/", b"", 0o755, 100),
            ("pkg-1.0/default.nix", b"import ./lib\n", 0o644, 200),
            ("pkg-1.0/lib/default.nix", b"{ }\n", 0o644, 300),
            ("pkg-1.0/bin/run", b"#!/bin/sh\n", 0o755, 150),
            // A later member replaces an earlier one.
            (
                "pkg-1.0/default.nix",
                b"import ./lib/default.nix\n",
                0o644,
                250,
            ),
        ]);
        for (name, archive) in [("pkg.tar.gz", gzip(&tar)), ("pkg.tar", tar.clone())] {
            let whole = unpack_archive(&archive, name, &no_warnings).unwrap();
            for trickle in [false, true] {
                let mut seen = Seen::default();
                let streamed = if trickle {
                    unpack_stream(&mut Trickle(&archive), name, &no_warnings, Some(&mut seen))
                } else {
                    unpack_stream(&mut &archive[..], name, &no_warnings, Some(&mut seen))
                }
                .expect("unpacks as it arrives");
                assert_eq!(
                    crate::fetch::tree_nar_hash(&streamed.tree).unwrap(),
                    crate::fetch::tree_nar_hash(&whole.tree).unwrap()
                );
                assert_eq!(streamed.last_modified, whole.last_modified);
                assert_eq!(streamed.stripped.as_deref(), Some(&b"pkg-1.0"[..]));
                assert_eq!(whole.stripped.as_deref(), Some(&b"pkg-1.0"[..]));
                let paths: Vec<&[u8]> = seen.0.iter().map(|(path, _)| &path[..]).collect();
                assert_eq!(
                    paths,
                    [
                        &b"pkg-1.0/default.nix"[..],
                        b"pkg-1.0/lib/default.nix",
                        b"pkg-1.0/bin/run",
                        b"pkg-1.0/default.nix",
                    ]
                );
                assert_eq!(seen.0[3].1, b"import ./lib/default.nix\n");
            }
        }
    }

    /// What cannot be unpacked as it arrives is left to `unpack_archive`,
    /// which says what is wrong with it: a zip archive, a damaged one, and
    /// one with damage after its end, which the tar reader never reads.
    #[test]
    fn streamed_unpacking_declines_what_the_whole_archive_decides() {
        let tar = make_tar(&[("pkg-1.0/default.nix", b"1\n", 0o644, 100)]);
        let gz = gzip(&tar);
        let streams = |archive: &[u8], name: &str| {
            unpack_stream(&mut &archive[..], name, &|_| {}, None).is_some()
        };
        assert!(streams(&gz, "pkg.tar.gz"));

        let truncated = &gz[..gz.len() / 2];
        assert!(!streams(truncated, "pkg.tar.gz"));
        assert!(unpack_archive(truncated, "pkg.tar.gz", &|_| {}).is_err());

        // A second gzip member that is cut short follows the archive.
        let mut trailing = gz.clone();
        trailing.extend_from_slice(&gz[..gz.len() - 4]);
        assert!(!streams(&trailing, "pkg.tar.gz"));
        assert!(unpack_archive(&trailing, "pkg.tar.gz", &|_| {}).is_err());

        let zip = ZipBuilder::new()
            .member(b"pkg/", b"", 0o40755, 0)
            .member(b"pkg/default.nix", b"1\n", 0o100644, 0)
            .finish();
        assert!(!streams(&zip, "pkg.zip"));
        assert!(unpack_archive(&zip, "pkg.zip", &|_| {}).is_ok());
    }

    #[test]
    fn unpacks_and_dereferences_singleton_directory() {
        let tar = make_tar(&[
            ("pkg-1.0/", b"", 0o755, 100),
            ("pkg-1.0/README", b"hello\n", 0o644, 200),
            ("pkg-1.0/bin/run", b"#!/bin/sh\n", 0o755, 150),
        ]);
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(&tar).unwrap();
        let gz = gz.finish().unwrap();
        let unpacked = unpack_archive(&gz, "pkg.tar.gz", &no_warnings).unwrap();
        assert_eq!(unpacked.last_modified, 200);
        let readme = unpacked
            .tree
            .lookup(&CanonPath::new("/README").unwrap())
            .unwrap();
        assert!(matches!(
            readme,
            MemoryNode::Regular {
                executable: false,
                ..
            }
        ));
        let run = unpacked
            .tree
            .lookup(&CanonPath::new("/bin/run").unwrap())
            .unwrap();
        assert!(matches!(
            run,
            MemoryNode::Regular {
                executable: true,
                ..
            }
        ));
    }

    #[test]
    fn multiple_top_level_entries_are_kept() {
        let tar = make_tar(&[("a", b"1", 0o644, 1), ("b", b"2", 0o644, 2)]);
        let unpacked = unpack_archive(&tar, "x.tar", &no_warnings).unwrap();
        assert!(unpacked
            .tree
            .lookup(&CanonPath::new("/a").unwrap())
            .is_some());
        assert!(unpacked
            .tree
            .lookup(&CanonPath::new("/b").unwrap())
            .is_some());
    }

    #[test]
    fn zip_archives() {
        let mut buf = Cursor::new(Vec::new());
        {
            let mut zip = zip::ZipWriter::new(&mut buf);
            let opts = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated)
                .unix_permissions(0o755);
            zip.start_file("top/script", opts).unwrap();
            zip.write_all(b"echo").unwrap();
            zip.finish().unwrap();
        }
        let unpacked = unpack_archive(&buf.into_inner(), "x.zip", &no_warnings).unwrap();
        let script = unpacked
            .tree
            .lookup(&CanonPath::new("/script").unwrap())
            .unwrap();
        assert!(matches!(
            script,
            MemoryNode::Regular {
                executable: true,
                ..
            }
        ));
    }

    /// A zip whose single member has the DOS time 2001-02-03 04:05:06 and,
    /// if `ut` is given, an extended timestamp (`UT`) with that time.
    fn zip_with_times(ut: Option<u32>) -> Vec<u8> {
        let mut buf = Cursor::new(Vec::new());
        {
            let mut zip = zip::ZipWriter::new(&mut buf);
            let mut opts = zip::write::FullFileOptions::default().last_modified_time(
                zip::DateTime::from_date_and_time(2001, 2, 3, 4, 5, 6).unwrap(),
            );
            if let Some(t) = ut {
                let mut data = vec![1u8];
                data.extend_from_slice(&t.to_le_bytes());
                opts.add_extra_data(0x5455, data.into_boxed_slice(), false)
                    .unwrap();
            }
            zip.start_file("f", opts).unwrap();
            zip.write_all(b"x").unwrap();
            zip.finish().unwrap();
        }
        buf.into_inner()
    }

    /// The DOS time is local time: 2001-02-03 04:05:06 is 981173106 in
    /// UTC and five hours later in New York (EST), as `nix` reports the
    /// zip's `lastModified` with `TZ=UTC` and `TZ=EST5`.
    #[test]
    fn zip_times_are_local_and_prefer_the_extended_timestamp() {
        let utc = TimeZone::UTC;
        let est = TimeZone::fixed(jiff::tz::offset(-5));
        let dos = zip_with_times(None);
        assert_eq!(
            unpack_zip(&dos, &utc, &no_warnings).unwrap().last_modified,
            981173106
        );
        assert_eq!(
            unpack_zip(&dos, &est, &no_warnings).unwrap().last_modified,
            981173106 + 5 * 3600
        );
        let ut = zip_with_times(Some(1_700_000_000));
        assert_eq!(
            unpack_zip(&ut, &est, &no_warnings).unwrap().last_modified,
            1_700_000_000
        );
        // `unpack_archive` reads it in the system's time zone.
        let local = jiff::civil::date(2001, 2, 3)
            .at(4, 5, 6, 0)
            .to_zoned(TimeZone::system())
            .unwrap()
            .timestamp()
            .as_second();
        assert_eq!(
            unpack_archive(&dos, "x.zip", &no_warnings)
                .unwrap()
                .last_modified,
            local as u64
        );
    }

    /// Names, link targets and hard link targets of a ustar archive are
    /// bytes, unchecked; the tree lists them in unsigned byte order. The
    /// NAR hash is what `nix flake prefetch --json tarball+file://…`
    /// reports for the same members.
    #[test]
    fn ustar_names_are_bytes() {
        let tar = TarBuilder::new()
            .dir(b"top/")
            .file(b"top/caf\xe9", b"1", false)
            .file(b"top/z", b"2", false)
            .file(b"top/\xff", b"3", false)
            .file(b"top/a\x80b", b"4", false)
            .dir(b"top/d\xe9/")
            .file(b"top/d\xe9/f", b"5", false)
            .symlink(b"top/l\xff", b"t\xe9")
            .hardlink(b"top/h\xe9", b"top/caf\xe9")
            .finish();
        let unpacked = unpack_archive(&tar, "x.tar", &no_warnings).unwrap();
        let tree = &unpacked.tree;
        assert_eq!(
            root_names(tree),
            [
                &b"a\x80b"[..],
                b"caf\xe9",
                b"d\xe9",
                b"h\xe9",
                b"l\xff",
                b"z",
                b"\xff"
            ]
        );
        assert_eq!(tree.read_link(&p(b"/l\xff")).unwrap(), b"t\xe9");
        assert_eq!(tree.read_file(&p(b"/h\xe9")).unwrap(), b"1");
        assert_eq!(tree.read_file(&p(b"/d\xe9/f")).unwrap(), b"5");
        assert_eq!(
            crate::fetch::tree_nar_hash(tree).unwrap().to_sri(),
            "sha256-3yz6omTE6VYttcenT6zh9wbFk7zr8Kh3WAh0hFoB22s="
        );
    }

    /// GNU long names and long link targets are bytes too.
    #[test]
    fn gnu_long_names_are_bytes() {
        let mut long = b"top/".to_vec();
        long.extend_from_slice(&[b'x'; 113]);
        long.push(0xe9);
        let tar = TarBuilder::new()
            .dir(b"top/")
            .gnu_long_name(&long)
            .file(&long, b"6", false)
            .gnu_long_link(b"t\xe9")
            .symlink(b"top/l2", b"trunc")
            .finish();
        let unpacked = unpack_archive(&tar, "x.tar", &no_warnings).unwrap();
        assert_eq!(
            unpacked.tree.read_file(&p(&long[3..])).unwrap(),
            b"6",
            "the member is at its long name"
        );
        assert_eq!(unpacked.tree.read_link(&p(b"/l2")).unwrap(), b"t\xe9");
        assert_eq!(
            crate::fetch::tree_nar_hash(&unpacked.tree)
                .unwrap()
                .to_sri(),
            "sha256-Ez225XeSjeUm2RnQpy0nUCLpyV4cEUu7k5qxLR5zBN0="
        );
    }

    /// pax names are UTF-8 to libarchive: one that is not ASCII cannot be
    /// converted to Nix's C locale, is kept as its bytes, and is warned
    /// about, naming the last attribute that failed; `hdrcharset=BINARY`
    /// keeps the bytes without a word. The warnings and NAR hashes are
    /// those of `nix flake prefetch` on the same archives.
    #[test]
    fn pax_names_are_kept_and_warned_about_like_libarchive() {
        let cases: [(TarBuilder, &[u8], &str, &[&str]); 5] = [
            (
                {
                    let mut tar = TarBuilder::new();
                    tar.dir(b"top/")
                        .pax(&[("path", b"top/caf\xc3\xa9")])
                        .file(b"top/placeholder", b"7", false);
                    tar
                },
                b"/caf\xc3\xa9",
                "sha256-o4JvcWS7thfrr/BYZmukTjiLhQLf7QaNecmJSHxJr60=",
                &["getting archive member 'top/caf\u{e9}': Pathname can't be converted from UTF-8 to current locale"],
            ),
            (
                {
                    let mut tar = TarBuilder::new();
                    tar.dir(b"top/")
                        .pax(&[("hdrcharset", b"BINARY"), ("path", b"top/caf\xe9")])
                        .file(b"top/placeholder", b"8", false);
                    tar
                },
                b"/caf\xe9",
                "sha256-3DjMOFtDLAa4Oup5wuO/om55v7JfIqeRErgJgHFNDpI=",
                &[],
            ),
            // Nix's warning has the byte 0xe9 where brix's text has
            // U+FFFD: fetcher messages are text (KNOWN_DEVIATIONS).
            (
                {
                    let mut tar = TarBuilder::new();
                    tar.dir(b"top/")
                        .pax(&[("path", b"top/caf\xe9")])
                        .file(b"top/placeholder", b"9", false);
                    tar
                },
                b"/caf\xe9",
                "sha256-fAsgvD/HEKlwDOfUdDxkM1HPS2GNoFk7cXx+J7KZ4sk=",
                &["getting archive member 'top/caf\u{fffd}': Pathname can't be converted from UTF-8 to current locale"],
            ),
            (
                {
                    let mut tar = TarBuilder::new();
                    tar.dir(b"top/")
                        .pax(&[("linkpath", b"t\xc3\xa9")])
                        .symlink(b"top/l", b"x");
                    tar
                },
                b"/l",
                "sha256-34t44gj2/cUSicMVdqWzoNAhSsKB6+ilh3xQ8Q9Et3k=",
                &["getting archive member 'top/l': Linkpath can't be converted from UTF-8 to current locale"],
            ),
            (
                {
                    let mut tar = TarBuilder::new();
                    tar.dir(b"top/")
                        .pax(&[("uname", b"jos\xc3\xa9"), ("path", b"top/caf\xc3\xa9")])
                        .file(b"top/placeholder", b"7", false)
                        .pax(&[("gname", b"gr\xc3\xa9"), ("linkpath", b"t\xc3\xa9")])
                        .symlink(b"top/l", b"x")
                        .pax(&[("uname", b"jos\xc3\xa9")])
                        .file(b"top/u", b"8", false);
                    tar
                },
                b"/l",
                "sha256-1Z2+qst6g84OXr29GcO1QHYlRgd/mEU67OdGMH3Xz6c=",
                &[
                    "getting archive member 'top/caf\u{e9}': Uname can't be converted from UTF-8 to current locale",
                    "getting archive member 'top/l': Linkpath can't be converted from UTF-8 to current locale",
                    "getting archive member 'top/u': Uname can't be converted from UTF-8 to current locale",
                ],
            ),
        ];
        for (tar, present, nar_hash, warnings) in cases {
            let (unpacked, given) = unpack_warning(&tar.finish(), "x.tar");
            let unpacked = unpacked.unwrap();
            assert!(
                unpacked.tree.path_exists(&p(present)).unwrap(),
                "{nar_hash}"
            );
            assert_eq!(given, warnings, "{nar_hash}");
            assert_eq!(
                crate::fetch::tree_nar_hash(&unpacked.tree)
                    .unwrap()
                    .to_sri(),
                nar_hash
            );
        }
    }

    /// A zip member's name is its bytes (no CP437 decoding), and so is a
    /// symlink's target, UTF-8 flag or not.
    #[test]
    fn zip_names_and_targets_are_bytes() {
        let zip = ZipBuilder::new()
            .member(b"top/", b"", 0o40755, 0)
            .member(b"top/caf\x82", b"1", 0o100644, 0)
            .member(b"top/z", b"2", 0o100755, 0)
            .member(b"top/l", b"t\xe9", 0o120777, 0)
            .member(b"top/m", b"t\xc3\xa9", 0o120777, ZIP_UTF8_FLAG)
            .member(b"top/f", b"3", 0o100644, ZIP_UTF8_FLAG)
            .finish();
        let unpacked = unpack_archive(&zip, "x.zip", &no_warnings).unwrap();
        let tree = &unpacked.tree;
        assert_eq!(root_names(tree), [&b"caf\x82"[..], b"f", b"l", b"m", b"z"]);
        assert_eq!(tree.read_link(&p(b"/l")).unwrap(), b"t\xe9");
        assert_eq!(tree.read_link(&p(b"/m")).unwrap(), b"t\xc3\xa9");
    }

    /// With the UTF-8 flag, a name that is not ASCII cannot be converted
    /// to Nix's C locale and the member has no name: the error `nix`
    /// gives for the same zip (`builtins.fetchTarball`).
    #[test]
    fn a_flagged_zip_name_that_is_not_ascii_has_no_name() {
        let zip = ZipBuilder::new()
            .member(b"top/", b"", 0o40755, ZIP_UTF8_FLAG)
            .member(b"top/caf\xc3\xa9", b"1", 0o100644, ZIP_UTF8_FLAG)
            .finish();
        let err = unpack_archive(&zip, "x.zip", &no_warnings).err().unwrap();
        assert_eq!(
            err.to_string(),
            "cannot get archive member name: Pathname cannot be converted from UTF-8 to current locale"
        );
    }

    /// An Info-ZIP Unicode Path field replaces the name when it is ASCII
    /// and its CRC-32 is the name's (or there is no name to check after a
    /// failed conversion, which leaves a warning), and leaves the member
    /// without a name when it is not ASCII, with no message of its own:
    /// what `nix` makes of the same zips.
    #[test]
    fn unicode_path_fields_are_converted_like_names() {
        let field = zip_unicode_path_field(b"top/caf?", b"top/cafe");
        let zip = ZipBuilder::new()
            .member(b"top/", b"", 0o40755, 0)
            .member_with_extra(b"top/caf?", b"1", 0o100644, 0, &field, &field)
            .finish();
        let unpacked = unpack_archive(&zip, "x.zip", &no_warnings).unwrap();
        assert_eq!(root_names(&unpacked.tree), [&b"cafe"[..]]);

        let field = zip_unicode_path_field(b"top/caf?", b"top/caf\xc3\xa9");
        let zip = ZipBuilder::new()
            .member(b"top/", b"", 0o40755, 0)
            .member_with_extra(b"top/caf?", b"1", 0o100644, 0, &field, &field)
            .finish();
        let err = unpack_archive(&zip, "x.zip", &no_warnings).err().unwrap();
        assert_eq!(err.to_string(), "cannot get archive member name: ");

        // The flagged name fails; the field (in the local header only, as
        // the zip crate checks the CRC-32 of a central one) is not
        // checked against it.
        let field = zip_unicode_path_field(b"whatever", b"top/cafe");
        let zip = ZipBuilder::new()
            .member(b"top/", b"", 0o40755, 0)
            .member_with_extra(
                b"top/caf\xc3\xa9",
                b"1",
                0o100644,
                ZIP_UTF8_FLAG,
                &field,
                b"",
            )
            .finish();
        let (unpacked, warnings) = unpack_warning(&zip, "x.zip");
        assert_eq!(root_names(&unpacked.unwrap().tree), [&b"cafe"[..]]);
        assert_eq!(
            warnings,
            ["getting archive member 'top/cafe': Pathname cannot be converted from UTF-8 to current locale"]
        );

        // A CRC-32 that is not the name's: the field is passed over.
        let field = zip_unicode_path_field(b"nope", b"top/cafe");
        let zip = ZipBuilder::new()
            .member(b"top/", b"", 0o40755, 0)
            .member_with_extra(b"top/caf?", b"1", 0o100644, 0, &field, b"")
            .finish();
        let unpacked = unpack_archive(&zip, "x.zip", &no_warnings).unwrap();
        assert_eq!(root_names(&unpacked.tree), [&b"caf?"[..]]);
    }

    /// libarchive's errors for a malformed extra field in a local header.
    #[test]
    fn malformed_zip_extra_fields_are_errors() {
        assert!(zip_extra_fields(b"\0\0").unwrap().is_empty());
        assert_eq!(
            zip_extra_fields(b"\x01\0").unwrap_err().to_string(),
            "failed to extract archive (Too-small extra data: Need at least 4 bytes, but only found 2 bytes)"
        );
        assert_eq!(
            zip_extra_fields(b"\x75\x70\x09\0abc")
                .unwrap_err()
                .to_string(),
            "failed to extract archive (Extra data field contents exceed extra data field length)"
        );
        assert_eq!(
            zip_extra_fields(b"\x55\x54\x01\0\x00\x75\x70\0\0\x01").unwrap(),
            [(0x5455, &b"\x00"[..]), (0x7075, &b""[..])],
            "a trailing piece too short for a field is ignored"
        );
    }

    #[test]
    fn broken_headers_are_errors() {
        let mut tar = make_tar(&[("a", b"1", 0o644, 1)]);
        // The mtime field (12 octal digits at offset 136) made unreadable.
        tar[136..148].copy_from_slice(b"zzzzzzzzzzz\0");
        let mut header = tar::Header::new_old();
        header.as_mut_bytes().copy_from_slice(&tar[..512]);
        header.set_cksum();
        tar[..512].copy_from_slice(header.as_bytes());
        let err = unpack_archive(&tar, "x.tar", &no_warnings).err().unwrap();
        assert!(
            err.to_string().starts_with("reading the header of 'a': "),
            "{err}"
        );
    }
}
