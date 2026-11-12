//! `github:`, `gitlab:` and `sourcehut:` inputs
//! (`src/libfetchers/github.cc`): a Git revision fetched as a tarball from
//! the forge's archive endpoint, with the forge's API used only to resolve
//! a branch or tag to a revision.

use std::sync::Arc;

use crate::attrs::{get_str_attr, maybe_get_str_attr, Attr};
use crate::fetch::FetchedTree;
use crate::hashes::GitRev;
use crate::input::{FetchSettings, Input, Scheme};
use crate::native::http::HttpClient;
use crate::native::tarball::unpack_archive;
use crate::{err, Error, Result};

fn host(input: &Input) -> Result<String> {
    let default = input
        .scheme
        .and_then(Scheme::default_host)
        .expect("git archive scheme has a default host");
    Ok(maybe_get_str_attr(&input.attrs, "host")?
        .unwrap_or(default)
        .to_string())
}

/// `GitArchiveInputScheme::getAccessToken`: the longest `host/owner/repo`
/// prefix match in `access-tokens`, else the host's token.
pub fn access_token(settings: &FetchSettings, host: &str, host_and_path: &str) -> Option<String> {
    let mut answer: Option<(usize, &str)> = None;
    for (prefix, token) in &settings.access_tokens {
        if host_and_path.starts_with(prefix.as_str())
            && (host_and_path.len() == prefix.len()
                || host_and_path.as_bytes()[prefix.len()] == b'/')
            && answer.is_none_or(|(len, _)| prefix.len() > len)
        {
            answer = Some((prefix.len(), token));
        }
    }
    answer
        .map(|(_, t)| t.to_string())
        .or_else(|| settings.access_tokens.get(host).cloned())
}

/// `accessHeaderFromToken` for each forge.
fn auth_header(scheme: Scheme, token: &str) -> Option<(String, String)> {
    match scheme {
        Scheme::GitHub => Some(("Authorization".into(), format!("token {token}"))),
        Scheme::GitLab => {
            let (kind, value) = token.split_once(':').unwrap_or(("", token));
            match kind {
                "OAuth2" => Some(("Authorization".into(), format!("Bearer {value}"))),
                "PAT" => Some(("Private-token".into(), value.to_string())),
                // Unrecognised token types are passed as `<header>:<value>`.
                _ => token
                    .split_once(':')
                    .map(|(h, v)| (h.to_string(), v.to_string())),
            }
        }
        Scheme::SourceHut => Some(("Authorization".into(), format!("Bearer {token}"))),
        _ => None,
    }
}

fn headers(
    settings: &FetchSettings,
    scheme: Scheme,
    host: &str,
    input: &Input,
) -> Result<Vec<(String, String)>> {
    let owner = get_str_attr(&input.attrs, "owner")?;
    let repo = get_str_attr(&input.attrs, "repo")?;
    let host_and_path = format!("{host}/{owner}/{repo}");
    Ok(access_token(settings, host, &host_and_path)
        .and_then(|t| auth_header(scheme, &t))
        .into_iter()
        .collect())
}

/// `getRevFromRef`: ask the forge which revision a ref points to.
fn rev_from_ref(
    http: &HttpClient,
    settings: &FetchSettings,
    scheme: Scheme,
    input: &Input,
    ref_: &str,
) -> Result<GitRev> {
    let host = host(input)?;
    let owner = get_str_attr(&input.attrs, "owner")?;
    let repo = get_str_attr(&input.attrs, "repo")?;
    let headers = headers(settings, scheme, &host, input)?;
    match scheme {
        Scheme::GitHub => {
            let url = if host == "github.com" {
                format!("https://api.{host}/repos/{owner}/{repo}/commits/{ref_}")
            } else {
                format!("https://{host}/api/v3/repos/{owner}/{repo}/commits/{ref_}")
            };
            let json: serde_json::Value =
                serde_json::from_slice(&http.download(&url, &headers, settings)?.data)?;
            let sha = json
                .get("sha")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| err!("unexpected response from GitHub for '{}'", url))?;
            GitRev::parse_sha1(sha, settings.xp())
        }
        Scheme::GitLab => {
            let url = format!(
                "https://{host}/api/v4/projects/{owner}%2F{repo}/repository/commits?ref_name={ref_}"
            );
            let json: serde_json::Value =
                serde_json::from_slice(&http.download(&url, &headers, settings)?.data)?;
            match json.as_array() {
                Some(arr) if !arr.is_empty() => {
                    let id = arr[0]
                        .get("id")
                        .and_then(serde_json::Value::as_str)
                        .ok_or_else(|| {
                            err!("Unexpected response received from GitLab: {}", json)
                        })?;
                    GitRev::parse_sha1(id, settings.xp())
                }
                Some(_) => Err(err!(
                    "No commits returned by GitLab API -- does the git ref really exist?"
                )),
                None => Err(err!("Unexpected response received from GitLab: {}", json)),
            }
        }
        Scheme::SourceHut => {
            let base = format!("https://{host}/{owner}/{repo}");
            let wanted: String = if ref_ == "HEAD" {
                let head = String::from_utf8_lossy(
                    &http
                        .download(&format!("{base}/HEAD"), &headers, settings)?
                        .data,
                )
                .into_owned();
                let line = head.lines().next().unwrap_or("");
                // `ref: refs/heads/master`
                line.strip_prefix("ref: ")
                    .map(str::trim)
                    .map(str::to_string)
                    .ok_or_else(|| err!("in '{}', couldn't resolve HEAD ref '{}'", input, ref_))?
            } else {
                format!("refs/(heads|tags)/{ref_}")
            };
            let refs = String::from_utf8_lossy(
                &http
                    .download(&format!("{base}/info/refs"), &headers, settings)?
                    .data,
            )
            .into_owned();
            let re = regex::Regex::new(&format!("^{wanted}$")).map_err(|e| err!("{}", e))?;
            for line in refs.lines() {
                let mut parts = line.split_whitespace();
                let (Some(id), Some(name)) = (parts.next(), parts.next()) else {
                    continue;
                };
                if re.is_match(name) {
                    return GitRev::parse_sha1(id, settings.xp());
                }
            }
            Err(err!("in '{}', couldn't find ref '{}'", input, ref_))
        }
        _ => unreachable!("not a git archive scheme"),
    }
}

