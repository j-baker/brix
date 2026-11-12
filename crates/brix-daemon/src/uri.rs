//! Store URIs the client understands (`StoreReference::parse` in Nix).
//!
//! brix never opens a local store, so only the remote forms are accepted:
//!
//! | URI | meaning |
//! |-----|---------|
//! | `daemon` | the system daemon's socket ([`default_socket_path`]) |
//! | `unix://`, `unix:///path/to/socket` | an explicit unix socket (empty path means the default) |
//! | `ssh-ng://[user@]host[:port]` | `nix-daemon --stdio` over ssh |
//!
//! The URI is read as Nix reads it: as a URL if Nix's (lenient) URL parser
//! takes it ([`brix_store::url`]), otherwise as one of the special forms
//! (`daemon`, `auto`, `local`, a file system path) with a query, or as a
//! scheme followed by an unbracketed IPv6 address. Query parameters
//! (`?key=value&…`) are store settings: a parameter without `=` is
//! ignored with Nix's warning, and of a repeated one the first counts.
//! Each store type accepts the settings Nix defines for it, validated as
//! Nix validates them; an unknown one draws Nix's `warning: unknown
//! setting '…'` and is ignored, as in Nix. Warnings go to the sink the
//! caller passes to [`StoreUri::parse`].
//!
//! Every setting of `StoreConfig` and `RemoteStoreConfig` is accepted by
//! both types:
//!
//! | setting | in brix |
//! |---------|---------|
//! | `store` | must agree with the store directory brix evaluates with, which [`crate::DaemonClient::connect`] checks |
//! | `path-info-cache-size`, `priority`, `want-mass-query`, `trusted`, `system-features` | validated only: they size a client-side cache or describe the store as a substituter or build machine, roles a `--store` never plays |
//! | `max-connections`, `max-connection-age` | validated only: brix opens one connection, within any limit, for the whole session (as Nix with `max-connections = 1`, so `ssh-ng` uses no ssh master) |
//!
//! `unix://` (`LocalFSStoreConfig`) adds `state` and `root`, which choose
//! the default socket (`getDaemonSocketPath`), and `log` and `real`, which
//! are validated only (brix reads build logs and store contents through
//! the daemon, never from those directories). `ssh-ng://`
//! (`CommonSSHStoreConfig`, `SSHStoreConfig`) adds `ssh-key`,
//! `base64-ssh-public-host-key`, `compress`, `remote-store` and
//! `remote-program`, all implemented.
//!
//! brix's URL parts are strings, so a socket path whose escapes decode to
//! bytes that are not UTF-8 cannot be named (Nix connects to it).

use std::collections::BTreeMap;
use std::path::PathBuf;

use brix_store::url::{
    decode_query, encode_url_path, is_ipv6_address, parse_url, path_to_url_path, percent_decode,
    url_path_to_path, Authority, BadUrl,
};

use crate::error::DaemonError;

/// `getDaemonSocketPath` for a store without `state` or `root` settings:
/// `$NIX_DAEMON_SOCKET_PATH`, else `daemon-socket/socket` in Nix's state
/// directory (`$NIX_STATE_DIR`, else `/nix/var/nix`).
pub fn default_socket_path() -> PathBuf {
    socket_path_in(&nix_state_dir())
}

/// `getDaemonSocketPath` for a store whose state directory is `state_dir`.
fn socket_path_in(state_dir: &str) -> PathBuf {
    match std::env::var_os("NIX_DAEMON_SOCKET_PATH") {
        Some(p) if !p.is_empty() => PathBuf::from(p),
        _ => PathBuf::from(format!("{state_dir}/daemon-socket/socket")),
    }
}

/// `settings.nixStateDir`: `$NIX_STATE_DIR` if set and not empty.
fn nix_state_dir() -> String {
    match std::env::var("NIX_STATE_DIR") {
        Ok(d) if !d.is_empty() => d,
        _ => "/nix/var/nix".to_owned(),
    }
}

/// Where the daemon is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Transport {
    /// A unix domain socket.
    UnixSocket(PathBuf),
    /// `ssh-ng://`: run `nix-daemon --stdio` on a remote host over ssh.
    SshNg(SshTarget),
}

/// The `ssh-ng://` parameters (`CommonSSHStoreConfig` + `SSHStoreConfig`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SshTarget {
    pub user: Option<String>,
    pub host: String,
    pub port: Option<u16>,
    /// `?remote-program=`: the daemon command on the remote side.
    /// Default `["nix-daemon"]`.
    pub remote_program: Vec<String>,
    /// `?remote-store=`: passed as `--store` to the remote program.
    pub remote_store: Option<String>,
    /// `?ssh-key=`: passed as `-i`.
    pub ssh_key: Option<String>,
    /// `?base64-ssh-public-host-key=`, decoded: the only host key ssh may
    /// accept, through a known-hosts file of its own.
    pub public_host_key: Option<Vec<u8>>,
    /// `?compress=`: passes `-C`.
    pub compress: bool,
}

