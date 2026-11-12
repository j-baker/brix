//! URL parsing and printing with Nix's exact rules (`src/libutil/url.cc`).
//!
//! Nix's `ParsedURL` is not a general-purpose URL type: the path is kept as
//! *decoded* segments, the query is a map sorted by key, and printing
//! re-encodes everything with Nix's own "keep" sets. Store references, lock
//! files and flake references round-trip through this representation, so
//! the details are observable and reproduced here.
//!
//! Nix validates URLs with Boost.URL and reports its grammar errors by
//! their message: a `%` that does not start a valid escape is a
//! `mismatch`, any other character a rule does not accept leaves
//! `leftover` input. The validation here follows the RFC 3986 grammar
//! Boost.URL implements and reports the same two words.
//!
//! Decoding a query warns about parameters without `=` (Nix's `warn`),
//! through the sink the caller passes.

use std::collections::BTreeMap;
use std::fmt;

/// `BadURL`: a URL that does not parse, with Nix's message.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct BadUrl(pub String);

macro_rules! bad_url {
    ($($arg:tt)*) => { BadUrl(format!($($arg)*)) };
}

/// Boost.URL's message for a `%` that does not start a valid escape.
const MISMATCH: &str = "mismatch";
/// Boost.URL's message for input a rule stopped before.
const LEFTOVER: &str = "leftover";

/// `ParsedURL::Authority::HostType`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum HostType {
    /// A registered name (possibly empty).
    #[default]
    Name,
    IPv4,
    IPv6,
    IPvFuture,
}

/// The `//user:password@host:port` part of a URL, decoded.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct Authority {
    pub host_type: HostType,
    pub host: String,
    pub user: Option<String>,
    pub password: Option<String>,
    pub port: Option<u16>,
}

/// An authority that passed the grammar, before its parts are decoded.
struct RawAuthority<'a> {
    userinfo: Option<&'a str>,
    host_type: HostType,
    /// The host as written, without the brackets of an IP literal.
    host: &'a str,
    port: &'a str,
}

/// `authority = [ userinfo "@" ] host [ ":" port ]` (RFC 3986), as
/// Boost.URL's `authority_rule` reads it. The error is Boost.URL's
/// message for the first character the grammar rejects.
fn check_authority(encoded: &str) -> Result<RawAuthority<'_>, &'static str> {
    // `userinfo` is everything up to the first `@`, if all of it is
    // userinfo; otherwise there is none and the host starts at the
    // beginning (where the same character then stops the host).
    let (userinfo, hostport) = match encoded.split_once('@') {
        Some((u, rest))
            if scan(u, |b| is_unreserved(b) || is_sub_delim(b) || b == b':').is_ok() =>
        {
            (Some(u), rest)
        }
        _ => (None, encoded),
    };
    if let Some(rest) = hostport.strip_prefix('[') {
        let close = rest.find(']').ok_or(MISMATCH)?;
        let literal = &rest[..close];
        let port = match &rest[close + 1..] {
            "" => "",
            after => after.strip_prefix(':').ok_or(LEFTOVER)?,
        };
        let host_type = if literal.starts_with(['v', 'V']) {
            if !is_ipv_future(literal) {
                return Err(MISMATCH);
            }
            HostType::IPvFuture
        } else {
            // RFC 6874 zone id: `%25` followed by the interface name.
            let (addr, zone) = match literal.split_once("%25") {
                Some((a, z)) => (a, Some(z)),
                None => (literal, None),
            };
            if !is_ipv6_address(addr) {
                return Err(MISMATCH);
            }
            if let Some(z) = zone {
                scan(z, is_unreserved)?;
            }
            HostType::IPv6
        };
        check_port(port)?;
        return Ok(RawAuthority {
            userinfo,
            host_type,
            host: literal,
            port,
        });
    }
    let (host, port) = match hostport.find(':') {
        Some(i) => (&hostport[..i], &hostport[i + 1..]),
        None => (hostport, ""),
    };
    scan(host, |b| is_unreserved(b) || is_sub_delim(b))?;
    check_port(port)?;
    let host_type = if is_ipv4_address(host) {
        HostType::IPv4
    } else {
        HostType::Name
    };
    Ok(RawAuthority {
        userinfo,
        host_type,
        host,
        port,
    })
}

/// `port = *DIGIT`.
fn check_port(port: &str) -> Result<(), &'static str> {
    if port.bytes().all(|b| b.is_ascii_digit()) {
        Ok(())
    } else {
        Err(LEFTOVER)
    }
}

