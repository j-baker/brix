//! Git LFS for Git inputs with `lfs = true` (`src/libfetchers/
//! git-lfs-fetch.cc`, used by `GitSourceAccessor::readBlob` in
//! `git-utils.cc`).
//!
//! When Nix reads a file of an exported commit whose `filter` attribute
//! is `lfs` (see [`super::git_attrs`]), it treats the blob as a Git LFS
//! pointer (`version https://git-lfs.github.com/spec/v1`, `oid
//! sha256:<hex>`, `size <n>`) and reads the object it names instead:
//!
//! - from `<cache>/git-lfs/<sha256 of the file's path>/<oid>` if an
//!   earlier fetch put it there;
//! - else it asks the LFS server for it with the batch API (`POST
//!   <endpoint>/objects/batch`, operation `download`), downloads it from
//!   the URL the answer gives (with the answer's `Authorization`), checks
//!   its size and SHA-256 against the pointer and keeps it in the cache.
//!
//! A blob of 1024 bytes or more, or one that is no valid pointer, is read
//! as it is, with a warning. The endpoint follows from the repository's
//! remote: the URL a clone was fetched from, or a local checkout's
//! `origin` (after `url.<base>.insteadOf`), normalised by `fixGitURL`.
//! For HTTP(S) it is `<remote>.git/info/lfs` (`.git` added unless there);
//! for SSH, `ssh [user@]host git-lfs-authenticate <path> download` says
//! it and the `Authorization` to send. (Nix's look-up of `lfs.url` is
//! guarded by an inverted check, so it never applies; neither does
//! `.lfsconfig`.)
//!
//! As in Nix, all of this happens when a file is read ([`LfsAccessor`]):
//! a tree nobody hashes (a flake's own, under lazy trees) fetches only
//! the objects of the files the evaluation reads, and a file that is no
//! valid pointer is warned about each time it is read. brix's fetch cache
//! keeps no copy of such a tree, only its NAR hash once something has
//! computed it (see `NativeFetcher::record`), as Nix's fetch-to-store
//! cache does. A working tree is read as it is on disk, without LFS: Nix
//! only smudges exported commits.

use std::collections::HashMap;
use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use brix_store::accessor::{AccessorError, DirEntries, FileType, SourceAccessor, Stat};
use brix_store::shell::escape_shell_arg_always;
use brix_store::url::{parse_url, Authority, ParsedUrl};
use brix_store::{BStr, BString, CanonPath, Hash, HashAlgo, HashFormat};
use gix::bstr::ByteSlice;
use serde_json::Value;

use super::git_attrs::{AttrValue, GitAttributes};
use super::http::HttpClient;
use crate::input::{FetchSettings, Input, Scheme};
use crate::url::fix_git_url;
use crate::{err, Result};

/// Whether the tree a fetch of `input` gave smudges Git LFS files as they
/// are read: a Git commit (it has a `rev`; a working tree is fetched
/// another way) with `lfs`.
pub fn smudges_lazily(input: &Input) -> Result<bool> {
    Ok(input.scheme == Some(Scheme::Git)
        && crate::attrs::maybe_get_bool_attr(&input.attrs, "lfs")?.unwrap_or(false)
        && input.rev()?.is_some())
}

/// What smudging needs from the fetch it is part of.
pub struct Smudge<'a> {
    pub http: &'a HttpClient,
    /// brix's cache directory; objects are kept in its `git-lfs`.
    pub cache_dir: &'a Path,
    pub settings: &'a FetchSettings,
    /// Where warnings while the accessor is made go; those of later reads
    /// go to `settings.warn`, which outlives the fetch.
    pub warn: &'a dyn Fn(&str),
}

impl Smudge<'_> {
    /// `GitSourceAccessor` with `smudgeLfs`: `tree`, the export of a commit
    /// of `repo` (its own files, without submodules), with every file
    /// whose `filter` attribute (`attrs`) is `lfs` read as the LFS object
    /// its pointer names. `remote` is the URL `repo` is a clone of, `None`
    /// for a local checkout (whose `origin` is the remote). The remote's
    /// URL is found now, as Nix finds it when it makes the accessor: one
    /// that gives none fails before any file is read.
    pub fn accessor(
        &self,
        repo: &gix::Repository,
        remote: Option<&str>,
        tree: Arc<dyn SourceAccessor>,
        attrs: Arc<GitAttributes>,
    ) -> Result<LfsAccessor> {
        Ok(LfsAccessor {
            fetch: Fetch::new(self, repo, remote)?,
            inner: tree,
            attrs,
            lfs_paths: Mutex::new(HashMap::new()),
        })
    }
}

/// A commit's tree whose Git LFS files are smudged when they are read
/// (`GitSourceAccessor::readBlob`): each read of such a file reads the
/// object from the cache or fetches it, or, for a blob that is no valid
/// pointer, warns and reads the blob, every time, as Nix does. Its size
/// is not known before it is read.
pub struct LfsAccessor {
    inner: Arc<dyn SourceAccessor>,
    attrs: Arc<GitAttributes>,
    fetch: Fetch,
    /// `shouldFetch`'s answer for each path asked about.
    lfs_paths: Mutex<HashMap<CanonPath, bool>>,
}

impl LfsAccessor {
    /// `Fetch::shouldFetch`: whether `path`'s `filter` attribute is `lfs`.
    fn should_fetch(&self, path: &CanonPath) -> std::result::Result<bool, AccessorError> {
        let poisoned = |_| AccessorError::Io("an earlier Git LFS lookup panicked".into());
        if let Some(&known) = self.lfs_paths.lock().map_err(poisoned)?.get(path) {
            return Ok(known);
        }
        let filter = self
            .attrs
            .get(path, "filter")
            .map_err(|e| AccessorError::Io(format!("cannot get git-lfs attribute: {e}").into()))?;
        let lfs = filter == Some(AttrValue::Value(b"lfs".to_vec()));
        self.lfs_paths
            .lock()
            .map_err(poisoned)?
            .insert(path.clone(), lfs);
        Ok(lfs)
    }

    /// `readBlob` for a file [`LfsAccessor::should_fetch`] says is one:
    /// the object `blob` points to, or `blob` itself when it is no
    /// pointer.
    fn smudge(
        &self,
        path: &CanonPath,
        blob: Vec<u8>,
    ) -> std::result::Result<Vec<u8>, AccessorError> {
        match self.fetch.fetch(&blob, path) {
            Ok(Some(object)) => Ok(object),
            Ok(None) => Ok(blob),
            Err(e) => Err(accessor_error(
                e.trace(format!("while smudging git-lfs file '{path}'")),
            )),
        }
    }
}

