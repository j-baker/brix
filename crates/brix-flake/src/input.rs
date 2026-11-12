//! Fetcher inputs and their schemes (`src/libfetchers/fetchers.cc`,
//! `github.cc`, `git.cc`, `tarball.cc`, `path.cc`, `indirect.cc`).
//!
//! An [`Input`] is a bag of [`Attrs`] plus the scheme that interprets them.
//! Each scheme knows how to read itself from a URL and from attrs, how to
//! print itself as a URL, which attributes it accepts, and what it takes
//! for it to count as *locked*. Everything here is pure; fetching lives in
//! [`crate::fetch`].

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::Arc;

use regex::Regex;
use std::sync::LazyLock;

use brix_store::url::{parse_url, parse_url_scheme, path_to_url_path, url_path_to_path, ParsedUrl};
use brix_store::{HashFormat, XpSettings};

use crate::attrs::{
    attrs_to_json_string, attrs_to_query, get_str_attr, maybe_get_bool_attr, maybe_get_int_attr,
    maybe_get_str_attr, Attr, Attrs,
};
use crate::hashes::{GitRev, NarHash};
use crate::url::fix_git_url;
use crate::{bad_url, err, Result};

/// The subset of `fetchers::Settings` that affects parsing and locking.
#[derive(Clone, Debug)]
pub struct FetchSettings {
    /// `trust-tarballs-from-git-forges` (default `true`): a `github:` input
    /// with a `rev` counts as locked without a `narHash`.
    pub trust_tarballs_from_git_forges: bool,
    /// `nix-219-compat` (default `false`): keep `?dir=` in the `url`
    /// attribute of `git`/`tarball`/`file` inputs, and export a Git
    /// commit with `git archive`, as Nix < 2.20 did.
    pub nix219_compat: bool,
    /// `allow-dirty-locks` (default `false`): accept lock entries that are
    /// only pinned by `narHash`.
    pub allow_dirty_locks: bool,
    /// `allow-dirty` (default `true`): a Git working tree with uncommitted
    /// changes may be used; with `false` it is an error.
    pub allow_dirty: bool,
    /// `warn-dirty` (default `true`): warn when a Git working tree with
    /// uncommitted changes is used.
    pub warn_dirty: bool,
    /// `access-tokens`: `host` or `host/owner/repo` → token.
    pub access_tokens: BTreeMap<String, String>,
    /// Enabled experimental features (`verified-fetches` gates some git
    /// attributes).
    pub experimental_features: BTreeSet<String>,
    /// `tarball-ttl` (default 3600 s): how long a cached resolution of a
    /// mutable reference (a branch name, a tarball URL) is reused before
    /// the forge or server is asked again. `nix --offline` makes it
    /// `u32::MAX` (every earlier download stays current) unless the user
    /// set it; `--refresh`, `nix flake lock` and `nix flake update` make
    /// it 0.
    pub tarball_ttl: u64,
    /// `netrc-file`: credentials for HTTP fetches, applied like curl's
    /// optional netrc (basic auth for hosts the file names when a request
    /// has no `Authorization` header of its own). `None` sends none.
    pub netrc_file: Option<std::path::PathBuf>,
    /// `filetransfer-retry-attempts` (alias `download-attempts`, default 5): how
    /// many times a download that fails transiently is tried in all; 0
    /// tries once, as 1 does (`nix --offline` makes it 0 unless the user
    /// set it).
    pub download_attempts: u32,
    /// `filetransfer-retry-delay` (default 100 ms): the longest delay
    /// before the first retry of a download, doubled for each further one.
    pub download_retry_delay_ms: u32,
    /// `connect-timeout` (default 15 s, `unsigned long`): how many seconds
    /// a download may spend connecting (resolving the host, connecting and
    /// the TLS handshake) before the attempt fails, as curl's
    /// `CURLOPT_CONNECTTIMEOUT` does for Nix; 0 means no limit. `nix
    /// --offline` makes it 1 unless the user set it.
    pub connect_timeout: u64,
    /// Where warnings from parsing references go (Nix's `warn`), such as
    /// the one about a query parameter without `=`.
    pub warn: Warn,
    /// Where notices go (Nix's `notice`, shown unless the verbosity is
    /// below `notice`), such as the one about searching a flake's parent
    /// directories for its `flake.nix`.
    pub notice: Warn,
    /// Where the activities Nix starts at the `info` level are announced,
    /// as its logger prints them when `info` is within the verbosity
    /// (`unpacking '…' into the Git cache...`): Nix's default off a
    /// terminal, where its progress bar shows them instead.
    pub info: Warn,
    /// Where an error a fetch gets past goes, without its `error: `
    /// prefix (Nix's `logError`), such as a failed `git fetch` into a
    /// clone that has an older version of the ref.
    pub error: Warn,
    /// Who is shown the members of the archives a fetch downloads, as
    /// they are unpacked (see [`crate::ArchiveObserver`]).
    pub archive_observer: Option<ArchiveWatch>,
}

/// A [`crate::ArchiveObserver`] as a setting. Cheap to clone.
#[derive(Clone)]
pub struct ArchiveWatch(pub Arc<dyn crate::ArchiveObserver>);

impl fmt::Debug for ArchiveWatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ArchiveWatch(..)")
    }
}

impl Default for FetchSettings {
    fn default() -> Self {
        FetchSettings {
            trust_tarballs_from_git_forges: true,
            nix219_compat: false,
            allow_dirty_locks: false,
            allow_dirty: true,
            warn_dirty: true,
            access_tokens: BTreeMap::new(),
            experimental_features: BTreeSet::new(),
            tarball_ttl: 3600,
            netrc_file: None,
            download_attempts: 5,
            download_retry_delay_ms: 100,
            connect_timeout: 15,
            warn: Warn::stderr(),
            notice: Warn::new(|msg| eprintln!("{msg}")),
            info: Warn::new(|msg| eprintln!("{msg}")),
            error: Warn::new(|msg| eprintln!("error: {msg}")),
            archive_observer: None,
        }
    }
}

/// A warning sink: called with the message, without Nix's `warning: `
/// prefix. Cheap to clone; it dereferences to the function, so
/// `&*settings.warn` is what URL parsing takes.
#[derive(Clone)]
pub struct Warn(Arc<dyn Fn(&str) + Send + Sync>);

impl Warn {
    pub fn new(f: impl Fn(&str) + Send + Sync + 'static) -> Warn {
        Warn(Arc::new(f))
    }

    /// `warning: …` on stderr.
    pub fn stderr() -> Warn {
        Warn::new(|msg| eprintln!("warning: {msg}"))
    }
}

impl std::ops::Deref for Warn {
    type Target = dyn Fn(&str) + Send + Sync;

    fn deref(&self) -> &Self::Target {
        &*self.0
    }
}

impl fmt::Debug for Warn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Warn(..)")
    }
}

impl FetchSettings {
    /// The experimental features the store code checks
    /// (`experimental-features`).
    pub fn xp(&self) -> XpSettings {
        XpSettings::from_names(self.experimental_features.iter().map(String::as_str))
    }

    pub(crate) fn require_feature(&self, name: &str) -> Result<()> {
        if self.experimental_features.contains(name) {
            Ok(())
        } else {
            Err(err!(
                "experimental Nix feature '{0}' is disabled; add '--extra-experimental-features {0}' to enable it",
                name
            ))
        }
    }
}

