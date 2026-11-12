//! Whether this machine has an Internet connection, as `nix` decides it
//! before running a command (`haveInternet` in `src/nix/main.cc`): when
//! it has none, `nix` warns and runs as with `--offline`.
//!
//! The decision is Nix's, quirks included: any address of an interface
//! counts except IPv4 `127.0.0.1` (exactly that one, not all of
//! `127.0.0.0/8`) and IPv6 loopback and link-local addresses, whether or
//! not the interface is up; a machine whose interfaces cannot be listed
//! has a connection; and so does one with a proxy variable set
//! (`haveNetworkProxyConnection`), even to an empty value. Of the proxy
//! variables only the lower-case ones count: `getAllVariables` in
//! `src/libcmd/network-proxy.cc` means to add the upper-case names too,
//! but `std::transform` writes into an empty string, so it adds `""`,
//! which `getenv` never finds.
//!
//! `BRIX_HAVE_INTERNET` overrides the check: `1` says there is a
//! connection, `0` that there is none. Tests use it, since they cannot
//! take a machine's network away (the package's test run, in a build
//! sandbox without a network, sets it to `1` so that brix's output there
//! is what it is on a connected machine).

use std::ffi::OsStr;
use std::net::{IpAddr, Ipv4Addr};

/// The warning `nix` gives when it finds no connection.
pub const NO_INTERNET_WARNING: &str =
    "you don't have Internet access; disabling some network-dependent features";

/// The variable that overrides the check.
pub const OVERRIDE_VAR: &str = "BRIX_HAVE_INTERNET";

/// The proxy variables that count as a connection: Nix's
/// `excludingNoProxyVariables`, which by the bug described in the module
/// documentation holds only the lower-case names.
const PROXY_VARIABLES: [&str; 4] = ["all_proxy", "ftp_proxy", "http_proxy", "https_proxy"];

/// `haveInternet()`, unless [`OVERRIDE_VAR`] decides: an error names a
/// value of it that is neither `1` nor `0`.
pub fn have_internet() -> Result<bool, String> {
    if let Some(forced) = forced(std::env::var_os(OVERRIDE_VAR).as_deref())? {
        return Ok(forced);
    }
    // `if (getifaddrs(&addrs)) return true;`: see [`decide`].
    let addresses = interface_addresses().ok();
    Ok(decide(addresses.as_deref(), |name| {
        std::env::var_os(name).is_some()
    }))
}

/// What [`OVERRIDE_VAR`]'s value says: `None` when it is unset, so that
/// the machine is asked.
fn forced(value: Option<&OsStr>) -> Result<Option<bool>, String> {
    match value {
        None => Ok(None),
        Some(v) if v == "1" => Ok(Some(true)),
        Some(v) if v == "0" => Ok(Some(false)),
        Some(v) => Err(format!(
            "{OVERRIDE_VAR} must be 1 or 0, not '{}'",
            v.to_string_lossy()
        )),
    }
}

/// `haveInternet()` given the addresses of the machine's interfaces
/// (`None` when they could not be listed, which Nix takes as a
/// connection) and whether an environment variable is set.
fn decide(addresses: Option<&[IpAddr]>, is_set: impl Fn(&str) -> bool) -> bool {
    let Some(addresses) = addresses else {
        return true;
    };
    addresses.iter().any(reaches_beyond_the_machine) || PROXY_VARIABLES.into_iter().any(is_set)
}

/// Whether an interface address counts as a connection in
/// `haveInternet()`: an IPv4 address other than `INADDR_LOOPBACK`, or an
/// IPv6 address that is neither `IN6_IS_ADDR_LOOPBACK` (`::1`) nor
/// `IN6_IS_ADDR_LINKLOCAL` (`fe80::/10`).
fn reaches_beyond_the_machine(address: &IpAddr) -> bool {
    match address {
        IpAddr::V4(v4) => *v4 != Ipv4Addr::LOCALHOST,
        IpAddr::V6(v6) => !v6.is_loopback() && !v6.is_unicast_link_local(),
    }
}

/// The IPv4 and IPv6 addresses of the machine's network interfaces, as
/// `getifaddrs` lists them (entries without an address, and addresses of
/// other families, left out as Nix leaves them out).
fn interface_addresses() -> std::io::Result<Vec<IpAddr>> {
    let interfaces = nix::ifaddrs::getifaddrs().map_err(std::io::Error::from)?;
    Ok(interfaces
        .filter_map(|interface| {
            let address = interface.address?;
            if let Some(v4) = address.as_sockaddr_in() {
                Some(IpAddr::V4(v4.ip()))
            } else {
                address.as_sockaddr_in6().map(|v6| IpAddr::V6(v6.ip()))
            }
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    fn unset(_: &str) -> bool {
        false
    }

    #[test]
    fn loopback_and_link_local_are_no_connection() {
        let local = [ip("127.0.0.1"), ip("::1"), ip("fe80::1"), ip("febf::1")];
        assert!(!decide(Some(&local[..]), unset));
        let none: [IpAddr; 0] = [];
        assert!(!decide(Some(&none[..]), unset), "no interface at all");
    }

    /// Nix compares with `INADDR_LOOPBACK` itself, and the IPv6 tests
    /// look at the IPv6 address alone.
    #[test]
    fn any_other_address_is_a_connection() {
        for other in [
            "127.0.0.2",
            "10.0.0.1",
            "169.254.1.1",
            "192.168.1.2",
            "::ffff:127.0.0.1",
            "fec0::1",
            "fc00::1",
            "2001:db8::1",
        ] {
            assert!(
                decide(Some(&[ip("127.0.0.1"), ip(other)][..]), unset),
                "{other}"
            );
        }
    }

    /// `getifaddrs` failing is taken as a connection.
    #[test]
    fn unlisted_interfaces_are_a_connection() {
        assert!(decide(None, unset));
    }

    /// A lower-case proxy variable is a connection, even when empty (the
    /// closure sees only whether it is set); the upper-case ones and
    /// `no_proxy` are not.
    #[test]
    fn only_lower_case_proxy_variables_count() {
        let local = [ip("127.0.0.1")];
        for name in ["all_proxy", "ftp_proxy", "http_proxy", "https_proxy"] {
            assert!(decide(Some(&local[..]), |v| v == name), "{name}");
        }
        for name in [
            "ALL_PROXY",
            "FTP_PROXY",
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "no_proxy",
            "NO_PROXY",
        ] {
            assert!(!decide(Some(&local[..]), |v| v == name), "{name}");
        }
    }

    #[test]
    fn the_override_is_one_or_zero() {
        assert_eq!(forced(None), Ok(None));
        assert_eq!(forced(Some(OsStr::new("1"))), Ok(Some(true)));
        assert_eq!(forced(Some(OsStr::new("0"))), Ok(Some(false)));
        assert_eq!(
            forced(Some(OsStr::new("yes"))),
            Err("BRIX_HAVE_INTERNET must be 1 or 0, not 'yes'".to_owned())
        );
        assert_eq!(
            forced(Some(OsStr::new(""))),
            Err("BRIX_HAVE_INTERNET must be 1 or 0, not ''".to_owned())
        );
    }

    /// The machine's own interfaces can be listed (what they are depends
    /// on the machine: a build sandbox has only `lo`).
    #[test]
    fn interfaces_can_be_listed() {
        interface_addresses().expect("getifaddrs");
    }
}
