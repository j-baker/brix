//! The reference Nix as the oracle of differential tests.

use std::ffi::{OsStr, OsString};
use std::io::Write;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};

use crate::env::{nix_bin, nix_tool};

/// The oracle's configuration, pinned rather than inherited from the
/// machine's `nix.conf`: the Determinate installer sets `lazy-trees =
/// true` and `eval-cores = 0`, and both change what Nix prints (path
/// display, `flake metadata`'s `Path:` line, JSON error traces), so they
/// are fixed to the values brix reproduces wherever the tests run. The
/// evaluation cache is off because parallel tests evaluate flakes with
/// the same fingerprint (copies of one fixture) and would race on its
/// rows.
pub const ORACLE_NIX_CONFIG: &str = "experimental-features = nix-command flakes\neval-cache = false\nlazy-trees = true\neval-cores = 0";

/// A run of the reference `nix`, or of one of the tools next to it, built
/// up like a [`Command`]: [`ORACLE_NIX_CONFIG`] as `NIX_CONFIG` (which
/// [`Oracle::config`] extends), stdin empty unless given, and stderr
/// without the lines [`is_noise`] recognises.
#[derive(Clone, Debug)]
pub struct Oracle {
    program: PathBuf,
    arg0: Option<OsString>,
    config: String,
    store: Option<String>,
    dir: Option<PathBuf>,
    args: Vec<OsString>,
    envs: Vec<(OsString, Option<OsString>)>,
    stdin: Option<Vec<u8>>,
}

impl Oracle {
    /// The reference `nix` ([`nix_bin`]).
    #[allow(clippy::new_without_default)]
    pub fn new() -> Oracle {
        Oracle::program(nix_bin())
    }

    /// The reference Nix's `name` (`nix-store`, `nix-daemon`, …), run
    /// under that name, which usage errors quote.
    pub fn tool(name: &str) -> Oracle {
        let mut oracle = Oracle::program(nix_tool(name));
        oracle.arg0 = Some(name.into());
        oracle
    }

    /// The reference `nix-instantiate`.
    pub fn nix_instantiate() -> Oracle {
        Oracle::tool("nix-instantiate")
    }

    fn program(program: PathBuf) -> Oracle {
        Oracle {
            program,
            arg0: None,
            config: ORACLE_NIX_CONFIG.to_owned(),
            store: None,
            dir: None,
            args: Vec::new(),
            envs: Vec::new(),
            stdin: None,
        }
    }

    /// Appends `lines` to the configuration: a setting given again
    /// overrides the pinned one, `extra-…` adds to it.
    pub fn config(&mut self, lines: &str) -> &mut Oracle {
        self.config.push('\n');
        self.config.push_str(lines);
        self
    }

    /// The store to use, as `NIX_REMOTE`: a URL (`dummy://`, a chroot
    /// store's directory, `local?root=…`).
    pub fn store(&mut self, url: &str) -> &mut Oracle {
        self.store = Some(url.to_owned());
        self
    }

    /// The `dummy://` store, which has no valid paths and cannot be
    /// written: for evaluations that must not need a daemon.
    pub fn dummy_store(&mut self) -> &mut Oracle {
        self.store("dummy://")
    }

    /// The working directory.
    pub fn dir(&mut self, dir: impl AsRef<Path>) -> &mut Oracle {
        self.dir = Some(dir.as_ref().to_owned());
        self
    }

    pub fn arg(&mut self, arg: impl AsRef<OsStr>) -> &mut Oracle {
        self.args.push(arg.as_ref().to_owned());
        self
    }