/// A parsed store URI.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoreUri {
    pub transport: Transport,
    /// `?store=`: the store directory the URI asks for, canonicalised.
    pub store_dir: Option<String>,
    /// The URI without its settings, as Nix shows it in messages
    /// (`getHumanReadableURI`).
    display: String,
}

/// `StoreReference::Variant`: what a store URI names, before the store
/// type interprets it.
enum Reference {
    Auto,
    Daemon,
    Local,
    /// `<scheme>://<authority>`: the authority and path, as the store
    /// type receives them.
    Specified {
        scheme: String,
        authority: String,
    },
}

/// The store types Nix knows that brix does not open.
const UNSUPPORTED_SCHEMES: [&str; 7] = [
    "dummy",
    "file",
    "http",
    "https",
    "local-overlay",
    "mounted-ssh-ng",
    "s3",
];

impl StoreUri {
    /// The system daemon.
    pub fn daemon() -> Self {
        StoreUri {
            transport: Transport::UnixSocket(default_socket_path()),
            store_dir: None,
            display: "daemon".to_owned(),
        }
    }

    /// The URI as Nix names the store in messages: without settings, and
    /// `daemon` for the default socket (`getHumanReadableURI`).
    pub fn display(&self) -> &str {
        &self.display
    }

    /// Parses `uri` as Nix's `StoreReference::parse` and
    /// `resolveStoreConfig` do, giving `warn` Nix's warnings (a query
    /// parameter without `=`, a setting the store type does not have).
    pub fn parse(uri: &str, warn: &dyn Fn(&str)) -> Result<Self, DaemonError> {
        let invalid = |reason: &str| DaemonError::InvalidUri {
            uri: uri.to_owned(),
            reason: reason.to_owned(),
        };
        let (reference, params) = parse_reference(uri, warn)?;
        let settings = Settings { params, warn };
        let (transport, store_dir, display) = match reference {
            Reference::Auto => {
                return Err(invalid(
                    "brix has no local store; use 'daemon', 'unix://…' or 'ssh-ng://…'",
                ))
            }
            Reference::Local => {
                return Err(invalid("brix cannot open a local store; use 'daemon'"))
            }
            Reference::Daemon => {
                let (store_dir, path, display) = settings.unix(None)?;
                (Transport::UnixSocket(path), store_dir, display)
            }
            Reference::Specified { scheme, authority } => match scheme.as_str() {
                "unix" => {
                    let path = if authority.is_empty() {
                        None
                    } else {
                        Some(socket_path(&authority)?)
                    };
                    let (store_dir, path, display) = settings.unix(path)?;
                    (Transport::UnixSocket(path), store_dir, display)
                }
                "ssh-ng" => {
                    let authority = Authority::parse(&authority).map_err(bad_url)?;
                    let display = format!("ssh-ng://{authority}");
                    let (store_dir, target) = settings.ssh_ng(authority)?;
                    (Transport::SshNg(target), store_dir, display)
                }
                "local" => {
                    return Err(invalid("brix cannot open a local store; use 'daemon'"))
                }
                "ssh" => {
                    return Err(invalid(
                        "the legacy 'ssh://' store speaks the serve protocol, which brix does not implement; use 'ssh-ng://'",
                    ))
                }
                s if UNSUPPORTED_SCHEMES.contains(&s) => {
                    return Err(invalid(&format!("unsupported store scheme '{s}'")))
                }
                other => {
                    return Err(DaemonError::Config(format!(
                        "don't know how to open Nix store with scheme '{other}'"
                    )))
                }
            },
        };
        Ok(StoreUri {
            transport,
            store_dir,
            display,
        })
    }
}

/// A `BadURL` is reported with its message alone.
fn bad_url(e: BadUrl) -> DaemonError {
    DaemonError::Config(e.0)
}

