//! Opening the byte stream the protocol runs over: a unix socket, or the
//! pipes of `ssh` (or, for `ssh-ng://localhost`, of the remote program
//! itself).

use std::ffi::OsString;
use std::io::{self, BufRead, BufReader, BufWriter, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

use crate::error::DaemonError;
use crate::uri::{SshTarget, Transport};

/// How long a local remote program (`ssh-ng://localhost`) may take to exit
/// after its stdin closes before it is killed (`SSHMaster::startCommand`
/// gives the fake-ssh child this timeout instead of an immediate SIGKILL).
const LOCAL_EXIT_GRACE: Duration = Duration::from_secs(10);

/// The reading end of a connection.
pub(crate) enum ReadHalf {
    Socket(UnixStream),
    Pipe(ChildStdout),
    /// Released when the connection is torn down.
    Closed,
}

/// The writing end of a connection.
pub(crate) enum WriteHalf {
    Socket(UnixStream),
    Pipe(ChildStdin),
    /// Half-closed ([`Connection::close_write`]) or torn down.
    Closed,
}

impl Read for ReadHalf {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            ReadHalf::Socket(s) => s.read(buf),
            ReadHalf::Pipe(p) => p.read(buf),
            ReadHalf::Closed => Ok(0),
        }
    }
}

impl Write for WriteHalf {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            WriteHalf::Socket(s) => s.write(buf),
            WriteHalf::Pipe(p) => p.write(buf),
            WriteHalf::Closed => Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "the connection to the Nix daemon is closed for writing",
            )),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            WriteHalf::Socket(s) => s.flush(),
            WriteHalf::Pipe(p) => p.flush(),
            WriteHalf::Closed => Ok(()),
        }
    }
}

/// The process at the other end of a pipe connection, stopped when the
/// connection is dropped (Nix's `Pid` destructor).
struct Remote {
    child: Child,
    /// `None`: `ssh`, killed outright (SIGKILL), since killing the client
    /// does not affect the remote side. `Some`: the remote program itself,
    /// given this long to exit on its own after its stdin closes (e.g. to
    /// finish writing), then killed.
    grace: Option<Duration>,
    /// The `UserKnownHostsFile` directory, removed after ssh is gone.
    known_hosts: Option<tempfile::TempDir>,
}

impl Remote {
    fn stop(mut self) -> io::Result<()> {
        if let Some(grace) = self.grace {
            let deadline = Instant::now() + grace;
            while Instant::now() < deadline {
                if self.child.try_wait()?.is_some() {
                    return Ok(());
                }
                std::thread::sleep(Duration::from_millis(25));
            }
        }
        self.child.kill()?;
        self.child.wait().map(drop)
    }
}

/// A connected, buffered duplex stream, plus the process that carries it.
pub(crate) struct Connection {
    pub reader: BufReader<ReadHalf>,
    pub writer: BufWriter<WriteHalf>,
    remote: Option<Remote>,
}

impl Connection {
    pub fn open(transport: &Transport) -> Result<Self, DaemonError> {
        match transport {
            Transport::UnixSocket(path) => Self::unix(path),
            Transport::SshNg(target) => {
                let opts = if is_fake_ssh(target) {
                    Vec::new()
                } else {
                    nix_ssh_opts()?
                };
                Self::ssh(target, &opts, |cmd| cmd)
            }
        }
    }

    /// `UDSRemoteStore::openConnection`.
    fn unix(path: &Path) -> Result<Self, DaemonError> {
        let stream = UnixStream::connect(path).map_err(|source| DaemonError::Connect {
            path: path.to_owned(),
            source,
        })?;
        Self::from_unix_stream(stream)
    }

    /// Wrap an already connected socket (also used by the in-process fake
    /// daemon tests).
    pub(crate) fn from_unix_stream(stream: UnixStream) -> Result<Self, DaemonError> {
        let reader = stream.try_clone()?;
        Ok(Connection {
            reader: BufReader::with_capacity(64 * 1024, ReadHalf::Socket(reader)),
            writer: BufWriter::with_capacity(64 * 1024, WriteHalf::Socket(stream)),
            remote: None,
        })
    }