/// An error of this crate as an accessor's, its traces kept.
fn accessor_error(e: crate::Error) -> AccessorError {
    // `traces()` is outermost first; the innermost is wrapped first.
    e.traces()
        .into_iter()
        .rev()
        .fold(AccessorError::Io(e.message().into()), |inner, trace| {
            AccessorError::traced(inner, trace)
        })
}

impl SourceAccessor for LfsAccessor {
    fn maybe_lstat(&self, path: &CanonPath) -> std::result::Result<Option<Stat>, AccessorError> {
        match self.inner.maybe_lstat(path)? {
            Some(stat) => Ok(Some(self.smudged_stat(path, stat)?)),
            None => Ok(None),
        }
    }

    fn lstat(&self, path: &CanonPath) -> std::result::Result<Stat, AccessorError> {
        let stat = self.inner.lstat(path)?;
        self.smudged_stat(path, stat)
    }

    fn read_dir(&self, path: &CanonPath) -> std::result::Result<DirEntries, AccessorError> {
        self.inner.read_dir(path)
    }

    fn read_file(&self, path: &CanonPath) -> std::result::Result<Vec<u8>, AccessorError> {
        let blob = self.inner.read_file(path)?;
        if self.should_fetch(path)? {
            return self.smudge(path, blob);
        }
        Ok(blob)
    }

    fn read_file_to(
        &self,
        path: &CanonPath,
        out: &mut dyn Write,
    ) -> std::result::Result<u64, AccessorError> {
        if !self.should_fetch(path)? {
            return self.inner.read_file_to(path, out);
        }
        let data = self.read_file(path)?;
        out.write_all(&data)
            .map_err(|e| AccessorError::Io(e.to_string().into()))?;
        Ok(data.len() as u64)
    }

    fn read_link(&self, path: &CanonPath) -> std::result::Result<BString, AccessorError> {
        let target = self.inner.read_link(path)?;
        if !self.should_fetch(path)? {
            return Ok(target);
        }
        // `readLink` reads a symlink's target through `readBlob` too.
        let smudged = self.smudge(path, target.into())?;
        Ok(smudged.into())
    }

    fn display_prefix(&self) -> &BStr {
        self.inner.display_prefix()
    }

    fn show_path(&self, path: &CanonPath) -> BString {
        self.inner.show_path(path)
    }

    // No `physical_path` and no `precomputed_nar_hash`: both would give
    // the pointers, not the files.
}

impl LfsAccessor {
    /// `stat` of a file the LFS filter marks, without its size, which is
    /// the object's, known once it is read (Nix's Git accessor gives none
    /// either).
    fn smudged_stat(
        &self,
        path: &CanonPath,
        mut stat: Stat,
    ) -> std::result::Result<Stat, AccessorError> {
        if stat.file_type == FileType::Regular && self.should_fetch(path)? {
            stat.size = None;
        }
        Ok(stat)
    }
}

/// A git-lfs pointer (`lfs::Pointer`): the object's SHA-256, as written
/// (64 hexadecimal digits), and its size.
#[derive(Debug, PartialEq, Eq)]
struct Pointer {
    oid: String,
    size: u64,
}

/// `parseLfsPointer`: `None` for what is no v1 pointer. Lines other than
/// `version`, `oid sha256:` and `size` (extensions) are ignored, and the
/// last `oid` and `size` count. A size too large for 64 bits is the
/// error `std::stoul` throws.
fn parse_pointer(content: &[u8]) -> Result<Option<Pointer>> {
    if !content.starts_with(b"version https://git-lfs.github.com/spec/v1") {
        return Ok(None);
    }
    let mut oid: &[u8] = b"";
    let mut size: &[u8] = b"";
    for line in content.split(|&b| b == b'\n').filter(|l| !l.is_empty()) {
        if line.starts_with(b"version ") {
            continue;
        }
        if let Some(rest) = line.strip_prefix(b"oid sha256:") {
            oid = rest;
        } else if let Some(rest) = line.strip_prefix(b"size ") {
            size = rest;
        }
    }
    if oid.len() != 64 || !oid.iter().all(u8::is_ascii_hexdigit) {
        return Ok(None);
    }
    if size.is_empty() || !size.iter().all(u8::is_ascii_digit) {
        return Ok(None);
    }
    let size = std::str::from_utf8(size)
        .expect("ASCII digits")
        .parse()
        .map_err(|_| err!("stoul"))?;
    Ok(Some(Pointer {
        oid: String::from_utf8(oid.to_vec()).expect("ASCII hex digits"),
        size,
    }))
}

/// What the LFS batch API is asked through (`lfs::LfsApiInfo`).
#[derive(Debug, PartialEq, Eq)]
struct LfsApi {
    endpoint: String,
    auth_header: Option<String>,
}

/// `lfs::Fetch`: what smudging the files of one commit needs, kept with
/// the tree for as long as it is read.
struct Fetch {
    http: HttpClient,
    /// The fetch's settings: downloads follow them, and warnings go to
    /// their `warn`.
    settings: FetchSettings,
    /// `getCacheDir() / "git-lfs"`.
    cache_dir: PathBuf,
    /// The remote: `fixGitURL(<remote URL>).canonicalise()`.
    url: ParsedUrl,
}

impl Fetch {
    /// `Fetch::Fetch`: the remote's URL, from `remote` or else `origin`
    /// (`getLfsEndpointUrl`), made a canonical URL. `fixGitURL` warns
    /// about a relative SCP-style path.
    fn new(ctx: &Smudge<'_>, repo: &gix::Repository, remote: Option<&str>) -> Result<Fetch> {
        let remote_url = remote_url(repo, remote)?;
        let url = fix_git_url(&remote_url, ctx.warn)?;
        Ok(Fetch {
            http: ctx.http.clone(),
            settings: ctx.settings.clone(),
            cache_dir: ctx.cache_dir.join("git-lfs"),
            url: canonicalise(url)?,
        })
    }

    fn warn(&self, msg: &str) {
        (self.settings.warn)(msg);
    }

