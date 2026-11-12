//! Git submodules as Nix's Git fetcher finds them: the `.gitmodules`
//! file read as libgit2 reads it (`GitRepoImpl::parseSubmodules` and
//! `getSubmodules` in `src/libfetchers/git-utils.cc`), and a submodule's
//! URL resolved against its parent as libgit2's `git_submodule_resolve_url`
//! does (`resolveSubmoduleUrl`), quirks included.

use std::collections::BTreeMap;
use std::path::Path;

use brix_store::CanonPath;

use crate::hashes::GitRev;
use crate::{err, Result};

/// `GitRepo::Submodule`: one entry of `.gitmodules`. `url` and `branch`
/// are empty when the entry does not set them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Submodule {
    pub path: CanonPath,
    pub url: String,
    pub branch: String,
}

/// `parseSubmodules`: the `submodule.<name>.path`, `.url` and `.branch`
/// values of a `.gitmodules` file, one submodule per `path`, in the order
/// of their names (Nix collects the values in a sorted map). As with
/// libgit2's config iterator, section and variable names are matched
/// without regard to case, and the first value given for a name wins.
pub fn parse_gitmodules(bytes: &[u8]) -> Result<Vec<Submodule>> {
    use gix::bstr::ByteSlice;
    let file = gix::config::File::from_bytes_no_includes(
        bytes,
        gix::config::file::Metadata::api(),
        Default::default(),
    )
    .map_err(|e| err!("parsing .gitmodules file: {}", e))?;
    // `submodule.<name>.<variable>` without its `submodule.` prefix, as
    // Nix keys its map.
    let mut entries: BTreeMap<String, String> = BTreeMap::new();
    for section in file.sections() {
        let header = section.header();
        if !header.name().eq_ignore_ascii_case(b"submodule") {
            continue;
        }
        // `^submodule\..*\.(path|url|branch)$` needs a subsection.
        let Some(name) = header.subsection_name() else {
            continue;
        };
        let name = name.to_str().map_err(|_| {
            err!(
                "submodule name '{}' in .gitmodules is not valid UTF-8, which brix does not support",
                name
            )
        })?;
        for variable in ["path", "url", "branch"] {
            if let Some(value) = section.body().values(variable).into_iter().next() {
                let value = value.to_str().map_err(|_| {
                    err!(
                        "'submodule.{}.{}' in .gitmodules is not valid UTF-8, which brix does not support",
                        name,
                        variable
                    )
                })?;
                entries
                    .entry(format!("{name}.{variable}"))
                    .or_insert_with(|| value.to_owned());
            }
        }
    }
    let mut submodules = Vec::new();
    for (key, value) in &entries {
        let Some(name) = key.strip_suffix(".path") else {
            continue;
        };
        let value_of = |variable: &str| {
            entries
                .get(&format!("{name}.{variable}"))
                .cloned()
                .unwrap_or_default()
        };
        submodules.push(Submodule {
            path: CanonPath::new(value)
                .map_err(|e| err!("invalid submodule path '{}': {}", value, e))?,
            url: value_of("url"),
            branch: value_of("branch"),
        });
    }
    Ok(submodules)
}

/// The submodules of a working tree's `.gitmodules` (`getWorkdirInfo`
/// reads it from disk); none when there is no such file.
pub fn workdir_gitmodules(workdir: &Path) -> Result<Vec<Submodule>> {
    let file = workdir.join(".gitmodules");
    match std::fs::read(&file) {
        Ok(bytes) => parse_gitmodules(&bytes),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(crate::Error::Io {
            context: format!("reading '{}'", file.display()),
            source: e,
        }),
    }
}

