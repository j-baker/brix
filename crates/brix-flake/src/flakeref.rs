//! Flake references (`src/libflake/flakeref.cc`).
//!
//! A flake reference is an [`Input`] plus an optional subdirectory. Three
//! spellings are accepted, tried in this order: the flake-id shorthand
//! (`nixpkgs`, `nixpkgs/nixos-25.05`), a URL (`github:…`, `git+https://…`,
//! `path:…`), and a bare file system path (`.`, `./foo`, `/abs`).

use std::collections::BTreeMap;
use std::fmt;

use regex::Regex;
use std::sync::LazyLock;

use crate::attrs::{maybe_get_str_attr, Attr, Attrs};
use crate::input::{FetchSettings, Input};
use crate::{bad_url, err, Result};
use brix_store::url::{
    decode_query, parse_url, path_to_url_path, percent_decode, url_path_to_path, Authority,
    ParsedUrl,
};

/// `FlakeRef`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct FlakeRef {
    pub input: Input,
    /// `?dir=`: the subdirectory of the source tree holding `flake.nix`.
    pub subdir: String,
}

impl FlakeRef {
    pub fn new(input: Input, subdir: impl Into<String>) -> FlakeRef {
        FlakeRef {
            input,
            subdir: subdir.into(),
        }
    }

    /// `FlakeRef::to_string(abbreviate)`.
    pub fn to_url_string(&self, abbreviate: bool) -> Result<String> {
        let mut extra = BTreeMap::new();
        if !self.subdir.is_empty() {
            extra.insert("dir".to_string(), self.subdir.clone());
        }
        self.input.to_url_string(&extra, abbreviate)
    }

    /// `FlakeRef::toAttrs`: the input attrs plus `dir`.
    pub fn to_attrs(&self) -> Attrs {
        let mut attrs = self.input.attrs.clone();
        if !self.subdir.is_empty() {
            attrs.insert("dir".into(), Attr::String(self.subdir.clone()));
        }
        attrs
    }

    /// `FlakeRef::fromAttrs`.
    pub fn from_attrs(settings: &FetchSettings, attrs: &Attrs) -> Result<FlakeRef> {
        let subdir = maybe_get_str_attr(attrs, "dir")?.unwrap_or("").to_string();
        let mut attrs2 = attrs.clone();
        attrs2.remove("dir");
        Ok(FlakeRef {
            input: Input::from_attrs(settings, attrs2)?,
            subdir,
        })
    }

    /// `FlakeRef::canonicalize`: drop a `?dir=` from the `url` attribute
    /// when it merely repeats `subdir`, so lock files written by Nix < 2.20
    /// still compare equal to freshly parsed references.
    /// A `url` that does not parse is left alone, as in Nix; `warn` gets
    /// the warnings parsing it gives.
    pub fn canonicalize(&self, warn: &dyn Fn(&str)) -> FlakeRef {
        let mut flake_ref = self.clone();
        if let Ok(Some(url)) = maybe_get_str_attr(&flake_ref.input.attrs, "url") {
            if let Ok(mut parsed) = parse_url(url, true, warn) {
                if let Some(dir2) = parsed.query.get("dir") {
                    if !flake_ref.subdir.is_empty() && flake_ref.subdir == *dir2 {
                        parsed.query.remove("dir");
                    }
                }
                flake_ref
                    .input
                    .attrs
                    .insert("url".into(), Attr::String(parsed.to_string()));
            }
        }
        flake_ref
    }
}

impl fmt::Display for FlakeRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.to_url_string(false) {
            Ok(s) => f.write_str(&s),
            Err(_) => write!(f, "{}", self.input),
        }
    }
}

/// How to interpret bare paths in a flake reference.
#[derive(Clone, Debug, Default)]
pub struct ParseOptions {
    /// The directory relative paths are resolved against. With `None`,
    /// relative paths are rejected unless `preserve_relative_paths`.
    pub base_dir: Option<String>,
    /// Accept a path without a `flake.nix` (for `nix flake init`).
    pub allow_missing: bool,
    /// Whether the reference denotes a flake (`true`) or a plain source
    /// tree (`inputs.x.flake = false`). Affects which URL schemes apply
    /// and whether paths are searched for `flake.nix`.
    pub is_flake: bool,
    /// Keep `path:./foo` relative (for inputs resolved against the parent
    /// flake later) instead of rejecting it.
    pub preserve_relative_paths: bool,
}

