//! `.netrc` credentials for HTTP fetches.
//!
//! Nix hands curl the file named by the `netrc-file` setting with
//! `CURLOPT_NETRC = CURL_NETRC_OPTIONAL` (`src/libstore/filetransfer.cc`):
//! when a request has no explicit `Authorization` header and the file has
//! an entry for the request's host (or a `default` entry), curl sends the
//! login and password as HTTP basic authentication. This is how private
//! FlakeHub and GitHub-hosted inputs are fetched. The file format is
//! curl's: whitespace-separated tokens `machine NAME`, `login USER`,
//! `password PASS`, `account ACCT` (ignored), `default`, and `macdef NAME`
//! followed by a body that ends at an empty line.

use std::path::Path;

use brix_store::hash::base64;

/// One `machine` (or the `default`) entry.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Credentials {
    pub login: String,
    pub password: String,
}

/// A parsed `.netrc` file.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Netrc {
    machines: Vec<(String, Credentials)>,
    default: Option<Credentials>,
}

impl Netrc {
    /// Parses netrc text. Like curl, this is lenient: unknown tokens are
    /// skipped and an entry missing a login or password is kept with the
    /// missing field empty.
    pub fn parse(text: &str) -> Netrc {
        let mut netrc = Netrc::default();
        // `macdef` bodies run until an empty line, so tokenise line by
        // line rather than over the whole text.
        let mut lines = text.lines().peekable();
        // The entry currently being filled: `None` before any `machine`.
        let mut current: Option<(Option<String>, Credentials)> = None;
        let flush = |current: &mut Option<(Option<String>, Credentials)>, netrc: &mut Netrc| {
            if let Some((name, creds)) = current.take() {
                match name {
                    Some(host) => netrc.machines.push((host, creds)),
                    None => netrc.default = Some(creds),
                }
            }
        };
        while let Some(line) = lines.next() {
            let mut tokens = line.split_whitespace();
            while let Some(tok) = tokens.next() {
                match tok {
                    "machine" => {
                        flush(&mut current, &mut netrc);
                        let host = tokens.next().unwrap_or("").to_owned();
                        current = Some((Some(host), Credentials::default()));
                    }
                    "default" => {
                        flush(&mut current, &mut netrc);
                        current = Some((None, Credentials::default()));
                    }
                    "login" => {
                        if let Some((_, c)) = current.as_mut() {
                            c.login = tokens.next().unwrap_or("").to_owned();
                        }
                    }
                    "password" => {
                        if let Some((_, c)) = current.as_mut() {
                            c.password = tokens.next().unwrap_or("").to_owned();
                        }
                    }
                    "account" => {
                        tokens.next();
                    }
                    "macdef" => {
                        // Skip the macro body: everything up to a blank line.
                        while let Some(l) = lines.peek() {
                            if l.trim().is_empty() {
                                break;
                            }
                            lines.next();
                        }
                        break;
                    }
                    _ => {}
                }
            }
        }
        flush(&mut current, &mut netrc);
        netrc
    }

    /// Reads and parses `path`. A missing or unreadable file is no
    /// credentials at all, as with curl's optional netrc.
    pub fn load(path: &Path) -> Option<Netrc> {
        std::fs::read_to_string(path).ok().map(|t| Netrc::parse(&t))
    }

    /// The credentials for `host`: the first matching `machine` entry
    /// (compared case-insensitively, as curl does), else the `default`.
    pub fn lookup(&self, host: &str) -> Option<&Credentials> {
        self.machines
            .iter()
            .find(|(m, _)| m.eq_ignore_ascii_case(host))
            .map(|(_, c)| c)
            .or(self.default.as_ref())
    }

    /// The `Authorization: Basic …` header value for `host`, if any.
    pub fn basic_auth_header(&self, host: &str) -> Option<String> {
        let c = self.lookup(host)?;
        let raw = format!("{}:{}", c.login, c.password);
        Some(format!("Basic {}", base64::encode(raw.as_bytes())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_machines_and_default() {
        let n = Netrc::parse(
            "machine api.flakehub.com login user password s3cr3t\n\
             machine github.com\n  login x-access-token\n  password ghp_abc\n\
             default login anon password none\n",
        );
        assert_eq!(n.lookup("api.flakehub.com").unwrap().password, "s3cr3t");
        assert_eq!(n.lookup("GitHub.com").unwrap().login, "x-access-token");
        assert_eq!(n.lookup("example.org").unwrap().login, "anon");
    }

    #[test]
    fn no_default_means_no_match() {
        let n = Netrc::parse("machine a.example login u password p");
        assert!(n.lookup("b.example").is_none());
        // `u:p` in base64.
        assert_eq!(n.basic_auth_header("a.example").unwrap(), "Basic dTpw");
    }

    #[test]
    fn macdef_bodies_are_skipped_and_accounts_ignored() {
        let n = Netrc::parse(
            "machine a login u account acct password p\n\
             macdef init\nmachine evil login x password y\n\n\
             machine b login v password q\n",
        );
        assert_eq!(n.lookup("a").unwrap().password, "p");
        assert!(n.lookup("evil").is_none());
        assert_eq!(n.lookup("b").unwrap().login, "v");
    }

    #[test]
    fn missing_fields_are_empty() {
        let n = Netrc::parse("machine a login only");
        assert_eq!(n.lookup("a").unwrap().password, "");
        assert!(Netrc::load(Path::new("/nonexistent/netrc")).is_none());
    }
}