/// `GitRepoImpl::getSubmodules(rev)`: the submodules `.gitmodules` lists
/// at `commit` that are gitlinks there, each with the revision it is at.
/// An entry whose path is no gitlink (or nothing) in the commit is left
/// out.
pub fn at_commit(
    repo: &gix::Repository,
    commit: &gix::Commit<'_>,
) -> Result<Vec<(Submodule, GitRev)>> {
    let gix_err = |context: &str, e: &dyn std::fmt::Display| err!("{}: {}", context, e);
    let tree = commit
        .tree()
        .map_err(|e| gix_err("reading commit tree", &e))?;
    let Some(entry) = tree
        .lookup_entry_by_path(".gitmodules")
        .map_err(|e| gix_err("looking up .gitmodules", &e))?
    else {
        return Ok(Vec::new());
    };
    use gix::objs::tree::EntryKind;
    if !matches!(
        entry.mode().kind(),
        EntryKind::Blob | EntryKind::BlobExecutable
    ) {
        // `readFile` of what is no regular file.
        return Err(err!("'/.gitmodules' is not a regular file"));
    }
    let config = repo
        .find_object(entry.object_id())
        .map_err(|e| gix_err("reading .gitmodules", &e))?
        .detach()
        .data;
    let mut result = Vec::new();
    for submodule in parse_gitmodules(&config)? {
        // `getSubmoduleRev`: the gitlink at the submodule's path, if any.
        let gitlink = if submodule.path.is_root() {
            None
        } else {
            tree.lookup_entry_by_path(
                brix_store::os_path::to_path(submodule.path.rel())
                    .ok_or_else(|| err!("submodule path cannot be represented on this platform"))?,
            )
            .map_err(|e| gix_err(&format!("looking up '{}'", submodule.path), &e))?
        };
        if let Some(gitlink) = gitlink {
            if gitlink.mode().kind() == EntryKind::Commit {
                let rev = super::git::rev_from_oid(gitlink.object_id())?;
                result.push((submodule, rev));
            }
        }
    }
    Ok(result)
}

/// libgit2's `get_url_base` for a local, non-bare repository: the URL of
/// the remote the checked-out branch tracks (`branch.<name>.remote`, with
/// `branch.<name>.merge` set), else of `origin`, else the working tree
/// (with the trailing `/` of `git_repository_workdir`). A remote's URL is
/// rewritten by `url.<base>.insteadOf` first, as `git_remote_lookup`
/// rewrites it ([`apply_insteadof`]).
pub fn local_url_base(repo: &gix::Repository, workdir: &Path) -> Result<String> {
    let config = repo.config_snapshot();
    let remote_url = |name: &str| -> Result<Option<String>> {
        config
            .string(format!("remote.{name}.url").as_str())
            .map(|url| apply_insteadof(repo, &url.to_string()))
            .transpose()
    };
    let head = repo
        .head_name()
        .map_err(|e| err!("looking up HEAD: {}", e))?;
    if let Some(head) = head {
        use gix::bstr::ByteSlice;
        if let Some(branch) = head.as_bstr().strip_prefix(b"refs/heads/") {
            let branch = branch.to_str_lossy();
            let tracked = config.string(format!("branch.{branch}.remote").as_str());
            let merge = config.string(format!("branch.{branch}.merge").as_str());
            if let (Some(remote), Some(_)) = (tracked, merge) {
                if remote.as_bytes() != b"." {
                    if let Some(url) = remote_url(&remote.to_str_lossy())? {
                        return Ok(url);
                    }
                }
            }
        }
    }
    if let Some(url) = remote_url("origin")? {
        return Ok(url);
    }
    let mut base = workdir
        .to_str()
        .ok_or_else(|| {
            err!(
                "path '{}' is not valid UTF-8, which brix does not support",
                workdir.display()
            )
        })?
        .to_owned();
    if !base.ends_with('/') {
        base.push('/');
    }
    Ok(base)
}