/// `IPvFuture = "v" 1*HEXDIG "." 1*( unreserved / sub-delims / ":" )`.
fn is_ipv_future(s: &str) -> bool {
    let Some((version, rest)) = s[1..].split_once('.') else {
        return false;
    };
    !version.is_empty()
        && version.bytes().all(|b| b.is_ascii_hexdigit())
        && !rest.is_empty()
        && rest
            .bytes()
            .all(|b| is_unreserved(b) || is_sub_delim(b) || b == b':')
}

impl Authority {
    /// `ParsedURL::Authority::parse`: parse the encoded authority component.
    pub fn parse(encoded: &str) -> Result<Authority, BadUrl> {
        let raw = check_authority(encoded)
            .map_err(|e| bad_url!("invalid URL authority: '{}': {}", encoded, e))?;
        Authority::decode(raw)
    }

    /// The decoded parts of an authority the grammar accepted. A port that
    /// is zero or too large is Nix's own error.
    fn decode(raw: RawAuthority<'_>) -> Result<Authority, BadUrl> {
        let (user, password) = match raw.userinfo {
            Some(ui) => match ui.split_once(':') {
                Some((u, p)) => (Some(percent_decode(u)?), Some(percent_decode(p)?)),
                None => (Some(percent_decode(ui)?), None),
            },
            None => (None, None),
        };
        let host = match raw.host_type {
            HostType::IPv4 | HostType::IPvFuture => raw.host.to_owned(),
            HostType::IPv6 => match raw.host.split_once("%25") {
                Some((addr, zone)) => format!("{addr}%{}", percent_decode(zone)?),
                None => raw.host.to_owned(),
            },
            HostType::Name => percent_decode(raw.host)?,
        };
        let port = match raw.port {
            "" => None,
            p => match p.parse::<u16>() {
                Ok(0) | Err(_) => return Err(bad_url!("port '{}' is invalid", p)),
                Ok(n) => Some(n),
            },
        };
        Ok(Authority {
            host_type: raw.host_type,
            host,
            user,
            password,
            port,
        })
    }
}

impl fmt::Display for Authority {
    /// `operator<<(std::ostream &, const ParsedURL::Authority &)`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(user) = &self.user {
            f.write_str(&percent_encode(user, ""))?;
            if let Some(password) = &self.password {
                write!(f, ":{}", percent_encode(password, ""))?;
            }
            f.write_str("@")?;
        }
        match self.host_type {
            HostType::Name => f.write_str(&percent_encode(&self.host, ""))?,
            HostType::IPv4 => f.write_str(&self.host)?,
            // Re-encode the percent sign of RFC 4007 zone ids.
            HostType::IPv6 | HostType::IPvFuture => {
                write!(f, "[{}]", percent_encode(&self.host, ":"))?
            }
        }
        if let Some(port) = self.port {
            write!(f, ":{port}")?;
        }
        Ok(())
    }
}

/// A URL split the way Nix's `ParsedURL` splits it.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct ParsedUrl {
    pub scheme: String,
    pub authority: Option<Authority>,
    /// Decoded path segments. An absolute path starts with an empty
    /// segment; a trailing slash ends with one.
    pub path: Vec<String>,
    /// Decoded query parameters, sorted by key (Nix uses `std::map`).
    pub query: BTreeMap<String, String>,
    /// Decoded fragment.
    pub fragment: String,
}

/// `(applicationScheme+)?transportScheme`, e.g. `tarball+https`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UrlScheme<'a> {
    pub application: Option<&'a str>,
    pub transport: &'a str,
}

/// `parseUrlScheme`.
pub fn parse_url_scheme(scheme: &str) -> UrlScheme<'_> {
    match scheme.split_once('+') {
        Some((app, transport)) => UrlScheme {
            application: Some(app),
            transport,
        },
        None => UrlScheme {
            application: None,
            transport: scheme,
        },
    }
}

/// `isValidSchemeName`: `[a-zA-Z][a-zA-Z0-9+.-]*`.
fn is_valid_scheme_name(s: &str) -> bool {
    let mut bytes = s.bytes();
    matches!(bytes.next(), Some(b) if b.is_ascii_alphabetic())
        && bytes.all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'.' | b'-'))
}

fn is_unreserved(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~')
}

fn is_sub_delim(b: u8) -> bool {
    matches!(
        b,
        b'!' | b'$' | b'&' | b'\'' | b'(' | b')' | b'*' | b'+' | b',' | b';' | b'='
    )
}

/// `pchar` without `pct-encoded`, which [`scan`] handles.
fn is_pchar(b: u8) -> bool {
    is_unreserved(b) || is_sub_delim(b) || matches!(b, b':' | b'@')
}

