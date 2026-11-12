//! Canonical absolute paths inside a source accessor.
//!
//! Mirrors `src/libutil/canon-path.hh`: a `CanonPath` always starts with
//! `/`, has no trailing slash (except the root itself), no empty
//! components, and no `.` or `..` components. Canonicalisation is purely
//! lexical (no symlink resolution) and `..` at the root is dropped, exactly
//! like Nix's `canonPathInner`.
//!
//! A path is bytes, as Nix's `std::string` is: a name need not be UTF-8
//! (`/a\xff` is a path, and so is a file named `caf\xe9`). Paths compare
//! and sort as their bytes do, unsigned, as `std::string` compares. Their
//! `Display` is lossy and meant for Rust-side text only (logs, brix's own
//! errors); a message Nix shows is built from the bytes
//! ([`CanonPath::as_bytes`], [`CanonPathError::message_bytes`]).

use std::borrow::Borrow;
use std::fmt;

use bstr::{BStr, BString, ByteSlice};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CanonPathError {
    /// `BadCanonPath` from `ensureNoNullBytes`. The segment is kept with
    /// its NUL bytes replaced by `␀`, as Nix shows it; its other bytes are
    /// as they were (see [`CanonPathError::message_bytes`]).
    #[error("path segment '{0}' must not contain null (\\0) bytes")]
    NullByte(BString),
}

impl CanonPathError {
    /// The message as Nix writes it: the `Display` text, but with the
    /// segment's bytes as they are.
    pub fn message_bytes(&self) -> Vec<u8> {
        match self {
            CanonPathError::NullByte(segment) => [
                b"path segment '".as_slice(),
                segment,
                b"' must not contain null (\\0) bytes",
            ]
            .concat(),
        }
    }
}

/// `CanonPath`. Equality, hashing and ordering are those of the bytes of
/// the absolute form (derived over the one field), so that a map keyed by
/// paths can be searched with a `&[u8]` or a `&BStr` (an ancestor from
/// [`CanonPath::ancestors`], say) without building a path: see the
/// `Borrow` impls below.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CanonPath {
    /// Canonical, starts with `/`, no NUL byte.
    path: Vec<u8>,
}

impl CanonPath {
    /// The root path `/`.
    pub fn root() -> CanonPath {
        CanonPath { path: vec![b'/'] }
    }

    /// `CanonPath(raw)`: canonicalise `raw` as a path below the root. A
    /// relative `raw` is treated as if it started with `/`. One pass and
    /// one allocation: Nix's `concatStrings("/", raw)` is not needed, as
    /// a leading `/` changes no component.
    pub fn new(raw: impl AsRef<[u8]>) -> Result<CanonPath, CanonPathError> {
        CanonPath::from_raw(raw.as_ref())
    }

    fn from_raw(raw: &[u8]) -> Result<CanonPath, CanonPathError> {
        ensure_no_null_bytes(raw)?;
        let mut path = Vec::with_capacity(raw.len() + 1);
        push_canonical_components(&mut path, raw);
        if path.is_empty() {
            path.push(b'/');
        }
        Ok(CanonPath { path })
    }

    /// `CanonPath(raw, root)`: `raw` is absolute, or relative to `root`.
    /// As `root` is canonical, its components are copied as they are and
    /// only `raw`'s are resolved (against them).
    pub fn new_relative(
        raw: impl AsRef<[u8]>,
        root: &CanonPath,
    ) -> Result<CanonPath, CanonPathError> {
        CanonPath::relative_from_raw(raw.as_ref(), root)
    }

    fn relative_from_raw(raw: &[u8], root: &CanonPath) -> Result<CanonPath, CanonPathError> {
        ensure_no_null_bytes(raw)?;
        let mut path;
        if raw.first() == Some(&b'/') {
            path = Vec::with_capacity(raw.len());
        } else {
            path = Vec::with_capacity(root.path.len() + 1 + raw.len());
            path.extend_from_slice(root.abs_or_empty());
        }
        push_canonical_components(&mut path, raw);
        if path.is_empty() {
            path.push(b'/');
        }
        Ok(CanonPath { path })
    }

