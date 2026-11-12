//! Interrupt real CLI processes, with readiness handshakes rather than sleeps.
//! The fake daemon accepts a request but never answers it; SIGINT must end
//! the client without waiting for daemon I/O or session teardown.

use std::io::Read;
use std::os::unix::net::UnixListener;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use assert_cmd::prelude::*;

struct Process(Child);
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn wait_for_exit(child: &mut Child) -> std::process::ExitStatus {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        assert!(
            Instant::now() < deadline,
            "one SIGINT must stop the CLI promptly"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn one_interrupt_stops_blocked_daemon_io_even_with_inherited_signal_state() {
    for inherited in ["default", "blocked", "ignored"] {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("daemon.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        let mut command = Command::cargo_bin("brix").unwrap();
        command.args(["--store", &format!("unix://{}", socket.display()),
            "build", "--no-link", "--expr",
            "derivation { name = \"interrupt\"; system = \"x86_64-linux\"; builder = \"/bin/sh\"; }"])
            .env("BRIX_HAVE_INTERNET", "1")
            .stdout(Stdio::null()).stderr(Stdio::piped());
        // SAFETY: only async-signal-safe libc calls between fork and exec.
        unsafe {
            command.pre_exec(move || {
                if inherited == "blocked" {
                    let mut set = std::mem::zeroed();
                    libc::sigemptyset(&mut set);
                    libc::sigaddset(&mut set, libc::SIGINT);
                    if libc::sigprocmask(libc::SIG_BLOCK, &set, std::ptr::null_mut()) != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                } else if inherited == "ignored" {
                    libc::signal(libc::SIGINT, libc::SIG_IGN);
                }
                Ok(())
            });
        }
        let mut process = Process(command.spawn().unwrap());
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut connection = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "CLI did not reach daemon");
                    assert!(process.0.try_wait().unwrap().is_none(), "CLI exited early");
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(e) => panic!("{e}"),
            }
        };
        connection
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut magic = [0; 8];
        connection.read_exact(&mut magic).unwrap();
        // SAFETY: send SIGINT only to this live test child.
        assert_eq!(
            unsafe { libc::kill(process.0.id() as libc::pid_t, libc::SIGINT) },
            0
        );
        assert_eq!(
            wait_for_exit(&mut process.0).code(),
            Some(130),
            "{inherited}"
        );
        let mut stderr = String::new();
        process
            .0
            .stderr
            .take()
            .unwrap()
            .read_to_string(&mut stderr)
            .unwrap();
        assert!(
            stderr.contains("interrupted by the user"),
            "{inherited}: {stderr}"
        );
        assert!(
            !stderr.contains('\x1b'),
            "pipes must not receive terminal controls"
        );
        // Closing the client socket is what tells the daemon to cancel.
        let mut remaining = Vec::new();
        connection
            .read_to_end(&mut remaining)
            .expect("daemon connection closed on interrupt");
    }
}

fn wait_for_ready(child: &mut Child) {
    use std::io::BufRead;
    let stderr = child.stderr.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in std::io::BufReader::new(stderr).lines() {
            let line = line.unwrap();
            if line.contains("ready-to-interrupt") {
                let _ = tx.send(());
                return;
            }
        }
    });
    rx.recv_timeout(Duration::from_secs(10))
        .expect("child reached its blocking work");
}

#[test]
fn one_interrupt_stops_cpu_evaluation() {
    let mut process = Process(
        Command::cargo_bin("brix")
            .unwrap()
            .args([
                "--store",
                "dummy://",
                "eval",
                "--expr",
                "let loop = n: loop (n + 1); in builtins.trace \"ready-to-interrupt\" (loop 0)",
            ])
            .env("BRIX_HAVE_INTERNET", "1")
            .stderr(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .unwrap(),
    );
    wait_for_ready(&mut process.0);
    // SAFETY: this is a live child owned by the test.
    assert_eq!(
        unsafe { libc::kill(process.0.id() as libc::pid_t, libc::SIGINT) },
        0
    );
    assert_eq!(wait_for_exit(&mut process.0).code(), Some(130));
}

#[test]
fn delegated_programs_keep_default_interrupt_handling() {
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::process::ExitStatusExt;
    let dir = tempfile::tempdir().unwrap();
    let delegate = dir.path().join("nix");
    std::fs::write(
        &delegate,
        "#!/bin/sh\necho ready-to-interrupt >&2\nread line\n",
    )
    .unwrap();
    std::fs::set_permissions(&delegate, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut process = Process(
        Command::cargo_bin("brix")
            .unwrap()
            .args(["repl"])
            .env("BRIX_DELEGATE_NIX", delegate)
            .stdin(Stdio::piped())
            .stderr(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .unwrap(),
    );
    wait_for_ready(&mut process.0);
    // SAFETY: the delegate replaced our live child via exec.
    assert_eq!(
        unsafe { libc::kill(process.0.id() as libc::pid_t, libc::SIGINT) },
        0
    );
    assert_eq!(wait_for_exit(&mut process.0).signal(), Some(libc::SIGINT));
}
