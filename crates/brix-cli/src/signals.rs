//! Process-level cancellation for the CLI. Libraries leave signal policy to
//! their caller. Install before starting any threads so an inherited blocked
//! or ignored SIGINT cannot leave the evaluator or a daemon read unkillable.
//!
//! Exiting closes the daemon connection, which cancels its build request.
//! Do not unwind or take locks here: the interrupted thread may hold the
//! progress, allocator, cache or daemon lock. Cache publication is atomic and
//! stale temporary files are cleaned by the next invocation.

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};

static TERMINAL: AtomicBool = AtomicBool::new(false);

extern "C" fn interrupt(_: libc::c_int) {
    // Only async-signal-safe operations. In particular, don't use stderr's
    // Rust lock, the progress mutex, allocation, formatting or destructors.
    let message: &[u8] = if TERMINAL.load(Ordering::Relaxed) {
        b"\r\x1b[0m\x1b[Kerror: interrupted by the user\n"
    } else {
        b"error: interrupted by the user\n"
    };
    // SAFETY: the message is static valid memory; write and _exit are
    // async-signal-safe. _exit terminates all threads and closes all FDs.
    unsafe {
        libc::write(libc::STDERR_FILENO, message.as_ptr().cast(), message.len());
        libc::_exit(128 + libc::SIGINT);
    }
}

pub fn init() -> io::Result<()> {
    TERMINAL.store(brix::log::is_tty(), Ordering::Relaxed);
    // SAFETY: these are plain C signal structures, initialised before use.
    // init runs on the main thread before brix starts workers. A caught
    // disposition resets to default on exec; the mask remains unblocked,
    // so delegated Nix and programs launched by run/develop remain interruptible.
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = interrupt as *const () as libc::sighandler_t;
        libc::sigemptyset(&mut action.sa_mask);
        // A second signal retains the default escape hatch even if writing
        // the interruption message is blocked by a stalled terminal/pipe.
        action.sa_flags = libc::SA_RESETHAND | libc::SA_NODEFER;
        if libc::sigaction(libc::SIGINT, &action, std::ptr::null_mut()) != 0 {
            return Err(io::Error::last_os_error());
        }
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGINT);
        let error = libc::pthread_sigmask(libc::SIG_UNBLOCK, &set, std::ptr::null_mut());
        if error != 0 {
            return Err(io::Error::from_raw_os_error(error));
        }
    }
    Ok(())
}