    /// Build from already-canonical bytes without checking. For use
    /// within the crate where the invariant is known to hold (a suffix of
    /// a canonical path that starts at one of its slashes, say).
    pub(crate) fn unchecked(path: Vec<u8>) -> CanonPath {
        debug_assert!(path.first() == Some(&b'/'), "{:?}", path.as_bstr());
        CanonPath { path }
    }

    pub fn is_root(&self) -> bool {
        self.path.len() <= 1
    }

    /// The absolute form, e.g. `/foo/bar`; `/` for the root.
    pub fn abs(&self) -> &BStr {
        self.path.as_bstr()
    }

    /// `absOrEmpty`: the absolute form, or `""` for the root.
    pub fn abs_or_empty(&self) -> &BStr {
        if self.is_root() {
            BStr::new(b"")
        } else {
            self.path.as_bstr()
        }
    }

    /// The path without its leading `/`, e.g. `foo/bar`; `""` for the root.
    pub fn rel(&self) -> &BStr {
        self.path[1..].as_bstr()
    }

    /// The bytes of the absolute form ([`CanonPath::abs`]).
    pub fn as_bytes(&self) -> &[u8] {
        &self.path
    }

    /// The bytes of the absolute form, without copying.
    pub fn into_bytes(self) -> Vec<u8> {
        self.path
    }

    /// The absolute form as text, if it is UTF-8: for the consumers that
    /// can only take text (URLs, JSON, lock files, store URIs), which say
    /// what they do with a path that is not.
    pub fn to_str(&self) -> Option<&str> {
        std::str::from_utf8(&self.path).ok()
    }

    /// The path components, in order; empty for the root.
    pub fn components(&self) -> impl Iterator<Item = &BStr> {
        self.component_bytes().map(BStr::new)
    }

    /// [`CanonPath::components`] as byte slices.
    fn component_bytes(&self) -> impl Iterator<Item = &[u8]> {
        self.path[1..]
            .split(|&b| b == b'/')
            .filter(|c| !c.is_empty())
    }

    /// The last component, or `None` for the root.
    pub fn base_name(&self) -> Option<&BStr> {
        if self.is_root() {
            return None;
        }
        let slash = last_slash(&self.path);
        Some(self.path[slash + 1..].as_bstr())
    }

    /// This path and then each of its ancestors, nearest first, in
    /// absolute form: `/a/b`, `/a`, `/`. Like [`std::path::Path::ancestors`],
    /// but borrowed slices of this path rather than new paths.
    pub fn ancestors(&self) -> impl Iterator<Item = &BStr> {
        std::iter::successors(Some(self.path.as_slice()), |&p| {
            if p == b"/" {
                return None;
            }
            match last_slash(p) {
                0 => Some(&p[..1]),
                cut => Some(&p[..cut]),
            }
        })
        .map(BStr::new)
    }

    /// The parent, or `None` for the root.
    pub fn parent(&self) -> Option<CanonPath> {
        if self.is_root() {
            return None;
        }
        let cut = last_slash(&self.path).max(1);
        Some(CanonPath::unchecked(self.path[..cut].to_vec()))
    }

    /// Remove the last component. Panics on the root.
    pub fn pop(&mut self) {
        assert!(!self.is_root(), "cannot pop the root path");
        let cut = last_slash(&self.path).max(1);
        self.path.truncate(cut);
    }

    /// Append a single component, which must not contain `/` or be `.`
    /// or `..`.
    pub fn push(&mut self, component: impl AsRef<[u8]>) -> Result<(), CanonPathError> {
        let component = component.as_ref();
        check_component(component)?;
        if !self.is_root() {
            self.path.push(b'/');
        }
        self.path.extend_from_slice(component);
        Ok(())
    }

