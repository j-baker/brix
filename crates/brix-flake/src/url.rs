//! Git's URL spellings (`fixGitURL` in `src/libutil/url.cc`).
//!
//! The URL grammar itself (`parseURL`, `ParsedURL`, percent encoding) is
//! shared with the store and lives in [`brix_store::url`]; this module
//! adds what only Git inputs need: the many forms Git accepts for a
//! repository location.

use brix_store::url::{
    is_ipv4_address, is_ipv6_address, parse_url, parse_url_scheme, path_to_url_path, Authority,
    HostType, ParsedUrl, UrlScheme,
};

use crate::{bad_url, Result};

/// `fixGitURL`: turn the many spellings Git accepts (absolute paths,
/// `user@host:path` SCP syntax, `git+https://`) into a canonical URL.
/// Query parameters without `=`, and an SCP-style path that is relative
/// (which the URL makes absolute), are warned about through `warn`.
pub fn fix_git_url(url: &str, warn: &dyn Fn(&str)) -> Result<ParsedUrl> {
    if url.starts_with('/') {
        // A local path, not a URL: percent escapes in it are literal.
        return Ok(ParsedUrl {
            scheme: "file".into(),
            authority: Some(Authority::default()),
            path: path_to_url_path(url),
            ..Default::default()
        });
    }
    if let Some(scp) = try_parse_scp_style(url, warn)? {
        return Ok(scp);
    }
    let mut parsed = parse_url(url, false, warn)?;
    if let UrlScheme {
        application: Some("git"),
        transport,
    } = parse_url_scheme(&parsed.scheme)
    {
        parsed.scheme = transport.to_string();
    }
    Ok(parsed)
}