/// Check that every byte of `s` is `allowed` or starts a well-formed
/// percent escape. The error is Boost.URL's for the first offending byte:
/// `mismatch` for a bad escape, `leftover` for anything else.
fn scan(s: &str, allowed: impl Fn(u8) -> bool) -> Result<(), &'static str> {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'%' {
            match bytes.get(i + 1..i + 3) {
                Some([h, l]) if h.is_ascii_hexdigit() && l.is_ascii_hexdigit() => i += 3,
                _ => return Err(MISMATCH),
            }
        } else if allowed(b) {
            i += 1;
        } else {
            return Err(LEFTOVER);
        }
    }
    Ok(())
}

/// The characters of a path (`segment *( "/" segment )`).
fn check_path(s: &str) -> Result<(), &'static str> {
    scan(s, |b| is_pchar(b) || b == b'/')
}

/// The query: `*( pchar / "/" / "?" )`. Boost.URL's query rule stops at
/// a bad escape instead of failing on it, so everything is `leftover`.
fn check_query(s: &str) -> Result<(), &'static str> {
    scan(s, |b| is_pchar(b) || b == b'/' || b == b'?').map_err(|_| LEFTOVER)
}

/// The fragment: `*( pchar / "/" / "?" )`, and Boost.URL also takes `#`.
fn check_fragment(s: &str) -> Result<(), &'static str> {
    scan(s, |b| is_pchar(b) || matches!(b, b'/' | b'?' | b'#'))
}

/// Whether `s` is a dotted-decimal IPv4 address (`IPv4address`).
pub fn is_ipv4_address(s: &str) -> bool {
    let parts: Vec<&str> = s.split('.').collect();
    parts.len() == 4
        && parts.iter().all(|p| {
            !p.is_empty()
                && p.len() <= 3
                && p.bytes().all(|b| b.is_ascii_digit())
                && p.parse::<u16>().is_ok_and(|n| n <= 255)
        })
}

/// Whether `s` is an IPv6 address (`IPv6address`).
pub fn is_ipv6_address(s: &str) -> bool {
    s.parse::<std::net::Ipv6Addr>().is_ok()
}

/// `percentEncode(s, keep)`: encode every byte that is not unreserved and
/// not in `keep`, as `%XX` with upper-case hex.
pub fn percent_encode(s: &str, keep: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        if is_unreserved(b) || keep.as_bytes().contains(&b) {
            out.push(b as char);
        } else {
            push_escape(&mut out, b);
        }
    }
    out
}

fn push_escape(out: &mut String, b: u8) {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    out.push('%');
    out.push(HEX[usize::from(b >> 4)] as char);
    out.push(HEX[usize::from(b & 0xf)] as char);
}

/// Encode only the characters in `set` (Nix's `percentEncodeCharSet`, used
/// to make lenient URLs well-formed before strict parsing).
fn percent_encode_char_set(s: &str, set: &[u8]) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match u8::try_from(c) {
            Ok(b) if set.contains(&b) => push_escape(&mut out, b),
            _ => out.push(c),
        }
    }
    out
}

/// `percentDecode`: decode `%XX` escapes, with Boost.URL's messages for
/// malformed ones. Nix decodes to bytes; brix's URL parts are strings, so
/// an escape that decodes to bytes that are not UTF-8 is an error too.
pub fn percent_decode(s: &str) -> Result<String, BadUrl> {
    let mut out = Vec::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let (h, l) = match bytes.get(i + 1..i + 3) {
                Some(&[h, l]) => (h, l),
                _ => {
                    return Err(bad_url!(
                        "invalid URI parameter '{}': incomplete pct-encoding",
                        s
                    ))
                }
            };
            let digit = |d: u8| (d as char).to_digit(16);
            let (Some(h), Some(l)) = (digit(h), digit(l)) else {
                return Err(bad_url!(
                    "invalid URI parameter '{}': bad hexdig in pct-encoding",
                    s
                ));
            };
            out.push((h * 16 + l) as u8);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).map_err(|_| bad_url!("invalid URI parameter '{}': not valid UTF-8", s))
}

/// `decodeQuery`: `k=v&k2=v2` into a sorted map. A parameter without `=`
/// is ignored with Nix's warning, given to `warn`; of a repeated key the
/// first value is kept. With `lenient`, unescaped spaces and `"` are
/// accepted (old flake references and store URIs have them).
pub fn decode_query(
    query: &str,
    lenient: bool,
    warn: &dyn Fn(&str),
) -> Result<BTreeMap<String, String>, BadUrl> {
    let fixed;
    let encoded = if lenient {
        fixed = percent_encode_char_set(query, b" \"");
        fixed.as_str()
    } else {
        query
    };
    check_query(encoded).map_err(|e| bad_url!("invalid URI query '{}': {}", query, e))?;
    let mut result = BTreeMap::new();
    if encoded.is_empty() {
        return Ok(result);
    }
    for param in encoded.split('&') {
        let Some((k, v)) = param.split_once('=') else {
            warn(&format!(
                "dubious URI query '{param}' is missing equal sign '=', ignoring"
            ));
            continue;
        };
        let k = percent_decode(k)?;
        let v = percent_decode(v)?;
        result.entry(k).or_insert(v);
    }
    Ok(result)
}