    /// `operator/(std::string_view)`: a copy with one component appended,
    /// built in one allocation.
    pub fn join_component(&self, component: impl AsRef<[u8]>) -> Result<CanonPath, CanonPathError> {
        let component = component.as_ref();
        check_component(component)?;
        let mut path = Vec::with_capacity(self.path.len() + 1 + component.len());
        path.extend_from_slice(self.abs_or_empty());
        path.push(b'/');
        path.extend_from_slice(component);
        Ok(CanonPath { path })
    }

    /// `operator/(const CanonPath &)`: concatenation.
    pub fn join(&self, other: &CanonPath) -> CanonPath {
        let mut res = self.clone();
        res.extend(other);
        res
    }

    /// `extend`: append all of `other`'s components.
    pub fn extend(&mut self, other: &CanonPath) {
        if other.is_root() {
            return;
        }
        if self.is_root() {
            self.path.extend_from_slice(other.rel());
        } else {
            self.path.extend_from_slice(other.abs());
        }
    }

    /// `isWithin`: is `self` equal to or below `parent`?
    pub fn is_within(&self, parent: &CanonPath) -> bool {
        let (p, q) = (self.path.as_slice(), parent.path.as_slice());
        !(p.len() < q.len()
            || !p.starts_with(q)
            || (q.len() > 1 && p.len() > q.len() && p[q.len()] != b'/'))
    }

    /// `removePrefix`: `self` relative to `prefix`, which must contain it.
    pub fn remove_prefix(&self, prefix: &CanonPath) -> CanonPath {
        assert!(self.is_within(prefix));
        if prefix.is_root() {
            return self.clone();
        }
        if self.path.len() == prefix.path.len() {
            return CanonPath::root();
        }
        CanonPath::unchecked(self.path[prefix.path.len()..].to_vec())
    }

    /// `makeRelative`: a relative path from `self` to `path`, using `..`
    /// where needed; `.` when they are equal.
    pub fn make_relative(&self, path: &CanonPath) -> BString {
        let mine: Vec<&[u8]> = self.component_bytes().collect();
        let theirs: Vec<&[u8]> = path.component_bytes().collect();
        let common = mine
            .iter()
            .zip(theirs.iter())
            .take_while(|(a, b)| a == b)
            .count();
        if common == mine.len() && common == theirs.len() {
            return BString::from(".");
        }
        let mut res = Vec::new();
        let ups = std::iter::repeat_n(&b".."[..], mine.len() - common);
        let downs = theirs[common..].iter().copied();
        for (i, part) in ups.chain(downs).enumerate() {
            if i > 0 {
                res.push(b'/');
            }
            res.extend_from_slice(part);
        }
        BString::from(res)
    }
}

impl fmt::Debug for CanonPath {
    /// `CanonPath("/a/caf\xe9")`: the bytes with `BStr`'s escapes.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "CanonPath({:?})", self.path.as_bstr())
    }
}

impl fmt::Display for CanonPath {
    /// Lossy (bytes that are not UTF-8 show as U+FFFD): for Rust-side
    /// text only. A message Nix shows takes [`CanonPath::as_bytes`].
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self.path.as_bstr(), f)
    }
}

/// A path hashes, compares and orders exactly as the bytes of its
/// absolute form, so a map keyed by paths can be searched with a `&[u8]`.
impl Borrow<[u8]> for CanonPath {
    fn borrow(&self) -> &[u8] {
        &self.path
    }
}

/// As `Borrow<[u8]>`, for a `&BStr` key ([`CanonPath::ancestors`] yields
/// them): `BStr` hashes and orders as its bytes.
impl Borrow<BStr> for CanonPath {
    fn borrow(&self) -> &BStr {
        self.path.as_bstr()
    }
}

/// The index of the last `/` in `path`, which has one (a canonical path,
/// or a prefix of one ending before a slash).
fn last_slash(path: &[u8]) -> usize {
    path.iter()
        .rposition(|&b| b == b'/')
        .expect("a canonical path starts with '/'")
}