static REV_REGEX: LazyLock<Regex> = LazyLock::new(|| Regex::new("^[0-9a-fA-F]{40}$").unwrap());
static FLAKE_ID_REGEX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new("^[a-zA-Z][a-zA-Z0-9_-]*$").unwrap());
/// `refRegex`: what Mercurial branch and tag names may look like.
static HG_REF_REGEX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[a-zA-Z0-9@][a-zA-Z0-9_.\/@+-]*$").unwrap());
static HOST_REGEX: LazyLock<Regex> = LazyLock::new(|| Regex::new("^[a-zA-Z0-9.-]*$").unwrap());

/// `revRegex`: a 40-digit hex SHA-1.
pub fn is_rev(s: &str) -> bool {
    REV_REGEX.is_match(s)
}

/// `flakeIdRegex`: `[a-zA-Z][a-zA-Z0-9_-]*`.
pub fn is_flake_id(s: &str) -> bool {
    FLAKE_ID_REGEX.is_match(s)
}

/// `isLegalRefName`: whether `name` is acceptable as a Git branch or tag
/// name. Nix asks libgit2 whether it is a valid reference, branch or tag
/// name; the tag rules are the most permissive, so this is
/// `git check-ref-format refs/tags/<name>` plus Nix's own rejection of `@`
/// and the DEL character.
pub fn is_legal_ref_name(name: &str) -> bool {
    if name.is_empty() || name == "@" || name.contains('\x7f') {
        return false;
    }
    if name.ends_with('/') || name.ends_with('.') || name.contains("..") || name.contains("@{") {
        return false;
    }
    if name
        .bytes()
        .any(|b| b < 0x20 || matches!(b, b' ' | b'~' | b'^' | b':' | b'?' | b'*' | b'[' | b'\\'))
    {
        return false;
    }
    name.split('/')
        .all(|comp| !comp.is_empty() && !comp.starts_with('.') && !comp.ends_with(".lock"))
}

/// The input schemes brix knows, in the order Nix tries them for a URL
/// (alphabetical by scheme name, since Nix keeps them in a `std::map`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Scheme {
    File,
    Git,
    GitHub,
    GitLab,
    /// `hg`, fetched by running the `hg` program as Nix does
    /// (`native::mercurial`).
    Mercurial,
    Indirect,
    Path,
    SourceHut,
    Tarball,
}

impl Scheme {
    pub const ALL: [Scheme; 9] = [
        Scheme::File,
        Scheme::Git,
        Scheme::GitHub,
        Scheme::GitLab,
        Scheme::Mercurial,
        Scheme::Indirect,
        Scheme::Path,
        Scheme::SourceHut,
        Scheme::Tarball,
    ];

    /// `InputScheme::schemeName`: the value of the `type` attribute.
    pub fn name(self) -> &'static str {
        match self {
            Scheme::File => "file",
            Scheme::Git => "git",
            Scheme::GitHub => "github",
            Scheme::GitLab => "gitlab",
            Scheme::Mercurial => "hg",
            Scheme::Indirect => "indirect",
            Scheme::Path => "path",
            Scheme::SourceHut => "sourcehut",
            Scheme::Tarball => "tarball",
        }
    }

    pub fn from_name(name: &str) -> Option<Scheme> {
        Scheme::ALL.into_iter().find(|s| s.name() == name)
    }

    /// `InputScheme::allowedAttrs`, without `type`.
    pub fn allowed_attrs(self) -> &'static [&'static str] {
        match self {
            Scheme::GitHub | Scheme::GitLab | Scheme::SourceHut => &[
                "owner",
                "repo",
                "ref",
                "rev",
                "narHash",
                "lastModified",
                "host",
                "treeHash",
            ],
            Scheme::Git => &[
                "url",
                "ref",
                "rev",
                "shallow",
                "submodules",
                "lfs",
                "exportIgnore",
                "lastModified",
                "revCount",
                "narHash",
                "allRefs",
                "name",
                "dirtyRev",
                "dirtyShortRev",
                "verifyCommit",
                "keytype",
                "publicKey",
                "publicKeys",
            ],
            Scheme::Tarball | Scheme::File => &[
                "url",
                "narHash",
                "name",
                "unpack",
                "rev",
                "revCount",
                "lastModified",
            ],
            Scheme::Mercurial => &["url", "ref", "rev", "revCount", "narHash", "name"],
            Scheme::Path => &["path", "rev", "revCount", "lastModified", "narHash"],
            Scheme::Indirect => &["id", "ref", "rev", "narHash"],
        }
    }

    /// The default host of a git forge scheme.
    pub fn default_host(self) -> Option<&'static str> {
        match self {
            Scheme::GitHub => Some("github.com"),
            Scheme::GitLab => Some("gitlab.com"),
            Scheme::SourceHut => Some("git.sr.ht"),
            _ => None,
        }
    }
}

/// `fetchers::Input`: attributes plus the scheme that interprets them.
/// `scheme` is `None` for an input whose `type` brix does not know; such
/// inputs can be stored and printed as attrs but not fetched.
///
/// An input also remembers the experimental features of the settings it
/// was made with, for reading its `rev` and `narHash` (where Nix consults
/// its global settings: a BLAKE3 hash needs `blake3-hashes`). They are not
/// part of the input's identity: equality and order compare the scheme
/// and the attributes only.
#[derive(Clone, Debug)]
pub struct Input {
    pub scheme: Option<Scheme>,
    pub attrs: Attrs,
    xp: XpSettings,
}

impl PartialEq for Input {
    fn eq(&self, other: &Input) -> bool {
        (self.scheme, &self.attrs) == (other.scheme, &other.attrs)
    }
}

impl Eq for Input {}

impl PartialOrd for Input {
    fn partial_cmp(&self, other: &Input) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Input {
    fn cmp(&self, other: &Input) -> std::cmp::Ordering {
        (self.scheme, &self.attrs).cmp(&(other.scheme, &other.attrs))
    }
}

impl Input {
    /// `Input::fromURL(settings, url, requireTree)`. `require_tree` is
    /// true for flake references (a plain `https://` URL then means a
    /// tarball) and false for `builtins.fetchTree`-style URLs without a
    /// tarball extension (then it means a single file).
    pub fn from_url(
        settings: &FetchSettings,
        url: &ParsedUrl,
        require_tree: bool,
    ) -> Result<Input> {
        for scheme in Scheme::ALL {
            if let Some(input) = scheme.input_from_url(settings, url, require_tree)? {
                return Ok(input);
            }
        }
        let parts = parse_url_scheme(&url.scheme);
        if parts.application == Some("file") && parts.transport == "git" {
            return Err(err!(
                "input '{}' is unsupported; did you mean 'git+file' instead of 'file+git'?",
                url
            ));
        }
        Err(err!("input '{}' is unsupported", url))
    }

    /// `Input::fromURL` on a string (strict URL parsing).
    pub fn from_url_str(settings: &FetchSettings, url: &str, require_tree: bool) -> Result<Input> {
        Input::from_url(
            settings,
            &parse_url(url, false, &*settings.warn)?,
            require_tree,
        )
    }