/// `StoreReference::parse`: a URL Nix's lenient parser takes names its
/// store type by its scheme; otherwise the query is split off
/// (`splitUriAndParams`) and the rest is one of the special forms.
fn parse_reference(
    uri: &str,
    warn: &dyn Fn(&str),
) -> Result<(Reference, BTreeMap<String, String>), DaemonError> {
    if let Ok(parsed) = parse_url(uri, true, warn) {
        let authority = parsed.render_authority_and_path();
        let reference = Reference::Specified {
            scheme: parsed.scheme,
            authority,
        };
        return Ok((reference, parsed.query));
    }
    let (base, params) = match uri.split_once('?') {
        Some((base, query)) => (base, decode_query(query, true, warn).map_err(bad_url)?),
        None => (uri, BTreeMap::new()),
    };
    let specified = |scheme: &str, authority: String| Reference::Specified {
        scheme: scheme.to_owned(),
        authority,
    };
    let reference = match base {
        "" | "auto" => Reference::Auto,
        "daemon" if params.is_empty() => Reference::Daemon,
        "daemon" => specified("unix", String::new()),
        "local" if params.is_empty() => Reference::Local,
        "local" => specified("local", String::new()),
        // `isNonUriPath`: a file system path names a local store.
        _ if !base.contains("://") && base.contains('/') => {
            specified("local", encode_url_path(&path_to_url_path(base)))
        }
        _ => match unbracketed_ipv6(base) {
            Some((scheme, authority)) => specified(scheme, authority),
            None => {
                return Err(DaemonError::Usage(format!(
                    "Cannot parse Nix store '{uri}'"
                )))
            }
        },
    };
    Ok((reference, params))
}

/// The back-compatibility shim of `StoreReference::parse` for an IPv6
/// address written without brackets after the scheme
/// (`ssh-ng://fe80::1`, `ssh-ng://user@fe80::1%eth0`): the scheme and the
/// authority with brackets and an encoded zone id.
fn unbracketed_ipv6(base: &str) -> Option<(&str, String)> {
    let (scheme, rest) = base.split_once(':')?;
    let rest = rest.strip_prefix("//").unwrap_or(rest);
    let (userinfo, mut host) = match rest.split_once('@') {
        Some((user, host)) => (Some(user), host),
        None => (None, rest),
    };
    if let Some(inner) = host.strip_prefix('[').and_then(|h| h.strip_suffix(']')) {
        host = inner;
    }
    let (address, zone) = match host.split_once('%') {
        Some((address, zone)) => (address, Some(zone)),
        None => (host, None),
    };
    if !is_ipv6_address(address) {
        return None;
    }
    let mut authority = String::new();
    if let Some(user) = userinfo {
        authority.push_str(user);
        authority.push('@');
    }
    authority.push('[');
    authority.push_str(address);
    if let Some(zone) = zone {
        authority.push_str("%25");
        authority.push_str(zone);
    }
    authority.push(']');
    Some((scheme, authority))
}

/// The socket a `unix://` store's authority and path name (the factory
/// `Implementations::add` registers for path-based stores):
/// `canonPath(urlPathToPath(splitString(percentDecode(authority), "/")))`.
fn socket_path(authority: &str) -> Result<PathBuf, DaemonError> {
    let decoded = percent_decode(authority).map_err(bad_url)?;
    let segments: Vec<String> = decoded.split('/').map(str::to_owned).collect();
    let path = url_path_to_path(&segments).map_err(bad_url)?;
    Ok(PathBuf::from(canon_abs_path(&path)?))
}

/// A store URI's settings while they are being consumed: each store type
/// takes the ones it defines, and whatever is left over is unknown.
struct Settings<'a> {
    params: BTreeMap<String, String>,
    warn: &'a dyn Fn(&str),
}

