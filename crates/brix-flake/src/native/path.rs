//! `path:` inputs (`src/libfetchers/path.cc`): a directory on the local
//! file system, read through an [`FsAccessor`] rooted at it.

use std::sync::Arc;

use brix_store::{CanonPath, FsAccessor};

use crate::attrs::get_str_attr;
use crate::fetch::FetchedTree;
use crate::input::Input;
use crate::{err, Result};

/// `PathInputScheme::getAbsPath`: the path must be absolute (relative
/// `path:` inputs are resolved by the flake locking code, never fetched).
pub fn abs_path(input: &Input) -> Result<String> {
    let path = get_str_attr(&input.attrs, "path")?;
    if path.starts_with('/') {
        // `canonPath`, without resolving symlinks.
        Ok(CanonPath::new(path)
            .map_err(|e| err!("{}", e))?
            .to_str()
            .expect("canonicalizing a textual path preserves UTF-8")
            .to_owned())
    } else {
        Err(err!(
            "cannot fetch input '{}' because it uses a relative path",
            input
        ))
    }
}

/// `SysError`'s text for an I/O error: the C library's message, without
/// Rust's ` (os error N)`.
pub(crate) fn sys_error_text(e: &std::io::Error) -> String {
    let text = e.to_string();
    match e.raw_os_error() {
        Some(code) => match text.strip_suffix(&format!(" (os error {code})")) {
            Some(message) => message.to_owned(),
            None => text,
        },
        None => text,
    }
}

/// `PathInputScheme::getAccessor`.
pub fn fetch(input: &Input) -> Result<FetchedTree> {
    let path = abs_path(input)?;
    // Nix opens the directory when it first reads the tree.
    std::fs::metadata(&path)
        .map_err(|e| err!("opening file \"{}\": {}", path, sys_error_text(&e)))?;
    Ok(FetchedTree {
        accessor: Arc::new(FsAccessor::new(path)),
        input: input.clone(),
        verified: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path_input(path: &str) -> Input {
        Input::from_attrs(
            &crate::FetchSettings::default(),
            crate::attrs::attrs([("type", "path".into()), ("path", path.into())]),
        )
        .unwrap()
    }

    #[test]
    fn canonicalises() {
        assert_eq!(abs_path(&path_input("/a//b/./c/../d/")).unwrap(), "/a/b/d");
        assert_eq!(abs_path(&path_input("/")).unwrap(), "/");
        assert_eq!(
            abs_path(&path_input("a/b")).unwrap_err().to_string(),
            "cannot fetch input 'path:a/b' because it uses a relative path"
        );
    }

    #[test]
    fn a_missing_path_fails_like_nix() {
        let err = fetch(&path_input("/nonexistent/brix-test")).err().unwrap();
        assert_eq!(
            err.to_string(),
            "opening file \"/nonexistent/brix-test\": No such file or directory"
        );
    }
}