/// What [`CanonPath::push`] takes: one component, neither `.` nor `..`,
/// with no `/` (a programming error, as in Nix's `assert`) and no NUL
/// byte (Nix's `BadCanonPath`).
fn check_component(component: &[u8]) -> Result<(), CanonPathError> {
    assert!(
        !component.contains(&b'/') && component != b"." && component != b"..",
        "invalid path component {:?}",
        component.as_bstr()
    );
    ensure_no_null_bytes(component)
}

/// `ensureNoNullBytes`: the segment in the error has its NUL bytes
/// replaced by `␀`, as Nix shows it.
fn ensure_no_null_bytes(s: &[u8]) -> Result<(), CanonPathError> {
    if s.contains(&0) {
        return Err(CanonPathError::NullByte(s.replace(b"\0", "␀").into()));
    }
    Ok(())
}

/// `canonPathInner` for Unix paths, appending to `out`: the components of
/// `raw` with runs of `/` collapsed, `.` dropped and `..` removing the
/// component before it (ignored at the root). `out` holds the components
/// of a canonical path already (`""` for the root, `/a/b` otherwise),
/// which a `..` in `raw` may remove too; the caller turns an empty result
/// into `/`.
fn push_canonical_components(out: &mut Vec<u8>, raw: &[u8]) {
    for comp in raw.split(|&b| b == b'/') {
        match comp {
            b"" | b"." => {}
            b".." => {
                if let Some(cut) = out.iter().rposition(|&b| b == b'/') {
                    out.truncate(cut);
                }
            }
            _ => {
                out.push(b'/');
                out.extend_from_slice(comp);
            }
        }
    }
}