impl ParseOptions {
    /// Nix's defaults: no base dir, must exist, is a flake.
    pub fn flake() -> ParseOptions {
        ParseOptions {
            is_flake: true,
            ..Default::default()
        }
    }
}

/// `parseFlakeRef`: a reference without a fragment.
pub fn parse_flake_ref(
    settings: &FetchSettings,
    url: &str,
    opts: &ParseOptions,
) -> Result<FlakeRef> {
    let (flake_ref, fragment) = parse_flake_ref_with_fragment(settings, url, opts)?;
    if !fragment.is_empty() {
        return Err(err!(
            "unexpected fragment '{}' in flake reference '{}'",
            fragment,
            url
        ));
    }
    Ok(flake_ref)
}

/// `parseFlakeRefWithFragment`: `<ref>#<fragment>`.
pub fn parse_flake_ref_with_fragment(
    settings: &FetchSettings,
    url: &str,
    opts: &ParseOptions,
) -> Result<(FlakeRef, String)> {
    if let Some(res) = parse_flake_id_ref(settings, url, opts.is_flake)? {
        return Ok(res);
    }
    if let Some(res) = parse_url_flake_ref(settings, url, opts)? {
        return Ok(res);
    }
    parse_path_flake_ref_with_fragment(settings, url, opts)
}

static FLAKE_ID_REF_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    // flakeIdRegexS, refAndOrRevRegex and fragmentRegex from Nix.
    let flake_id = "[a-zA-Z][a-zA-Z0-9_-]*";
    let rev = "[0-9a-fA-F]{40}";
    let ref_ = "[a-zA-Z0-9@][a-zA-Z0-9_.\\/@+-]*";
    let ref_and_or_rev = format!("(?:({rev})|(?:({ref_})(?:/({rev}))?))");
    let pct = "(?:%[0-9a-fA-F][0-9a-fA-F])";
    let pchar = format!("(?:[a-zA-Z0-9._~-]|{pct}|[!$&'\"()*+,;=]|[:@])");
    let fragment = format!("(?:{pchar}|[/? \"^])*");
    Regex::new(&format!(
        "^(({flake_id})(?:/(?:{ref_and_or_rev}))?)(?:#({fragment}))?$"
    ))
    .unwrap()
});

/// `parseFlakeIdRef`: the `nixpkgs[/ref[/rev]][#fragment]` shorthand for
/// `flake:…`.
fn parse_flake_id_ref(
    settings: &FetchSettings,
    url: &str,
    is_flake: bool,
) -> Result<Option<(FlakeRef, String)>> {
    let Some(m) = FLAKE_ID_REF_REGEX.captures(url) else {
        return Ok(None);
    };
    let parsed = ParsedUrl {
        scheme: "flake".into(),
        authority: None,
        path: m[1].split('/').map(str::to_string).collect(),
        ..Default::default()
    };
    let input = Input::from_url(settings, &parsed, is_flake)?;
    let fragment = percent_decode(m.get(6).map_or("", |f| f.as_str()))?;
    Ok(Some((FlakeRef::new(input, ""), fragment)))
}

/// `parseURLFlakeRef`: a syntactically valid URL. Returns `None` when the
/// string is not a URL at all (then it is tried as a path); errors while
/// *interpreting* a valid URL are reported.
fn parse_url_flake_ref(
    settings: &FetchSettings,
    url: &str,
    opts: &ParseOptions,
) -> Result<Option<(FlakeRef, String)>> {
    let Ok(mut parsed) = parse_url(url, true, &*settings.warn) else {
        return Ok(None);
    };
    if let Some(base_dir) = &opts.base_dir {
        if parsed.scheme == "path" || parsed.scheme == "git+file" {
            let path = url_path_to_path(&parsed.path)?;
            if !path.starts_with('/') {
                parsed.path = path_to_url_path(&abs_path(&path, base_dir)?);
            }
        }
    }
    Ok(Some(from_parsed_url(settings, parsed, opts.is_flake)?))
}