    /// `Fetch::fetch`: what a file whose blob is `content` reads as.
    /// `None` when it stays as it is (it is no pointer, after a warning).
    fn fetch(&self, content: &[u8], path: &CanonPath) -> Result<Option<Vec<u8>>> {
        if content.len() >= 1024 {
            self.warn(&format!(
                "encountered file '{path}' that should have been a git-lfs pointer, but is too large"
            ));
            return Ok(None);
        }
        let Some(pointer) = parse_pointer(content)? else {
            self.warn(&format!(
                "encountered file '{path}' that should have been a git-lfs pointer, but is invalid"
            ));
            return Ok(None);
        };

        let key =
            Hash::of(HashAlgo::Sha256, path.rel().as_bytes()).to_string(HashFormat::Base16, false);
        let cache_path = self.cache_dir.join(key).join(&pointer.oid);
        if let Some(data) = read_cached(&cache_path)? {
            return Ok(Some(data));
        }

        let objects = self.fetch_urls(&pointer)?;
        // Nix takes the first object without checking that there is one.
        let obj = objects.first().ok_or_else(|| {
            err!(
                "bad json from /info/lfs/objects/batch: the response lists no objects for '{}'",
                path
            )
        })?;
        // `obj.at("actions").at("download").at("href")`.
        let download = batch_at(obj, batch_at(obj, obj, "actions")?, "download")?;
        let href = json_string(batch_at(obj, download, "href")?)?;
        let auth_header = match download.get("header").and_then(|h| h.get("Authorization")) {
            Some(value) => Some(json_string(value)?),
            None => None,
        };
        // The committed pointer's oid and size are what the object is
        // checked against; the server's must agree with them.
        let obj_oid = json_object_string(obj, "oid")?;
        let obj_size = json_object_unsigned(obj, "size")?;
        if obj_oid != pointer.oid || obj_size != pointer.size {
            return Err(err!(
                "LFS server returned mismatched oid/size for '{}' (got oid={} size={}, expected oid={} size={})",
                path,
                obj_oid,
                obj_size,
                pointer.oid,
                pointer.size
            ));
        }
        let data = self.download(&href, auth_header.as_deref(), &pointer)?;
        write_cached(&cache_path, &data)?;
        Ok(Some(data))
    }

    /// `Fetch::fetchUrls`: the batch API's answer for `pointer`, its
    /// `objects`.
    fn fetch_urls(&self, pointer: &Pointer) -> Result<Vec<Value>> {
        let api = self.lfs_api()?;
        let url = parse_url(
            &format!("{}/objects/batch", api.endpoint),
            false,
            &*self.settings.warn,
        )?;
        let mut headers = Vec::new();
        if let Some(auth) = api.auth_header {
            headers.push(("Authorization".to_owned(), auth));
        }
        headers.push((
            "Content-Type".to_owned(),
            "application/vnd.git-lfs+json".to_owned(),
        ));
        headers.push((
            "Accept".to_owned(),
            "application/vnd.git-lfs+json".to_owned(),
        ));
        // nlohmann's `dump()` of `{"operation": …, "objects": […]}`, whose
        // keys it sorts.
        let payload = format!(
            r#"{{"objects":[{{"oid":"{}","size":{}}}],"operation":"download"}}"#,
            pointer.oid, pointer.size
        );
        let response = self.http.upload(
            &url.to_string(),
            &headers,
            payload.as_bytes(),
            &self.settings,
        )?;
        let response: Value = serde_json::from_slice(&response.data).map_err(|e| {
            err!(
                "response did not parse as json: {}",
                json_parse_error(&response.data, &e)
            )
        })?;
        // `resp.contains("objects")`, then the elements of whatever it is.
        match response.get("objects") {
            None => Err(err!("response does not contain 'objects'")),
            Some(Value::Array(objects)) => Ok(objects.clone()),
            Some(Value::Object(map)) => {
                let mut entries: Vec<_> = map.iter().collect();
                entries.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
                Ok(entries.into_iter().map(|(_, v)| v.clone()).collect())
            }
            Some(Value::Null) => Ok(Vec::new()),
            Some(other) => Ok(vec![other.clone()]),
        }
    }

    /// `getLfsApi`: the batch API's endpoint for the remote, and the
    /// `Authorization` to send it.
    fn lfs_api(&self) -> Result<LfsApi> {
        let Some(authority) = &self.url.authority else {
            // Nix asserts that the URL has an authority.
            return Err(err!(
                "cannot find the Git LFS server of '{}': the URL has no authority",
                self.url
            ));
        };
        if self.url.scheme == "ssh" {
            return git_lfs_authenticate(&self.url, authority);
        }
        Ok(LfsApi {
            endpoint: http_endpoint(&self.url),
            auth_header: None,
        })
    }

    /// `downloadToSink`: the object at `url`, checked against `pointer`.
    fn download(&self, url: &str, auth_header: Option<&str>, pointer: &Pointer) -> Result<Vec<u8>> {
        let parsed = parse_url(url, false, &*self.settings.warn)?;
        let headers: Vec<(String, String)> = auth_header
            .map(|auth| ("Authorization".to_owned(), auth.to_owned()))
            .into_iter()
            .collect();
        let data = self
            .http
            .download(&parsed.to_string(), &headers, &self.settings)?
            .data;
        if data.len() as u64 != pointer.size {
            return Err(err!(
                "size mismatch while fetching {}: expected {} but got {}",
                url,
                pointer.size,
                data.len()
            ));
        }
        let actual = Hash::of(HashAlgo::Sha256, &data).to_string(HashFormat::Base16, false);
        if actual != pointer.oid {
            return Err(err!(
                "hash mismatch while fetching {}: expected sha256:{} but got sha256:{}",
                url,
                pointer.oid,
                actual
            ));
        }
        Ok(data)
    }
}

/// nlohmann's message for JSON it cannot parse (`e.what()`), which Nix
/// reports; serde's explanation only for what nlohmann would accept.
fn json_parse_error(input: &[u8], e: &serde_json::Error) -> String {
    match brix_store::json::nlohmann_parse_error(input) {
        Some(message) => message,
        None => e.to_string(),
    }
}