/// `getDownloadUrl`.
fn download_url(
    settings: &FetchSettings,
    scheme: Scheme,
    input: &Input,
    rev: &GitRev,
) -> Result<(String, Vec<(String, String)>)> {
    let host = host(input)?;
    let owner = get_str_attr(&input.attrs, "owner")?;
    let repo = get_str_attr(&input.attrs, "repo")?;
    let headers = headers(settings, scheme, &host, input)?;
    let url = match scheme {
        Scheme::GitHub => {
            if host != "github.com" {
                format!("https://{host}/api/v3/repos/{owner}/{repo}/tarball/{rev}")
            } else if headers.is_empty() {
                // Without auth, use the public archive URL to avoid API
                // rate limits.
                format!("https://{host}/{owner}/{repo}/archive/{rev}.tar.gz")
            } else {
                format!("https://api.{host}/repos/{owner}/{repo}/tarball/{rev}")
            }
        }
        Scheme::GitLab => format!(
            "https://{host}/api/v4/projects/{owner}%2F{repo}/repository/archive.tar.gz?sha={rev}"
        ),
        Scheme::SourceHut => format!("https://{host}/{owner}/{repo}/archive/{rev}.tar.gz"),
        _ => unreachable!("not a git archive scheme"),
    };
    Ok((url, headers))
}

/// `GitArchiveInputScheme::getAccessor`.
pub fn fetch(http: &HttpClient, settings: &FetchSettings, input: &Input) -> Result<FetchedTree> {
    let scheme = input.scheme.expect("git archive scheme");
    let mut input = input.clone();
    if !input.attrs.contains_key("ref") {
        input
            .attrs
            .insert("ref".into(), Attr::String("HEAD".into()));
    }
    let rev = match input.rev()? {
        Some(rev) => rev,
        None => {
            let ref_ = input.ref_()?.unwrap_or("HEAD").to_string();
            rev_from_ref(http, settings, scheme, &input, &ref_)?
        }
    };
    input.attrs.remove("ref");
    input.attrs.insert("rev".into(), Attr::String(rev.to_hex()));

    let (url, headers) = download_url(settings, scheme, &input, &rev)?;
    // `downloadArchive`'s activity, which covers the download too.
    (settings.info)(&format!("unpacking '{input}' into the Git cache..."));
    let download = http
        .download(&url, &headers, settings)
        .map_err(|e: Error| e.trace(format!("while downloading '{url}'")))?;
    let unpacked = unpack_archive(&download.data, &url, &*settings.warn)?;
    input
        .attrs
        .insert("lastModified".into(), Attr::Int(unpacked.last_modified));

    let mut tree = unpacked.tree;
    tree.set_display_prefix(format!("«{}»", input.to_url_text(true)?));
    // Nix only records narHash here when forge tarballs are not trusted;
    // brix always computes it (fetch_input needs it anyway).
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

    #[test]
    fn access_tokens_prefer_longest_path_match() {
        let mut settings = FetchSettings::default();
        settings
            .access_tokens
            .insert("github.com".into(), "host".into());
        settings
            .access_tokens
            .insert("github.com/org".into(), "org".into());
        settings
            .access_tokens
            .insert("github.com/org/repo".into(), "repo".into());
        assert_eq!(
            access_token(&settings, "github.com", "github.com/org/repo").as_deref(),
            Some("repo")
        );
        assert_eq!(
            access_token(&settings, "github.com", "github.com/org/other").as_deref(),
            Some("org")
        );
        assert_eq!(
            access_token(&settings, "github.com", "github.com/orgx/y").as_deref(),
            Some("host")
        );
        assert_eq!(
            access_token(&settings, "gitlab.com", "gitlab.com/a/b"),
            None
        );
    }

    #[test]
    fn auth_headers() {
        assert_eq!(
            auth_header(Scheme::GitHub, "t"),
            Some(("Authorization".into(), "token t".into()))
        );
        assert_eq!(
            auth_header(Scheme::GitLab, "OAuth2:t"),
            Some(("Authorization".into(), "Bearer t".into()))
        );
        assert_eq!(
            auth_header(Scheme::GitLab, "PAT:t"),
            Some(("Private-token".into(), "t".into()))
        );
        assert_eq!(
            auth_header(Scheme::GitLab, "X-Thing:v"),
            Some(("X-Thing".into(), "v".into()))
        );
        assert_eq!(
            auth_header(Scheme::SourceHut, "t"),
            Some(("Authorization".into(), "Bearer t".into()))
        );
    }
}