    pub fn args<I, S>(&mut self, args: I) -> &mut Oracle
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.args
            .extend(args.into_iter().map(|a| a.as_ref().to_owned()));
        self
    }

    /// Sets an environment variable; applied after `NIX_CONFIG` and
    /// `NIX_REMOTE`, so it can replace those too.
    pub fn env(&mut self, key: impl AsRef<OsStr>, value: impl AsRef<OsStr>) -> &mut Oracle {
        self.envs
            .push((key.as_ref().to_owned(), Some(value.as_ref().to_owned())));
        self
    }

    /// Removes an environment variable, `NIX_CONFIG` included.
    pub fn env_remove(&mut self, key: impl AsRef<OsStr>) -> &mut Oracle {
        self.envs.push((key.as_ref().to_owned(), None));
        self
    }

    /// What the program reads on stdin.
    pub fn stdin(&mut self, input: impl Into<Vec<u8>>) -> &mut Oracle {
        self.stdin = Some(input.into());
        self
    }

    /// The program name the process sees as `argv[0]`.
    pub fn arg0(&mut self, name: impl AsRef<OsStr>) -> &mut Oracle {
        self.arg0 = Some(name.as_ref().to_owned());
        self
    }

    /// The command line, for messages.
    pub fn describe(&self) -> String {
        let mut text = self.program.display().to_string();
        for arg in &self.args {
            text.push(' ');
            text.push_str(&arg.to_string_lossy());
        }
        text
    }

    /// The process to run, for a caller that drives it itself (its
    /// stderr is not cleaned, and stdin is not written).
    pub fn command(&self) -> Command {
        let mut cmd = Command::new(&self.program);
        if let Some(arg0) = &self.arg0 {
            cmd.arg0(arg0);
        }
        cmd.args(&self.args).env("NIX_CONFIG", &self.config);
        if let Some(store) = &self.store {
            cmd.env("NIX_REMOTE", store);
        }
        if let Some(dir) = &self.dir {
            cmd.current_dir(dir);
        }
        for (key, value) in &self.envs {
            match value {
                Some(value) => cmd.env(key, value),
                None => cmd.env_remove(key),
            };
        }
        cmd
    }

    /// Runs the program to completion: its status, stdout, and stderr
    /// without noise.
    pub fn output(&self) -> OracleOutput {
        let mut cmd = self.command();
        cmd.stdin(if self.stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
        let mut child = cmd
            .spawn()
            .unwrap_or_else(|e| panic!("cannot run {}: {e}", self.describe()));
        if let Some(input) = &self.stdin {
            let mut pipe = child.stdin.take().expect("stdin is piped");
            match pipe.write_all(input) {
                Ok(()) => {}
                // It exited without reading everything; its output says why.
                Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => {}
                Err(e) => panic!("writing the stdin of {}: {e}", self.describe()),
            }
        }
        let out = child
            .wait_with_output()
            .unwrap_or_else(|e| panic!("waiting for {}: {e}", self.describe()));
        OracleOutput {
            status: out.status,
            stdout: out.stdout,
            stderr: strip_noise(&out.stderr),
        }
    }

    /// Runs the program, which must succeed; its stdout as text.
    pub fn stdout(&self) -> String {
        let out = self.output();
        assert!(
            out.success(),
            "{} failed ({}):\n{}",
            self.describe(),
            out.status,
            out.stderr_lossy()
        );
        String::from_utf8(out.stdout)
            .unwrap_or_else(|e| panic!("{} printed non-UTF-8 output: {e}", self.describe()))
    }
}

/// Writes the store objects `expr` makes into the real store: `nix eval`
/// with lazy trees off, so `builtins.toFile` texts and the derivations
/// the expression instantiates are written eagerly. The [`REAL_STORE`]
/// and [`REAL_DAEMON`] comparisons read such objects back from the
/// store, which only agrees once the store knows them: a developer's
/// store has had them for ages, a fresh one (a new machine, the flake
/// check's own daemon) has not — `nix-instantiate --eval` cannot write
/// a derivation it names as `drvPath` and then hashes dependents of it
/// differently, and a lazy `toFile` is never materialised for reading
/// back as a path. A failure here is the compared run's to report, so
/// the outcome is dropped.
///
/// [`REAL_STORE`]: crate::REAL_STORE
/// [`REAL_DAEMON`]: crate::REAL_DAEMON
pub fn warm_real_store(expr: &str) {
    Oracle::new()
        .config("lazy-trees = false")
        .args(["eval", "--expr", expr])
        .output();
}

/// What an [`Oracle`] run gave.
#[derive(Clone, Debug)]
pub struct OracleOutput {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    /// stderr without noise (see [`strip_noise`]).
    pub stderr: Vec<u8>,
}

impl OracleOutput {
    pub fn success(&self) -> bool {
        self.status.success()
    }

    /// The exit code; -1 when a signal ended the process.
    pub fn code(&self) -> i32 {
        self.status.code().unwrap_or(-1)
    }

    pub fn stdout_lossy(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }

    pub fn stderr_lossy(&self) -> String {
        String::from_utf8_lossy(&self.stderr).into_owned()
    }
}

