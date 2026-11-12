//! Exclusive locks on files across processes (`flock`), as Nix's
//! `PathLocks` takes them: brix processes sharing a cache directory, and
//! the threads of one process, wait for each other through them.
//!
//! A lock is released when its file is closed, which the operating system
//! does when the process exits, however it exits: a lock that can be
//! taken without waiting is one whose holder is gone. Lock files are left
//! in place; removing one another process may be waiting on would let a
//! third take a lock on a new file while the second holds the old one.

use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

use crate::{err, Result};

/// An exclusive lock on a file, held until dropped.
#[derive(Debug)]
pub struct FileLock {
    /// Closing the file releases the lock.
    _file: File,
    path: PathBuf,
}

impl FileLock {
    /// Takes the lock on `path` (created if missing), waiting for whoever
    /// holds it.
    pub fn acquire(path: &Path) -> Result<FileLock> {
        let file = open(path)?;
        lock(&file, path, true)?;
        Ok(FileLock {
            _file: file,
            path: path.to_path_buf(),
        })
    }

    /// Takes the lock on `path` (created if missing) if nobody holds it.
    pub fn try_acquire(path: &Path) -> Result<Option<FileLock>> {
        let file = open(path)?;
        if !lock(&file, path, false)? {
            return Ok(None);
        }
        Ok(Some(FileLock {
            _file: file,
            path: path.to_path_buf(),
        }))
    }

    /// Takes the lock on the file at `path`, which is not created, if
    /// nobody holds it: `None` when somebody does, or when there is no
    /// such file (any more). For looking at another writer's lock, whose
    /// file that writer removes when it is done: making it again would
    /// leave it behind.
    pub fn try_acquire_existing(path: &Path) -> Result<Option<FileLock>> {
        let file = match options().open(path) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(open_error(path, &e)),
        };
        if !lock(&file, path, false)? {
            return Ok(None);
        }
        Ok(Some(FileLock {
            _file: file,
            path: path.to_path_buf(),
        }))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Opening a lock file for reading and writing, as it is.
fn options() -> OpenOptions {
    let mut options = OpenOptions::new();
    options.read(true).write(true).truncate(false);
    options
}

fn open_error(path: &Path, e: &std::io::Error) -> crate::Error {
    err!(
        "opening lock file \"{}\": {}",
        path.display(),
        super::path::sys_error_text(e)
    )
}

fn open(path: &Path) -> Result<File> {
    options()
        .create(true)
        .open(path)
        .map_err(|e| open_error(path, &e))
}

/// `lockFile(fd, ltWrite, wait)`: whether the lock was taken.
#[cfg(unix)]
fn lock(file: &File, path: &Path, wait: bool) -> Result<bool> {
    use rustix::fs::{flock, FlockOperation};
    use rustix::io::Errno;
    let op = if wait {
        FlockOperation::LockExclusive
    } else {
        FlockOperation::NonBlockingLockExclusive
    };
    loop {
        match flock(file, op) {
            Ok(()) => return Ok(true),
            Err(Errno::INTR) => continue,
            Err(Errno::WOULDBLOCK) if !wait => return Ok(false),
            Err(e) => {
                let e = std::io::Error::from(e);
                return Err(err!(
                    "acquiring lock on \"{}\": {}",
                    path.display(),
                    super::path::sys_error_text(&e)
                ));
            }
        }
    }
}

#[cfg(not(unix))]
fn lock(_file: &File, path: &Path, _wait: bool) -> Result<bool> {
    Err(err!(
        "cannot lock \"{}\": file locks are only implemented on Unix",
        path.display()
    ))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::process::{Child, Command};

    /// The variable that makes [`holder`] hold a lock: the paths of the
    /// lock, of the file it creates once it holds it and of the file whose
    /// appearance releases it, a line each.
    const HOLDER: &str = "BRIX_TEST_LOCK_HOLDER";

    /// Another process holding the lock on `lock` until `release` exists;
    /// it creates `ready` once it has the lock.
    pub(crate) fn spawn_holder(lock: &Path, ready: &Path, release: &Path) -> Child {
        let child = Command::new(std::env::current_exe().expect("the test binary"))
            .args([
                "--exact",
                "native::lock::tests::holder",
                "--ignored",
                "--nocapture",
            ])
            .env(
                HOLDER,
                format!(
                    "{}\n{}\n{}",
                    lock.display(),
                    ready.display(),
                    release.display()
                ),
            )
            .spawn()
            .expect("start the lock holder");
        while !ready.exists() {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        child
    }

    /// Run by [`spawn_holder`] in a process of its own.
    #[test]
    #[ignore = "a helper the other tests run in a child process"]
    fn holder() {
        let spec = std::env::var(HOLDER).expect("run by spawn_holder, which sets the variable");
        let mut parts = spec.lines();
        let mut next = || PathBuf::from(parts.next().expect("three paths"));
        let (lock, ready, release) = (next(), next(), next());
        let _held = FileLock::acquire(&lock).unwrap();
        std::fs::write(&ready, "").unwrap();
        while !release.exists() {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    /// A lock another process holds cannot be taken until that process
    /// lets go; a waiting `acquire` gets it then.
    #[test]
    fn locks_exclude_other_processes() {
        let dir = tempfile::tempdir().unwrap();
        let lock = dir.path().join("x.lock");
        let (ready, release) = (dir.path().join("ready"), dir.path().join("release"));
        let mut child = spawn_holder(&lock, &ready, &release);
        assert!(FileLock::try_acquire(&lock).unwrap().is_none());
        let waiter = std::thread::spawn({
            let lock = lock.clone();
            move || FileLock::acquire(&lock).unwrap()
        });
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert!(!waiter.is_finished(), "acquire waited for the holder");
        std::fs::write(&release, "").unwrap();
        assert!(child.wait().unwrap().success());
        let held = waiter.join().unwrap();
        assert_eq!(held.path(), lock);
        // Within one process, locks on the same file exclude each other too.
        assert!(FileLock::try_acquire(&lock).unwrap().is_none());
        drop(held);
        // Dropping closes our descriptor, but a child that another test's
        // thread forked meanwhile has a copy of it until it execs (where
        // O_CLOEXEC closes it), and a flock lasts while any copy is open:
        // allow that window instead of failing on it.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while FileLock::try_acquire(&lock).unwrap().is_none() {
            assert!(
                std::time::Instant::now() < deadline,
                "dropping the lock released it"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    /// A process that exits without letting go releases its lock.
    #[test]
    fn a_dead_holder_releases_its_lock() {
        let dir = tempfile::tempdir().unwrap();
        let lock = dir.path().join("x.lock");
        let (ready, release) = (dir.path().join("ready"), dir.path().join("release"));
        let mut child = spawn_holder(&lock, &ready, &release);
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(FileLock::try_acquire(&lock).unwrap().is_some());
    }

    /// Another writer's lock is looked at without making its file: one
    /// that is gone stays gone.
    #[test]
    fn an_existing_lock_is_taken_without_making_one() {
        let dir = tempfile::tempdir().unwrap();
        let lock = dir.path().join("x.lock");
        assert!(FileLock::try_acquire_existing(&lock).unwrap().is_none());
        assert!(!lock.exists());
        std::fs::write(&lock, "").unwrap();
        let taken = FileLock::try_acquire_existing(&lock).unwrap();
        assert!(taken.is_some());
        // Held now, so not taken a second time.
        assert!(FileLock::try_acquire_existing(&lock).unwrap().is_none());
    }
}
