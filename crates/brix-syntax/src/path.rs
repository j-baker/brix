//! Lexical path canonicalisation, as Nix's `CanonPath` does it for path
//! literals at parse time.
//!
//! Paths are bytes, as in Nix: the directory a relative literal is
//! resolved against need not be UTF-8 (a checkout in `d\xe9/`), and the
//! resolved path keeps its bytes. Only `/`, `.` and `..` are looked at.

/// Canonicalises an absolute path lexically: collapses repeated slashes,
/// drops `.` components, resolves `..` against the preceding component (or
/// ignores it at the root) and removes any trailing slash. The result is
/// `/` or starts with `/` and has no trailing slash. (`canonPath` in Nix's
/// `file-system.cc`, without symlink resolution.)
pub fn canon_path_bytes(path: &[u8]) -> Vec<u8> {
    // Built in a single pass: a `..` truncates the output back to its
    // previous `/`. nixpkgs has tens of thousands of path literals, so
    // this runs often enough that collecting the components first showed
    // up in parse profiles.
    let mut out = Vec::with_capacity(path.len() + 1);
    for component in path.split(|&b| b == b'/') {
        match component {
            b"" | b"." => {}
            b".." => {
                let cut = out.iter().rposition(|&b| b == b'/').unwrap_or(0);
                out.truncate(cut);
            }
            c => {
                out.push(b'/');
                out.extend_from_slice(c);
            }
        }
    }
    if out.is_empty() {
        out.push(b'/');
    }
    out
}

/// [`canon_path_bytes`] for text (the command line's paths, say), which
/// stays text: only whole components and slashes are removed.
pub fn canon_path(path: &str) -> String {
    String::from_utf8(canon_path_bytes(path.as_bytes()))
        .expect("canonicalising text removes whole components only")
}

/// Resolves a path literal the way Nix's parser does: absolute literals are
/// canonicalised as they are, relative ones are first joined to `base_dir`
/// (the directory of the source file, itself absolute, as bytes). A
/// trailing slash in the literal is put back after canonicalisation, as
/// `path_start` in `parser.y` does. The literal itself is ASCII (the
/// lexer's path characters).
pub fn resolve_path_literal(literal: &str, base_dir: &[u8]) -> Vec<u8> {
    let mut path = if literal.starts_with('/') {
        canon_path_bytes(literal.as_bytes())
    } else {
        // `base_dir` is already canonical, so only the literal's own
        // components need resolving against it.
        let mut joined = Vec::with_capacity(base_dir.len() + literal.len() + 1);
        joined.extend_from_slice(base_dir);
        joined.push(b'/');
        joined.extend_from_slice(literal.as_bytes());
        canon_path_bytes(&joined)
    };
    if literal.len() > 1 && literal.ends_with('/') {
        path.push(b'/');
    }
    path
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonicalises_lexically() {
        assert_eq!(canon_path("/foo/bar/../baz/./x/"), "/foo/baz/x");
        assert_eq!(canon_path("/"), "/");
        assert_eq!(canon_path("/../.."), "/");
        assert_eq!(canon_path("//a//b//"), "/a/b");
        assert_eq!(canon_path("/caf\u{e9}/../d\u{e9}"), "/d\u{e9}");
    }

    #[test]
    fn canonicalises_bytes() {
        assert_eq!(canon_path_bytes(b"/a/\xff/.."), b"/a");
        assert_eq!(canon_path_bytes(b"//a\xff//b/./"), b"/a\xff/b");
        assert_eq!(canon_path_bytes(b"/.."), b"/");
    }

    #[test]
    fn resolves_literals_against_the_base_directory() {
        assert_eq!(resolve_path_literal("./a/../../b", b"/tmp"), b"/b");
        assert_eq!(resolve_path_literal("../..", b"/tmp"), b"/");
        assert_eq!(resolve_path_literal("foo/", b"/base"), b"/base/foo/");
        assert_eq!(
            resolve_path_literal("/foo/bar/../baz/./x/", b"/base"),
            b"/foo/baz/x/"
        );
        assert_eq!(resolve_path_literal("./foo", b"/base"), b"/base/foo");
    }

    /// The base directory is bytes, kept as they are. Oracle:
    /// `nix-instantiate --parse` from a directory named `d\xe9`
    /// (`tests/oracle.rs`).
    #[test]
    fn resolves_against_a_base_directory_that_is_not_utf8() {
        assert_eq!(resolve_path_literal("./x", b"/a\xff"), b"/a\xff/x");
        assert_eq!(resolve_path_literal("../x/", b"/a\xff/d\xe9"), b"/a\xff/x/");
        assert_eq!(resolve_path_literal("./.", b"/a\xff"), b"/a\xff");
        assert_eq!(resolve_path_literal("/abs", b"/a\xff"), b"/abs");
    }
}