    /// `SSHStore::openConnection` and `SSHMaster::startCommand`, without
    /// the connection-sharing master (brix opens one connection, as Nix
    /// does with `max-connections = 1`). `run` sees the command before it
    /// is started (the tests substitute a stand-in for ssh).
    fn ssh(
        target: &SshTarget,
        ssh_opts: &[OsString],
        run: impl FnOnce(Command) -> Command,
    ) -> Result<Self, DaemonError> {
        if is_fake_ssh(target) {
            return Self::spawn(run(ssh_command(target, &[], None)?), None, None);
        }
        // `SSHMaster::SSHMaster`: a pinned host key goes into a known-hosts
        // file of its own, `<host> <key>`.
        let known_hosts = match &target.public_host_key {
            Some(key) => {
                let dir = private_temp_dir()?;
                let mut line = target.host.as_bytes().to_vec();
                line.push(b' ');
                line.extend_from_slice(key);
                line.push(b'\n');
                std::fs::write(dir.path().join("host-key"), line)?;
                Some(dir)
            }
            None => None,
        };
        let file = known_hosts.as_ref().map(|d| d.path().join("host-key"));
        let cmd = run(ssh_command(target, ssh_opts, file.as_deref())?);
        Self::spawn(cmd, Some(&target.host), known_hosts)
    }

    /// Start `cmd` with piped stdin and stdout. For real ssh (`ssh_host`),
    /// wait for the `started` line its `LocalCommand` prints once the
    /// session is up; anything else (EOF included) means ssh failed.
    fn spawn(
        mut cmd: Command,
        ssh_host: Option<&str>,
        known_hosts: Option<tempfile::TempDir>,
    ) -> Result<Self, DaemonError> {
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        let mut child = cmd.spawn().map_err(|source| DaemonError::Spawn {
            program: cmd.get_program().to_string_lossy().into_owned(),
            source,
        })?;
        let stdin = child.stdin.take().expect("stdin is piped");
        let stdout = child.stdout.take().expect("stdout is piped");
        let mut conn = Connection {
            reader: BufReader::with_capacity(64 * 1024, ReadHalf::Pipe(stdout)),
            writer: BufWriter::with_capacity(64 * 1024, WriteHalf::Pipe(stdin)),
            remote: Some(Remote {
                child,
                grace: match ssh_host {
                    Some(_) => None,
                    None => Some(LOCAL_EXIT_GRACE),
                },
                known_hosts,
            }),
        };
        if let Some(host) = ssh_host {
            let mut line = Vec::new();
            conn.reader.read_until(b'\n', &mut line)?;
            if line.strip_suffix(b"\n") != Some(b"started") {
                // Dropping the connection kills ssh.
                return Err(DaemonError::SshStartFailed(host.to_owned()));
            }
        }
        Ok(conn)
    }

    /// Half-close for writing (`closeWrite`): tells the other side we are
    /// done without dropping the read side.
    pub fn close_write(&mut self) -> io::Result<()> {
        self.writer.flush()?;
        match self.writer.get_mut() {
            WriteHalf::Socket(s) => s.shutdown(Shutdown::Write),
            // Closing the pipe is all `SSHStore::Connection::closeWrite` does.
            half @ WriteHalf::Pipe(_) => {
                *half = WriteHalf::Closed;
                Ok(())
            }
            WriteHalf::Closed => Ok(()),
        }
    }

    #[cfg(test)]
    fn remote_pid(&self) -> Option<u32> {
        self.remote.as_ref().map(|r| r.child.id())
    }
}

impl Drop for Connection {
    /// `~BasicClientConnection` flushes; then, as `SSHMaster::Connection`
    /// is destroyed, the pipes close before the process is stopped. Errors
    /// have nowhere to go but stderr, as with Nix's
    /// `ignoreExceptionInDestructor`.
    fn drop(&mut self) {
        if let Err(e) = self.writer.flush() {
            eprintln!("error (ignored): {e}");
        }
        *self.writer.get_mut() = WriteHalf::Closed;
        *self.reader.get_mut() = ReadHalf::Closed;
        if let Some(mut remote) = self.remote.take() {
            let known_hosts = remote.known_hosts.take();
            if let Err(e) = remote.stop() {
                eprintln!("error (ignored): {e}");
            }
            // `AutoDelete`, once ssh no longer reads the file.
            if let Some(Err(e)) = known_hosts.map(tempfile::TempDir::close) {
                eprintln!("error (ignored): {e}");
            }
        }
    }
}