impl Settings<'_> {
    fn take(&mut self, name: &str) -> Option<String> {
        self.params.remove(name)
    }

    /// `StoreConfig` and `RemoteStoreConfig`, common to both store types.
    /// Returns the `store` setting.
    fn common(&mut self) -> Result<Option<String>, DaemonError> {
        let store_dir = match self.take("store") {
            Some(v) => Some(parse_store_dir(&v)?),
            None => None,
        };
        for name in ["path-info-cache-size", "priority", "max-connections"] {
            if let Some(v) = self.take(name) {
                parse_int::<i32>(name, &v)?;
            }
        }
        if let Some(v) = self.take("max-connection-age") {
            parse_int::<u32>("max-connection-age", &v)?;
        }
        for name in ["trusted", "want-mass-query"] {
            if let Some(v) = self.take(name) {
                parse_bool(name, &v)?;
            }
        }
        // A whitespace-separated set: any string is valid.
        self.take("system-features");
        Ok(store_dir)
    }

    /// `warnUnknownSettings`: whatever no store setting claimed draws
    /// Nix's warning, in the settings' (sorted) order.
    fn finish(self) {
        for name in self.params.keys() {
            (self.warn)(&format!("unknown setting '{name}'"));
        }
    }

    /// `UDSRemoteStoreConfig`: the store directory, the socket and the
    /// human-readable URI.
    fn unix(
        mut self,
        path: Option<PathBuf>,
    ) -> Result<(Option<String>, PathBuf, String), DaemonError> {
        let store_dir = self.common()?;
        let root = match self.take("root") {
            Some(v) => parse_opt_abs_path("root", &v)?,
            None => None,
        };
        let state = match self.take("state") {
            Some(v) => Some(parse_abs_path("state", &v)?),
            None => None,
        };
        for name in ["log", "real"] {
            if let Some(v) = self.take(name) {
                parse_abs_path(name, &v)?;
            }
        }
        self.finish();
        // `LocalFSStoreConfig::stateDir` defaults to `<root>/nix/var/nix`
        // when there is a root.
        let state_dir = state
            .or_else(|| root.map(|r| format!("{}/nix/var/nix", r.trim_end_matches('/'))))
            .unwrap_or_else(nix_state_dir);
        let default = socket_path_in(&state_dir);
        let path = path.unwrap_or_else(|| default.clone());
        // `UDSRemoteStoreConfig::getReference`: the default socket is
        // shown as `daemon`.
        let display = if path == default {
            "daemon".to_owned()
        } else {
            let path = path.to_str().expect("socket paths are made from strings");
            format!("unix://{}", encode_url_path(&path_to_url_path(path)))
        };
        Ok((store_dir, path, display))
    }

    /// `SSHStoreConfig` for `authority`, then what `SSHMaster`'s
    /// constructor checks.
    fn ssh_ng(mut self, authority: Authority) -> Result<(Option<String>, SshTarget), DaemonError> {
        let store_dir = self.common()?;
        let remote_program = match self.take("remote-program") {
            // `Setting<Strings>`: `tokenizeString`.
            Some(p) => p
                .split([' ', '\t', '\n', '\r'])
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect(),
            None => vec!["nix-daemon".to_owned()],
        };
        let remote_store = self.take("remote-store").filter(|s| !s.is_empty());
        let ssh_key = match self.take("ssh-key") {
            Some(v) => parse_opt_abs_path("ssh-key", &v)?,
            None => None,
        };
        let compress = match self.take("compress") {
            Some(v) => parse_bool("compress", &v)?,
            None => false,
        };
        let public_host_key = self.take("base64-ssh-public-host-key");
        self.finish();

        // `SSHMaster::SSHMaster`: the key is decoded (`parsePublicHostKey`)
        // before the authority is checked.
        let public_host_key = match public_host_key {
            Some(k) => {
                let key = brix_store::hash::base64::decode(&k).ok_or_else(|| {
                    DaemonError::Traced {
                        trace: format!(
                            "while decoding ssh public host key for host '{}'",
                            authority.host
                        ),
                        // Nix formats the offending character with `%c`,
                        // which prints nothing for its coloured argument.
                        error: Box::new(DaemonError::Config(
                            "invalid character in Base64 string: ''".to_owned(),
                        )),
                    }
                })?;
                Some(key).filter(|k| !k.is_empty())
            }
            None => None,
        };
        check_valid_authority(&authority)?;
        Ok((
            store_dir,
            SshTarget {
                user: authority.user,
                host: authority.host,
                port: authority.port,
                remote_program,
                remote_store,
                ssh_key,
                public_host_key,
                compress,
            },
        ))
    }
}

/// The checks `checkValidAuthority` (`ssh.cc`) makes before handing the
/// strings to ssh.
fn check_valid_authority(authority: &Authority) -> Result<(), DaemonError> {
    let invalid = |reason: String| DaemonError::InvalidSshAuthority(authority.to_string(), reason);
    if let Some(u) = &authority.user {
        if u.is_empty() {
            return Err(invalid("user name must not be empty".into()));
        }
        if u.starts_with('-') {
            return Err(invalid(format!("user name '{u}' must not start with '-'")));
        }
    }
    let host = &authority.host;
    if host.is_empty() {
        return Err(invalid("host name must not be empty".into()));
    }
    if host.starts_with('-') {
        return Err(invalid(format!(
            "host name '{host}' must not start with '-'"
        )));
    }
    Ok(())
}

/// `BaseSetting<bool>::parse`.
fn parse_bool(name: &str, value: &str) -> Result<bool, DaemonError> {
    brix_store::setting::parse_bool(name, value).map_err(|e| DaemonError::Usage(e.to_string()))
}