    /// `Input::fromAttrs`.
    pub fn from_attrs(settings: &FetchSettings, attrs: Attrs) -> Result<Input> {
        let Some(type_name) = maybe_get_str_attr(&attrs, "type")? else {
            return Err(err!(
                "'type' attribute to specify input scheme is required but not provided"
            ));
        };
        let Some(scheme) = Scheme::from_name(type_name) else {
            // An input whose scheme we do not know: keep the attrs so lock
            // files mentioning it still round-trip.
            let input = Input {
                scheme: None,
                attrs,
                xp: settings.xp(),
            };
            input.fixup()?;
            return Ok(input);
        };
        for name in attrs.keys() {
            if name != "type"
                && name != "__final"
                && name != "__legacyExport"
                && !scheme.allowed_attrs().contains(&name.as_str())
            {
                return Err(err!(
                    "input attribute '{}' not supported by scheme '{}'",
                    name,
                    type_name
                ));
            }
        }
        let input = scheme.input_from_attrs(settings, attrs)?;
        input.fixup()?;
        Ok(input)
    }

    /// `fixupInput`: type-check the common attributes.
    fn fixup(&self) -> Result<()> {
        self.type_name()?;
        self.ref_()?;
        self.rev_count()?;
        self.last_modified()?;
        Ok(())
    }

    /// `Input::toURL(abbreviate)`.
    pub fn to_url(&self, abbreviate: bool) -> Result<ParsedUrl> {
        let Some(scheme) = self.scheme else {
            return Err(err!(
                "cannot show unsupported input '{}'",
                attrs_to_json_string(&self.attrs)
            ));
        };
        let mut url = scheme.to_url(self, abbreviate)?;
        if abbreviate {
            url.query.remove("narHash");
        }
        Ok(url)
    }

    /// `Input::toURLString(extraQuery, abbreviate)`. Extra parameters do
    /// not override ones the scheme produced.
    pub fn to_url_string(
        &self,
        extra_query: &BTreeMap<String, String>,
        abbreviate: bool,
    ) -> Result<String> {
        let mut url = self.to_url(abbreviate)?;
        for (k, v) in extra_query {
            url.query.entry(k.clone()).or_insert_with(|| v.clone());
        }
        Ok(url.to_string())
    }

    /// `Input::to_string(abbreviate)`.
    pub fn to_url_text(&self, abbreviate: bool) -> Result<String> {
        Ok(self.to_url(abbreviate)?.to_string())
    }

    /// `Input::isDirect`: false only for `indirect` (registry) inputs.
    pub fn is_direct(&self) -> bool {
        self.scheme != Some(Scheme::Indirect)
    }

    /// `Input::isLocked`: whether fetching can only ever yield one tree.
    /// A malformed `rev` or `narHash` is an error, as in Nix.
    pub fn is_locked(&self, settings: &FetchSettings) -> Result<bool> {
        Ok(match self.scheme {
            Some(Scheme::GitHub | Scheme::GitLab | Scheme::SourceHut) => {
                self.rev()?.is_some()
                    && (settings.trust_tarballs_from_git_forges || self.nar_hash()?.is_some())
            }
            Some(Scheme::Git) => matches!(self.rev()?, Some(rev) if !rev.is_null()),
            Some(Scheme::Mercurial) => self.rev_hash()?.is_some(),
            Some(Scheme::Tarball | Scheme::File | Scheme::Path) => self.nar_hash()?.is_some(),
            Some(Scheme::Indirect) | None => false,
        })
    }

    /// `Input::isRelative`: a `path:` input with a relative path, which is
    /// interpreted relative to the parent flake.
    pub fn is_relative(&self) -> Option<String> {
        if self.scheme != Some(Scheme::Path) {
            return None;
        }
        let path = maybe_get_str_attr(&self.attrs, "path").ok().flatten()?;
        if path.starts_with('/') {
            None
        } else {
            Some(path.to_string())
        }
    }

    /// `Input::isFinal`: the attrs came from a lock file (or a completed
    /// fetch) and must not be changed by fetching.
    pub fn is_final(&self) -> bool {
        maybe_get_bool_attr(&self.attrs, "__final")
            .ok()
            .flatten()
            .unwrap_or(false)
    }

    /// Mark the input as final (what Nix does to every lock file entry).
    pub fn set_final(&mut self) {
        self.attrs.insert("__final".into(), Attr::Bool(true));
    }

    /// `Input::contains(other)`: equal, or equal once `other`'s `ref`/`rev`
    /// are ignored. Used for registry matching.
    pub fn contains(&self, other: &Input) -> bool {
        if self.attrs == other.attrs {
            return true;
        }
        let mut other2 = other.attrs.clone();
        other2.remove("ref");
        other2.remove("rev");
        self.attrs == other2
    }

    /// `Input::applyOverrides(ref, rev)`.
    pub fn apply_overrides(&self, ref_: Option<&str>, rev: Option<GitRev>) -> Result<Input> {
        let Some(scheme) = self.scheme else {
            return Ok(self.clone());
        };
        scheme.apply_overrides(self, ref_, rev)
    }

    /// `Input::getName`: the store path name, `source` by default. A
    /// `name` that is not a string is an error.
    pub fn name(&self) -> Result<String> {
        Ok(maybe_get_str_attr(&self.attrs, "name")?
            .unwrap_or("source")
            .to_string())
    }

    /// `Input::getType`.
    pub fn type_name(&self) -> Result<&str> {
        get_str_attr(&self.attrs, "type")
    }

    /// `Input::getNarHash`.
    pub fn nar_hash(&self) -> Result<Option<NarHash>> {
        match maybe_get_str_attr(&self.attrs, "narHash")? {
            Some(s) => Ok(Some(NarHash::parse_sri(s, self.xp)?)),
            None => Ok(None),
        }
    }

    /// `Input::getRef`.
    pub fn ref_(&self) -> Result<Option<&str>> {
        maybe_get_str_attr(&self.attrs, "ref")
    }

    /// `Input::getRev`.
    pub fn rev(&self) -> Result<Option<GitRev>> {
        match maybe_get_str_attr(&self.attrs, "rev")? {
            Some(s) => Ok(Some(GitRev::parse(s, self.xp)?)),
            None => Ok(None),
        }
    }

    /// `Input::getRev` as Nix has it: a hash of any algorithm. The
    /// Mercurial scheme reads its `rev` so, as Nix's does, until its
    /// fetcher refuses one that is not SHA-1; [`Input::rev`] refuses that
    /// at once, Git's revisions being SHA-1.
    pub fn rev_hash(&self) -> Result<Option<brix_store::Hash>> {
        match maybe_get_str_attr(&self.attrs, "rev")? {
            Some(s) => Ok(Some(crate::hashes::parse_rev_hash(s, self.xp)?)),
            None => Ok(None),
        }
    }

    /// `Input::getRevCount`.
    pub fn rev_count(&self) -> Result<Option<u64>> {
        maybe_get_int_attr(&self.attrs, "revCount")
    }

    /// `Input::getLastModified`.
    pub fn last_modified(&self) -> Result<Option<u64>> {
        maybe_get_int_attr(&self.attrs, "lastModified")
    }

