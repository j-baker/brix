//! File names of the operating system as bytes, and back.
//!
//! Nix's paths are bytes (`std::string`), and so are brix's
//! ([`crate::CanonPath`]). On Unix a file name is any bytes but `/` and
//! NUL, so every path converts both ways unchanged. Elsewhere the
//! operating system's names are text to Rust (`OsStr` is WTF-8 on
//! Windows), and only UTF-8 bytes convert: the functions return `None`
//! for the rest, and the caller says what it cannot do.
//!
//! Nothing here touches the file system, so the module is compiled for
//! every target, wasm included.

use std::path::{Path, PathBuf};

/// `bytes` as a path of the operating system: on Unix always, elsewhere
/// when they are UTF-8.
#[cfg(unix)]
pub fn to_path(bytes: &[u8]) -> Option<&Path> {
    use std::os::unix::ffi::OsStrExt;
    Some(Path::new(std::ffi::OsStr::from_bytes(bytes)))
}

/// `bytes` as a path of the operating system: on Unix always, elsewhere
/// when they are UTF-8.
#[cfg(not(unix))]
pub fn to_path(bytes: &[u8]) -> Option<&Path> {
    std::str::from_utf8(bytes).ok().map(Path::new)
}

/// [`to_path`] for owned bytes, without copying them.
#[cfg(unix)]
pub fn to_path_buf(bytes: Vec<u8>) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStringExt;
    Some(PathBuf::from(std::ffi::OsString::from_vec(bytes)))
}

/// [`to_path`] for owned bytes, without copying them.
#[cfg(not(unix))]
pub fn to_path_buf(bytes: Vec<u8>) -> Option<PathBuf> {
    String::from_utf8(bytes).ok().map(PathBuf::from)
}

/// The bytes of a path of the operating system: on Unix always,
/// elsewhere when it is valid Unicode (as UTF-8).
#[cfg(unix)]
pub fn path_bytes(path: &Path) -> Option<&[u8]> {
    use std::os::unix::ffi::OsStrExt;
    Some(path.as_os_str().as_bytes())
}

/// The bytes of a path of the operating system: on Unix always,
/// elsewhere when it is valid Unicode (as UTF-8).
#[cfg(not(unix))]
pub fn path_bytes(path: &Path) -> Option<&[u8]> {
    path.to_str().map(str::as_bytes)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn unix_paths_are_any_bytes() {
        let name = b"/d\xe9/caf\xe9";
        let path = to_path(name).unwrap();
        assert_eq!(path_bytes(path), Some(&name[..]));
        assert_eq!(
            path.file_name().map(path_bytes_of_os),
            Some(&b"caf\xe9"[..])
        );
        let owned = to_path_buf(name.to_vec()).unwrap();
        assert_eq!(path_bytes(&owned), Some(&name[..]));
        assert_eq!(owned.parent(), to_path(b"/d\xe9"));
    }

    fn path_bytes_of_os(name: &std::ffi::OsStr) -> &[u8] {
        path_bytes(Path::new(name)).unwrap()
    }
}