/// `getLfsEndpointUrl`: the remote's URL. A clone brix made is of
/// `remote`; Nix records that as the clone's `origin`. A local checkout's
/// is its `origin`'s, `""` if it has none. libgit2's `git_remote_lookup`
/// applies `url.<base>.insteadOf`.
fn remote_url(repo: &gix::Repository, remote: Option<&str>) -> Result<String> {
    let config = repo.config_snapshot();
    let url = match remote {
        Some(url) => url.to_owned(),
        None => match config.string("remote.origin.url") {
            None => return Ok(String::new()),
            Some(url) => url
                .to_str()
                .map_err(|_| err!("the URL of remote 'origin' is not valid UTF-8: '{}'", url))?
                .to_owned(),
        },
    };
    if url.is_empty() {
        return Ok(url);
    }
    // `apply_insteadof`: the longest `insteadOf` prefix (the first of
    // equally long ones) is replaced with its section's base URL.
    let mut best: Option<(usize, String)> = None;
    if let Some(sections) = config.plumbing().sections_by_name("url") {
        for section in sections {
            let Some(base) = section.header().subsection_name() else {
                continue;
            };
            for prefix in section.values("insteadOf") {
                let prefix: &[u8] = &prefix;
                let longer = best.as_ref().is_none_or(|(len, _)| prefix.len() > *len);
                if longer && !prefix.is_empty() && url.as_bytes().starts_with(prefix) {
                    let base = base
                        .to_str()
                        .map_err(|_| err!("Git URL base '{}' is not valid UTF-8", base))?;
                    best = Some((prefix.len(), base.to_owned()));
                }
            }
        }
    }
    Ok(match best {
        None => url,
        Some((len, base)) => {
            let mut rewritten = base.into_bytes();
            rewritten.extend_from_slice(&url.as_bytes()[len..]);
            String::from_utf8(rewritten)
                .map_err(|_| err!("the rewritten URL of '{}' is not valid UTF-8", url))?
        }
    })
}

/// `ParsedURL::canonicalise`: the path as `CanonPath` makes it (absolute,
/// without `.`, `..`, empty or trailing segments).
fn canonicalise(mut url: ParsedUrl) -> Result<ParsedUrl> {
    let path = CanonPath::new(url.path.join("/")).map_err(|e| err!("{}", e))?;
    url.path = path
        .to_str()
        .expect("canonicalizing a textual URL preserves UTF-8")
        .split('/')
        .map(str::to_owned)
        .collect();
    Ok(url)
}

/// `getLfsApi` for a remote that is not SSH, as git-lfs finds the server
/// of a plain remote: `<remote>.git/info/lfs`, `.git` added to the last
/// path segment that is not empty unless it ends so already.
fn http_endpoint(url: &ParsedUrl) -> String {
    let mut url = url.clone();
    let path = &mut url.path;
    match path.iter().rposition(|segment| !segment.is_empty()) {
        Some(last_named) => {
            if !path[last_named].ends_with(".git") {
                path[last_named].push_str(".git");
            }
        }
        None => match path.len() {
            0 => {
                path.push(String::new());
                path.push(".git".to_owned());
            }
            1 => path.push(".git".to_owned()),
            _ => *path.last_mut().expect("not empty") = ".git".to_owned(),
        },
    }
    match path.last_mut() {
        Some(last) if last.is_empty() => *last = "info".to_owned(),
        _ => path.push("info".to_owned()),
    }
    path.push("lfs".to_owned());
    url.to_string()
}

/// `getLfsApi` for an SSH remote: `ssh $NIX_SSHOPTS [-pPORT] [user@]host
/// -- git-lfs-authenticate <path> download` prints the endpoint (`href`)
/// and the `Authorization` header to send it. Its exit status is not
/// looked at, only its output.
fn git_lfs_authenticate(url: &ParsedUrl, authority: &Authority) -> Result<LfsApi> {
    let mut args =
        brix_store::shell::nix_ssh_opts().map_err(|e| err!("{}", e.message).trace(e.trace))?;
    if let Some(port) = authority.port {
        args.push(format!("-p{port}").into());
    }
    args.push(
        match &authority.user {
            Some(user) => format!("{user}@{}", authority.host),
            None => authority.host.clone(),
        }
        .into(),
    );
    args.push("--".into());
    args.push("git-lfs-authenticate".into());
    args.push(url.path.join("/").into());
    args.push("download".into());

    // `runProgram`: stdin and stderr are the caller's (see
    // `super::stderr`).
    let output = match std::process::Command::new("ssh")
        .args(&args)
        .stdin(std::process::Stdio::inherit())
        .stderr(super::stderr::for_program())
        .output()
    {
        Ok(output) => {
            super::stderr::pass_on(&output.stderr);
            output.stdout
        }
        Err(e) => {
            // Nix's forked child reports that it cannot run the program
            // and exits; the caller then sees no output.
            super::stderr::write(
                format!("error: executing \"ssh\": {}\n", strerror(&e)).as_bytes(),
            );
            Vec::new()
        }
    };
    if output.is_empty() {
        let cmd: Vec<String> = args
            .iter()
            .map(|arg: &OsString| escape_shell_arg_always(&arg.to_string_lossy()))
            .collect();
        return Err(err!(
            "git-lfs-authenticate: no output (cmd: 'ssh {}')",
            cmd.join(" ")
        ));
    }
    let response: Value =
        serde_json::from_slice(&output).map_err(|e| err!("{}", json_parse_error(&output, &e)))?;
    // nlohmann's `find` finds nothing in what is no object.
    let header = response
        .get("header")
        .ok_or_else(|| err!("no header in git-lfs-authenticate response"))?;
    let auth = header
        .get("Authorization")
        .ok_or_else(|| err!("no Authorization in git-lfs-authenticate response"))?;
    let href = at(&response, "href").map_err(|e| err!("{}", e))?;
    Ok(LfsApi {
        endpoint: json_string(href)?,
        auth_header: Some(json_string(auth)?),
    })
}

/// `strerror` of an I/O error: its text without Rust's `(os error N)`.
fn strerror(e: &std::io::Error) -> String {
    let text = e.to_string();
    match e.raw_os_error() {
        Some(code) => match text.strip_suffix(&format!(" (os error {code})")) {
            Some(stripped) => stripped.to_owned(),
            None => text,
        },
        None => text,
    }
}

/// A cached object: Nix opens the cache file without following a final
/// symlink, so a symlink there is no cache entry.
fn read_cached(path: &Path) -> Result<Option<Vec<u8>>> {
    let io = |e| crate::Error::Io {
        context: format!("reading '{}'", path.display()),
        source: e,
    };
    match std::fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(io(e)),
        Ok(meta) if meta.file_type().is_symlink() => Ok(None),
        Ok(_) => std::fs::read(path).map(Some).map_err(io),
    }
}