    /// `InputScheme::getFingerprint`, for the schemes where it does not
    /// depend on the file system: a string that identifies the tree.
    pub fn fingerprint(&self) -> Result<Option<String>> {
        Ok(match self.scheme {
            Some(Scheme::GitHub | Scheme::GitLab | Scheme::SourceHut) => {
                self.rev()?.map(|rev| format!("github:{rev}"))
            }
            Some(Scheme::Git) => match self.rev()? {
                Some(rev) => Some(format!(
                    "git:{rev}{}{}{}",
                    if self.bool_attr("exportIgnore")? {
                        ";e"
                    } else {
                        ""
                    },
                    if self.bool_attr("lfs")? { ";l" } else { "" },
                    if self.bool_attr("__legacyExport")? {
                        ";legacy"
                    } else {
                        ""
                    }
                )),
                None => None,
            },
            Some(Scheme::Mercurial) => self
                .rev_hash()?
                .map(|rev| format!("hg:{}", rev.to_string(HashFormat::Base16, false))),
            Some(Scheme::Tarball) => match (self.nar_hash()?, self.rev()?) {
                (Some(h), _) => Some(format!("tarball:{h}")),
                (None, Some(rev)) => Some(format!("tarball:{rev}")),
                (None, None) => None,
            },
            _ => None,
        })
    }

    fn bool_attr(&self, name: &str) -> Result<bool> {
        Ok(maybe_get_bool_attr(&self.attrs, name)?.unwrap_or(false))
    }
}

impl fmt::Display for Input {
    /// The URL form, or the attrs as JSON for inputs that have no URL form.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.to_url(false) {
            Ok(url) => write!(f, "{url}"),
            Err(_) => f.write_str(&attrs_to_json_string(&self.attrs)),
        }
    }
}

impl Scheme {
    /// `InputScheme::inputFromURL`.
    fn input_from_url(
        self,
        settings: &FetchSettings,
        url: &ParsedUrl,
        require_tree: bool,
    ) -> Result<Option<Input>> {
        match self {
            Scheme::GitHub | Scheme::GitLab | Scheme::SourceHut => {
                if url.scheme != self.name() {
                    return Ok(None);
                }
                self.git_archive_from_url(settings, url).map(Some)
            }
            Scheme::Git => {
                if url.scheme != "git" && parse_url_scheme(&url.scheme).application != Some("git") {
                    return Ok(None);
                }
                git_from_url(settings, url).map(Some)
            }
            Scheme::Tarball | Scheme::File => {
                if !self.curl_url_is_valid(url, require_tree) {
                    return Ok(None);
                }
                self.curl_from_url(settings, url).map(Some)
            }
            Scheme::Mercurial => {
                if !matches!(
                    url.scheme.as_str(),
                    "hg+http" | "hg+https" | "hg+ssh" | "hg+file"
                ) {
                    return Ok(None);
                }
                hg_from_url(settings, url).map(Some)
            }
            Scheme::Path => {
                if url.scheme != "path" {
                    return Ok(None);
                }
                path_from_url(settings, url).map(Some)
            }
            Scheme::Indirect => {
                if url.scheme != "flake" {
                    return Ok(None);
                }
                indirect_from_url(settings, url).map(Some)
            }
        }
    }

    /// `InputScheme::inputFromAttrs`, after the allowed-attribute check.
    fn input_from_attrs(self, settings: &FetchSettings, attrs: Attrs) -> Result<Input> {
        let input = Input {
            scheme: Some(self),
            attrs,
            xp: settings.xp(),
        };
        match self {
            Scheme::GitHub | Scheme::GitLab | Scheme::SourceHut => {
                get_str_attr(&input.attrs, "owner")?;
                get_str_attr(&input.attrs, "repo")?;
                let ref_ = maybe_get_str_attr(&input.attrs, "ref")?;
                let rev = maybe_get_str_attr(&input.attrs, "rev")?;
                if let (Some(ref_), Some(rev)) = (ref_, rev) {
                    return Err(bad_url!(
                        "input {} contains both a commit hash ('{}') and a branch/tag name ('{}')",
                        attrs_to_json_string(&input.attrs),
                        rev,
                        ref_
                    ));
                }
                if let Some(rev) = rev {
                    // `Hash::parseAny(rev, SHA1)`.
                    GitRev::parse_sha1(rev, settings.xp())?;
                }
                if let Some(ref_) = ref_ {
                    if !is_legal_ref_name(ref_) {
                        return Err(bad_url!(
                            "input {} contains an invalid branch/tag name",
                            attrs_to_json_string(&input.attrs)
                        ));
                    }
                }
                if let Some(host) = maybe_get_str_attr(&input.attrs, "host")? {
                    if !HOST_REGEX.is_match(host) {
                        return Err(bad_url!(
                            "input {} contains an invalid instance host",
                            attrs_to_json_string(&input.attrs)
                        ));
                    }
                }
                Ok(input)
            }
            Scheme::Git => {
                let mut input = input;
                for name in ["verifyCommit", "keytype", "publicKey", "publicKeys"] {
                    if input.attrs.contains_key(name) {
                        settings.require_feature("verified-fetches")?;
                    }
                }
                maybe_get_bool_attr(&input.attrs, "verifyCommit")?;
                if let Some(ref_) = maybe_get_str_attr(&input.attrs, "ref")? {
                    if !is_legal_ref_name(ref_) {
                        return Err(bad_url!("invalid Git branch/tag name '{}'", ref_));
                    }
                }
                let url =
                    fix_git_url(get_str_attr(&input.attrs, "url")?, &*settings.warn)?.to_string();
                input.attrs.insert("url".into(), Attr::String(url));
                for name in ["shallow", "submodules", "allRefs"] {
                    maybe_get_bool_attr(&input.attrs, name)?;
                }
                Ok(input)
            }
            Scheme::Tarball | Scheme::File => Ok(input),
            Scheme::Mercurial => {
                parse_url(get_str_attr(&input.attrs, "url")?, false, &*settings.warn)?;
                if let Some(ref_) = maybe_get_str_attr(&input.attrs, "ref")? {
                    if !HG_REF_REGEX.is_match(ref_) {
                        return Err(bad_url!("invalid Mercurial branch/tag name '{}'", ref_));
                    }
                }
                Ok(input)
            }
            Scheme::Path => {
                get_str_attr(&input.attrs, "path")?;
                Ok(input)
            }
            Scheme::Indirect => {
                let id = get_str_attr(&input.attrs, "id")?;
                if !is_flake_id(id) {
                    return Err(bad_url!("'{}' is not a valid flake ID", id));
                }
                Ok(input)
            }
        }
    }