/// `SSHMaster::fakeSSH`: an authority of exactly `localhost` runs the
/// remote program directly instead of through ssh.
fn is_fake_ssh(target: &SshTarget) -> bool {
    target.host == "localhost" && target.user.is_none() && target.port.is_none()
}

/// The command `SSHMaster::startCommand` runs for `target` when no ssh
/// master is used:
///
/// ```text
/// ssh [user@]host -x $NIX_SSHOPTS [-i key] [-oUserKnownHostsFile=F] [-C] [-pPORT]
///     -oPermitLocalCommand=yes -oLocalCommand='echo started' -S none --
///     <remote-program> --stdio [--store <remote-store>]
/// ```
///
/// For `localhost` (no user, no port) it is just the remote command. Either
/// way `SHELL` is `/bin/sh`, so ssh runs the local command predictably
/// (`createSSHEnv`).
fn ssh_command(
    target: &SshTarget,
    ssh_opts: &[OsString],
    known_hosts_file: Option<&Path>,
) -> Result<Command, DaemonError> {
    let mut remote_command = target.remote_program.clone();
    remote_command.push("--stdio".to_owned());
    if let Some(store) = &target.remote_store {
        remote_command.push("--store".to_owned());
        remote_command.push(store.clone());
    }

    let mut cmd;
    if is_fake_ssh(target) {
        let (program, args) = remote_command
            .split_first()
            .ok_or_else(|| DaemonError::Unsupported("empty remote-program".into()))?;
        cmd = Command::new(program);
        cmd.args(args);
    } else {
        let host_and_user = match &target.user {
            Some(u) => format!("{u}@{}", target.host),
            None => target.host.clone(),
        };
        cmd = Command::new("ssh");
        cmd.arg(host_and_user).arg("-x");
        // `addCommonSSHOpts`.
        cmd.args(ssh_opts);
        if let Some(key) = &target.ssh_key {
            cmd.arg("-i").arg(key);
        }
        if let Some(file) = known_hosts_file {
            let mut opt = OsString::from("-oUserKnownHostsFile=");
            opt.push(file);
            cmd.arg(opt);
        }
        if target.compress {
            cmd.arg("-C");
        }
        if let Some(port) = target.port {
            cmd.arg(format!("-p{port}"));
        }
        cmd.arg("-oPermitLocalCommand=yes")
            .arg("-oLocalCommand=echo started")
            .arg("-S")
            .arg("none");
        cmd.arg("--");
        cmd.args(&remote_command);
    }
    cmd.env("SHELL", "/bin/sh");
    Ok(cmd)
}

/// `getNixSshOpts`: `$NIX_SSHOPTS` split by `shellSplitString`, as bytes
/// (see [`brix_store::shell`]).
fn nix_ssh_opts() -> Result<Vec<OsString>, DaemonError> {
    brix_store::shell::nix_ssh_opts().map_err(ssh_opts_error)
}

#[cfg(test)]
fn split_ssh_opts(opts: &std::ffi::OsStr) -> Result<Vec<OsString>, DaemonError> {
    brix_store::shell::split_ssh_opts(opts).map_err(ssh_opts_error)
}

/// The split's error with the trace Nix adds to it.
fn ssh_opts_error(e: brix_store::shell::SshOptsError) -> DaemonError {
    DaemonError::Traced {
        trace: e.trace,
        error: Box::new(DaemonError::Config(e.message)),
    }
}