const ALLOWED_IN_QUERY: &str = ":@/?";
const ALLOWED_IN_PATH: &str = ":@";

/// `encodeQuery`.
pub fn encode_query(query: &BTreeMap<String, String>) -> String {
    query
        .iter()
        .map(|(k, v)| {
            format!(
                "{}={}",
                percent_encode(k, ALLOWED_IN_QUERY),
                percent_encode(v, ALLOWED_IN_QUERY)
            )
        })
        .collect::<Vec<_>>()
        .join("&")
}

/// `encodeUrlPath`.
pub fn encode_url_path(path: &[String]) -> String {
    path.iter()
        .map(|p| percent_encode(p, ALLOWED_IN_PATH))
        .collect::<Vec<_>>()
        .join("/")
}

/// `parseURL`. With `lenient`, unescaped spaces and `"` in the query and
/// spaces, `"` and `^` in the fragment are accepted, for compatibility with
/// old flake references and store URIs. Query parameters without `=` are
/// warned about through `warn`.
pub fn parse_url(url: &str, lenient: bool, warn: &dyn Fn(&str)) -> Result<ParsedUrl, BadUrl> {
    let fixed;
    let input = if lenient {
        fixed = lenient_fixup(url);
        fixed.as_str()
    } else {
        url
    };
    // A grammar error names the URL as given; the later checks name
    // the URL they parsed (`urlView.buffer()`).
    let parts = split_url(input).map_err(|e| bad_url!("'{}' is not a valid URL: {}", url, e))?;
    from_parts(input, parts, lenient, warn)
}

fn lenient_fixup(url: &str) -> String {
    let mut fixed = String::with_capacity(url.len());
    let mut rest = url;
    if let Some((before, after)) = rest.split_once('?') {
        fixed.push_str(before);
        fixed.push('?');
        let frag = after.find('#').unwrap_or(after.len());
        fixed.push_str(&percent_encode_char_set(&after[..frag], b" \""));
        rest = &after[frag..];
    }
    if let Some((before, after)) = rest.split_once('#') {
        fixed.push_str(before);
        fixed.push('#');
        fixed.push_str(&percent_encode_char_set(after, b" \"^"));
        return fixed;
    }
    fixed.push_str(rest);
    fixed
}

/// A URI reference split into its encoded components, each checked
/// against the grammar.
struct UrlParts<'a> {
    /// `None` for a relative reference.
    scheme: Option<&'a str>,
    authority: Option<RawAuthority<'a>>,
    path: &'a str,
    query: Option<&'a str>,
    fragment: Option<&'a str>,
}

/// `URI-reference` (RFC 3986), as `boost::urls::url_view` parses it.
fn split_url(url: &str) -> Result<UrlParts<'_>, &'static str> {
    let colon = url.find(':').filter(|&i| is_valid_scheme_name(&url[..i]));
    let (scheme, mut rest) = match colon {
        Some(i) => (Some(&url[..i]), &url[i + 1..]),
        None => (None, url),
    };

    let authority = match rest.strip_prefix("//") {
        Some(after) => {
            let end = after.find(['/', '?', '#']).unwrap_or(after.len());
            let auth = check_authority(&after[..end])?;
            rest = &after[end..];
            Some(auth)
        }
        None => None,
    };

    let path_end = rest.find(['?', '#']).unwrap_or(rest.len());
    let path = &rest[..path_end];
    check_path(path)?;
    // `path-noscheme`: in a relative reference, the first segment of a
    // path that is not absolute cannot hold a colon (it would be a
    // scheme).
    if scheme.is_none()
        && authority.is_none()
        && path.split('/').next().is_some_and(|s| s.contains(':'))
    {
        return Err(MISMATCH);
    }
    rest = &rest[path_end..];

    let query = match rest.strip_prefix('?') {
        Some(after) => {
            let end = after.find('#').unwrap_or(after.len());
            check_query(&after[..end])?;
            rest = &after[end..];
            Some(&after[..end])
        }
        None => None,
    };
    let fragment = match rest.strip_prefix('#') {
        Some(f) => {
            check_fragment(f)?;
            Some(f)
        }
        None => None,
    };
    Ok(UrlParts {
        scheme,
        authority,
        path,
        query,
        fragment,
    })
}