    /// `InputScheme::toURL`.
    fn to_url(self, input: &Input, abbreviate: bool) -> Result<ParsedUrl> {
        // A `url` attribute that is rendered again was normalised (Git) or
        // is warned about where it is parsed with the fetch settings at
        // hand (fetching a tarball). Nix repeats the warning about a
        // parameter without `=` each time it renders a tarball or
        // Mercurial input; brix does not.
        let rendering = |_: &str| {};
        match self {
            Scheme::GitHub | Scheme::GitLab | Scheme::SourceHut => {
                let owner = get_str_attr(&input.attrs, "owner")?;
                let repo = get_str_attr(&input.attrs, "repo")?;
                let mut path = vec![owner.to_string(), repo.to_string()];
                if let Some(ref_) = input.ref_()? {
                    path.push(ref_.to_string());
                }
                if let Some(rev) = input.rev()? {
                    path.push(if abbreviate {
                        rev.short()
                    } else {
                        rev.to_hex()
                    });
                }
                let mut url = ParsedUrl {
                    scheme: self.name().to_string(),
                    path,
                    ..Default::default()
                };
                if let Some(h) = input.nar_hash()? {
                    url.query.insert("narHash".into(), h.to_sri());
                }
                if let Some(host) = maybe_get_str_attr(&input.attrs, "host")? {
                    url.query.insert("host".into(), host.to_string());
                }
                Ok(url)
            }
            Scheme::Git => {
                let mut url = parse_url(get_str_attr(&input.attrs, "url")?, false, &rendering)?;
                if url.scheme != "git" {
                    url.scheme = format!("git+{}", url.scheme);
                }
                if let Some(rev) = input.rev()? {
                    url.query.insert("rev".into(), rev.to_hex());
                }
                if let Some(ref_) = input.ref_()? {
                    if !abbreviate || (ref_ != "master" && ref_ != "main") {
                        url.query.insert("ref".into(), ref_.to_string());
                    }
                }
                for name in [
                    "shallow",
                    "lfs",
                    "submodules",
                    "exportIgnore",
                    "verifyCommit",
                ] {
                    if input.bool_attr(name)? {
                        url.query.insert(name.into(), "1".into());
                    }
                }
                let public_keys = git_public_keys(&input.attrs)?;
                if public_keys.len() == 1 {
                    url.query
                        .insert("keytype".into(), public_keys[0].key_type.clone());
                    url.query
                        .insert("publicKey".into(), public_keys[0].key.clone());
                } else if public_keys.len() > 1 {
                    url.query
                        .insert("publicKeys".into(), public_keys_to_string(&public_keys));
                }
                Ok(url)
            }
            Scheme::Tarball | Scheme::File => {
                let mut url = parse_url(get_str_attr(&input.attrs, "url")?, false, &rendering)?;
                if let Some(h) = input.nar_hash()? {
                    url.query.insert("narHash".into(), h.to_sri());
                }
                Ok(url)
            }
            Scheme::Mercurial => {
                let mut url = parse_url(get_str_attr(&input.attrs, "url")?, false, &rendering)?;
                url.scheme = format!("hg+{}", url.scheme);
                if let Some(rev) = input.rev_hash()? {
                    url.query
                        .insert("rev".into(), rev.to_string(HashFormat::Base16, false));
                }
                if let Some(ref_) = input.ref_()? {
                    url.query.insert("ref".into(), ref_.to_string());
                }
                Ok(url)
            }
            Scheme::Path => {
                let mut query = attrs_to_query(&input.attrs);
                query.remove("path");
                query.remove("type");
                query.remove("__final");
                Ok(ParsedUrl {
                    scheme: "path".into(),
                    path: path_to_url_path(get_str_attr(&input.attrs, "path")?),
                    query,
                    ..Default::default()
                })
            }
            Scheme::Indirect => {
                let mut path = vec![get_str_attr(&input.attrs, "id")?.to_string()];
                if let Some(ref_) = input.ref_()? {
                    path.push(ref_.to_string());
                }
                if let Some(rev) = input.rev()? {
                    path.push(rev.to_hex());
                }
                Ok(ParsedUrl {
                    scheme: "flake".into(),
                    path,
                    ..Default::default()
                })
            }
        }
    }

    /// `InputScheme::applyOverrides`.
    fn apply_overrides(
        self,
        input: &Input,
        ref_: Option<&str>,
        rev: Option<GitRev>,
    ) -> Result<Input> {
        let mut res = input.clone();
        match self {
            Scheme::GitHub | Scheme::GitLab | Scheme::SourceHut => {
                if let (Some(rev), Some(ref_)) = (rev, ref_) {
                    return Err(bad_url!(
                        "cannot apply both a commit hash ({}) and a branch/tag name ('{}') to input '{}'",
                        rev,
                        ref_,
                        input
                    ));
                }
                if let Some(rev) = rev {
                    res.attrs.insert("rev".into(), Attr::String(rev.to_hex()));
                    res.attrs.remove("ref");
                }
                if let Some(ref_) = ref_ {
                    res.attrs.insert("ref".into(), Attr::String(ref_.into()));
                    res.attrs.remove("rev");
                }
                Ok(res)
            }
            Scheme::Git => {
                if let Some(rev) = rev {
                    res.attrs.insert("rev".into(), Attr::String(rev.to_hex()));
                }
                if let Some(ref_) = ref_ {
                    res.attrs.insert("ref".into(), Attr::String(ref_.into()));
                }
                if res.ref_()?.is_none() && res.rev()?.is_some() {
                    return Err(err!(
                        "Git input '{}' has a commit hash but no branch/tag name",
                        res
                    ));
                }
                Ok(res)
            }
            Scheme::Indirect | Scheme::Mercurial => {
                if let Some(rev) = rev {
                    res.attrs.insert("rev".into(), Attr::String(rev.to_hex()));
                }
                if let Some(ref_) = ref_ {
                    res.attrs.insert("ref".into(), Attr::String(ref_.into()));
                }
                Ok(res)
            }
            Scheme::Tarball | Scheme::File | Scheme::Path => {
                if let Some(ref_) = ref_ {
                    return Err(err!(
                        "don't know how to set branch/tag name of input '{}' to '{}'",
                        input,
                        ref_
                    ));
                }
                if let Some(rev) = rev {
                    return Err(err!(
                        "don't know how to set revision of input '{}' to '{}'",
                        input,
                        rev
                    ));
                }
                Ok(res)
            }
        }
    }

    /// `GitArchiveInputScheme::inputFromURL`.
    fn git_archive_from_url(self, settings: &FetchSettings, url: &ParsedUrl) -> Result<Input> {
        // Empty path segments are ignored for back-compat.
        let path: Vec<&str> = url.non_empty_path_segments().collect();
        let mut attrs = Attrs::new();
        match path.len() {
            3 => {
                if is_rev(path[2]) {
                    attrs.insert("rev".into(), Attr::String(path[2].into()));
                } else {
                    attrs.insert("ref".into(), Attr::String(path[2].into()));
                }
            }
            n if n > 3 => {
                attrs.insert("ref".into(), Attr::String(path[2..].join("/")));
            }
            n if n < 2 => return Err(bad_url!("URL '{}' is invalid", url)),
            _ => {}
        }
        for (name, value) in &url.query {
            match name.as_str() {
                "rev" => {
                    if attrs.contains_key("rev") {
                        return Err(bad_url!("URL '{}' contains multiple commit hashes", url));
                    }
                    attrs.insert("rev".into(), Attr::String(value.clone()));
                }
                "ref" => {
                    if attrs.contains_key("ref") {
                        return Err(bad_url!("URL '{}' contains multiple branch/tag names", url));
                    }
                    attrs.insert("ref".into(), Attr::String(value.clone()));
                }
                "host" | "narHash" => {
                    attrs.insert(name.clone(), Attr::String(value.clone()));
                }
                _ => {
                    return Err(bad_url!(
                        "URL '{}' contains unknown parameter '{}'",
                        url,
                        name
                    ))
                }
            }
        }
        attrs.insert("type".into(), Attr::String(self.name().into()));
        attrs.insert("owner".into(), Attr::String(path[0].into()));
        attrs.insert("repo".into(), Attr::String(path[1].into()));
        self.input_from_attrs(settings, attrs)
    }