/// `fromParsedURL`: extract `dir` and the fragment, then build the input.
fn from_parsed_url(
    settings: &FetchSettings,
    mut parsed: ParsedUrl,
    is_flake: bool,
) -> Result<(FlakeRef, String)> {
    let dir = parsed.query.remove("dir").unwrap_or_default();
    let fragment = std::mem::take(&mut parsed.fragment);
    let mut input = Input::from_url(settings, &parsed, is_flake)?;

    // Nix < 2.20 kept `?dir=` in the `url` attribute of inputs that have
    // one; reproduce that in compat mode so such lock files stay current.
    if settings.nix219_compat && !dir.is_empty() {
        if let Some(url) = maybe_get_str_attr(&input.attrs, "url")? {
            let mut parsed_url_attr = parse_url(url, true, &*settings.warn)?;
            parsed_url_attr.query.insert("dir".into(), dir.clone());
            input
                .attrs
                .insert("url".into(), Attr::String(parsed_url_attr.to_string()));
        }
    }

    Ok((FlakeRef::new(input, dir), fragment))
}

/// `absPath(path, dir)` without symlink resolution: `path`, made absolute
/// relative to `dir`, as a canonical path.
fn abs_path(path: &str, dir: &str) -> Result<String> {
    let dir = brix_store::CanonPath::new(dir).map_err(|e| err!("{}", e))?;
    Ok(brix_store::CanonPath::new_relative(path, &dir)
        .map_err(|e| err!("{}", e))?
        .to_str()
        .expect("canonicalizing a textual path preserves UTF-8")
        .to_owned())
}