/// `canonPathInner` for Unix paths: collapse runs of `/`, drop `.`,
/// resolve `..` lexically (ignored at the root), no trailing slash. NUL
/// bytes are not looked for.
pub(crate) fn canonicalise(raw: &[u8]) -> Vec<u8> {
    let mut result = Vec::with_capacity(raw.len() + 1);
    push_canonical_components(&mut result, raw);
    if result.is_empty() {
        result.push(b'/');
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: impl AsRef<[u8]>) -> CanonPath {
        CanonPath::new(s).unwrap()
    }

    #[test]
    fn canonicalises_like_nix() {
        assert_eq!(p("").abs(), "/");
        assert_eq!(p("/").abs(), "/");
        assert_eq!(p("foo").abs(), "/foo");
        assert_eq!(p("/foo//bar/").abs(), "/foo/bar");
        assert_eq!(p("/foo/./bar").abs(), "/foo/bar");
        assert_eq!(p("/foo/../bar").abs(), "/bar");
        assert_eq!(p("/../../bar").abs(), "/bar");
        assert_eq!(p("/foo/..").abs(), "/");
        assert_eq!(p("...").abs(), "/...");
        assert_eq!(p("/a/..b").abs(), "/a/..b");
    }

    /// Names are bytes: canonicalisation looks only at `/`, `.` and `..`
    /// components, whatever the other bytes are. Oracle: `nix-instantiate
    /// --eval --expr '/a + "/\xff/.."'` (raw bytes) is `/a`.
    #[test]
    fn canonicalises_bytes() {
        assert_eq!(p(b"/a/\xff/..").abs(), "/a");
        assert_eq!(p(b"/a\xff//b/./").abs(), b"/a\xff/b");
        assert_eq!(p(b"caf\xe9").as_bytes(), b"/caf\xe9");
        assert_eq!(p(b"/d\xe9/x").base_name(), Some(BStr::new(b"x")));
        assert_eq!(p(b"/d\xe9/x").parent(), Some(p(b"/d\xe9")));
        assert_eq!(
            p(b"/d\xe9/caf\xe9").components().collect::<Vec<_>>(),
            [BStr::new(b"d\xe9"), BStr::new(b"caf\xe9")]
        );
        assert_eq!(p(b"/d\xe9").to_str(), None);
        assert_eq!(p("/d").to_str(), Some("/d"));
        assert_eq!(p(b"/a\xff").into_bytes(), b"/a\xff");
        // Lossy for Rust-side text; escaped for debugging.
        assert_eq!(p(b"/a\xff").to_string(), "/a\u{fffd}");
        assert_eq!(format!("{:?}", p(b"/a\xff")), r#"CanonPath("/a\xff")"#);
    }

    #[test]
    fn relative_construction() {
        // `CanonPath(raw, root)`: an absolute `raw` ignores `root`, a
        // relative one is canonicalised below it.
        let root = p("/base/dir");
        assert_eq!(
            CanonPath::new_relative("x/y", &root).unwrap().abs(),
            "/base/dir/x/y"
        );
        assert_eq!(CanonPath::new_relative("/x", &root).unwrap().abs(), "/x");
        assert_eq!(
            CanonPath::new_relative("../x", &root).unwrap().abs(),
            "/base/x"
        );
        assert_eq!(
            CanonPath::new_relative("../../../x/./y//", &root)
                .unwrap()
                .abs(),
            "/x/y"
        );
        assert_eq!(
            CanonPath::new_relative("", &root).unwrap().abs(),
            "/base/dir"
        );
        assert_eq!(
            CanonPath::new_relative("x", &CanonPath::root())
                .unwrap()
                .abs(),
            "/x"
        );
        assert_eq!(
            CanonPath::new_relative(b"../\xff", &p(b"/b\xe9/d"))
                .unwrap()
                .abs(),
            b"/b\xe9/\xff"
        );
        assert_eq!(
            CanonPath::new_relative("x\0y", &root)
                .unwrap_err()
                .to_string(),
            "path segment 'x␀y' must not contain null (\\0) bytes"
        );
    }

    #[test]
    fn parent_pop_push_join() {
        let mut q = p("/a/b/c");
        assert_eq!(q.parent().unwrap().abs(), "/a/b");
        assert_eq!(p("/a").parent().unwrap().abs(), "/");
        assert!(CanonPath::root().parent().is_none());
        q.pop();
        assert_eq!(q.abs(), "/a/b");
        q.push("d").unwrap();
        assert_eq!(q.abs(), "/a/b/d");
        q.push(b"\xe9").unwrap();
        assert_eq!(q.abs(), b"/a/b/d/\xe9");
        assert_eq!(CanonPath::root().join_component("x").unwrap().abs(), "/x");
        assert_eq!(p("/a").join_component(b"\xff").unwrap().abs(), b"/a/\xff");
        assert_eq!(p("/a").join(&p("/b/c")).abs(), "/a/b/c");
        assert_eq!(CanonPath::root().join(&p("/b")).abs(), "/b");
        assert_eq!(p("/a").join(&CanonPath::root()).abs(), "/a");
        assert_eq!(p("/a/b").base_name(), Some(BStr::new("b")));
        assert_eq!(CanonPath::root().base_name(), None);
        assert_eq!(p("/a/b").components().collect::<Vec<_>>(), ["a", "b"]);
        assert_eq!(CanonPath::root().components().count(), 0);
        assert_eq!(p("/a").abs_or_empty(), "/a");
        assert_eq!(CanonPath::root().abs_or_empty(), "");
        assert_eq!(p("/a/b").rel(), "a/b");
        assert_eq!(CanonPath::root().rel(), "");
    }

    #[test]
    fn within_and_prefix() {
        assert!(p("/a/b").is_within(&p("/a")));
        assert!(p("/a").is_within(&p("/a")));
        assert!(!p("/ab").is_within(&p("/a")));
        assert!(p("/a").is_within(&CanonPath::root()));
        assert_eq!(p("/a/b/c").remove_prefix(&p("/a")).abs(), "/b/c");
        assert_eq!(p("/a").remove_prefix(&p("/a")).abs(), "/");
        assert!(!p(b"/a\xff").is_within(&p("/a")));
        assert_eq!(p(b"/a/\xff").remove_prefix(&p("/a")).abs(), b"/\xff");
    }

    #[test]
    fn make_relative_matches_nix() {
        // libutil-tests/canon-path.cc, `TEST(CanonPath, makeRelative)`.
        let d = p("/foo/bar");
        for (to, expected) in [
            ("/foo/bar", "."),
            ("/foo", ".."),
            ("/", "../.."),
            ("/foo/bar/xyzzy", "xyzzy"),
            ("/foo/bar/xyzzy/bla", "xyzzy/bla"),
            ("/foo/xyzzy/bla", "../xyzzy/bla"),
            ("/xyzzy/bla", "../../xyzzy/bla"),
        ] {
            assert_eq!(d.make_relative(&p(to)), expected, "{to}");
        }
        assert_eq!(CanonPath::root().make_relative(&CanonPath::root()), ".");
        assert_eq!(CanonPath::root().make_relative(&p("/a/b")), "a/b");
        // Components are compared whole: `/ab` is not below `/a`.
        assert_eq!(p("/a").make_relative(&p("/ab")), "../ab");
        assert_eq!(
            p(b"/a/\xe9").make_relative(&p(b"/a/\xff/x")),
            b"../\xff/x".as_slice()
        );
    }

    #[test]
    fn ancestors_nearest_first() {
        let all = |s: &str| -> Vec<BString> { p(s).ancestors().map(BStr::to_owned).collect() };
        assert_eq!(all("/a/b/c"), ["/a/b/c", "/a/b", "/a", "/"]);
        assert_eq!(all("/a"), ["/a", "/"]);
        assert_eq!(all("/"), ["/"]);
        assert_eq!(
            all("/nix/store/x-a.b"),
            ["/nix/store/x-a.b", "/nix/store", "/nix", "/"]
        );
        assert_eq!(
            p(b"/\xe9/b").ancestors().collect::<Vec<_>>(),
            [BStr::new(b"/\xe9/b"), BStr::new(b"/\xe9"), BStr::new("/")]
        );
    }

    #[test]
    fn paths_are_found_by_their_bytes() {
        let mut map = std::collections::HashMap::new();
        map.insert(p("/a/b"), 1);
        map.insert(p(b"/a/\xff"), 3);
        assert_eq!(map.get(b"/a/b".as_slice()), Some(&1));
        assert_eq!(map.get(BStr::new("/a/b")), Some(&1));
        assert_eq!(map.get(b"/a/\xff".as_slice()), Some(&3));
        assert_eq!(map.get(b"/a".as_slice()), None);
        let tree: std::collections::BTreeMap<_, _> = [(p("/a/b"), 1), (p("/a"), 2)].into();
        assert_eq!(tree.get(BStr::new("/a")), Some(&2));
        assert_eq!(tree.get(b"/a/b".as_slice()), Some(&1));
    }

    /// Paths order as their bytes, unsigned, as `std::string` compares
    /// (`sort (<)` on paths in Nix: `[ /a/b /ab /a\351z /a\377 ]`).
    #[test]
    fn paths_order_as_unsigned_bytes() {
        let mut paths = vec![p(b"/a\xff"), p("/ab"), p("/a/b"), p(b"/a\xe9z")];
        paths.sort();
        assert_eq!(paths, [p("/a/b"), p("/ab"), p(b"/a\xe9z"), p(b"/a\xff")]);
    }

    #[test]
    fn rejects_null_bytes() {
        assert_eq!(
            CanonPath::new("a\0b").unwrap_err().to_string(),
            "path segment 'a␀b' must not contain null (\\0) bytes"
        );
        // The other bytes are kept as they are.
        assert_eq!(
            CanonPath::new(b"\xff\0").unwrap_err().message_bytes(),
            b"path segment '\xff\xe2\x90\x80' must not contain null (\\0) bytes"
        );
        assert!(p("/a").push(b"x\0").is_err());
        assert!(p("/a").join_component(b"x\0").is_err());
    }
}