    /// `CurlInputScheme::isValidURL`.
    fn curl_url_is_valid(self, url: &ParsedUrl, require_tree: bool) -> bool {
        let parts = parse_url_scheme(&url.scheme);
        if !matches!(parts.transport, "file" | "http" | "https") {
            return false;
        }
        match parts.application {
            Some(app) => app == self.name(),
            None => {
                let tarball_ext = has_tarball_extension(url);
                match self {
                    Scheme::Tarball => require_tree || tarball_ext,
                    _ => !require_tree && !tarball_ext,
                }
            }
        }
    }

    /// `CurlInputScheme::inputFromURL`.
    fn curl_from_url(self, settings: &FetchSettings, url: &ParsedUrl) -> Result<Input> {
        let mut url = url.clone();
        url.scheme = parse_url_scheme(&url.scheme).transport.to_string();
        let mut attrs = Attrs::new();
        if let Some(h) = url.query.get("narHash") {
            attrs.insert("narHash".into(), Attr::String(h.clone()));
        }
        if let Some(rev) = url.query.get("rev") {
            attrs.insert("rev".into(), Attr::String(rev.clone()));
        }
        // Malformed numbers are silently ignored, as in Nix.
        for name in ["revCount", "lastModified"] {
            if let Some(n) = url.query.get(name).and_then(|v| v.parse::<u64>().ok()) {
                attrs.insert(name.into(), Attr::Int(n));
            }
        }
        // The Nix-specific parameters are not part of the HTTP request.
        for name in self.allowed_attrs() {
            url.query.remove(*name);
        }
        attrs.insert("type".into(), Attr::String(self.name().into()));
        attrs.insert("url".into(), Attr::String(url.to_string()));
        Ok(Input {
            scheme: Some(self),
            attrs,
            xp: settings.xp(),
        })
    }
}

/// `CurlInputScheme::hasTarballExtension`.
pub fn has_tarball_extension(url: &ParsedUrl) -> bool {
    let Some(last) = url.path.last() else {
        return false;
    };
    [
        ".zip", ".tar", ".tgz", ".tar.gz", ".tar.xz", ".tar.bz2", ".tar.zst",
    ]
    .iter()
    .any(|ext| last.ends_with(ext))
}

/// `GitInputScheme::inputFromURL`.
fn git_from_url(settings: &FetchSettings, url: &ParsedUrl) -> Result<Input> {
    let mut url2 = url.clone();
    url2.query.clear();
    let mut attrs = Attrs::new();
    attrs.insert("type".into(), Attr::String("git".into()));
    for (name, value) in &url.query {
        match name.as_str() {
            "rev" | "ref" | "keytype" | "publicKey" | "publicKeys" => {
                attrs.insert(name.clone(), Attr::String(value.clone()));
            }
            "shallow" | "submodules" | "lfs" | "exportIgnore" | "allRefs" | "verifyCommit" => {
                attrs.insert(name.clone(), Attr::Bool(value == "1"));
            }
            _ => {
                url2.query.insert(name.clone(), value.clone());
            }
        }
    }
    attrs.insert("url".into(), Attr::String(url2.to_string()));
    Scheme::Git.input_from_attrs(settings, attrs)
}

/// `MercurialInputScheme::inputFromURL`: `hg+<transport>://…`, with `rev`
/// and `ref` taken out of the query.
fn hg_from_url(settings: &FetchSettings, url: &ParsedUrl) -> Result<Input> {
    let mut url2 = url.clone();
    url2.scheme = url.scheme[3..].to_string();
    url2.query.clear();
    let mut attrs = Attrs::new();
    attrs.insert("type".into(), Attr::String("hg".into()));
    for (name, value) in &url.query {
        if name == "rev" || name == "ref" {
            attrs.insert(name.clone(), Attr::String(value.clone()));
        } else {
            url2.query.insert(name.clone(), value.clone());
        }
    }
    attrs.insert("url".into(), Attr::String(url2.to_string()));
    Scheme::Mercurial.input_from_attrs(settings, attrs)
}

/// `PathInputScheme::inputFromURL`.
fn path_from_url(settings: &FetchSettings, url: &ParsedUrl) -> Result<Input> {
    if let Some(auth) = &url.authority {
        if !auth.host.is_empty() {
            return Err(err!(
                "path URL '{}' should not have an authority ('{}')",
                url,
                auth
            ));
        }
    }
    let mut attrs = Attrs::new();
    attrs.insert("type".into(), Attr::String("path".into()));
    attrs.insert("path".into(), Attr::String(url_path_to_path(&url.path)?));
    for (name, value) in &url.query {
        match name.as_str() {
            "rev" | "narHash" => {
                attrs.insert(name.clone(), Attr::String(value.clone()));
            }
            "revCount" | "lastModified" => match value.parse::<u64>() {
                Ok(n) => {
                    attrs.insert(name.clone(), Attr::Int(n));
                }
                Err(_) => return Err(err!("path URL '{}' has invalid parameter '{}'", url, name)),
            },
            _ => {
                return Err(err!(
                    "path URL '{}' has unsupported parameter '{}'",
                    url,
                    name
                ))
            }
        }
    }
    Ok(Input {
        scheme: Some(Scheme::Path),
        attrs,
        xp: settings.xp(),
    })
}

/// `IndirectInputScheme::inputFromURL`.
fn indirect_from_url(settings: &FetchSettings, url: &ParsedUrl) -> Result<Input> {
    let path: Vec<&str> = url.non_empty_path_segments().collect();
    let mut rev = None;
    let mut ref_ = None;
    match path.len() {
        1 => {}
        2 => {
            if is_rev(path[1]) {
                rev = Some(path[1]);
            } else if is_legal_ref_name(path[1]) {
                ref_ = Some(path[1]);
            } else {
                return Err(bad_url!(
                    "in flake URL '{}', '{}' is not a commit hash or branch/tag name",
                    url,
                    path[1]
                ));
            }
        }
        3 => {
            if !is_legal_ref_name(path[1]) {
                return Err(bad_url!(
                    "in flake URL '{}', '{}' is not a branch/tag name",
                    url,
                    path[1]
                ));
            }
            ref_ = Some(path[1]);
            if !is_rev(path[2]) {
                return Err(bad_url!(
                    "in flake URL '{}', '{}' is not a commit hash",
                    url,
                    path[2]
                ));
            }
            rev = Some(path[2]);
        }
        _ => return Err(bad_url!("GitHub URL '{}' is invalid", url)),
    }
    let id = path[0];
    if !is_flake_id(id) {
        return Err(bad_url!("'{}' is not a valid flake ID", id));
    }
    let mut attrs = Attrs::new();
    attrs.insert("type".into(), Attr::String("indirect".into()));
    attrs.insert("id".into(), Attr::String(id.into()));
    if let Some(rev) = rev {
        attrs.insert(
            "rev".into(),
            Attr::String(GitRev::parse(rev, settings.xp())?.to_hex()),
        );
    }
    if let Some(ref_) = ref_ {
        attrs.insert("ref".into(), Attr::String(ref_.into()));
    }
    Scheme::Indirect.input_from_attrs(settings, attrs)
}