/// `stderr` without the lines [`is_noise`] recognises; every other byte
/// is kept as it is.
pub fn strip_noise(stderr: &[u8]) -> Vec<u8> {
    stderr
        .split_inclusive(|&b| b == b'\n')
        .filter(|line| {
            let line = line.strip_suffix(b"\n").unwrap_or(line);
            !is_noise(line.strip_suffix(b"\r").unwrap_or(line))
        })
        .flatten()
        .copied()
        .collect()
}

/// Whether a line on the oracle's stderr (without its newline) comes
/// from the environment it runs in rather than from what it was asked to
/// do, so that no brix output is compared with it:
///
/// - Determinate Nix's crash reporter, which writes from a helper process
///   rather than through Nix's logger: Crashpad's
///   `[pid:tid:date,time:LEVEL file.cc:line] …` (failed uploads of
///   earlier reports, `HTTP status 400`; `ReadExactly: expected 8,
///   observed 0`; a report lock another run removed under `…/sentry/…`),
///   and Sentry's own `[sentry] …`;
/// - Nix's remarks about the machine: no network (`warning: you don't
///   have Internet access; …`), no `/nix/var`, so a chroot store is used
///   (`… does not exist, so Nix will use … as a chroot store`), a thread
///   limit such as the build sandbox's (`warning: could only create …`),
///   and another process holding a lock (`waiting for another Nix
///   process …`).
pub fn is_noise(line: &[u8]) -> bool {
    is_crash_reporter_line(line)
        || line.starts_with(b"[sentry] ")
        || line.starts_with(b"warning: you don't have Internet access")
        || contains(line, b"does not exist, so Nix will use")
        || line.starts_with(b"warning: could only create ")
        || line.starts_with(b"waiting for another Nix process")
}

/// Crashpad's log line: `[pid:tid:date,time:LEVEL file.cc:line] …`, or
/// any bracketed line naming Sentry's directory.
fn is_crash_reporter_line(line: &[u8]) -> bool {
    let Some(rest) = line.strip_prefix(b"[") else {
        return false;
    };
    let Some(end) = rest.iter().position(|&b| b == b']') else {
        return false;
    };
    if contains(line, b"/sentry/") {
        return true;
    }
    let fields: Vec<&[u8]> = rest[..end].splitn(4, |&b| b == b':').collect();
    fields.len() == 4
        && fields[..2]
            .iter()
            .all(|f| !f.is_empty() && f.iter().all(u8::is_ascii_digit))
        && [&b"ERROR "[..], b"WARNING ", b"INFO ", b"FATAL "]
            .iter()
            .any(|level| fields[3].starts_with(level))
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crash_reporter_lines_are_noise() {
        for line in [
            "[2119886:2119886:20261109,002647.728733:ERROR file_io.cc:107] ReadExactly: expected 8, observed 0",
            "[2602936:2602937:20261108,222804.828788:ERROR http_transport.cc:48] HTTP status 400",
            "[1487078:1487078:20261108,214916.753096:ERROR file_io_posix.cc:145] open /home/u/.cache/nix/sentry/630b0ce3.run/__sentry-attachments: No such file or directory (2)",
            "[1:2:3:WARNING filesystem_posix.cc:62] sentry",
            "[sentry] DEBUG crash-safe telemetry flush",
        ] {
            assert!(is_noise(line.as_bytes()), "{line}");
        }
    }

    #[test]
    fn environment_remarks_are_noise() {
        for line in [
            "warning: you don't have Internet access; disabling some network-dependent features",
            "warning: '/nix/var/nix' does not exist, so Nix will use '/home/u/.local/share/nix/root' as a chroot store",
            "warning: could only create 2 threads",
            "waiting for another Nix process to finish",
        ] {
            assert!(is_noise(line.as_bytes()), "{line}");
        }
    }

    #[test]
    fn what_nix_says_is_kept() {
        for line in [
            "error: undefined variable 'x'",
            "[1 built, 0.0 MiB DL]",
            "[a:b:c:ERROR x] not digits",
            "       … while evaluating the attribute 'a'",
            "trace: [1:2:3:INFO] inside a trace",
        ] {
            assert!(!is_noise(line.as_bytes()), "{line}");
        }
    }

    #[test]
    fn stripping_keeps_every_other_byte() {
        let stderr =
            b"warning: one\n[1:2:3:ERROR file_io.cc:107] ReadExactly\r\nerror: \xff two\nlast";
        assert_eq!(strip_noise(stderr), b"warning: one\nerror: \xff two\nlast");
        assert_eq!(strip_noise(b"[sentry] DEBUG x"), b"");
    }
}