/// Keeps `data` at `path`, written to a temporary file in the same
/// directory and renamed into place.
fn write_cached(path: &Path, data: &[u8]) -> Result<()> {
    let dir = path
        .parent()
        .expect("invariant: a cache entry is in a directory");
    let io = |context: String| move |e| crate::Error::Io { context, source: e };
    std::fs::create_dir_all(dir).map_err(io(format!("creating '{}'", dir.display())))?;
    let mut file = tempfile::NamedTempFile::new_in(dir).map_err(io(format!(
        "creating a temporary file in '{}'",
        dir.display()
    )))?;
    file.write_all(data)
        .map_err(io(format!("writing '{}'", file.path().display())))?;
    file.persist(path).map_err(|e| crate::Error::Io {
        context: format!("renaming a temporary file to '{}'", path.display()),
        source: e.error,
    })?;
    Ok(())
}

/// Why nlohmann's `at` failed.
enum AtError {
    /// `out_of_range.403`: no such key.
    Missing(String),
    /// `type_error.304`: the value is no object.
    Type(String),
}

impl std::fmt::Display for AtError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AtError::Missing(what) | AtError::Type(what) => f.write_str(what),
        }
    }
}

/// nlohmann's `json::at(key)`, with its exceptions' texts.
fn at<'v>(value: &'v Value, key: &str) -> std::result::Result<&'v Value, AtError> {
    match value {
        Value::Object(map) => map.get(key).ok_or_else(|| {
            AtError::Missing(format!(
                "[json.exception.out_of_range.403] key '{key}' not found"
            ))
        }),
        other => Err(AtError::Type(format!(
            "[json.exception.type_error.304] cannot use at() with {}",
            json_type(other)
        ))),
    }
}

/// [`at`] in an object `obj` of the batch API's answer: a missing key is
/// caught and reported with the object, a value of the wrong type is
/// nlohmann's error as it is.
fn batch_at<'v>(obj: &Value, value: &'v Value, key: &str) -> Result<&'v Value> {
    at(value, key).map_err(|e| match e {
        AtError::Missing(what) => err!(
            "bad json from /info/lfs/objects/batch: {} {}",
            dump(obj),
            what
        ),
        AtError::Type(what) => err!("{}", what),
    })
}

/// nlohmann's conversion of a value to a string.
fn json_string(value: &Value) -> Result<String> {
    match value {
        Value::String(s) => Ok(s.clone()),
        other => Err(err!(
            "[json.exception.type_error.302] type must be string, but is {}",
            json_type(other)
        )),
    }
}

/// `getString(valueAt(getObject(obj), key))` for an object.
fn json_object_string(obj: &Value, key: &str) -> Result<String> {
    match value_at(obj, key)? {
        Value::String(s) => Ok(s.clone()),
        other => Err(err!(
            "Expected JSON value to be of type 'string' but it is of type '{}': {}",
            json_type(other),
            dump(other)
        )),
    }
}

/// `getUnsigned(valueAt(getObject(obj), key))` for an object.
fn json_object_unsigned(obj: &Value, key: &str) -> Result<u64> {
    let value = value_at(obj, key)?;
    let type_name = match value {
        Value::Number(n) => match n.as_u64() {
            Some(n) => return Ok(n),
            None if n.as_i64().is_some() => "signed integral number",
            None => "floating point number",
        },
        other => json_type(other),
    };
    Err(err!(
        "Expected JSON value to be an unsigned integral number but it is of type '{}': {}",
        type_name,
        dump(value)
    ))
}

/// `valueAt`: `obj`'s `key`, which must be there.
fn value_at<'v>(obj: &'v Value, key: &str) -> Result<&'v Value> {
    obj.get(key).ok_or_else(|| {
        err!(
            "Expected JSON object to contain key '{}' but it doesn't: {}",
            key,
            dump(obj)
        )
    })
}