/// `fetchers::PublicKey`: a key used for commit signature verification.
/// The fields are in the order nlohmann's sorted objects write them
/// (`publicKeys_to_string`).
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct PublicKey {
    pub key: String,
    #[serde(rename = "type")]
    pub key_type: String,
}

impl PublicKey {
    /// `adl_serializer<PublicKey>::from_json`: an object with a string
    /// `type` (default `ssh-ed25519`), looked at first, and a string
    /// `key`; other members are ignored. The errors are `json-utils.cc`'s.
    fn from_json(json: &serde_json::Value) -> Result<PublicKey> {
        let obj = json_expect(json, "object", serde_json::Value::as_object)?;
        let key_type = match obj.get("type") {
            Some(t) => json_expect(t, "string", serde_json::Value::as_str)?.to_owned(),
            None => "ssh-ed25519".to_owned(),
        };
        let key = obj.get("key").ok_or_else(|| {
            err!(
                "Expected JSON object to contain key 'key' but it doesn't: {}",
                json_dump(json)
            )
        })?;
        Ok(PublicKey {
            key: json_expect(key, "string", serde_json::Value::as_str)?.to_owned(),
            key_type,
        })
    }
}

/// `json-utils.cc`'s `ensureType`: `value` as the JSON type `expected`
/// (which `get` takes out), or Nix's error.
fn json_expect<'v, T>(
    value: &'v serde_json::Value,
    expected: &str,
    get: impl FnOnce(&'v serde_json::Value) -> Option<T>,
) -> Result<T> {
    use serde_json::Value;
    get(value).ok_or_else(|| {
        let actual = match value {
            Value::Null => "null",
            Value::Bool(_) => "boolean",
            Value::Number(_) => "number",
            Value::String(_) => "string",
            Value::Array(_) => "array",
            Value::Object(_) => "object",
        };
        err!(
            "Expected JSON value to be of type '{}' but it is of type '{}': {}",
            expected,
            actual,
            json_dump(value)
        )
    })
}

/// nlohmann's compact `dump()`: object members sorted by name.
fn json_dump(value: &serde_json::Value) -> String {
    use serde_json::Value;
    fn sorted(value: &Value) -> Value {
        match value {
            Value::Object(map) => {
                let mut members: Vec<(&String, &Value)> = map.iter().collect();
                members.sort_by(|a, b| a.0.cmp(b.0));
                Value::Object(
                    members
                        .into_iter()
                        .map(|(name, v)| (name.clone(), sorted(v)))
                        .collect(),
                )
            }
            Value::Array(items) => Value::Array(items.iter().map(sorted).collect()),
            other => other.clone(),
        }
    }
    sorted(value).to_string()
}

/// `getPublicKeys`: the keys from the `publicKeys` (a JSON array) and
/// `publicKey` (+ `keytype`) attributes.
///
/// A `publicKeys` that is not JSON fails with nlohmann's exception
/// (`[json.exception.parse_error.101] …`), which takes no traces.
pub fn git_public_keys(attrs: &Attrs) -> Result<Vec<PublicKey>> {
    let mut keys = Vec::new();
    if let Some(json) = maybe_get_str_attr(attrs, "publicKeys")? {
        let parsed = crate::json::parse(json.as_bytes())?;
        for key in json_expect(&parsed, "array", serde_json::Value::as_array)? {
            keys.push(PublicKey::from_json(key)?);
        }
    }
    if let Some(key) = maybe_get_str_attr(attrs, "publicKey")? {
        keys.push(PublicKey {
            key_type: maybe_get_str_attr(attrs, "keytype")?
                .unwrap_or("ssh-ed25519")
                .to_string(),
            key: key.to_string(),
        });
    }
    Ok(keys)
}