/// `BaseSetting<T>::parse` for integers: `string2IntWithUnitPrefix`, which
/// takes a `K`, `M`, `G` or `T` suffix (powers of 1024).
fn parse_int<T: brix_store::setting::SettingInt>(
    name: &str,
    value: &str,
) -> Result<T, DaemonError> {
    brix_store::setting::parse_int(name, value).map_err(|e| DaemonError::Usage(e.to_string()))
}

/// `parseAbsolutePath`: not empty, absolute, canonicalised.
fn parse_abs_path(name: &str, value: &str) -> Result<String, DaemonError> {
    if value.is_empty() {
        return Err(DaemonError::Usage(format!(
            "setting '{name}' is a path and paths cannot be empty"
        )));
    }
    canon_abs_path(value)
}

/// `BaseSetting<std::optional<AbsolutePath>>::parse`: empty means unset.
fn parse_opt_abs_path(name: &str, value: &str) -> Result<Option<String>, DaemonError> {
    if value.is_empty() {
        return Ok(None);
    }
    parse_abs_path(name, value).map(Some)
}

/// `StoreDirSetting::parse` (`canonStoreDir`).
fn parse_store_dir(value: &str) -> Result<String, DaemonError> {
    if value.is_empty() {
        return Err(DaemonError::Usage(
            "setting 'store' is a path and paths cannot be empty".to_owned(),
        ));
    }
    brix_store::StoreDir::parse_setting(value)
        .map(|dir| dir.as_str().to_owned())
        .map_err(|e| DaemonError::Usage(e.to_string()))
}