/// A private temporary directory (`createTempDir(…, "nix", 0700)`): in
/// `$TMPDIR`, or `/tmp` when that is unset or empty (`defaultTempDir`).
fn private_temp_dir() -> io::Result<tempfile::TempDir> {
    use std::os::unix::fs::PermissionsExt;
    let base = match std::env::var_os("TMPDIR") {
        Some(d) if !d.is_empty() => PathBuf::from(d),
        _ => PathBuf::from("/tmp"),
    };
    tempfile::Builder::new()
        .prefix("nix-")
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir_in(base)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::WireRead;
    use std::ffi::OsStr;

    // `shellSplitString` itself is tested in `brix_store::shell`.

    #[test]
    fn nix_sshopts_errors_carry_nix_trace() {
        // `NIX_SSHOPTS='-o "unterminated' nix store info --store ssh-ng://host`.
        let e = split_ssh_opts(OsStr::new("-o \"unterminated")).unwrap_err();
        assert_eq!(
            e.render(false),
            "error:\n       … while splitting NIX_SSHOPTS '-o \"unterminated'\n\n       error: unterminated double quote"
        );
    }

    fn target(uri: &str) -> SshTarget {
        match crate::uri::StoreUri::parse(uri, &|w| panic!("unexpected warning: {w}"))
            .unwrap()
            .transport
        {
            Transport::SshNg(t) => t,
            other => panic!("{other:?}"),
        }
    }

    fn argv(cmd: &Command) -> Vec<String> {
        std::iter::once(cmd.get_program())
            .chain(cmd.get_args())
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn ssh_argv_follows_nix() {
        // The argv `nix store info` gives a logging stand-in for ssh with
        // NIX_SSHOPTS="-o Foo=1 'a b'" and the store
        // 'ssh-ng://alice@host:2222?ssh-key=/k&compress=true&max-connections=1&remote-store=/mnt'.
        let t = target(
            "ssh-ng://alice@host:2222?ssh-key=/k&compress=true&max-connections=1&remote-store=/mnt",
        );
        let opts = split_ssh_opts(OsStr::new("-o Foo=1 'a b'")).unwrap();
        let cmd = ssh_command(&t, &opts, Some(Path::new("/tmp/nix-1-0/host-key"))).unwrap();
        assert_eq!(
            argv(&cmd),
            [
                "ssh",
                "alice@host",
                "-x",
                "-o",
                "Foo=1",
                "a b",
                "-i",
                "/k",
                "-oUserKnownHostsFile=/tmp/nix-1-0/host-key",
                "-C",
                "-p2222",
                "-oPermitLocalCommand=yes",
                "-oLocalCommand=echo started",
                "-S",
                "none",
                "--",
                "nix-daemon",
                "--stdio",
                "--store",
                "/mnt",
            ]
        );
        let envs: Vec<_> = cmd.get_envs().collect();
        assert_eq!(envs, [(OsStr::new("SHELL"), Some(OsStr::new("/bin/sh")))]);

        let t = target("ssh-ng://host?remote-program=sudo%20nix-daemon");
        assert_eq!(
            argv(&ssh_command(&t, &[], None).unwrap()),
            [
                "ssh",
                "host",
                "-x",
                "-oPermitLocalCommand=yes",
                "-oLocalCommand=echo started",
                "-S",
                "none",
                "--",
                "sudo",
                "nix-daemon",
                "--stdio"
            ]
        );
    }

    #[test]
    fn localhost_runs_the_remote_program_directly() {
        let t = target("ssh-ng://localhost?remote-program=/bin/nix-daemon%20-v&remote-store=/s");
        let cmd = ssh_command(&t, &[], None).unwrap();
        assert_eq!(
            argv(&cmd),
            ["/bin/nix-daemon", "-v", "--stdio", "--store", "/s"]
        );
        assert_eq!(
            cmd.get_envs().collect::<Vec<_>>(),
            [(OsStr::new("SHELL"), Some(OsStr::new("/bin/sh")))]
        );
        // With a user or a port it is a real ssh connection.
        let cmd = ssh_command(&target("ssh-ng://me@localhost"), &[], None).unwrap();
        assert_eq!(argv(&cmd)[0], "ssh");
    }

    fn sh(script: &str) -> Command {
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c").arg(script);
        cmd
    }

    #[test]
    fn ssh_must_say_started() {
        let err = Connection::spawn(sh("echo 'Permission denied'"), Some("host"), None)
            .map(drop)
            .unwrap_err();
        assert!(
            matches!(&err, DaemonError::SshStartFailed(h) if h == "host"),
            "{err:?}"
        );
        assert_eq!(err.to_string(), "failed to start SSH connection to 'host'");
        // EOF before any line fails the same way.
        let err = Connection::spawn(sh("exit 0"), Some("host"), None)
            .map(drop)
            .unwrap_err();
        assert!(matches!(err, DaemonError::SshStartFailed(_)), "{err:?}");
        // After `started`, the stream is the protocol's.
        let mut conn =
            Connection::spawn(sh("echo started; printf 'ABCDEFGH'"), Some("host"), None).unwrap();
        assert_eq!(
            conn.reader.read_u64().unwrap(),
            u64::from_le_bytes(*b"ABCDEFGH")
        );
    }

    #[test]
    fn missing_program_is_reported_like_nix() {
        let err = Connection::spawn(Command::new("/nonexistent/nix-daemon"), None, None)
            .map(drop)
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "unable to execute '/nonexistent/nix-daemon': No such file or directory"
        );
    }

    /// A stand-in that ignores EOF on stdin and SIGPIPE, and writes forever.
    const STUBBORN: &str = "trap '' PIPE; echo started; while :; do echo spam 2>/dev/null; done";

    #[test]
    fn dropping_an_ssh_connection_kills_ssh() {
        let mut conn = Connection::spawn(sh(STUBBORN), Some("host"), None).unwrap();
        let mut line = String::new();
        conn.reader.read_line(&mut line).unwrap();
        assert_eq!(line, "spam\n");
        let pid = conn.remote_pid().unwrap();
        let start = Instant::now();
        drop(conn);
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "{:?}",
            start.elapsed()
        );
        assert!(
            !Path::new(&format!("/proc/{pid}")).exists(),
            "ssh stand-in {pid} was not reaped"
        );
    }

    #[test]
    fn a_local_remote_program_may_exit_by_itself() {
        let dir = private_temp_dir().unwrap();
        let marker = dir.path().join("exited");
        // Exits cleanly on EOF, after writing its marker.
        let script = format!("cat >/dev/null; echo done > '{}'", marker.display());
        let mut conn = Connection::spawn(sh(&script), None, None).unwrap();
        conn.writer.write_all(b"some request").unwrap();
        drop(conn);
        assert_eq!(std::fs::read_to_string(&marker).unwrap(), "done\n");
    }

    #[test]
    fn a_local_remote_program_that_lingers_is_killed() {
        let mut conn = Connection::spawn(sh(STUBBORN), None, None).unwrap();
        conn.remote.as_mut().unwrap().grace = Some(Duration::from_millis(200));
        let pid = conn.remote_pid().unwrap();
        let start = Instant::now();
        drop(conn);
        let waited = start.elapsed();
        assert!(waited >= Duration::from_millis(200), "{waited:?}");
        assert!(waited < Duration::from_secs(5), "{waited:?}");
        assert!(!Path::new(&format!("/proc/{pid}")).exists());
    }

    #[test]
    fn close_write_gives_the_other_side_eof() {
        let mut conn = Connection::spawn(sh("cat; echo after-eof"), None, None).unwrap();
        conn.writer.write_all(b"x\n").unwrap();
        conn.close_write().unwrap();
        let mut out = String::new();
        conn.reader.read_to_string(&mut out).unwrap();
        assert_eq!(out, "x\nafter-eof\n");
        assert!(conn
            .writer
            .write_all(b"y")
            .and_then(|()| conn.writer.flush())
            .is_err());

        let (a, b) = UnixStream::pair().unwrap();
        let mut conn = Connection::from_unix_stream(a).unwrap();
        conn.close_write().unwrap();
        let mut rest = Vec::new();
        (&b).read_to_end(&mut rest).unwrap();
        assert!(rest.is_empty(), "the peer sees EOF");
    }

    #[test]
    fn pinned_host_key_goes_to_a_private_known_hosts_file() {
        let t = target("ssh-ng://builder?base64-ssh-public-host-key=c3NoLWVkMjU1MTkgQUFBQQ==");
        // The stand-in for ssh shows the known-hosts file it was given and
        // the mode of its directory.
        let mut conn = Connection::ssh(&t, &[], |cmd| {
            let opt = cmd
                .get_args()
                .map(|a| a.to_string_lossy().into_owned())
                .find(|a| a.starts_with("-oUserKnownHostsFile="))
                .expect("known-hosts option");
            let file = opt.trim_start_matches("-oUserKnownHostsFile=").to_owned();
            sh(&format!(
                "echo started; cat '{file}'; ls -ld \"$(dirname '{file}')\""
            ))
        })
        .unwrap();
        let mut line = String::new();
        conn.reader.read_line(&mut line).unwrap();
        assert_eq!(line, "builder ssh-ed25519 AAAA\n");
        line.clear();
        conn.reader.read_line(&mut line).unwrap();
        assert!(line.starts_with("drwx------"), "{line}");
        let dir = conn
            .remote
            .as_ref()
            .unwrap()
            .known_hosts
            .as_ref()
            .unwrap()
            .path()
            .to_owned();
        drop(conn);
        assert!(
            !dir.exists(),
            "{} is removed with the connection",
            dir.display()
        );
    }
}