/// `publicKeys_to_string`: the JSON array form.
pub fn public_keys_to_string(keys: &[PublicKey]) -> String {
    serde_json::to_string(keys).expect("public keys serialise")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attrs::attrs;

    fn settings() -> FetchSettings {
        FetchSettings::default()
    }

    /// `getPublicKeys`: `publicKeys` (JSON, as given) before `publicKey`,
    /// and `json-utils.cc`'s errors for JSON of the wrong shape (texts
    /// from the reference `nix`).
    #[test]
    fn public_keys_parse_like_nix() {
        let keys = |json: Option<&str>, key: Option<&str>| {
            let mut a = Attrs::new();
            if let Some(json) = json {
                a.insert("publicKeys".into(), Attr::String(json.into()));
            }
            if let Some(key) = key {
                a.insert("publicKey".into(), Attr::String(key.into()));
                a.insert("keytype".into(), Attr::String("ssh-rsa".into()));
            }
            git_public_keys(&a)
        };
        let pk = |t: &str, k: &str| PublicKey {
            key_type: t.into(),
            key: k.into(),
        };
        assert_eq!(
            keys(
                Some(r#"[{"key":"A"},{"type":"ssh-rsa","key":"B","extra":1}]"#),
                Some("C")
            )
            .unwrap(),
            [
                pk("ssh-ed25519", "A"),
                pk("ssh-rsa", "B"),
                pk("ssh-rsa", "C")
            ]
        );
        assert!(keys(Some("[]"), None).unwrap().is_empty());
        for (json, msg) in [
            (
                r#"[{"type":"ssh-rsa"}]"#,
                r#"Expected JSON object to contain key 'key' but it doesn't: {"type":"ssh-rsa"}"#,
            ),
            (
                r#"["foo"]"#,
                r#"Expected JSON value to be of type 'object' but it is of type 'string': "foo""#,
            ),
            (
                r#"{"key":"x"}"#,
                r#"Expected JSON value to be of type 'array' but it is of type 'object': {"key":"x"}"#,
            ),
            (
                r#"[{"key":1}]"#,
                "Expected JSON value to be of type 'string' but it is of type 'number': 1",
            ),
            (
                r#"[{"type":2,"key":"AAAA"}]"#,
                "Expected JSON value to be of type 'string' but it is of type 'number': 2",
            ),
            (
                r#"[{"type":null}]"#,
                "Expected JSON value to be of type 'string' but it is of type 'null': null",
            ),
        ] {
            assert_eq!(
                keys(Some(json), None).unwrap_err().to_string(),
                msg,
                "{json}"
            );
        }
        assert_eq!(
            public_keys_to_string(&[pk("ssh-ed25519", "A")]),
            r#"[{"key":"A","type":"ssh-ed25519"}]"#
        );
        // JSON that does not parse: nlohmann's exception, which takes no
        // traces.
        let err = keys(Some("notjson"), None).unwrap_err();
        assert!(err.is_foreign());
        assert_eq!(
            err.to_string(),
            "[json.exception.parse_error.101] parse error at line 1, column 2: syntax error while parsing value - invalid literal; last read: 'no'"
        );
    }

    #[test]
    fn ref_name_legality() {
        for ok in [
            "A/b",
            "AaA/b",
            "FOO/BAR/BAZ",
            "HEAD",
            "refs/tags/1.2.3",
            "refs/heads/master",
            "main",
            "v1.0",
            "feature-x_y",
        ] {
            assert!(is_legal_ref_name(ok), "{ok}");
        }
        for bad in [
            "", "@", "a..b", ".hidden", "foo/", "foo.", "a b", "a~b", "a^b", "a:b", "a?b", "a*b",
            "a[b", "a\\b", "a@{b", "foo.lock", "a//b", "a\x7fb",
        ] {
            assert!(!is_legal_ref_name(bad), "{bad}");
        }
    }

    #[test]
    fn git_strips_plus_prefix_in_attrs() {
        let input = Input::from_attrs(
            &settings(),
            attrs([
                ("url", "git+ssh://git@github.com/NixOS/nixpkgs".into()),
                ("type", "git".into()),
            ]),
        )
        .unwrap();
        assert_eq!(
            input.attrs,
            attrs([
                ("url", "ssh://git@github.com/NixOS/nixpkgs".into()),
                ("type", "git".into()),
            ])
        );
        assert_eq!(
            input.to_url_text(false).unwrap(),
            "git+ssh://git@github.com/NixOS/nixpkgs"
        );
        let again = Input::from_attrs(&settings(), input.attrs.clone()).unwrap();
        assert_eq!(again, input);
    }

    #[test]
    fn github_rejects_unknown_params() {
        let err = Input::from_url_str(&settings(), "github:a/b?tag=foo", true).unwrap_err();
        assert!(err.to_string().contains("tag"), "{err}");
    }

    #[test]
    fn unknown_type_is_kept_raw() {
        let input = Input::from_attrs(
            &settings(),
            attrs([("type", "darcs".into()), ("url", "https://x".into())]),
        )
        .unwrap();
        assert_eq!(input.scheme, None);
        assert!(!input.is_locked(&settings()).unwrap());
        assert!(input.to_url(false).is_err());
    }

    /// `hg` inputs parse, print and lock as in Nix (`native::mercurial`
    /// fetches them).
    #[test]
    fn mercurial_inputs() {
        let s = settings();
        let url = "hg+https://x/y?ref=default&rev=2aae6c35c94fcfb415dbe95f408b9ce91ee846ed";
        let input = Input::from_url_str(&s, url, true).unwrap();
        assert_eq!(input.scheme, Some(Scheme::Mercurial));
        assert_eq!(get_str_attr(&input.attrs, "url").unwrap(), "https://x/y");
        assert_eq!(input.to_url_text(false).unwrap(), url);
        assert!(input.is_locked(&s).unwrap());
        assert_eq!(
            input.fingerprint().unwrap().as_deref(),
            Some("hg:2aae6c35c94fcfb415dbe95f408b9ce91ee846ed")
        );
        let unlocked = Input::from_url_str(&s, "hg+file:///r?ref=stable", true).unwrap();
        assert!(!unlocked.is_locked(&s).unwrap());
        assert_eq!(
            Input::from_url_str(&s, "hg+https://x/y?ref=-bad", true)
                .unwrap_err()
                .to_string(),
            "invalid Mercurial branch/tag name '-bad'"
        );
        // `refRegex`, as `nix eval --expr 'builtins.parseFlakeRef …'` has it.
        let input = Input::from_url_str(&s, "hg+https://x/y?ref=rel@1.0%2Bx", true).unwrap();
        assert_eq!(input.ref_().unwrap(), Some("rel@1.0+x"));
        assert_eq!(
            Input::from_url_str(&s, "hg+https://x/y?ref=a%204", true)
                .unwrap_err()
                .to_string(),
            "invalid Mercurial branch/tag name 'a 4'"
        );
        assert!(Input::from_attrs(
            &s,
            attrs([
                ("type", "hg".into()),
                ("url", "https://x".into()),
                ("lastModified", 1u64.into())
            ])
        )
        .is_err());
    }

    #[test]
    fn locking_rules() {
        let s = settings();
        let gh = |a: Attrs| Input::from_attrs(&s, a).unwrap();
        let base = attrs([
            ("type", "github".into()),
            ("owner", "o".into()),
            ("repo", "r".into()),
        ]);
        assert!(!gh(base.clone()).is_locked(&s).unwrap());
        let mut with_rev = base.clone();
        with_rev.insert(
            "rev".into(),
            "2aae6c35c94fcfb415dbe95f408b9ce91ee846ed".into(),
        );
        assert!(gh(with_rev.clone()).is_locked(&s).unwrap());
        let strict = FetchSettings {
            trust_tarballs_from_git_forges: false,
            ..settings()
        };
        assert!(!gh(with_rev.clone()).is_locked(&strict).unwrap());
        with_rev.insert(
            "narHash".into(),
            "sha256-hM20uyap1a0M9d344I692r+ik4gTMyj60cQWO+hAYP8=".into(),
        );
        assert!(gh(with_rev).is_locked(&strict).unwrap());

        let git = Input::from_url_str(
            &s,
            "git+https://x/y?rev=0000000000000000000000000000000000000000",
            true,
        )
        .unwrap();
        assert!(!git.is_locked(&s).unwrap());
        let path = Input::from_url_str(
            &s,
            "path:/foo?narHash=sha256-hM20uyap1a0M9d344I692r+ik4gTMyj60cQWO+hAYP8=",
            true,
        )
        .unwrap();
        assert!(path.is_locked(&s).unwrap());
        // A malformed revision is an error, not "unlocked".
        let mut bad = Input::from_attrs(&s, base.clone()).unwrap();
        bad.attrs.insert("rev".into(), "nope".into());
        assert!(bad.is_locked(&s).is_err());
        let mut named = path.clone();
        named.attrs.insert("name".into(), 1u64.into());
        assert!(named.name().is_err());
        let rel = Input::from_url_str(&s, "path:./foo", true).unwrap();
        assert_eq!(rel.is_relative().as_deref(), Some("./foo"));
    }

    #[test]
    fn curl_scheme_selection() {
        let s = settings();
        let t = Input::from_url_str(&s, "https://example.org/x.tar.gz", false).unwrap();
        assert_eq!(t.scheme, Some(Scheme::Tarball));
        let f = Input::from_url_str(&s, "https://example.org/index.html", false).unwrap();
        assert_eq!(f.scheme, Some(Scheme::File));
        let t2 = Input::from_url_str(&s, "https://example.org/index.html", true).unwrap();
        assert_eq!(t2.scheme, Some(Scheme::Tarball));
        let f2 = Input::from_url_str(&s, "file+https://example.org/x.tar.gz", true).unwrap();
        assert_eq!(f2.scheme, Some(Scheme::File));
        assert_eq!(
            get_str_attr(&f2.attrs, "url").unwrap(),
            "https://example.org/x.tar.gz"
        );
        let hint = Input::from_url_str(&s, "file+git://x/y", true).unwrap_err();
        assert!(hint.to_string().contains("did you mean 'git+file'"));
    }

    #[test]
    fn fingerprints() {
        let s = settings();
        let gh = Input::from_url_str(
            &s,
            "github:o/r/2aae6c35c94fcfb415dbe95f408b9ce91ee846ed",
            true,
        )
        .unwrap();
        assert_eq!(
            gh.fingerprint().unwrap().as_deref(),
            Some("github:2aae6c35c94fcfb415dbe95f408b9ce91ee846ed")
        );
        let git = Input::from_url_str(
            &s,
            "git+https://x/y?rev=2aae6c35c94fcfb415dbe95f408b9ce91ee846ed&lfs=1",
            true,
        )
        .unwrap();
        assert_eq!(
            git.fingerprint().unwrap().as_deref(),
            Some("git:2aae6c35c94fcfb415dbe95f408b9ce91ee846ed;l")
        );
    }
}
