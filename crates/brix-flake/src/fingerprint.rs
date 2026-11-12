//! Flake fingerprints (`LockedFlake::getFingerprint` in
//! `src/libflake/flake.cc`), which `nix flake metadata` prints and Nix's
//! evaluation cache is keyed by.
//!
//! A fingerprint is the SHA-256 of the root input's own fingerprint
//! (`Input::getFingerprint`, which a fetcher takes from the tree it
//! produced), the flake's subdirectory, the lock file and the `revCount`
//! and `lastModified` that evaluation can see. The input's fingerprint
//! depends on how the tree was fetched; [`input_fingerprint`] computes
//! the ones that need nothing outside the tree.

use std::cmp::Ordering;

use brix_store::{ByteSlice, CanonPath, FileType, Hash, HashAlgo, HashFormat, SourceAccessor};

use crate::flake::LockedFlake;
use crate::hashes::GitRev;
use crate::input::{FetchSettings, Input, Scheme};
use crate::{err, Result};

/// `LockedFlake::getFingerprint`: none when the lock file has an unlocked
/// input or the root input has no fingerprint (`input` is its
/// [`input_fingerprint`]); otherwise the SHA-256 of
/// `<input>;<subdir>;<lock file>`, then `;hasRevCount` for a `revCount`
/// the fetcher computed (Nix's lazy attribute) or `;revCount=N` for one
/// that was given, and `;lastModified=N`.
///
/// `rev_count_computed` says which of the two the root's `revCount` is
/// (see [`rev_count_computed`]).
pub fn flake_fingerprint<T>(
    settings: &FetchSettings,
    locked: &LockedFlake<T>,
    input: Option<&str>,
    rev_count_computed: bool,
) -> Result<Option<Hash>> {
    if locked.lock_file.is_unlocked(settings)?.is_some() {
        return Ok(None);
    }
    let Some(input) = input else {
        return Ok(None);
    };
    let locked_ref = &locked.flake.locked_ref;
    let mut text = format!(
        "{input};{};{}",
        locked_ref.subdir,
        locked.lock_file.to_json_string()
    );
    if let Some(n) = locked_ref.input.rev_count()? {
        if rev_count_computed {
            text.push_str(";hasRevCount");
        } else {
            text.push_str(&format!(";revCount={n}"));
        }
    }
    if let Some(t) = locked_ref.input.last_modified()? {
        text.push_str(&format!(";lastModified={t}"));
    }
    Ok(Some(Hash::of(HashAlgo::Sha256, text.as_bytes())))
}

/// Whether the `revCount` of `locked` (the result of fetching
/// `requested`) is one the Git fetcher computed, which Nix keeps as a lazy
/// attribute, rather than one the reference gave: a working tree's always
/// is (unless it has no commits: then it is 0), an exported commit's is
/// when the reference had none.
pub fn rev_count_computed(requested: &Input, locked: &Input, workdir: bool) -> Result<bool> {
    if requested.scheme != Some(Scheme::Git) || locked.rev_count()?.is_none() {
        return Ok(false);
    }
    if workdir {
        return Ok(locked.rev()?.is_some_and(|rev| !rev.is_null()));
    }
    Ok(requested.rev_count()?.is_none())
}

/// `Input::getFingerprint` for the tree a fetch of `input` produced, as
/// far as it follows from the input and the tree: for a Git commit the
/// tree's own fingerprint when it has one (`fixupAccessor` keeps the
/// accessor's: an exported commit's says `;legacy` for Nix 2.19's export
/// and `;s` for mounted submodules), else `git:<rev>` (with `;e` for
/// `exportIgnore` and `;l` for `lfs`, as for a clean working tree),
/// `git:<tree>` for an archive (`github:`, `gitlab:`, `sourcehut:` and `tarball`
/// inputs are unpacked into Nix's Git-based tarball cache, whose accessor
/// names the tree by its Git object id), `tarball:<narHash>` or
/// `tarball:<rev>` for a `file`, `hg:<rev>` for a Mercurial revision.
/// `None` for the rest: a working tree with
/// uncommitted changes needs the files themselves (see
/// `native::git_workdir::dirty_fingerprint`), and a `path:` input has one
/// only when the store vouches for it.
pub fn input_fingerprint(input: &Input, tree: &dyn SourceAccessor) -> Result<Option<String>> {
    Ok(match input.scheme {
        Some(Scheme::Git) => match input.rev()? {
            Some(_) if tree.fingerprint().is_some() => tree.fingerprint().map(str::to_owned),
            Some(rev) => {
                let flag = |name| -> Result<bool> {
                    Ok(crate::attrs::maybe_get_bool_attr(&input.attrs, name)?.unwrap_or(false))
                };
                let mut fp = format!("git:{}", rev.to_hex());
                if flag("exportIgnore")? {
                    fp.push_str(";e");
                }
                if flag("lfs")? {
                    fp.push_str(";l");
                }
                Some(fp)
            }
            None => None,
        },
        Some(Scheme::GitHub | Scheme::GitLab | Scheme::SourceHut | Scheme::Tarball) => {
            Some(format!("git:{}", git_tree_hash(tree)?.to_hex()))
        }
        Some(Scheme::File) => match (input.nar_hash()?, input.rev()?) {
            (Some(hash), _) => Some(format!("tarball:{}", hash.to_sri())),
            (None, Some(rev)) => Some(format!("tarball:{}", rev.to_hex())),
            (None, None) => None,
        },
        // `MercurialInputScheme::getFingerprint`: none for a working tree
        // with uncommitted changes, which has no revision.
        Some(Scheme::Mercurial) => input.fingerprint()?,
        Some(Scheme::Path | Scheme::Indirect) | None => None,
    })
}