/// `fromBoostUrlView`: decode the parts of a URL the grammar accepted.
/// `url` is the URL that was parsed.
fn from_parts(
    url: &str,
    parts: UrlParts<'_>,
    lenient: bool,
    warn: &dyn Fn(&str),
) -> Result<ParsedUrl, BadUrl> {
    let Some(scheme) = parts.scheme else {
        return Err(bad_url!("'{}' doesn't have a scheme", url));
    };
    let authority = parts.authority.map(Authority::decode).transpose()?;

    // RFC 3986 §3.2.2: for `file`, no authority, an empty host and
    // `localhost` all mean this machine; Nix accepts only the first two.
    let transport_is_file = parse_url_scheme(scheme).transport == "file";
    if let Some(auth) = &authority {
        if transport_is_file && !auth.host.is_empty() {
            return Err(bad_url!(
                "file:// URL '{}' has unexpected authority '{}'",
                url,
                auth
            ));
        }
    }

    let fragment = percent_decode(parts.fragment.unwrap_or(""))?;
    let encoded_path = if transport_is_file && parts.path.is_empty() {
        "/"
    } else {
        parts.path
    };
    let path = encoded_path
        .split('/')
        .map(percent_decode)
        .collect::<Result<Vec<_>, _>>()?;

    Ok(ParsedUrl {
        scheme: scheme.to_owned(),
        authority,
        path,
        query: decode_query(parts.query.unwrap_or(""), lenient, warn)?,
        fragment,
    })
}

impl ParsedUrl {
    /// `ParsedURL::pathSegments(skipEmpty = true)`.
    pub fn non_empty_path_segments(&self) -> impl Iterator<Item = &str> {
        self.path
            .iter()
            .map(String::as_str)
            .filter(|s| !s.is_empty())
    }

    /// `ParsedURL::renderAuthorityAndPath`: what follows `<scheme>:` and
    /// `//`, up to the query.
    pub fn render_authority_and_path(&self) -> String {
        let mut out = match &self.authority {
            Some(auth) => auth.to_string(),
            None => String::new(),
        };
        out.push_str(&encode_url_path(&self.path));
        out
    }
}

impl fmt::Display for ParsedUrl {
    /// `ParsedURL::to_string`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:", self.scheme)?;
        if self.authority.is_some() {
            f.write_str("//")?;
        }
        f.write_str(&self.render_authority_and_path())?;
        if !self.query.is_empty() {
            write!(f, "?{}", encode_query(&self.query))?;
        }
        if !self.fragment.is_empty() {
            write!(f, "#{}", percent_encode(&self.fragment, ""))?;
        }
        Ok(())
    }
}

/// `pathToUrlPath`: split a file system path into URL path segments the
/// way `std::filesystem::path` iteration does (leading `""` for absolute
/// paths, trailing `""` for a trailing slash, repeated slashes collapsed).
pub fn path_to_url_path(path: &str) -> Vec<String> {
    let mut out = Vec::new();
    if path.starts_with('/') {
        out.push(String::new());
    }
    for comp in path.split('/').filter(|c| !c.is_empty()) {
        out.push(comp.to_string());
    }
    // `path.filename().empty()`: a trailing separator (or a root-only path).
    if path.ends_with('/') {
        out.push(String::new());
    }
    out
}

