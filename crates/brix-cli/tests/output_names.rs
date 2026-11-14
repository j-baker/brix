//! Building a floating content-addressed output whose name is not UTF-8
//! (`a` and the byte 0xc3), through a Nix daemon, as Nix does it.
//!
//! Such an output is never built: its store path would be named
//! `<drv name>-a\xc3`, which no store path can be. Asking for it by name
//! (`drv!a\xc3` on the wire) makes the daemon refuse the derived path;
//! building every output (`drv^*`) builds, then fails where the daemon
//! registers the output. Either way the daemon's message quotes the
//! name's bytes, and no out-link is made for it (see `create_out_links`).
//!
//! The system daemon need not have `ca-derivations`, so the test runs a
//! daemon of its own with the reference `nix`, on a store in a scratch
//! directory, and points both `nix` and brix at its socket. It builds,
//! so it is gated like the tests that build through the real daemon:
//! `BRIX_TEST_REAL_DAEMON=1` (and `BRIX_NIX_BIN`).

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use assert_cmd::prelude::*;
use brix_testutil::{nix_bin, require_env, strip_noise, ORACLE_NIX_CONFIG, REAL_DAEMON};

/// The settings of the daemon and of both clients: the oracle's, with
/// floating content-addressed derivations and no substituters.
fn nix_config() -> String {
    format!("{ORACLE_NIX_CONFIG}\nextra-experimental-features = ca-derivations\nsubstituters =\nbuild-users-group =")
}

/// `nix daemon` on a scratch store, listening on a socket of its own;
/// killed when dropped.
struct PrivateDaemon {
    child: Child,
    socket: PathBuf,
    // Removed last: the store and the socket live here.
    _dir: tempfile::TempDir,
}

impl PrivateDaemon {
    fn start() -> PrivateDaemon {
        // A short directory: a unix socket's path is limited in length.
        let dir = tempfile::Builder::new()
            .prefix("brix-daemon-")
            .tempdir_in("/tmp")
            .expect("create the daemon's directory");
        let socket = dir.path().join("socket");
        // `--force-trusted` (gated on the daemon-trust-override feature,
        // for the daemon alone): the daemon must trust its clients
        // wherever the test runs, as a developer's daemon trusts its
        // user through `trusted-users`; untrusted, `nix` would warn
        // about every restricted client setting and the outputs would
        // not compare.
        let child = Command::new(nix_bin())
            .args(["daemon", "--force-trusted", "--store"])
            .arg(dir.path().join("store"))
            .env("NIX_DAEMON_SOCKET_PATH", &socket)
            .env(
                "NIX_CONFIG",
                format!(
                    "{}\nextra-experimental-features = daemon-trust-override",
                    nix_config()
                ),
            )
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("start nix daemon");
        let deadline = Instant::now() + Duration::from_secs(30);
        while !socket.exists() {
            assert!(
                Instant::now() < deadline,
                "nix daemon did not create {}",
                socket.display()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        PrivateDaemon {
            child,
            socket,
            _dir: dir,
        }
    }

    fn store_uri(&self) -> String {
        format!("unix://{}", self.socket.display())
    }
}

impl Drop for PrivateDaemon {
    fn drop(&mut self) {
        // The daemon may have exited already; there is nothing to report.
        if self.child.kill().is_ok() {
            self.child.wait().expect("wait for nix daemon");
        }
    }
}

/// `program build --expr <expr> <installable>` against `store`, in `dir`,
/// where an out-link would go.
fn build(mut cmd: Command, dir: &Path, store: &str, expr: &str, installable: &str) -> Output {
    cmd.current_dir(dir)
        .args(["--store", store])
        .args(["--extra-experimental-features", "ca-derivations"])
        .args(["build", "--expr", expr, installable])
        .output()
        .expect("run build")
}

/// Bytes as text with every byte that is not printable ASCII escaped.
fn escaped(bytes: &[u8]) -> String {
    bytes.escape_ascii().to_string()
}

#[test]
#[ignore = "builds through a Nix daemon; set BRIX_TEST_REAL_DAEMON=1"]
fn outputs_whose_names_are_not_utf8_fail_to_build_like_nix() {
    require_env(REAL_DAEMON);
    let daemon = PrivateDaemon::start();
    let store = daemon.store_uri();
    let drv = r#"let l = builtins.substring 0 1 "é"; o = "a" + l; d = derivation { name = "brix-ob"; system = "SYSTEM"; builder = "/bin/sh"; outputs = [ "out" o ]; args = [ "-c" "echo a > $out; echo b > ${builtins.placeholder o}" ]; __contentAddressed = true; }; in "#
        .replace("SYSTEM", &brix::host_system());
    let dir = tempfile::tempdir().expect("create a working directory");
    // The output by name: the daemon refuses the derived path. Every
    // output: it builds, then the output's path is refused.
    for (expr, installable) in [(format!("{drv}d.${{o}}"), "."), (format!("{drv}d"), "^*")] {
        let mut nix = Command::new(nix_bin());
        nix.env("NIX_CONFIG", nix_config());
        let theirs = build(nix, dir.path(), &store, &expr, installable);
        let ours = build(
            Command::cargo_bin("brix").expect("brix binary"),
            dir.path(),
            &store,
            &expr,
            installable,
        );
        assert!(!theirs.status.success(), "{expr} {installable}");
        assert_eq!(
            (
                ours.status.code(),
                escaped(&strip_noise(&ours.stderr)),
                escaped(&ours.stdout)
            ),
            (
                theirs.status.code(),
                escaped(&strip_noise(&theirs.stderr)),
                escaped(&theirs.stdout)
            ),
            "{expr} {installable}"
        );
        // No out-link for an output that was never built.
        assert!(std::fs::read_dir(dir.path()).unwrap().next().is_none());
    }
}