/// libgit2's `apply_insteadof` for fetching, which `git_remote_lookup`
/// applies to a remote's URL: `url` with its longest prefix that a
/// `url.<base>.insteadOf` of `repo`'s configuration names replaced by
/// that base. Of prefixes as long as each other the first one found
/// wins, the repository's own configuration being looked at before the
/// user's and the system's.
pub fn apply_insteadof(repo: &gix::Repository, url: &str) -> Result<String> {
    use gix::bstr::ByteSlice;
    let config = repo.config_snapshot();
    let sections: Vec<_> = config.plumbing().sections().collect();
    let mut best: Option<(usize, Vec<u8>)> = None;
    for section in sections.into_iter().rev() {
        let header = section.header();
        if !header.name().eq_ignore_ascii_case(b"url") {
            continue;
        }
        let Some(base) = header.subsection_name() else {
            continue;
        };
        for prefix in section.body().values("insteadOf") {
            let longest = best.as_ref().map_or(0, |(len, _)| *len);
            if prefix.len() > longest && url.as_bytes().starts_with(prefix.as_bytes()) {
                best = Some((prefix.len(), base.to_vec()));
            }
        }
    }
    let Some((len, mut rewritten)) = best else {
        return Ok(url.to_owned());
    };
    rewritten.extend_from_slice(&url.as_bytes()[len..]);
    String::from_utf8(rewritten).map_err(|e| {
        err!(
            "URL '{}' rewritten by 'url.<base>.insteadOf' is not valid UTF-8, which brix does not support",
            e.into_bytes().as_bstr()
        )
    })
}

/// `GitRepo::resolveSubmoduleUrl` (libgit2's `git_submodule_resolve_url`):
/// a URL starting with `./` or `../` is joined to `base` (the parent's
/// URL, see [`local_url_base`]) and its `.` and `..` resolved; one with a
/// `:` or a leading `/` is taken as it is; anything else is an error.
///
/// libgit2's resolution keeps a URL's `scheme://` but drops the `/` after
/// it, so a relative submodule of a `file:///…` repository resolves to
/// `file://…`, which Nix then refuses ("has unexpected authority"); brix
/// does the same.
pub fn resolve_url(url: &str, base: &str) -> Result<String> {
    let failed = || err!("resolving Git submodule URL '{}'", url);
    // Backslashes count as separators, as `.gitmodules` may come from
    // Windows.
    let url_norm = url.replace('\\', "/");
    if url_norm.starts_with("./") || url_norm.starts_with("../") {
        // `git_str_joinpath`: exactly one `/` between the two.
        let mut joined = base.to_owned();
        if !joined.is_empty() && !joined.ends_with('/') {
            joined.push('/');
        }
        joined.push_str(&url_norm);
        let resolved = resolve_relative(joined.as_bytes()).ok_or_else(failed)?;
        return String::from_utf8(resolved).map_err(|_| failed());
    }
    if url_norm.contains(':') || url_norm.starts_with('/') {
        return Ok(url_norm);
    }
    Err(failed())
}

/// libgit2's `dos_drive_prefix_length`: `C:` (a letter, or one UTF-8
/// character, and a colon) at the start of a path.
fn dos_drive_prefix_length(path: &[u8]) -> usize {
    let Some(&first) = path.first() else {
        return 0;
    };
    if first & 0x80 == 0 {
        return if path.get(1) == Some(&b':') { 2 } else { 0 };
    }
    let width = match first {
        b if b & 0xe0 == 0xc0 => 2,
        b if b & 0xf0 == 0xe0 => 3,
        b if b & 0xf8 == 0xf0 => 4,
        _ => return 0,
    };
    if path.len() > width && path[1..width].iter().all(|b| b & 0xc0 == 0x80) && path[width] == b':'
    {
        width + 1
    } else {
        0
    }
}

/// `git_fs_path_root`: where the root `/` of a path is, if it has one.
fn path_root(path: &[u8]) -> Option<usize> {
    let offset = dos_drive_prefix_length(path);
    match path.get(offset) {
        Some(b'/') | Some(b'\\') => Some(offset),
        _ => None,
    }
}