/// `urlPathToPath`: join URL path segments back into a file system path.
pub fn url_path_to_path(segments: &[String]) -> Result<String, BadUrl> {
    for comp in segments {
        if comp.contains('/') {
            return Err(bad_url!(
                "URL path component '{}' contains '/', which is not allowed in file names",
                comp
            ));
        }
        if comp.contains('\0') {
            return Err(bad_url!(
                "URL path component '{}' contains NUL byte which is not allowed",
                comp.replace('\0', "␀")
            ));
        }
    }
    let Some((first, rest)) = segments.split_first() else {
        return Ok(String::new());
    };
    if first.is_empty() {
        Ok(format!("/{}", rest.join("/")))
    } else {
        Ok(segments.join("/"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    fn no_warning(msg: &str) {
        panic!("unexpected warning: {msg}");
    }

    fn parse(url: &str) -> Result<ParsedUrl, BadUrl> {
        parse_url(url, false, &no_warning)
    }

    fn err(url: &str) -> String {
        parse(url).unwrap_err().to_string()
    }

    fn q(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
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
    fn parses_as_expected_and_round_trips() {
        let cases: Vec<(&str, ParsedUrl)> = vec![
            (
                "http://www.example.org/file.tar.gz",
                ParsedUrl {
                    scheme: "http".into(),
                    authority: Some(name("www.example.org")),
                    path: segs(&["", "file.tar.gz"]),
                    ..Default::default()
                },
            ),
            (
                "https://www.example.org/file.tar.gz?download=fast&when=now#hello",
                ParsedUrl {
                    scheme: "https".into(),
                    authority: Some(name("www.example.org")),
                    path: segs(&["", "file.tar.gz"]),
                    query: q(&[("download", "fast"), ("when", "now")]),
                    fragment: "hello".into(),
                },
            ),
            (
                "file+https://www.example.org/video.mp4",
                ParsedUrl {
                    scheme: "file+https".into(),
                    authority: Some(name("www.example.org")),
                    path: segs(&["", "video.mp4"]),
                    ..Default::default()
                },
            ),
            (
                "http://127.0.0.1:8080/file.tar.gz?download=fast&when=now#hello",
                ParsedUrl {
                    scheme: "http".into(),
                    authority: Some(Authority {
                        host_type: HostType::IPv4,
                        host: "127.0.0.1".into(),
                        port: Some(8080),
                        ..Default::default()
                    }),
                    path: segs(&["", "file.tar.gz"]),
                    query: q(&[("download", "fast"), ("when", "now")]),
                    fragment: "hello".into(),
                },
            ),
            (
                "http://[fe80::818c:da4d:8975:415c%25enp0s25]:8080",
                ParsedUrl {
                    scheme: "http".into(),
                    authority: Some(Authority {
                        host_type: HostType::IPv6,
                        host: "fe80::818c:da4d:8975:415c%enp0s25".into(),
                        port: Some(8080),
                        ..Default::default()
                    }),
                    path: segs(&[""]),
                    ..Default::default()
                },
            ),
            (
                "http://[2a02:8071:8192:c100:311d:192d:81ac:11ea]:8080",
                ParsedUrl {
                    scheme: "http".into(),
                    authority: Some(Authority {
                        host_type: HostType::IPv6,
                        host: "2a02:8071:8192:c100:311d:192d:81ac:11ea".into(),
                        port: Some(8080),
                        ..Default::default()
                    }),
                    path: segs(&[""]),
                    ..Default::default()
                },
            ),
        ];
        for (input, expected) in cases {
            let parsed = parse(input).unwrap();
            assert_eq!(parsed, expected, "{input}");
            assert_eq!(parsed.to_string(), input);
        }
    }

    #[test]
    fn complex_fragment() {
        let parsed = parse("http://www.example.org/file.tar.gz?field=value#?foo=bar%23").unwrap();
        assert_eq!(parsed.query, q(&[("field", "value")]));
        assert_eq!(parsed.fragment, "?foo=bar#");
        // Boost.URL takes a `#` inside the fragment.
        assert_eq!(
            parse("https://x/#a#b").unwrap().to_string(),
            "https://x/#a%23b"
        );
    }

    #[test]
    fn rejects_authority_with_file_transport() {
        assert_eq!(
            err("file://www.example.org/video.mp4"),
            "file:// URL 'file://www.example.org/video.mp4' has unexpected authority 'www.example.org'"
        );
    }

    #[test]
    fn query_params_without_equal_sign_warn() {
        // `nix store info --store 'daemon?&&'` warns three times.
        let warnings = RefCell::new(Vec::new());
        let warn = |w: &str| warnings.borrow_mut().push(w.to_owned());
        let parsed = parse_url("http://127.0.0.1:8080/file.tar.gz?&&&&&", false, &warn).unwrap();
        assert!(parsed.query.is_empty());
        assert_eq!(warnings.borrow().len(), 6);
        assert_eq!(
            warnings.borrow()[0],
            "dubious URI query '' is missing equal sign '=', ignoring"
        );
        warnings.borrow_mut().clear();
        let query = decode_query("foo&&bar=1", true, &warn).unwrap();
        assert_eq!(query, q(&[("bar", "1")]));
        assert_eq!(
            *warnings.borrow(),
            [
                "dubious URI query 'foo' is missing equal sign '=', ignoring",
                "dubious URI query '' is missing equal sign '=', ignoring"
            ]
        );
    }

    #[test]
    fn user_password() {
        let s = "http://user:pass@www.example.org:8080/file.tar.gz";
        let parsed = parse(s).unwrap();
        let auth = parsed.authority.clone().unwrap();
        assert_eq!(auth.user.as_deref(), Some("user"));
        assert_eq!(auth.password.as_deref(), Some("pass"));
        assert_eq!(auth.port, Some(8080));
        assert_eq!(parsed.to_string(), s);
        let auth = parse("https://a:b:c@d/").unwrap().authority.unwrap();
        assert_eq!(auth.password.as_deref(), Some("b:c"));
    }

    #[test]
    fn file_urls() {
        let p = parse("file:///none/of//your/business").unwrap();
        assert_eq!(p.authority, Some(Authority::default()));
        assert_eq!(p.path, segs(&["", "none", "of", "", "your", "business"]));
        assert_eq!(p.to_string(), "file:///none/of//your/business");

        let p = parse("file:/none/of/your/business/").unwrap();
        assert_eq!(p.authority, None);
        assert_eq!(p.path, segs(&["", "none", "of", "your", "business", ""]));
        assert_eq!(p.to_string(), "file:/none/of/your/business/");

        let p = parse("file://///of/your/business//").unwrap();
        assert_eq!(
            p.path,
            segs(&["", "", "", "of", "your", "business", "", ""])
        );
        assert_eq!(p.to_string(), "file://///of/your/business//");

        let p = parse("file:none/of/your/business/").unwrap();
        assert_eq!(p.path, segs(&["none", "of", "your", "business", ""]));
        assert_eq!(p.to_string(), "file:none/of/your/business/");
    }

    #[test]
    fn http_trailing_slash_and_empty_port() {
        let p = parse("http://example.com/").unwrap();
        assert_eq!(p.path, segs(&["", ""]));
        assert_eq!(p.to_string(), "http://example.com/");

        let p = parse("http://www.example.org:/file.tar.gz?foo=bar").unwrap();
        assert_eq!(p.authority.as_ref().unwrap().port, None);
        assert_eq!(p.to_string(), "http://www.example.org/file.tar.gz?foo=bar");
        assert_eq!(parse("https://x:08/").unwrap().to_string(), "https://x:8/");
    }

    #[test]
    fn lenient_unescaped_chars() {
        let s = "http://www.example.org/file.tar.gz?query \"= 123\"#shevron^quote\"space ";
        assert!(parse(s).is_err());
        let p = parse_url(s, true, &no_warning).unwrap();
        assert_eq!(p.fragment, "shevron^quote\"space ");
        assert_eq!(p.query, q(&[("query \"", " 123\"")]));
    }

    #[test]
    fn misc_schemes() {
        for s in [
            "ftp://ftp.nixos.org/downloads/nixos.iso",
            "whatever://github.com/NixOS/nixpkgs.git",
            "whatever:github.com/NixOS/nixpkgs.git",
        ] {
            assert_eq!(parse(s).unwrap().to_string(), s);
        }
        assert_eq!(err(""), "'' doesn't have a scheme");
        assert_eq!(err("/foo/bar"), "'/foo/bar' doesn't have a scheme");
    }

    /// Boost.URL's messages, from `builtins.fetchTree { type = "git";
    /// url = …; }` (strict parsing) in Nix.
    #[test]
    fn grammar_errors_match_boost() {
        for (url, what) in [
            ("https://x/%zz", "mismatch"),
            ("https://x/%", "mismatch"),
            ("https://x/a%2", "mismatch"),
            ("https://x/a b", "leftover"),
            ("https://x/a^", "leftover"),
            ("https://x/a[", "leftover"),
            ("https://x/a\"", "leftover"),
            ("https://x/?a=%zz", "leftover"),
            ("https://x/?a=%", "leftover"),
            ("https://x/?a b=1", "leftover"),
            ("https://x/?a=%zz#%zz", "leftover"),
            ("https://x/?a=b#c%", "mismatch"),
            ("https://x/#a%z", "mismatch"),
            ("https://x/#a b", "leftover"),
            ("https://a b/", "leftover"),
            ("https://x%zz/", "mismatch"),
            ("https://x%/", "mismatch"),
            ("https://u%zz@x/", "mismatch"),
            ("https://a@b@c/", "leftover"),
            ("https://x:abc/", "leftover"),
            ("https://[::1/", "mismatch"),
            ("https://[zz]/", "mismatch"),
            ("https://[::1]x/", "leftover"),
            ("https://[::1%eth0]/", "mismatch"),
            ("https:/a%zz", "mismatch"),
            ("https:a%zz", "mismatch"),
            ("https:a b", "leftover"),
            ("a%zz", "mismatch"),
            ("a b", "leftover"),
            ("1http://x", "mismatch"),
        ] {
            assert_eq!(
                err(url),
                format!("'{url}' is not a valid URL: {what}"),
                "{url}"
            );
        }
        assert_eq!(err("https://x:99999/"), "port '99999' is invalid");
        assert_eq!(err("https://x:0/"), "port '0' is invalid");
        assert!(parse("https://[v1.x]/").is_ok());
        assert!(parse("https://[::1%25eth0]/").is_ok());
        // In lenient mode the message names the URL as given.
        assert_eq!(
            parse_url("https://x/a b?c d", true, &no_warning)
                .unwrap_err()
                .to_string(),
            "'https://x/a b?c d' is not a valid URL: leftover"
        );
    }

    #[test]
    fn authority_errors() {
        assert_eq!(
            Authority::parse("host/foo").unwrap_err().to_string(),
            "invalid URL authority: 'host/foo': leftover"
        );
        assert_eq!(
            Authority::parse("host:99999").unwrap_err().to_string(),
            "port '99999' is invalid"
        );
        let a = Authority::parse("alice@[fe80::1%25eth0]:22").unwrap();
        assert_eq!(a.user.as_deref(), Some("alice"));
        assert_eq!(a.host, "fe80::1%eth0");
        assert_eq!(a.host_type, HostType::IPv6);
        assert_eq!(a.to_string(), "alice@[fe80::1%25eth0]:22");
        assert_eq!(Authority::parse("ho%20st").unwrap().host, "ho st");
    }

    #[test]
    fn query_and_percent_helpers() {
        assert!(decode_query("", false, &no_warning).unwrap().is_empty());
        assert_eq!(
            decode_query("yi=one&er=two", false, &no_warning).unwrap(),
            q(&[("yi", "one"), ("er", "two")])
        );
        assert_eq!(
            decode_query("arg=%3D%3D%40%3D%3D", false, &no_warning).unwrap(),
            q(&[("arg", "==@==")])
        );
        assert_eq!(
            decode_query("arg=", false, &no_warning).unwrap(),
            q(&[("arg", "")])
        );
        // The first value of a repeated key wins.
        assert_eq!(
            decode_query("a=1&a=2", false, &no_warning).unwrap(),
            q(&[("a", "1")])
        );
        // `nix store info --store 'daemon?a=%zz'`, `'daemon?a^=c'`.
        for query in ["a=%zz", "a=%", "a^=c", "a=c#x"] {
            assert_eq!(
                decode_query(query, true, &no_warning)
                    .unwrap_err()
                    .to_string(),
                format!("invalid URI query '{query}': leftover")
            );
        }
        assert_eq!(
            decode_query("a b=c", true, &no_warning).unwrap(),
            q(&[("a b", "c")])
        );
        assert_eq!(percent_decode("%3D%3D%40%3D%3D%25").unwrap(), "==@==%");
        // `builtins.parseFlakeRef "/foo#%zz"` and friends.
        assert_eq!(
            percent_decode("%zz").unwrap_err().to_string(),
            "invalid URI parameter '%zz': bad hexdig in pct-encoding"
        );
        assert_eq!(
            percent_decode("%z").unwrap_err().to_string(),
            "invalid URI parameter '%z': incomplete pct-encoding"
        );
        assert_eq!(
            percent_decode("%").unwrap_err().to_string(),
            "invalid URI parameter '%': incomplete pct-encoding"
        );
        assert_eq!(percent_decode("%C3%A9").unwrap(), "é");
        assert_eq!(percent_encode("==@==", ""), "%3D%3D%40%3D%3D");
        assert_eq!(percent_encode("abd / def", ""), "abd%20%2F%20def");
        assert_eq!(percent_encode("abd / def", "/"), "abd%20/%20def");
        assert_eq!(percent_encode("é", ""), "%C3%A9");
    }

    #[test]
    fn scheme_names() {
        assert!(is_valid_scheme_name("http"));
        assert!(is_valid_scheme_name("file+https"));
        assert!(is_valid_scheme_name("file+"));
        assert!(!is_valid_scheme_name("+file"));
        assert!(!is_valid_scheme_name("1http"));
    }

    #[test]
    fn render_authority_and_path() {
        let p = parse("unix:///tmp/my%20sock").unwrap();
        assert_eq!(p.render_authority_and_path(), "/tmp/my%20sock");
        let p = parse("ssh-ng://alice@host:22").unwrap();
        assert_eq!(p.render_authority_and_path(), "alice@host:22");
        assert_eq!(parse("unix:").unwrap().render_authority_and_path(), "");
    }

    #[test]
    fn path_segment_helpers() {
        assert_eq!(path_to_url_path("/foo/bar"), segs(&["", "foo", "bar"]));
        assert_eq!(path_to_url_path("foo/bar"), segs(&["foo", "bar"]));
        assert_eq!(path_to_url_path("/foo/"), segs(&["", "foo", ""]));
        assert_eq!(path_to_url_path("/"), segs(&["", ""]));
        assert_eq!(url_path_to_path(&segs(&["", "a", "b"])).unwrap(), "/a/b");
        assert_eq!(url_path_to_path(&segs(&["a", "b", ""])).unwrap(), "a/b/");
        assert!(url_path_to_path(&segs(&["a/b"])).is_err());
    }
}