/// nlohmann's `type_name()`.
fn json_type(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// nlohmann's `dump()`: compact, with object keys in byte order (its
/// objects are `std::map`s).
fn dump(value: &Value) -> String {
    fn sorted(value: &Value) -> Value {
        match value {
            Value::Object(map) => {
                let mut entries: Vec<_> = map.iter().collect();
                entries.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
                Value::Object(
                    entries
                        .into_iter()
                        .map(|(k, v)| (k.clone(), sorted(v)))
                        .collect(),
                )
            }
            Value::Array(items) => Value::Array(items.iter().map(sorted).collect()),
            other => other.clone(),
        }
    }
    sorted(value).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fetch::{tree_nar_hash, FetchedTree};
    use brix_testutil::{git, GitRepo, LfsMode, LfsServer, LFS_AUTHORIZATION};

    fn p(s: &str) -> CanonPath {
        CanonPath::new(s).unwrap()
    }

    fn sha256(data: &[u8]) -> String {
        Hash::of(HashAlgo::Sha256, data).to_string(HashFormat::Base16, false)
    }

    /// A v1 pointer to `data`, as `git lfs` writes it.
    fn pointer(data: &[u8]) -> String {
        format!(
            "version https://git-lfs.github.com/spec/v1\noid sha256:{}\nsize {}\n",
            sha256(data),
            data.len()
        )
    }

    #[test]
    fn pointers_parse_as_nix_parses_them() {
        let oid = "4859402c258b836d02e955d1090e29f586e58b2040504d68afec3d8d43757bba";
        let parse = |text: String| parse_pointer(text.as_bytes()).unwrap();
        let ok = Some(Pointer {
            oid: oid.to_owned(),
            size: 10,
        });
        let v1 = "version https://git-lfs.github.com/spec/v1";
        assert_eq!(parse(format!("{v1}\noid sha256:{oid}\nsize 10\n")), ok);
        // Extensions and blank lines are skipped, the order does not
        // matter, the last `oid` counts, and the version is a prefix.
        assert_eq!(
            parse(format!(
                "{v1}x\n\nsize 10\next-0-foo sha256:00\noid sha256:{}\noid sha256:{oid}",
                "0".repeat(64)
            )),
            ok
        );
        // Upper-case hexadecimal passes the check (and then fails the
        // download's hash check).
        assert_eq!(
            parse(format!(
                "{v1}\noid sha256:{}\nsize 10\n",
                oid.to_uppercase()
            )),
            Some(Pointer {
                oid: oid.to_uppercase(),
                size: 10
            })
        );
        for invalid in [
            format!("version https://git-lfs.github.com/spec/v2\noid sha256:{oid}\nsize 10\n"),
            format!("oid sha256:{oid}\nsize 10\n"),
            format!("{v1}\noid sha256:{}\nsize 10\n", &oid[1..]),
            format!("{v1}\noid sha1:{oid}\nsize 10\n"),
            format!("{v1}\noid sha256:{oid}\n"),
            format!("{v1}\noid sha256:{oid}\nsize 10\r\n"),
            format!("{v1}\noid sha256:{oid}\nsize -1\n"),
        ] {
            assert_eq!(parse(invalid.clone()), None, "{invalid:?}");
        }
        let huge = format!("{v1}\noid sha256:{oid}\nsize 99999999999999999999\n");
        assert_eq!(
            parse_pointer(huge.as_bytes()).unwrap_err().to_string(),
            "stoul"
        );
    }

    #[test]
    fn http_endpoints_follow_git_lfs() {
        let endpoint =
            |url: &str| http_endpoint(&canonicalise(fix_git_url(url, &|_| {}).unwrap()).unwrap());
        assert_eq!(
            endpoint("https://github.com/NixOS/nix"),
            "https://github.com/NixOS/nix.git/info/lfs"
        );
        assert_eq!(
            endpoint("https://github.com/NixOS/nix/"),
            "https://github.com/NixOS/nix.git/info/lfs"
        );
        assert_eq!(
            endpoint("git+https://gitlab.com/a/../b.git"),
            "https://gitlab.com/b.git/info/lfs"
        );
        assert_eq!(endpoint("http://host"), "http://host/.git/info/lfs");
        assert_eq!(endpoint("/srv/repo"), "file:///srv/repo.git/info/lfs");
        // Without `canonicalise`, a path may have no segment at all.
        for path in [vec![], vec![String::new()]] {
            let url = ParsedUrl {
                scheme: "http".into(),
                authority: Some(Authority {
                    host: "h".into(),
                    ..Default::default()
                }),
                path,
                ..Default::default()
            };
            assert_eq!(http_endpoint(&url), "http://h/.git/info/lfs");
        }
    }

    const CONTENT: &[u8] = b"hello lfs\n";

    /// What the test server serves: [`CONTENT`].
    fn objects() -> Vec<(String, Vec<u8>)> {
        vec![(sha256(CONTENT), CONTENT.to_vec())]
    }

    /// The warnings of a fetch, through the fetcher's sink and the
    /// settings' (which a tree read later warns through).
    type Warnings = Arc<Mutex<Vec<String>>>;

    fn warnings(sink: &Warnings) -> Vec<String> {
        sink.lock().unwrap().clone()
    }

    /// Settings whose warnings go to `sink`, with one download attempt.
    fn settings(sink: &Warnings) -> FetchSettings {
        let sink = sink.clone();
        FetchSettings {
            download_attempts: 1,
            warn: crate::input::Warn::new(move |w| sink.lock().unwrap().push(w.to_owned())),
            ..FetchSettings::default()
        }
    }

    /// A checkout whose commit has LFS pointers (`a.bin`, `sub/b.bin`), a
    /// file `*.bin` marks that is no pointer (`bad.bin`) and one too large
    /// to be one (`big.bin`), and a pointer no attribute marks
    /// (`plain.txt`); its `origin` is `server`'s `repo`.
    struct Fixture {
        repo: GitRepo,
        rev: String,
        cache: PathBuf,
    }

    impl Fixture {
        fn new(server: &LfsServer) -> Fixture {
            let repo = GitRepo::new();
            repo.write(
                ".gitattributes",
                "*.bin filter=lfs diff=lfs merge=lfs -text\n",
            );
            repo.write("a.bin", pointer(CONTENT));
            repo.write("sub/b.bin", pointer(CONTENT));
            repo.write("bad.bin", "not a pointer\n");
            repo.write("big.bin", "x".repeat(2000));
            repo.write("plain.txt", pointer(CONTENT));
            let rev = repo.commit();
            repo.git(&[
                "remote",
                "add",
                "origin",
                &format!("{}/repo", server.base()),
            ]);
            let cache = repo.scratch().join("cache");
            Fixture { repo, rev, cache }
        }

        fn fetch(&self, query: &str, sink: &Warnings) -> Result<FetchedTree> {
            self.fetch_url(
                &format!("git+file://{}?{query}", self.repo.path().display()),
                sink,
            )
        }

        fn fetch_url(&self, url: &str, sink: &Warnings) -> Result<FetchedTree> {
            let settings = settings(sink);
            let input = Input::from_url_str(&settings, url, true).unwrap();
            super::super::git::fetch(
                &HttpClient::new(),
                &self.cache,
                &settings,
                &input,
                &|w: &str| sink.lock().unwrap().push(w.to_owned()),
            )
        }
    }

    /// The files are smudged when they are read, not when the commit is
    /// exported; hashing the tree reads them all, in NAR order.
    #[test]
    fn lfs_files_are_fetched_when_read() {
        let server = LfsServer::start(objects(), LfsMode::Objects);
        let fixture = Fixture::new(&server);
        let sink = Warnings::default();
        let fetched = fixture
            .fetch(&format!("rev={}&lfs=1", fixture.rev), &sink)
            .unwrap();
        assert!(server.requests().is_empty(), "nothing read yet");
        assert!(warnings(&sink).is_empty());

        let tree = &fetched.accessor;
        tree_nar_hash(tree.as_ref()).unwrap();
        // In NAR order, as `nix` prints them.
        assert_eq!(
            warnings(&sink),
            [
                "encountered file '/bad.bin' that should have been a git-lfs pointer, but is invalid",
                "encountered file '/big.bin' that should have been a git-lfs pointer, but is too large",
            ]
        );
        // A batch request and a download per file, as Nix makes them.
        let oid = sha256(CONTENT);
        let requests = server.requests();
        let targets: Vec<_> = requests
            .iter()
            .map(|r| format!("{} {}", r.method, r.target))
            .collect();
        let batch = "POST /repo.git/info/lfs/objects/batch".to_owned();
        let get = format!("GET /objects/{oid}");
        assert_eq!(targets, [batch.clone(), get.clone(), batch, get]);
        let post = &requests[0];
        assert_eq!(
            post.header("content-type"),
            Some("application/vnd.git-lfs+json")
        );
        assert_eq!(post.header("accept"), Some("application/vnd.git-lfs+json"));
        assert_eq!(post.header("authorization"), None);
        assert_eq!(
            String::from_utf8(post.body.clone()).unwrap(),
            format!(r#"{{"objects":[{{"oid":"{oid}","size":10}}],"operation":"download"}}"#)
        );
        // The download sends the `Authorization` the batch API gave.
        assert_eq!(requests[1].header("authorization"), Some(LFS_AUTHORIZATION));

        // Later reads: the objects from the cache, and a warning for a
        // file that is no pointer each time it is read, as in Nix.
        assert_eq!(tree.read_file(&p("/a.bin")).unwrap(), CONTENT);
        assert_eq!(tree.read_file(&p("/sub/b.bin")).unwrap(), CONTENT);
        assert_eq!(tree.read_file(&p("/bad.bin")).unwrap(), b"not a pointer\n");
        assert_eq!(
            tree.read_file(&p("/plain.txt")).unwrap(),
            pointer(CONTENT).as_bytes()
        );
        assert_eq!(server.requests().len(), 4);
        assert_eq!(warnings(&sink).len(), 3);
        // The size of an LFS file is the object's, known once it is read.
        assert_eq!(tree.lstat(&p("/a.bin")).unwrap().size, None);
        assert_eq!(
            tree.lstat(&p("/plain.txt")).unwrap().size,
            Some(pointer(CONTENT).len() as u64)
        );

        // The objects are kept by path and oid; another fetch asks the
        // server nothing.
        for path in ["a.bin", "sub/b.bin"] {
            let entry = fixture
                .cache
                .join("git-lfs")
                .join(sha256(path.as_bytes()))
                .join(&oid);
            assert_eq!(std::fs::read(entry).unwrap(), CONTENT, "{path}");
        }
        let again = fixture
            .fetch(&format!("rev={}&lfs=1", fixture.rev), &Warnings::default())
            .unwrap();
        assert_eq!(again.accessor.read_file(&p("/a.bin")).unwrap(), CONTENT);
        assert_eq!(server.requests().len(), 4);
    }

    /// Under lazy trees an evaluation reads some files: only their objects
    /// are fetched, and only they are warned about. An object the server
    /// lacks is no failure while nobody reads its file.
    #[test]
    fn unread_files_are_not_fetched() {
        let server = LfsServer::start(objects(), LfsMode::Objects);
        let fixture = Fixture::new(&server);
        fixture
            .repo
            .write("sub/b.bin", pointer(b"not on the server\n"));
        let rev = fixture.repo.commit();
        let sink = Warnings::default();
        let fetched = fixture.fetch(&format!("rev={rev}&lfs=1"), &sink).unwrap();
        assert_eq!(fetched.accessor.read_file(&p("/a.bin")).unwrap(), CONTENT);
        assert_eq!(server.requests().len(), 2);
        assert!(warnings(&sink).is_empty());
        // Reading the other is the failure, with Nix's trace.
        let err = fetched.accessor.read_file(&p("/sub/b.bin")).unwrap_err();
        assert_eq!(err.traces(), ["while smudging git-lfs file '/sub/b.bin'"]);
        assert!(err.to_string().starts_with("unable to download '"), "{err}");
    }

    /// A working tree is read as it is: no pointer is smudged, and no
    /// remote is needed. Nor does git-lfs's clean filter, which a user's
    /// configuration defines, make it dirty: libgit2 runs no filter
    /// driver (here one that would change `bad.bin`, whose time changed
    /// so that its contents are compared; `nix` finds such a tree clean).
    #[test]
    fn a_working_tree_is_not_smudged() {
        let server = LfsServer::start(objects(), LfsMode::Objects);
        let fixture = Fixture::new(&server);
        fixture.repo.git(&["remote", "remove", "origin"]);
        fixture
            .repo
            .git(&["config", "filter.lfs.clean", "sed s/not/NOT/"]);
        let later = std::time::SystemTime::now() + std::time::Duration::from_secs(10);
        std::fs::File::options()
            .write(true)
            .open(fixture.repo.path().join("bad.bin"))
            .unwrap()
            .set_modified(later)
            .unwrap();
        let sink = Warnings::default();
        let fetched = fixture.fetch("lfs=1", &sink).unwrap();
        assert_eq!(
            fetched.accessor.read_file(&p("/a.bin")).unwrap(),
            pointer(CONTENT).as_bytes()
        );
        tree_nar_hash(fetched.accessor.as_ref()).unwrap();
        assert!(warnings(&sink).is_empty(), "{:?}", warnings(&sink));
        assert!(server.requests().is_empty());
    }

    /// Files `export-ignore` hides are never read, so never smudged.
    #[test]
    fn export_ignored_files_are_not_read() {
        let server = LfsServer::start(objects(), LfsMode::Objects);
        let fixture = Fixture::new(&server);
        fixture.repo.write(
            ".gitattributes",
            "*.bin filter=lfs diff=lfs merge=lfs -text\nbad.bin export-ignore\nsub export-ignore\n",
        );
        let rev = fixture.repo.commit();
        let sink = Warnings::default();
        let fetched = fixture
            .fetch(&format!("rev={rev}&lfs=1&exportIgnore=1"), &sink)
            .unwrap();
        tree_nar_hash(fetched.accessor.as_ref()).unwrap();
        assert_eq!(fetched.accessor.read_file(&p("/a.bin")).unwrap(), CONTENT);
        assert_eq!(
            warnings(&sink),
            ["encountered file '/big.bin' that should have been a git-lfs pointer, but is too large"]
        );
        // Only `a.bin` was fetched.
        assert_eq!(server.requests().len(), 2);
    }

    /// `origin` after `url.<base>.insteadOf`.
    #[test]
    fn the_remote_url_is_rewritten() {
        let server = LfsServer::start(objects(), LfsMode::Objects);
        let fixture = Fixture::new(&server);
        let repo = &fixture.repo;
        repo.git(&["remote", "set-url", "origin", "lfs-test:repo"]);
        repo.git(&[
            "config",
            &format!("url.{}/.insteadOf", server.base()),
            "lfs-test:",
        ]);
        let fetched = fixture
            .fetch(&format!("rev={}&lfs=1", fixture.rev), &Warnings::default())
            .unwrap();
        assert_eq!(fetched.accessor.read_file(&p("/a.bin")).unwrap(), CONTENT);
        assert_eq!(
            server.requests()[0].target,
            "/repo.git/info/lfs/objects/batch"
        );
    }

    /// The errors Nix gives, with the trace naming the file, both as the
    /// hash of the tree reports them and as a read does.
    #[test]
    fn failures_are_reported_as_nix_reports_them() {
        let oid = sha256(CONTENT);
        let error_of = |mode: LfsMode| {
            let server = LfsServer::start(objects(), mode);
            let fixture = Fixture::new(&server);
            let fetched = fixture
                .fetch(&format!("rev={}&lfs=1", fixture.rev), &Warnings::default())
                .unwrap();
            let err = tree_nar_hash(fetched.accessor.as_ref()).expect_err("the hash fails");
            assert_eq!(err.traces(), ["while smudging git-lfs file '/a.bin'"]);
            let read = fetched.accessor.read_file(&p("/a.bin")).unwrap_err();
            assert_eq!(read.traces(), ["while smudging git-lfs file '/a.bin'"]);
            assert_eq!(read.to_string(), err.to_string());
            (err.to_string(), server.base().to_owned())
        };

        let (err, base) = error_of(LfsMode::Status(404, "{\"message\":\"not found\"}\n"));
        assert_eq!(
            err,
            format!("unable to upload '{base}/repo.git/info/lfs/objects/batch': HTTP error 404\n\nresponse body:\n\n{{\"message\":\"not found\"}}")
        );
        let (err, _) = error_of(LfsMode::Status(200, "this is not json"));
        assert_eq!(
            err,
            format!(
                "response did not parse as json: {}",
                brix_store::json::nlohmann_parse_error(b"this is not json").unwrap()
            )
        );
        let (err, _) = error_of(LfsMode::Status(200, "{\"foo\":1}"));
        assert_eq!(err, "response does not contain 'objects'");
        let (err, _) = error_of(LfsMode::ErrorObject);
        assert_eq!(
            err,
            format!("bad json from /info/lfs/objects/batch: {{\"error\":{{\"code\":404,\"message\":\"Object does not exist\"}},\"oid\":\"{oid}\",\"size\":10}} [json.exception.out_of_range.403] key 'actions' not found")
        );
        let (err, _) = error_of(LfsMode::WrongSize);
        assert_eq!(
            err,
            format!("LFS server returned mismatched oid/size for '/a.bin' (got oid={oid} size=11, expected oid={oid} size=10)")
        );
        let corrupt = {
            let mut data = CONTENT.to_vec();
            *data.last_mut().unwrap() ^= 1;
            sha256(&data)
        };
        let (err, base) = error_of(LfsMode::Corrupt);
        assert_eq!(
            err,
            format!("hash mismatch while fetching {base}/objects/{oid}: expected sha256:{oid} but got sha256:{corrupt}")
        );
        let (err, base) = error_of(LfsMode::Short);
        assert_eq!(
            err,
            format!("size mismatch while fetching {base}/objects/{oid}: expected 10 but got 9")
        );
    }

    /// Without a remote there is no URL to find the server from, which is
    /// an error before any file is read.
    #[test]
    fn a_checkout_without_origin_has_no_lfs_server() {
        let server = LfsServer::start(Vec::new(), LfsMode::Objects);
        let fixture = Fixture::new(&server);
        fixture.repo.git(&["remote", "remove", "origin"]);
        let err = fixture
            .fetch(&format!("rev={}&lfs=1", fixture.rev), &Warnings::default())
            .expect_err("no remote");
        assert_eq!(err.to_string(), "'' doesn't have a scheme");
    }

    /// A clone's remote is the URL it was fetched from: for a bare
    /// repository on disk a `file:` URL, which cannot be posted to.
    #[test]
    fn a_clone_asks_the_url_it_was_fetched_from() {
        let server = LfsServer::start(objects(), LfsMode::Objects);
        let fixture = Fixture::new(&server);
        git(
            fixture.repo.scratch(),
            &[
                "clone",
                "-q",
                "--bare",
                fixture.repo.path().to_str().unwrap(),
                "bare.git",
            ],
        );
        let bare = fixture.repo.scratch().join("bare.git");
        let fetched = fixture
            .fetch_url(
                &format!("git+file://{}?rev={}&lfs=1", bare.display(), fixture.rev),
                &Warnings::default(),
            )
            .unwrap();
        let err = tree_nar_hash(fetched.accessor.as_ref()).expect_err("file: is not posted to");
        let batch = format!("file://{}/info/lfs/objects/batch", bare.display());
        assert_eq!(
            err.to_string(),
            format!("uploading to '{batch}' is not supported")
        );
        assert_eq!(
            err.traces(),
            [
                "while smudging git-lfs file '/a.bin'".to_owned(),
                format!("during upload of '{batch}'"),
            ]
        );
    }

    /// Through the fetcher with its cache: nothing of the tree is kept,
    /// and the NAR hash `fetch_input` computes is remembered, so that the
    /// next fetch reads (and warns about) nothing, as Nix's fetch-to-store
    /// cache spares it.
    #[test]
    fn the_fetch_cache_remembers_the_nar_hash_only() {
        let server = LfsServer::start(objects(), LfsMode::Objects);
        let fixture = Fixture::new(&server);
        let sink = Warnings::default();
        let settings = settings(&sink);
        let mut fetcher = super::super::NativeFetcher::new()
            .unwrap()
            .with_cache_dir(fixture.cache.clone());
        let fetcher_sink = sink.clone();
        fetcher.warn = Box::new(move |w: &str| fetcher_sink.lock().unwrap().push(w.to_owned()));
        let url = format!(
            "git+file://{}?rev={}&lfs=1",
            fixture.repo.path().display(),
            fixture.rev
        );
        let input = Input::from_url_str(&settings, &url, true).unwrap();
        let first = crate::fetch::fetch_input(&fetcher, &settings, &input).unwrap();
        assert_eq!(warnings(&sink).len(), 2);
        assert_eq!(server.requests().len(), 4);
        let second = crate::fetch::fetch_input(&fetcher, &settings, &input).unwrap();
        assert_eq!(
            second.input.nar_hash().unwrap(),
            first.input.nar_hash().unwrap()
        );
        assert_eq!(warnings(&sink).len(), 2, "{:?}", warnings(&sink));
        assert_eq!(server.requests().len(), 4);
        assert!(
            !fixture.cache.join("fetch").join("trees").exists()
                || std::fs::read_dir(fixture.cache.join("fetch").join("trees"))
                    .unwrap()
                    .next()
                    .is_none(),
            "no copy of the tree is kept"
        );
    }
}