/// `git_fs_path_resolve_relative(path, 0)`: drop `.` components and let
/// `..` remove the one before it, never backing over the root of a path
/// or the `scheme://` of a URL (that is an error, `None`). A `..` with
/// nothing before it in a relative path stays. Slashes in a row are one,
/// and the first component after the root or `scheme://` loses its
/// leading `/`, exactly as libgit2 does it.
fn resolve_relative(path: &[u8]) -> Option<Vec<u8>> {
    let mut ceiling = path_root(path).map_or(0, |root| root + 1);
    if ceiling == 0 {
        // A URL prefix that must not be backed over.
        let alphas = path.iter().take_while(|c| c.is_ascii_alphabetic()).count();
        if path[alphas..].starts_with(b"://") {
            ceiling = alphas + 3;
        }
    }
    let ceiling = ceiling.min(path.len());
    let mut out = path[..ceiling].to_vec();
    let mut base = out.len();
    let mut from = ceiling;
    while from < path.len() {
        let next = path[from..]
            .iter()
            .position(|&c| c == b'/')
            .map_or(path.len(), |i| from + i);
        let mut len = next - from;
        let component = &path[from..next];
        if component == b"." {
            // Nothing to do with a single dot.
        } else if component == b".." {
            if out.len() == base && ceiling != 0 {
                // "cannot strip root component off url".
                return None;
            }
            if out.len() == base {
                // No more components to strip: `../` is the new base.
                if next < path.len() {
                    len += 1;
                }
                out.extend_from_slice(&path[from..from + len]);
                base = out.len();
            } else {
                // Back up a component.
                while out.len() > base && out.last() == Some(&b'/') {
                    out.pop();
                }
                while out.len() > base && out.last() != Some(&b'/') {
                    out.pop();
                }
            }
        } else {
            if next < path.len() && path[from] != b'/' {
                len += 1;
            }
            out.extend_from_slice(&path[from..from + len]);
        }
        from += len;
        while from < path.len() && path[from] == b'/' {
            from += 1;
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> CanonPath {
        CanonPath::new(s).unwrap()
    }

    #[test]
    fn gitmodules_are_read_as_libgit2_reads_them() {
        let text = br#"
[submodule "b"]
	path = deps/b/
	url = ../b
	branch = .
[submodule "a"]
	Path = ./x/../a
	url = https://example.org/a.git
	url = https://example.org/second.git
[Submodule "a"]
	path = ignored
	branch = dev
[submodule "no-path"]
	url = ../c
[submodule]
	path = no-name
"#;
        let modules = parse_gitmodules(text).unwrap();
        assert_eq!(
            modules,
            [
                Submodule {
                    path: p("/a"),
                    url: "https://example.org/a.git".into(),
                    branch: "dev".into(),
                },
                Submodule {
                    path: p("/deps/b"),
                    url: "../b".into(),
                    branch: ".".into(),
                },
            ]
        );
        assert!(parse_gitmodules(b"").unwrap().is_empty());
        assert!(parse_gitmodules(b"[submodule \"x\"\n").is_err());
    }

    #[test]
    fn relative_urls_resolve_like_libgit2() {
        let cases = [
            ("../sub", "/tmp/x/repo/", "/tmp/x/sub"),
            ("./sub", "/tmp/x/repo/", "/tmp/x/repo/sub"),
            ("../../sub", "/tmp/x/repo", "/tmp/sub"),
            (
                "../sub.git",
                "https://example.org/org/repo.git",
                "https://example.org/org/sub.git",
            ),
            (
                "./sub",
                "https://example.org/repo",
                "https://example.org/repo/sub",
            ),
            (
                "../other.git",
                "git@example.org:org/repo.git",
                "git@example.org:org/other.git",
            ),
            // The `/` after `file://` is lost, as in libgit2.
            ("../sub", "file:///tmp/x/repo", "file://tmp/x/sub"),
            // Nothing left to strip in a relative base: `..` stays.
            ("../../sub", "repo", "../sub"),
            ("..\\sub", "/r/repo/", "/r/sub"),
        ];
        for (url, base, expected) in cases {
            assert_eq!(
                resolve_url(url, base).unwrap(),
                expected,
                "{url} against {base}"
            );
        }
        assert_eq!(resolve_url("/abs/sub", "/r/").unwrap(), "/abs/sub");
        assert_eq!(
            resolve_url("https://example.org/x", "/r/").unwrap(),
            "https://example.org/x"
        );
        assert_eq!(
            resolve_url("sub", "/r/").unwrap_err().to_string(),
            "resolving Git submodule URL 'sub'"
        );
        // Backing over the host of a URL or the root of a path.
        assert!(resolve_url("../../../x", "https://example.org/repo").is_err());
        assert!(resolve_url("../../x", "/repo").is_err());
    }
}