/// `canonPath` (without following symlinks), which requires an absolute
/// path.
fn canon_abs_path(path: &str) -> Result<String, DaemonError> {
    if !path.starts_with('/') {
        return Err(DaemonError::Config(format!(
            "not an absolute path: \"{path}\""
        )));
    }
    // A store URI is text, so its canonical form is text too.
    brix_store::CanonPath::new(path)
        .map(|p| {
            p.to_str()
                .expect("invariant: canonicalising text gives text")
                .to_owned()
        })
        .map_err(|e| DaemonError::Config(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// Parses `uri`, returning the warnings too.
    fn parse_with_warnings(uri: &str) -> Result<(StoreUri, Vec<String>), DaemonError> {
        let warnings = RefCell::new(Vec::new());
        let parsed = StoreUri::parse(uri, &|w| warnings.borrow_mut().push(w.to_owned()))?;
        Ok((parsed, warnings.into_inner()))
    }

    fn parse(uri: &str) -> (StoreUri, Vec<String>) {
        parse_with_warnings(uri).unwrap()
    }

    fn ssh(uri: &str) -> SshTarget {
        match parse(uri).0.transport {
            Transport::SshNg(t) => t,
            other => panic!("{other:?}"),
        }
    }

    fn err(uri: &str) -> String {
        parse_with_warnings(uri).unwrap_err().to_string()
    }

    #[test]
    fn daemon_and_unix() {
        let (d, w) = parse("daemon");
        assert_eq!(d.transport, Transport::UnixSocket(default_socket_path()));
        assert_eq!(d.display(), "daemon");
        assert!(w.is_empty());
        for uri in ["unix://", "unix:", "daemon?", "unix://?trusted=1"] {
            let (u, w) = parse(uri);
            assert_eq!(u.transport, Transport::UnixSocket(default_socket_path()));
            assert_eq!(u.display(), "daemon", "{uri}");
            assert!(w.is_empty(), "{uri}: {w:?}");
        }
        let (u, w) = parse("unix:///tmp/my%20sock?trusted=1");
        assert_eq!(
            u.transport,
            Transport::UnixSocket(PathBuf::from("/tmp/my sock"))
        );
        assert_eq!(u.display(), "unix:///tmp/my%20sock");
        assert!(w.is_empty(), "'trusted' is a store setting: {w:?}");
        // From `nix store info --store …`: the path is decoded before it
        // is split and canonicalised.
        for (uri, path, display) in [
            (
                "unix:///tmp/x/../my%20sock",
                "/tmp/my sock",
                "unix:///tmp/my%20sock",
            ),
            ("unix:/tmp/sock", "/tmp/sock", "unix:///tmp/sock"),
            ("unix:///a%2Fb", "/a/b", "unix:///a/b"),
        ] {
            let (u, _) = parse(uri);
            assert_eq!(u.transport, Transport::UnixSocket(PathBuf::from(path)));
            assert_eq!(u.display(), display);
        }
        assert_eq!(err("unix://relative"), "not an absolute path: \"relative\"");
        assert_eq!(
            err("unix://h/tmp/sock"),
            "not an absolute path: \"h/tmp/sock\""
        );
        assert_eq!(
            err("unix:///tmp/a%00b"),
            "URL path component 'a␀b' contains NUL byte which is not allowed"
        );
    }

    #[test]
    fn local_forms_are_rejected() {
        for s in [
            "",
            "auto",
            "local",
            "local?trusted=1",
            "/nix/store",
            "./store",
            "ssh://host",
            "https://cache.nixos.org",
            "dummy://",
            "mounted-ssh-ng://x",
        ] {
            assert!(
                matches!(parse_with_warnings(s), Err(DaemonError::InvalidUri { .. })),
                "{s:?} should be rejected as a store brix cannot open"
            );
        }
        // Nix's messages for what is not a store it knows.
        assert_eq!(err("foo"), "Cannot parse Nix store 'foo'");
        assert_eq!(err("unix"), "Cannot parse Nix store 'unix'");
        assert_eq!(
            err("foo://bar"),
            "don't know how to open Nix store with scheme 'foo'"
        );
    }

    /// What Nix rejects with a `UsageError` (which `nix` follows with its
    /// "Try '… --help'" hint) and what with a plain `Error`, as
    /// `nix eval --store … --expr 1` shows.
    #[test]
    fn usage_errors_are_nix_usage_errors() {
        for uri in [
            "foo",
            "dummy",
            "unix:///tmp/%",
            "daemon?store=relative",
            "unix://?store=",
            "daemon?priority=x",
            "ssh-ng://host?compress=maybe",
            "unix://?state=",
        ] {
            assert!(
                matches!(parse_with_warnings(uri), Err(DaemonError::Usage(_))),
                "{uri}: {:?}",
                parse_with_warnings(uri)
            );
        }
        for uri in ["foo://bar", "unix://?state=relative", "ssh-ng://-x"] {
            let e = parse_with_warnings(uri).unwrap_err();
            assert!(!matches!(e, DaemonError::Usage(_)), "{uri}: {e:?}");
        }
    }

    #[test]
    fn invalid_escapes_are_nix_errors_not_panics() {
        // Slicing the text at byte offsets used to panic inside 'é'.
        assert_eq!(
            err("unix:///tmp/%aé"),
            "Cannot parse Nix store 'unix:///tmp/%aé'"
        );
        assert_eq!(
            err("unix:///tmp/%"),
            "Cannot parse Nix store 'unix:///tmp/%'"
        );
        // A bad query is `decodeQuery`'s `BadURL`.
        assert_eq!(
            err("daemon?state=%zz"),
            "invalid URI query 'state=%zz': leftover"
        );
        assert_eq!(err("daemon?a=%"), "invalid URI query 'a=%': leftover");
        assert_eq!(err("daemon?a^=c"), "invalid URI query 'a^=c': leftover");
        assert_eq!(
            err("ssh-ng://host?a=%zz"),
            "invalid URI query 'a=%zz': leftover"
        );
        let (u, _) = parse("unix:///tmp/%C3%A9");
        assert_eq!(u.transport, Transport::UnixSocket(PathBuf::from("/tmp/é")));
        let (_, w) = parse("daemon?%61=1&a%20b=c");
        assert_eq!(w, ["unknown setting 'a'", "unknown setting 'a b'"]);
        let (_, w) = parse("daemon?a b=c");
        assert_eq!(w, ["unknown setting 'a b'"]);
    }

    #[test]
    fn ssh_ng_forms() {
        let (s, w) = parse("ssh-ng://alice@builder.example:2222?remote-program=sudo%20nix-daemon&remote-store=/mnt&ssh-key=/k&compress=1&other=x");
        assert_eq!(w, ["unknown setting 'other'"]);
        assert_eq!(s.display(), "ssh-ng://alice@builder.example:2222");
        let Transport::SshNg(t) = s.transport else {
            panic!()
        };
        assert_eq!(t.user.as_deref(), Some("alice"));
        assert_eq!(t.host, "builder.example");
        assert_eq!(t.port, Some(2222));
        assert_eq!(t.remote_program, ["sudo", "nix-daemon"]);
        assert_eq!(t.remote_store.as_deref(), Some("/mnt"));
        assert_eq!(t.ssh_key.as_deref(), Some("/k"));
        assert!(t.compress);
        assert_eq!(t.public_host_key, None);

        let (s, _) = parse("ssh-ng://[fe80::1]:22");
        assert_eq!(s.display(), "ssh-ng://[fe80::1]:22");
        let Transport::SshNg(t) = s.transport else {
            panic!()
        };
        assert_eq!(t.host, "fe80::1");
        assert_eq!(t.port, Some(22));

        let t = ssh("ssh-ng://host");
        assert_eq!(t.user, None);
        assert_eq!(t.port, None);
        assert_eq!(t.remote_program, ["nix-daemon"]);
        assert!(!t.compress);
        assert!(ssh("ssh-ng://host?compress=yes").compress);
        assert!(!ssh("ssh-ng://host?compress=no").compress);
        assert_eq!(ssh("ssh-ng://host?ssh-key=").ssh_key, None);
        assert_eq!(
            ssh("ssh-ng://host?ssh-key=/a//b/../k").ssh_key.as_deref(),
            Some("/a/k")
        );
        assert_eq!(ssh("ssh-ng://host:").port, None);
    }

    /// Store URLs and authorities as `nix store info --store …` reports
    /// them ("Store URL: …", the host ssh is given, or the error).
    #[test]
    fn ssh_ng_authorities_match_nix() {
        // Unbracketed IPv6 addresses, the back-compatibility shim.
        let (s, _) = parse("ssh-ng://fe80::1");
        assert_eq!(s.display(), "ssh-ng://[fe80::1]");
        let (s, _) = parse("ssh-ng://u@fe80::1%eth0");
        assert_eq!(s.display(), "ssh-ng://u@[fe80::1%25eth0]");
        let Transport::SshNg(t) = s.transport else {
            panic!()
        };
        assert_eq!(
            (t.user.as_deref(), t.host.as_str()),
            (Some("u"), "fe80::1%eth0")
        );
        let (s, _) = parse("ssh-ng://[fe80::1%eth0]");
        assert_eq!(s.display(), "ssh-ng://[fe80::1%25eth0]");
        // Escapes in the host are decoded for ssh and kept in messages.
        let (s, _) = parse("ssh-ng://ho%20st");
        assert_eq!(s.display(), "ssh-ng://ho%20st");
        assert_eq!(ssh("ssh-ng://ho%20st").host, "ho st");
        assert_eq!(
            err("ssh-ng://host/foo"),
            "invalid URL authority: 'host/foo': leftover"
        );
        assert_eq!(
            err("ssh-ng://host:99999"),
            "Cannot parse Nix store 'ssh-ng://host:99999'"
        );
        assert_eq!(err("ssh-ng://a b"), "Cannot parse Nix store 'ssh-ng://a b'");
    }

    #[test]
    fn ssh_authority_validation() {
        assert_eq!(
            err("ssh-ng://-evil"),
            "invalid SSH authority: '-evil': host name '-evil' must not start with '-'"
        );
        assert!(matches!(
            parse_with_warnings("ssh-ng://-u@host"),
            Err(DaemonError::InvalidSshAuthority(..))
        ));
        assert_eq!(
            err("ssh-ng://@host"),
            "invalid SSH authority: '@host': user name must not be empty"
        );
    }

    #[test]
    fn public_host_key_is_decoded() {
        let t = ssh("ssh-ng://host?base64-ssh-public-host-key=c3NoLWVkMjU1MTkgQUFBQQ%3D%3D");
        assert_eq!(t.public_host_key.as_deref(), Some(&b"ssh-ed25519 AAAA"[..]));
        // Nix's rendering, from `nix store info --store
        // 'ssh-ng://host?base64-ssh-public-host-key=a*b'`.
        let e = parse_with_warnings("ssh-ng://host?base64-ssh-public-host-key=a*b").unwrap_err();
        assert_eq!(
            e.render(false),
            "error:\n       … while decoding ssh public host key for host 'host'\n\n       error: invalid character in Base64 string: ''"
        );
    }

    #[test]
    fn settings_are_validated_like_nix() {
        // Messages from `nix store info --store …`.
        assert_eq!(
            err("ssh-ng://host?compress=maybe"),
            "Boolean setting 'compress' has invalid value 'maybe'"
        );
        assert_eq!(
            err("ssh-ng://host?max-connections=abc"),
            "setting 'max-connections' has invalid value 'abc'"
        );
        assert_eq!(
            err("unix://?state=relative"),
            "not an absolute path: \"relative\""
        );
        assert_eq!(
            err("unix://?state="),
            "setting 'state' is a path and paths cannot be empty"
        );
        assert_eq!(
            err("ssh-ng://host?ssh-key=k"),
            "not an absolute path: \"k\""
        );
        assert_eq!(
            err("daemon?store=relative"),
            "store directory path \"relative\" is not an absolute path"
        );
        assert_eq!(
            err("daemon?priority=-"),
            "setting 'priority' has invalid value '-'"
        );
        assert_eq!(
            err("daemon?max-connection-age=-1"),
            "setting 'max-connection-age' has invalid value '-1'"
        );
        let (u, w) = parse(
            "daemon?priority=3K&path-info-cache-size=-1&max-connections=1&want-mass-query=yes&system-features=kvm%20big&log=/l&real=/r",
        );
        assert!(w.is_empty(), "{w:?}");
        assert_eq!(u.display(), "daemon");
        assert_eq!(parse_int::<i32>("x", "3K").unwrap(), 3072);
        // As `nix store info --store 'daemon?priority=+5'` (5) and
        // `priority=4G` (0): a leading `+` is taken, the product wraps.
        assert_eq!(parse_int::<i32>("x", "4G").unwrap(), 0);
        assert_eq!(parse_int::<i32>("x", "+3").unwrap(), 3);
        assert!(parse_int::<i32>("x", "3000000000").is_err());
        assert!(parse_int::<i32>("x", "3X").is_err());
    }

    #[test]
    fn unknown_settings_warn_in_order_and_are_ignored() {
        // `nix store info --store 'daemon?foo=bar&bar=1'` warns about 'bar'
        // then 'foo'; a setting of the other store type is unknown too.
        let (d, w) = parse("daemon?foo=bar&bar=1&remote-program=x");
        assert_eq!(
            w,
            [
                "unknown setting 'bar'",
                "unknown setting 'foo'",
                "unknown setting 'remote-program'"
            ]
        );
        assert_eq!(d.transport, Transport::UnixSocket(default_socket_path()));
        let (_, w) = parse("ssh-ng://host?state=/x&root=/r");
        assert_eq!(w, ["unknown setting 'root'", "unknown setting 'state'"]);
    }

    /// `decodeQuery`: a parameter without `=` is ignored with Nix's
    /// warning, an empty one included (`nix store info --store
    /// 'daemon?foo&&bar=1'`), before the settings are looked at.
    #[test]
    fn parameters_without_equal_sign_warn() {
        let dubious =
            |p: &str| format!("dubious URI query '{p}' is missing equal sign '=', ignoring");
        let (_, w) = parse("daemon?foo");
        assert_eq!(w, [dubious("foo")]);
        let (_, w) = parse("daemon?&&");
        assert_eq!(w, [dubious(""), dubious(""), dubious("")]);
        let (_, w) = parse("daemon?foo&&bar=1");
        assert_eq!(
            w,
            [
                dubious("foo"),
                dubious(""),
                "unknown setting 'bar'".to_owned()
            ]
        );
        let warnings = RefCell::new(Vec::new());
        let e = StoreUri::parse("ssh-ng://host?a&compress=x", &|w| {
            warnings.borrow_mut().push(w.to_owned())
        })
        .unwrap_err();
        assert_eq!(
            e.to_string(),
            "Boolean setting 'compress' has invalid value 'x'"
        );
        assert_eq!(warnings.into_inner(), [dubious("a")]);
    }

    #[test]
    fn repeated_settings_keep_the_first_value() {
        assert!(ssh("ssh-ng://host?compress=true&compress=false").compress);
        assert!(ssh("ssh-ng://host?compress=1&compress=x").compress);
    }

    #[test]
    fn state_and_root_choose_the_default_socket() {
        // `$NIX_DAEMON_SOCKET_PATH`, when set, wins over both.
        let socket = |state_dir: &str| match std::env::var_os("NIX_DAEMON_SOCKET_PATH") {
            Some(p) if !p.is_empty() => Transport::UnixSocket(PathBuf::from(p)),
            _ => Transport::UnixSocket(PathBuf::from(format!("{state_dir}/daemon-socket/socket"))),
        };
        // `nix store info --store 'unix://?state=/tmp/nostate'` connects to
        // /tmp/nostate/daemon-socket/socket and calls the store `daemon`.
        let (u, _) = parse("unix://?state=/tmp/nostate");
        assert_eq!(u.transport, socket("/tmp/nostate"));
        assert_eq!(u.display(), "daemon");
        let (u, _) = parse("daemon?root=/tmp/noroot");
        assert_eq!(u.transport, socket("/tmp/noroot/nix/var/nix"));
        let (u, _) = parse("daemon?root=/tmp/noroot&state=/s");
        assert_eq!(u.transport, socket("/s"));
        let (u, _) = parse("daemon?store=/nix//store/");
        assert_eq!(u.store_dir.as_deref(), Some("/nix/store"));
        let (u, _) = parse("daemon?store=%2Fnix%2Fstore");
        assert_eq!(u.store_dir.as_deref(), Some("/nix/store"));
    }
}