/// The Git object id of the tree in `accessor`, as Nix's tarball cache
/// stores an unpacked archive (`GitFileSystemObjectSink`): a blob per
/// file (mode `100755` when executable), a `120000` blob per symlink
/// holding its target, a tree per directory, empty ones included.
pub fn git_tree_hash(accessor: &dyn SourceAccessor) -> Result<GitRev> {
    let (_, id) = git_object(accessor, &CanonPath::root())?;
    Ok(GitRev::from_bytes(id))
}

/// The mode and object id of the file system object at `path`.
fn git_object(accessor: &dyn SourceAccessor, path: &CanonPath) -> Result<(&'static str, [u8; 20])> {
    let stat = accessor.lstat(path)?;
    match stat.file_type {
        FileType::Regular => {
            let mode = if stat.executable { "100755" } else { "100644" };
            Ok((mode, git_hash("blob", &accessor.read_file(path)?)))
        }
        FileType::Symlink => Ok((
            "120000",
            git_hash("blob", accessor.read_link(path)?.as_bytes()),
        )),
        FileType::Directory => {
            let mut entries = Vec::new();
            for name in accessor.read_dir(path)?.into_keys() {
                let child = path.join_component(&name).map_err(|e| err!("{}", e))?;
                let (mode, id) = git_object(accessor, &child)?;
                entries.push((name, mode, id));
            }
            // Git orders a tree's entries by name, a directory's as if it
            // ended in `/`.
            let key = |name: &[u8], mode: &str| -> Vec<u8> {
                let mut k = name.to_vec();
                if mode == "40000" {
                    k.push(b'/');
                }
                k
            };
            entries.sort_by_key(|(name, mode, _)| key(name, mode));
            let mut body = Vec::new();
            for (name, mode, id) in &entries {
                body.extend_from_slice(mode.as_bytes());
                body.push(b' ');
                body.extend_from_slice(name.as_bytes());
                body.push(0);
                body.extend_from_slice(id);
            }
            Ok(("40000", git_hash("tree", &body)))
        }
        other => Err(err!(
            "file '{}' has an unsupported type {:?}",
            accessor.show_path(path),
            other
        )),
    }
}

/// A Git object id: the SHA-1 of `<type> <length>\0<content>`.
fn git_hash(kind: &str, content: &[u8]) -> [u8; 20] {
    let mut hasher = brix_store::hash::Hasher::new(HashAlgo::Sha1);
    hasher.update(format!("{kind} {}\0", content.len()).as_bytes());
    hasher.update(content);
    hasher
        .finish()
        .0
        .as_bytes()
        .try_into()
        .expect("SHA-1 digests are 20 bytes")
}

/// `CanonPath`'s order in Nix (`operator<=>`): byte by byte as C `char`s
/// (with platform signedness), mapping `/` to zero before comparison.
pub fn nix_path_order(a: &CanonPath, b: &CanonPath) -> Ordering {
    let key = |c: u8| {
        if c == b'/' {
            0
        } else {
            c as std::ffi::c_char as i16
        }
    };
    let (a, b) = (a.abs().as_bytes(), b.abs().as_bytes());
    for (x, y) in a.iter().zip(b) {
        match key(*x).cmp(&key(*y)) {
            Ordering::Equal => {}
            other => return other,
        }
    }
    a.len().cmp(&b.len())
}

/// The lowercase hexadecimal form of a hash, as `Hash::to_string(Base16,
/// false)` prints it.
pub fn hex(hash: &Hash) -> String {
    hash.to_string(HashFormat::Base16, false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use brix_store::MemoryAccessor;

    fn p(s: &str) -> CanonPath {
        CanonPath::new(s).unwrap()
    }

    /// The tree id `git write-tree` gives for the same files (with an
    /// empty directory added through `git mktree`).
    #[test]
    fn tree_hashes_are_gits() {
        let mut tree = MemoryAccessor::new();
        tree.add_file(
            &p("/flake.nix"),
            b"{ outputs = { self }: { }; }\n".to_vec(),
            false,
        )
        .unwrap();
        tree.add_file(&p("/sub/f"), b"hi\n".to_vec(), false)
            .unwrap();
        tree.add_file(&p("/run.sh"), b"#!/bin/sh\n".to_vec(), true)
            .unwrap();
        tree.add_symlink(&p("/link"), "sub/f").unwrap();
        tree.add_dir(&p("/empty")).unwrap();
        // `sub.x` sorts before `sub/` but after `sub`.
        tree.add_file(&p("/sub.x"), b"x".to_vec(), false).unwrap();
        assert_eq!(
            git_tree_hash(&tree).unwrap().to_hex(),
            "3f3ed7d640f05f9e529468910de7ec68c55fcd1d"
        );
    }

    #[test]
    fn canon_paths_order_like_nix() {
        let mut paths = [p("/a.b"), p("/a/b"), p("/a"), p("/a\u{e9}"), p("/b")];
        paths.sort_by(nix_path_order);
        let order: Vec<_> = paths.iter().map(|p| p.abs()).collect();
        // `/` counts as 0; high bytes precede it only when C `char` is signed.
        let expected = if (0xffu8 as std::ffi::c_char) < 0 {
            ["/a", "/a\u{e9}", "/a/b", "/a.b", "/b"]
        } else {
            ["/a", "/a/b", "/a.b", "/a\u{e9}", "/b"]
        };
        assert_eq!(order, expected);
    }
}