static PATH_FLAKE_REGEX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^([^?#]*)(\?([^#]*))?(#(.*))?$").unwrap());

/// `parsePathFlakeRefWithFragment`: a bare path, possibly with `?query`
/// and `#fragment`. With a base directory the path is searched upward
/// for `flake.nix` and a `.git` directory (which turns it into a
/// `git+file:` reference with a `dir`); that needs the file system and is
/// only available with the `native` feature.
fn parse_path_flake_ref_with_fragment(
    settings: &FetchSettings,
    url: &str,
    opts: &ParseOptions,
) -> Result<(FlakeRef, String)> {
    let Some(m) = PATH_FLAKE_REGEX.captures(url) else {
        return Err(err!("invalid flakeref '{}'", url));
    };
    #[cfg_attr(not(feature = "native"), allow(unused_mut))]
    let mut path = m[1].to_string();
    let query = decode_query(m.get(3).map_or("", |q| q.as_str()), true, &*settings.warn)?;
    let fragment = percent_decode(m.get(5).map_or("", |f| f.as_str()))?;

    if let Some(base_dir) = &opts.base_dir {
        #[cfg(feature = "native")]
        {
            path = native_path::abs_path_resolving_symlinks(&path, base_dir)?;
            if opts.is_flake {
                if let Some(result) = native_path::resolve_flake_path(
                    settings,
                    &mut path,
                    query.clone(),
                    &fragment,
                    opts,
                    url,
                )? {
                    return Ok(result);
                }
            }
        }
        #[cfg(not(feature = "native"))]
        {
            let _ = base_dir;
            return Err(err!(
                "flake reference '{}' is a relative path, which needs the file system (the 'native' feature)",
                url
            ));
        }
    } else if !opts.preserve_relative_paths && !path.starts_with('/') {
        return Err(bad_url!(
            "flake reference '{}' is not an absolute path",
            url
        ));
    }

    let parsed = ParsedUrl {
        scheme: "path".into(),
        authority: if path.starts_with('/') {
            Some(Authority::default())
        } else {
            None
        },
        path: path_to_url_path(&path),
        query,
        fragment,
    };
    from_parsed_url(settings, parsed, opts.is_flake)
}

#[cfg(feature = "native")]
mod native_path {
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};

    use super::*;

    /// `absPath(path, dir, resolveSymlinks = true)`.
    pub fn abs_path_resolving_symlinks(path: &str, base_dir: &str) -> Result<String> {
        let joined = super::abs_path(path, base_dir)?;
        let mut resolved = PathBuf::from("/");
        for comp in joined.split('/').filter(|c| !c.is_empty()) {
            resolved.push(comp);
            let mut hops = 0;
            while let Ok(target) = std::fs::read_link(&resolved) {
                hops += 1;
                if hops > 1024 {
                    return Err(err!("infinite symlink recursion in path '{}'", joined));
                }
                resolved = if target.is_absolute() {
                    target
                } else {
                    resolved.parent().unwrap_or(Path::new("/")).join(target)
                };
                resolved = PathBuf::from(super::abs_path(&resolved.to_string_lossy(), "/")?);
            }
        }
        Ok(resolved.to_string_lossy().into_owned())
    }

    fn exists(p: &Path) -> bool {
        p.symlink_metadata().is_ok()
    }

    /// `lstat`, failing with Nix's `SysError`.
    fn lstat(p: &Path) -> Result<std::fs::Metadata> {
        p.symlink_metadata().map_err(|e| {
            err!(
                "getting status of \"{}\": {}",
                p.display(),
                crate::native::path::sys_error_text(&e)
            )
        })
    }

    /// The device a file is on (`st_dev`).
    #[cfg(unix)]
    fn device(meta: &std::fs::Metadata) -> u64 {
        std::os::unix::fs::MetadataExt::dev(meta)
    }

    /// Without devices, no boundary is ever found.
    #[cfg(not(unix))]
    fn device(_meta: &std::fs::Metadata) -> u64 {
        0
    }

    /// The flake-specific part of `parsePathFlakeRefWithFragment`: find the
    /// flake root and, inside a Git checkout, produce a `git+file:` ref.
    pub fn resolve_flake_path(
        settings: &FetchSettings,
        path: &mut String,
        query: BTreeMap<String, String>,
        fragment: &str,
        opts: &ParseOptions,
        url: &str,
    ) -> Result<Option<(FlakeRef, String)>> {
        // Paths are quoted as Nix's `PathFmt` quotes them: in double quotes.
        let mut p = PathBuf::from(path.as_str());
        if !lstat(&p)?.is_dir() {
            if p.file_name().map(|n| n == "flake.nix").unwrap_or(false) {
                // Be gentle with people who write `/foo/bar/flake.nix`.
                let parent = p
                    .parent()
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|| p.clone());
                (settings.warn)(&format!(
                    "Path \"{}\" should point at the directory containing the 'flake.nix' file, not the file itself. Pretending that you meant \"{}\"",
                    p.display(),
                    parent.display()
                ));
                p = parent;
            } else {
                return Err(bad_url!(
                    "path \"{}\" is not a flake (because it's not a directory)",
                    p.display()
                ));
            }
        }

        if !opts.allow_missing && !exists(&p.join("flake.nix")) {
            (settings.notice)(&format!(
                "path \"{}\" does not contain a 'flake.nix', searching up",
                p.display()
            ));
            // The search stops at the file system it started on.
            let start = device(&lstat(&p)?);
            let mut found = false;
            let mut cur = p.clone();
            while let Some(parent) = cur.parent() {
                if exists(&cur.join("flake.nix")) {
                    found = true;
                    break;
                } else if exists(&cur.join(".git")) {
                    return Err(err!(
                        "path \"{}\" is not part of a flake (neither it nor its parent directories contain a 'flake.nix' file)",
                        cur.display()
                    ));
                } else if device(&lstat(&cur)?) != start {
                    return Err(err!(
                        "unable to find a flake before encountering filesystem boundary at \"{}\"",
                        cur.display()
                    ));
                }
                cur = parent.to_path_buf();
            }
            if !found {
                return Err(bad_url!("could not find a flake.nix file"));
            }
            p = cur;
        }

        if !opts.allow_missing && !exists(&p.join("flake.nix")) {
            return Err(bad_url!(
                "path \"{}\" is not a flake (because it doesn't contain a 'flake.nix' file)",
                p.display()
            ));
        }

        let mut flake_root = p.clone();
        let mut subdir = String::new();
        while let Some(parent) = flake_root.parent() {
            if exists(&flake_root.join(".git")) {
                let mut parsed = ParsedUrl {
                    scheme: "git+file".into(),
                    authority: Some(Authority::default()),
                    path: path_to_url_path(&flake_root.to_string_lossy()),
                    query,
                    fragment: fragment.to_string(),
                };
                if !subdir.is_empty() {
                    if parsed.query.contains_key("dir") {
                        return Err(err!(
                            "flake URL '{}' has an inconsistent 'dir' parameter",
                            url
                        ));
                    }
                    parsed.query.insert("dir".into(), subdir);
                }
                if exists(&flake_root.join(".git").join("shallow")) {
                    parsed.query.insert("shallow".into(), "1".into());
                }
                return Ok(Some(from_parsed_url(settings, parsed, opts.is_flake)?));
            }
            let name = flake_root
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            subdir = if subdir.is_empty() {
                name
            } else {
                format!("{name}/{subdir}")
            };
            flake_root = parent.to_path_buf();
        }
        *path = p.to_string_lossy().into_owned();
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attrs::attrs;
    use std::sync::Arc;

    fn parse(s: &str) -> Result<FlakeRef> {
        parse_flake_ref(&FetchSettings::default(), s, &ParseOptions::flake())
    }

    fn parse_frag(s: &str) -> (FlakeRef, String) {
        parse_flake_ref_with_fragment(&FetchSettings::default(), s, &ParseOptions::flake()).unwrap()
    }

    /// `nix eval --expr 'builtins.parseFlakeRef …'` warns about each
    /// query parameter without `=`, and fails on a bad escape with
    /// Boost.URL's messages.
    #[test]
    fn query_warnings_and_errors_match_nix() {
        let warnings = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = warnings.clone();
        let settings = FetchSettings {
            warn: crate::input::Warn::new(move |w| sink.lock().unwrap().push(w.to_owned())),
            ..Default::default()
        };
        let parse = |s: &str| parse_flake_ref(&settings, s, &ParseOptions::flake());
        let r = parse("github:a/b?foo&&dir=x").unwrap();
        assert_eq!(r.subdir, "x");
        assert_eq!(
            *warnings.lock().unwrap(),
            [
                "dubious URI query 'foo' is missing equal sign '=', ignoring",
                "dubious URI query '' is missing equal sign '=', ignoring"
            ]
        );
        warnings.lock().unwrap().clear();
        assert_eq!(
            parse("/foo?a=1&b").unwrap_err().to_string(),
            "path URL 'path:///foo?a=1' has unsupported parameter 'a'"
        );
        assert_eq!(
            *warnings.lock().unwrap(),
            ["dubious URI query 'b' is missing equal sign '=', ignoring"]
        );
        for (url, msg) in [
            (
                "https://x/y.tar.gz?a=%zz",
                "invalid URI query 'a=%zz': leftover",
            ),
            ("github:a/b?a=%zz", "invalid URI query 'a=%zz': leftover"),
            (
                "/foo#%zz",
                "invalid URI parameter '%zz': bad hexdig in pct-encoding",
            ),
            (
                "/foo#%z",
                "invalid URI parameter '%z': incomplete pct-encoding",
            ),
        ] {
            assert_eq!(parse_frag_err(url), msg, "{url}");
        }
    }

    fn parse_frag_err(s: &str) -> String {
        parse_flake_ref_with_fragment(&FetchSettings::default(), s, &ParseOptions::flake())
            .unwrap_err()
            .to_string()
    }

    #[test]
    fn path_refs() {
        assert_eq!(parse("/foo/bar").unwrap().to_string(), "path:/foo/bar");
        assert_eq!(
            parse("/foo/bar?revCount=123&rev=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
                .unwrap()
                .to_string(),
            "path:/foo/bar?rev=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa&revCount=123"
        );
        assert!(parse("/foo/bar?xyzzy=123").is_err());
        assert!(parse("/foo/bar#bla").is_err());
        let (r, f) = parse_frag("/foo/bar#bla");
        assert_eq!(r.to_string(), "path:/foo/bar");
        assert_eq!(f, "bla");
        let (r, f) = parse_frag("/foo/bar?revCount=123#bla");
        assert_eq!(r.to_string(), "path:/foo/bar?revCount=123");
        assert_eq!(f, "bla");
        let r = parse("/foo bar/baz?dir=bla space").unwrap();
        assert_eq!(r.to_string(), "path:/foo%20bar/baz?dir=bla%20space");
        assert_eq!(
            r.to_attrs().get("dir"),
            Some(&Attr::String("bla space".into()))
        );
        assert!(parse("./relative").is_err());
    }

    #[test]
    fn git_archive_refs() {
        assert_eq!(
            parse("github:foo/bar/branch%23").unwrap().to_string(),
            "github:foo/bar/branch%23"
        );
        assert_eq!(
            parse("github:foo/bar?ref=branch%23").unwrap().to_string(),
            "github:foo/bar/branch%23"
        );
        let (r, f) = parse_frag("github:foo/bar?ref=branch#\"name.with.dot\"");
        assert_eq!(f, "\"name.with.dot\"");
        assert_eq!(r.to_string(), "github:foo/bar/branch");
        let (r, f) = parse_frag("github:foo/bar#\"name.with.dot\"");
        assert_eq!(f, "\"name.with.dot\"");
        assert_eq!(r.to_string(), "github:foo/bar");
        let err = parse("github:foo/bar?xyzzy=1").unwrap_err().to_string();
        assert!(err.contains("xyzzy") && !err.contains("not an absolute path"));
        assert!(parse(
            "github:nixos/nixpkgs/nixpkgs.git?ref=aead170c1a49253ebfa5027010dfd89a77b73ca4"
        )
        .is_err());
    }

    #[test]
    fn input_from_url_round_trips() {
        let cases: Vec<(&str, Attrs, &str)> = vec![
            (
                "flake:nixpkgs",
                attrs([("id", "nixpkgs".into()), ("type", "indirect".into())]),
                "flake:nixpkgs",
            ),
            (
                "flake:nixpkgs/branch",
                attrs([
                    ("id", "nixpkgs".into()),
                    ("type", "indirect".into()),
                    ("ref", "branch".into()),
                ]),
                "flake:nixpkgs/branch",
            ),
            (
                "nixpkgs/branch",
                attrs([
                    ("id", "nixpkgs".into()),
                    ("type", "indirect".into()),
                    ("ref", "branch".into()),
                ]),
                "flake:nixpkgs/branch",
            ),
            (
                "nixpkgs/branch/2aae6c35c94fcfb415dbe95f408b9ce91ee846ed",
                attrs([
                    ("id", "nixpkgs".into()),
                    ("type", "indirect".into()),
                    ("ref", "branch".into()),
                    ("rev", "2aae6c35c94fcfb415dbe95f408b9ce91ee846ed".into()),
                ]),
                "flake:nixpkgs/branch/2aae6c35c94fcfb415dbe95f408b9ce91ee846ed",
            ),
            (
                "nixpkgs/branch////",
                attrs([
                    ("id", "nixpkgs".into()),
                    ("type", "indirect".into()),
                    ("ref", "branch".into()),
                ]),
                "flake:nixpkgs/branch",
            ),
            (
                "nixpkgs/branch///2aae6c35c94fcfb415dbe95f408b9ce91ee846ed///",
                attrs([
                    ("id", "nixpkgs".into()),
                    ("type", "indirect".into()),
                    ("ref", "branch".into()),
                    ("rev", "2aae6c35c94fcfb415dbe95f408b9ce91ee846ed".into()),
                ]),
                "flake:nixpkgs/branch/2aae6c35c94fcfb415dbe95f408b9ce91ee846ed",
            ),
            (
                "git://somewhere/repo?ref=branch",
                attrs([
                    ("type", "git".into()),
                    ("ref", "branch".into()),
                    ("url", "git://somewhere/repo".into()),
                ]),
                "git://somewhere/repo?ref=branch",
            ),
            (
                "git+https://somewhere.aaaaaaa/repo?ref=branch",
                attrs([
                    ("type", "git".into()),
                    ("ref", "branch".into()),
                    ("url", "https://somewhere.aaaaaaa/repo".into()),
                ]),
                "git+https://somewhere.aaaaaaa/repo?ref=branch",
            ),
            (
                "flake:/nixpkgs///branch////",
                attrs([
                    ("id", "nixpkgs".into()),
                    ("type", "indirect".into()),
                    ("ref", "branch".into()),
                ]),
                "flake:nixpkgs/branch",
            ),
            (
                "github://////owner%42/////repo%41///branch%43////",
                attrs([
                    ("type", "github".into()),
                    ("owner", "ownerB".into()),
                    ("repo", "repoA".into()),
                    ("ref", "branchC".into()),
                ]),
                "github:ownerB/repoA/branchC",
            ),
            (
                "gitlab:/owner%252Fsubgroup/////repo%41///branch%43////",
                attrs([
                    ("type", "gitlab".into()),
                    ("owner", "owner%2Fsubgroup".into()),
                    ("repo", "repoA".into()),
                    ("ref", "branchC".into()),
                ]),
                "gitlab:owner%252Fsubgroup/repoA/branchC",
            ),
        ];
        let settings = FetchSettings::default();
        for (url, expected_attrs, expected_url) in cases {
            let flake_ref = parse(url).unwrap();
            assert_eq!(flake_ref.to_attrs(), expected_attrs, "{url}");
            assert_eq!(flake_ref.to_string(), expected_url, "{url}");
            let input = Input::from_url_str(&settings, &flake_ref.to_string(), true).unwrap();
            assert_eq!(input.to_url_text(false).unwrap(), expected_url, "{url}");
            assert_eq!(input.attrs, expected_attrs, "{url}");
            let input2 =
                Input::from_url_str(&settings, &input.to_url_text(false).unwrap(), true).unwrap();
            assert_eq!(input, input2, "{url}");
        }
    }

    #[test]
    fn nix219_compat_retains_dir_in_url_attr() {
        let settings = FetchSettings {
            nix219_compat: true,
            ..Default::default()
        };
        let opts = ParseOptions::flake();
        let r = parse_flake_ref(&settings, "git+https://example.org/repo?dir=sub", &opts).unwrap();
        assert_eq!(r.subdir, "sub");
        assert_eq!(
            r.to_attrs().get("url"),
            Some(&Attr::String("https://example.org/repo?dir=sub".into()))
        );
        let r =
            parse_flake_ref(&settings, "https://example.org/foo.tar.gz?dir=sub", &opts).unwrap();
        assert_eq!(r.subdir, "sub");
        assert_eq!(
            r.to_attrs().get("url"),
            Some(&Attr::String(
                "https://example.org/foo.tar.gz?dir=sub".into()
            ))
        );
        let r = parse_flake_ref(&settings, "github:NixOS/nix?dir=perl", &opts).unwrap();
        assert_eq!(r.subdir, "perl");
        assert_eq!(
            r.to_attrs(),
            attrs([
                ("dir", "perl".into()),
                ("owner", "NixOS".into()),
                ("repo", "nix".into()),
                ("type", "github".into()),
            ])
        );
    }

    #[test]
    fn does_not_reencode_url() {
        let r = parse("http://localhost:8181/test/+3d.tar.gz").unwrap();
        assert_eq!(r.to_string(), "http://localhost:8181/test/%2B3d.tar.gz");
    }

    #[test]
    fn canonicalize_drops_redundant_dir() {
        let settings = FetchSettings {
            nix219_compat: true,
            ..Default::default()
        };
        let r = parse_flake_ref(
            &settings,
            "git+https://example.org/repo?dir=sub",
            &ParseOptions::flake(),
        )
        .unwrap();
        let c = r.canonicalize(&*settings.warn);
        assert_eq!(
            c.to_attrs().get("url"),
            Some(&Attr::String("https://example.org/repo".into()))
        );
        assert_eq!(c.subdir, "sub");
    }

    #[cfg(feature = "native")]
    #[test]
    fn path_refs_with_base_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("repo/sub")).unwrap();
        std::fs::write(root.join("repo/flake.nix"), "{}").unwrap();
        let opts = ParseOptions {
            base_dir: Some(root.to_string_lossy().into_owned()),
            ..ParseOptions::flake()
        };
        let settings = FetchSettings::default();
        // Searching upward for flake.nix.
        // (`repo/sub` without `./` would be a flake id with a ref, as in Nix.)
        let r = parse_flake_ref(&settings, "./repo/sub", &opts).unwrap();
        assert_eq!(r.to_string(), format!("path:{}/repo", root.display()));
        // Inside a git checkout it becomes git+file with a dir.
        std::fs::create_dir_all(root.join("repo/.git")).unwrap();
        std::fs::write(root.join("repo/sub/flake.nix"), "{}").unwrap();
        let r = parse_flake_ref(&settings, "./repo/sub", &opts).unwrap();
        assert_eq!(r.subdir, "sub");
        assert_eq!(
            r.to_string(),
            format!("git+file://{}/repo?dir=sub", root.display())
        );
        let r = parse_flake_ref(&settings, "./repo/flake.nix", &opts).unwrap();
        assert_eq!(r.to_string(), format!("git+file://{}/repo", root.display()));
        assert!(parse_flake_ref(&settings, "./nope", &opts).is_err());
    }
}