/// Git's `host:path` pseudo-URL syntax, following git's `connect.c`
/// quirks exactly as Nix does. Returns `None` when `url` is not SCP-style.
fn try_parse_scp_style(url: &str, warn: &dyn Fn(&str)) -> Result<Option<ParsedUrl>> {
    if url.contains("://") {
        return Ok(None);
    }
    let Some(first_colon) = url.find(':') else {
        return Ok(None);
    };
    let mut scheme_or_host = &url[..first_colon];
    if scheme_or_host.contains('/') {
        return Ok(None);
    }
    const GIT_SCHEMES: [&str; 7] = ["ssh", "http", "https", "file", "ftp", "ftps", "git"];
    if GIT_SCHEMES
        .iter()
        .any(|s| *s == scheme_or_host || scheme_or_host.strip_prefix("git+") == Some(*s))
    {
        return Ok(None);
    }

    // Bracketed IPv6 hosts, following git's `host_end()`: prefer `@[`,
    // otherwise a leading `[`.
    let bracket_start = match url.find("@[") {
        Some(i) => Some(i + 1),
        None if url.starts_with('[') => Some(0),
        None => None,
    };
    if let Some(start) = bracket_start {
        if let Some(close_rel) = url[start + 1..].find(']') {
            let close = start + 1 + close_rel;
            if url.as_bytes().get(close + 1) == Some(&b':') {
                scheme_or_host = &url[..=close];
            } else if url[close + 1..].contains(':') {
                // git would silently discard the characters between `]`
                // and `:`; refuse to parse as SCP instead.
                return Ok(None);
            } else {
                return Ok(None);
            }
        }
    }

    let path_view = &url[scheme_or_host.len() + 1..];
    let mut host = scheme_or_host;

    let username = host.find('@').map(|i| {
        let u = &host[..i];
        host = &host[i + 1..];
        u.to_string()
    });

    let mut authority = if is_ipv4_address(host) {
        Authority {
            host_type: HostType::IPv4,
            host: host.to_string(),
            ..Default::default()
        }
    } else if let Some(inner) = host.strip_prefix('[').and_then(|h| h.strip_suffix(']')) {
        if !is_ipv6_address(inner) {
            return Err(bad_url!(
                "Git SCP bracketed URL is not valid: '{}' is not a valid IPv6 address",
                inner
            ));
        }
        Authority {
            host_type: HostType::IPv6,
            host: inner
                .parse::<std::net::Ipv6Addr>()
                .expect("checked by is_ipv6_address")
                .to_string(),
            ..Default::default()
        }
    } else {
        Authority {
            host_type: HostType::Name,
            host: host.to_string(),
            ..Default::default()
        }
    };
    authority.user = username;

    if path_view.is_empty() {
        return Err(bad_url!("SCP-style Git URL '{}' has an empty path", url));
    }
    // Everything after the colon is the path, forced to be absolute
    // (relative SCP paths cannot be represented in `ssh://` URLs).
    let mut path: Vec<String> = path_view.split('/').map(str::to_string).collect();
    let relative = !path.first().map(String::is_empty).unwrap_or(true);
    if relative {
        path.insert(0, String::new());
    }
    let parsed = ParsedUrl {
        scheme: "ssh".into(),
        authority: Some(authority),
        path,
        ..Default::default()
    };
    // A path under the remote user's home (`~…`) is fine: git strips the
    // `/` before `~` at the transport layer.
    if relative && !path_view.starts_with('~') {
        let mut tilde = parsed.clone();
        tilde.path.insert(1, "~".to_owned());
        warn(&format!(
            "SCP-style URL '{url}' has a relative path which cannot be faithfully converted to an SSH URL: \
             we will convert it to an absolute path when making an SSH URL.\n\n\
             Use an explicit absolute path:\n\n    {host}:/{path_view}\n\n\
             or with an explicit SSH URL:\n\n    {parsed}\n\n\
             instead, to explicitly use an absolute path (and silence this warning). \
             Or, to be relative to the remote user's home directory:\n\n    {host}:~/{path_view}\n\n\
             or with an explicit SSH URL:\n\n    {tilde}"
        ));
    }
    Ok(Some(parsed))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Only the warning about a relative SCP-style path is expected.
    fn scp_warning_only(msg: &str) {
        assert!(
            msg.starts_with("SCP-style URL '"),
            "unexpected warning: {msg}"
        );
    }

    fn fix(url: &str) -> Result<ParsedUrl> {
        fix_git_url(url, &scp_warning_only)
    }

    /// The warnings `fix_git_url` gives for `url`.
    fn warnings(url: &str) -> Vec<String> {
        let warnings = std::cell::RefCell::new(Vec::new());
        fix_git_url(url, &|w: &str| warnings.borrow_mut().push(w.to_owned())).unwrap();
        warnings.into_inner()
    }

    /// `tryParseScpStyle`'s warning about a relative path, as `nix` gives
    /// it for `builtins.fetchTree { type = "git"; url =
    /// "git@lfs.example:owner/repo.git"; … }`; none for an absolute path,
    /// one under `~`, or a URL that is not SCP-style.
    #[test]
    fn relative_scp_paths_are_warned_about() {
        assert_eq!(
            warnings("git@lfs.example:owner/repo.git"),
            ["SCP-style URL 'git@lfs.example:owner/repo.git' has a relative path which cannot be faithfully converted to an SSH URL: we will convert it to an absolute path when making an SSH URL.\n\nUse an explicit absolute path:\n\n    lfs.example:/owner/repo.git\n\nor with an explicit SSH URL:\n\n    ssh://git@lfs.example/owner/repo.git\n\ninstead, to explicitly use an absolute path (and silence this warning). Or, to be relative to the remote user's home directory:\n\n    lfs.example:~/owner/repo.git\n\nor with an explicit SSH URL:\n\n    ssh://git@lfs.example/~/owner/repo.git"]
        );
        for quiet in [
            "lfs.example:/owner/repo.git",
            "lfs.example:~/repo.git",
            "ssh://git@lfs.example/owner/repo.git",
            "https://lfs.example/owner/repo",
            "/srv/repo",
        ] {
            assert!(warnings(quiet).is_empty(), "{quiet}");
        }
    }

    fn segs(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| x.to_string()).collect()
    }

    fn name(host: &str) -> Authority {
        Authority {
            host: host.into(),
            ..Default::default()
        }
    }

    #[test]
    fn plain_urls_round_trip() {
        for url in [
            "http://www.example.org/file.tar.gz",
            "https://www.example.org/file.tar.gz?download=fast&when=now#hello",
            "http://[fe80::818c:da4d:8975:415c%25enp0s25]:8080",
        ] {
            assert_eq!(fix(url).unwrap().to_string(), url);
        }
    }

    #[test]
    fn fix_git_urls() {
        let cases: Vec<(&str, &str, ParsedUrl)> = vec![
            (
                "git+ssh://user@domain:1234/path",
                "ssh://user@domain:1234/path",
                ParsedUrl {
                    scheme: "ssh".into(),
                    authority: Some(Authority {
                        host: "domain".into(),
                        user: Some("user".into()),
                        port: Some(1234),
                        ..Default::default()
                    }),
                    path: segs(&["", "path"]),
                    ..Default::default()
                },
            ),
            (
                "git@github.com:owner/repo.git",
                "ssh://git@github.com/owner/repo.git",
                ParsedUrl {
                    scheme: "ssh".into(),
                    authority: Some(Authority {
                        host: "github.com".into(),
                        user: Some("git".into()),
                        ..Default::default()
                    }),
                    path: segs(&["", "owner", "repo.git"]),
                    ..Default::default()
                },
            ),
            (
                "github.com:owner/repo.git",
                "ssh://github.com/owner/repo.git",
                ParsedUrl {
                    scheme: "ssh".into(),
                    authority: Some(name("github.com")),
                    path: segs(&["", "owner", "repo.git"]),
                    ..Default::default()
                },
            ),
            (
                "github.com:/owner/repo.git",
                "ssh://github.com/owner/repo.git",
                ParsedUrl {
                    scheme: "ssh".into(),
                    authority: Some(name("github.com")),
                    path: segs(&["", "owner", "repo.git"]),
                    ..Default::default()
                },
            ),
            (
                "user@server.com:/path/to/repo",
                "ssh://user@server.com/path/to/repo",
                ParsedUrl {
                    scheme: "ssh".into(),
                    authority: Some(Authority {
                        host: "server.com".into(),
                        user: Some("user".into()),
                        ..Default::default()
                    }),
                    path: segs(&["", "path", "to", "repo"]),
                    ..Default::default()
                },
            ),
            (
                "/home/me/repo",
                "file:///home/me/repo",
                ParsedUrl {
                    scheme: "file".into(),
                    authority: Some(Authority::default()),
                    path: segs(&["", "home", "me", "repo"]),
                    ..Default::default()
                },
            ),
            (
                "file:/var/repos/x",
                "file:/var/repos/x",
                ParsedUrl {
                    scheme: "file".into(),
                    authority: None,
                    path: segs(&["", "var", "repos", "x"]),
                    ..Default::default()
                },
            ),
            (
                "git+file:///var/repos/x",
                "file:///var/repos/x",
                ParsedUrl {
                    scheme: "file".into(),
                    authority: Some(Authority::default()),
                    path: segs(&["", "var", "repos", "x"]),
                    ..Default::default()
                },
            ),
            (
                "/repos/git repo",
                "file:///repos/git%20repo",
                ParsedUrl {
                    scheme: "file".into(),
                    authority: Some(Authority::default()),
                    path: segs(&["", "repos", "git repo"]),
                    ..Default::default()
                },
            ),
            (
                "/repos/\"git repo\"",
                "file:///repos/%22git%20repo%22",
                ParsedUrl {
                    scheme: "file".into(),
                    authority: Some(Authority::default()),
                    path: segs(&["", "repos", "\"git repo\""]),
                    ..Default::default()
                },
            ),
            (
                "user@[2001:db8:1::2]:/home/file",
                "ssh://user@[2001:db8:1::2]/home/file",
                ParsedUrl {
                    scheme: "ssh".into(),
                    authority: Some(Authority {
                        host_type: HostType::IPv6,
                        host: "2001:db8:1::2".into(),
                        user: Some("user".into()),
                        ..Default::default()
                    }),
                    path: segs(&["", "home", "file"]),
                    ..Default::default()
                },
            ),
            (
                "git:github.com:nixos/nixpkgs",
                "git:github.com:nixos/nixpkgs",
                ParsedUrl {
                    scheme: "git".into(),
                    authority: None,
                    path: segs(&["github.com:nixos", "nixpkgs"]),
                    ..Default::default()
                },
            ),
            (
                "git+https:/codeberg.org/forgejo/forgejo",
                "https:/codeberg.org/forgejo/forgejo",
                ParsedUrl {
                    scheme: "https".into(),
                    authority: None,
                    path: segs(&["", "codeberg.org", "forgejo", "forgejo"]),
                    ..Default::default()
                },
            ),
            (
                "user%20@[::1]:repo/path",
                "ssh://user%2520@[::1]/repo/path",
                ParsedUrl {
                    scheme: "ssh".into(),
                    authority: Some(Authority {
                        host_type: HostType::IPv6,
                        host: "::1".into(),
                        user: Some("user%20".into()),
                        ..Default::default()
                    }),
                    path: segs(&["", "repo", "path"]),
                    ..Default::default()
                },
            ),
            (
                "[2a02:8071:8192:c100:311d:192d:81ac:11ea]:12345",
                "ssh://[2a02:8071:8192:c100:311d:192d:81ac:11ea]/12345",
                ParsedUrl {
                    scheme: "ssh".into(),
                    authority: Some(Authority {
                        host_type: HostType::IPv6,
                        host: "2a02:8071:8192:c100:311d:192d:81ac:11ea".into(),
                        ..Default::default()
                    }),
                    path: segs(&["", "12345"]),
                    ..Default::default()
                },
            ),
            (
                "/a/b/%20",
                "file:///a/b/%2520",
                ParsedUrl {
                    scheme: "file".into(),
                    authority: Some(Authority::default()),
                    path: segs(&["", "a", "b", "%20"]),
                    ..Default::default()
                },
            ),
            (
                "host[1]:repo",
                "ssh://host%5B1%5D/repo",
                ParsedUrl {
                    scheme: "ssh".into(),
                    authority: Some(name("host[1]")),
                    path: segs(&["", "repo"]),
                    ..Default::default()
                },
            ),
            (
                "[user]@[::1]:path",
                "ssh://%5Buser%5D@[::1]/path",
                ParsedUrl {
                    scheme: "ssh".into(),
                    authority: Some(Authority {
                        host_type: HostType::IPv6,
                        host: "::1".into(),
                        user: Some("[user]".into()),
                        ..Default::default()
                    }),
                    path: segs(&["", "path"]),
                    ..Default::default()
                },
            ),
            (
                "notuser:notpass@notjusthost:restofpath",
                "ssh://notuser/notpass@notjusthost:restofpath",
                ParsedUrl {
                    scheme: "ssh".into(),
                    authority: Some(name("notuser")),
                    path: segs(&["", "notpass@notjusthost:restofpath"]),
                    ..Default::default()
                },
            ),
        ];
        for (input, expected, parsed) in cases {
            let actual = fix(input).unwrap();
            assert_eq!(actual, parsed, "{input}");
            assert_eq!(actual.to_string(), expected, "{input}");
            // Normalising is idempotent.
            let again = fix(expected).unwrap();
            assert_eq!(again, parsed, "{input} (normalized)");
            assert_eq!(again.to_string(), expected, "{input} (normalized)");
        }
    }

    #[test]
    fn fix_git_url_rejections() {
        assert!(fix("file://www.example.org/video.mp4").is_err());
        assert!(fix("relative/path").is_err());
        assert!(fix("host:").is_err());
        // Mismatched brackets fall back to colon splitting.
        let p = fix("user[2001:db8::1]:path").unwrap();
        assert_eq!(p.authority.as_ref().unwrap().host, "user[2001");
        let p = fix("user@[host:path").unwrap();
        assert_eq!(p.authority.as_ref().unwrap().user.as_deref(), Some("user"));
        assert_eq!(p.authority.as_ref().unwrap().host, "[host");
        assert_eq!(fix("[:path").unwrap().authority.unwrap().host, "[");
        assert!(fix("[[]:path")
            .unwrap_err()
            .to_string()
            .contains("is not a valid IPv6 address"));
        assert!(fix("user@[::1]/path")
            .unwrap_err()
            .to_string()
            .contains("is not a valid URL"));
        assert!(fix("user@[host]")
            .unwrap_err()
            .to_string()
            .contains("is not a valid URL"));
        let p = fix("user:[2001:db8:1::2]:/home/@file").unwrap();
        assert_eq!(p.authority.as_ref().unwrap().host, "user");
        let p = fix("user:@[2001:db8:1::2]:/home/file").unwrap();
        assert_eq!(p.authority.as_ref().unwrap().host, "2001:db8:1::2");
        assert_eq!(p.authority.as_ref().unwrap().user.as_deref(), Some("user:"));
        assert!(fix("foo/bar:baz").is_err());
        assert!(fix("user@[::1]foo:path").is_err());
    }
}
