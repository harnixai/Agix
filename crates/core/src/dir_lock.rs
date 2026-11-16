//! An exclusive hold on a directory, against every other holder: other
//! threads of this process, and on unix other processes, through `flock` on
//! the directory itself. Settings tiers and memory directories are written by
//! every session of every daemon (and CLI) the user runs over them.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError, TryLockError};
use std::time::{Duration, Instant};

/// How long a writer of settings or memory waits for the directory before
/// giving up.
pub const DIR_LOCK_WAIT: Duration = Duration::from_secs(10);

const RETRY_BACKOFF: Duration = Duration::from_millis(20);

/// Carried by the `io::ErrorKind::TimedOut` error of [`DirLock::acquire_within`].
#[derive(Debug, thiserror::Error)]
#[error("the directory {} is locked by another holder", dir.display())]
pub struct LockTimeout {
    pub dir: PathBuf,
}

impl LockTimeout {
    pub fn of(error: &std::io::Error) -> Option<&Self> {
        error.get_ref()?.downcast_ref()
    }
}

pub struct DirLock {
    dir: PathBuf,
    _process: MutexGuard<'static, ()>,
    #[cfg(unix)]
    handle: std::fs::File,
}

impl DirLock {
    /// Create `dir` if it is missing and wait until no one else holds it.
    pub fn acquire(dir: &Path) -> std::io::Result<Self> {
        std::fs::create_dir_all(dir)?;
        let dir = dir.canonicalize()?;
        let process = process_lock(&dir);
        #[cfg(unix)]
        let handle = {
            let handle = std::fs::File::open(&dir)?;
            flock_exclusive(&handle)?;
            handle
        };
        Ok(Self {
            dir,
            _process: process,
            #[cfg(unix)]
            handle,
        })
    }

    /// [`DirLock::acquire`] that gives up after `wait` with a
    /// [`LockTimeout`], which a holder that never lets go would otherwise
    /// turn into a wait with no end.
    pub fn acquire_within(dir: &Path, wait: Duration) -> std::io::Result<Self> {
        std::fs::create_dir_all(dir)?;
        let dir = dir.canonicalize()?;
        let deadline = Instant::now() + wait;
        let timed_out =
            |dir: PathBuf| std::io::Error::new(std::io::ErrorKind::TimedOut, LockTimeout { dir });
        let process = loop {
            match try_process_lock(&dir) {
                Some(guard) => break guard,
                None if Instant::now() >= deadline => return Err(timed_out(dir)),
                None => std::thread::sleep(RETRY_BACKOFF),
            }
        };
        #[cfg(unix)]
        let handle = {
            let handle = std::fs::File::open(&dir)?;
            while !try_flock_exclusive(&handle)? {
                if Instant::now() >= deadline {
                    return Err(timed_out(dir));
                }
                std::thread::sleep(RETRY_BACKOFF);
            }
            handle
        };
        Ok(Self {
            dir,
            _process: process,
            #[cfg(unix)]
            handle,
        })
    }

    /// The directory, canonical.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Make the renames done in the directory durable.
    pub fn sync(&self) {
        #[cfg(unix)]
        let _ = self.handle.sync_all();
    }
}

/// Other threads of this process queue here — which is also all the
/// exclusion a platform without `flock` gets.
fn process_lock(dir: &Path) -> MutexGuard<'static, ()> {
    process_mutex(dir)
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
}

fn try_process_lock(dir: &Path) -> Option<MutexGuard<'static, ()>> {
    match process_mutex(dir).try_lock() {
        Ok(guard) => Some(guard),
        Err(TryLockError::Poisoned(e)) => Some(e.into_inner()),
        Err(TryLockError::WouldBlock) => None,
    }
}

fn process_mutex(dir: &Path) -> &'static Mutex<()> {
    static LOCKS: OnceLock<Mutex<HashMap<PathBuf, &'static Mutex<()>>>> = OnceLock::new();
    let mut locks = LOCKS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    let lock: &'static Mutex<()> = match locks.get(dir) {
        Some(lock) => lock,
        None => {
            let lock = Box::leak(Box::default());
            locks.insert(dir.to_path_buf(), lock);
            lock
        }
    };
    lock
}

#[cfg(unix)]
fn flock_exclusive(file: &std::fs::File) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    loop {
        // SAFETY: `flock` only reads the descriptor, which `file` keeps open.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == 0 {
            return Ok(());
        }
        let e = std::io::Error::last_os_error();
        if e.kind() != std::io::ErrorKind::Interrupted {
            return Err(e);
        }
    }
}

#[cfg(unix)]
fn try_flock_exclusive(file: &std::fs::File) -> std::io::Result<bool> {
    use std::os::fd::AsRawFd;
    loop {
        // SAFETY: `flock` only reads the descriptor, which `file` keeps open.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return Ok(true);
        }
        let e = std::io::Error::last_os_error();
        match e.kind() {
            std::io::ErrorKind::Interrupted => {}
            std::io::ErrorKind::WouldBlock => return Ok(false),
            _ => return Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn hold_flock(dir: &Path) -> std::fs::File {
        use std::os::fd::AsRawFd;
        let file = std::fs::File::open(dir).unwrap();
        // SAFETY: `flock` only reads the descriptor, which `file` keeps open.
        assert_eq!(unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) }, 0);
        file
    }

    #[cfg(unix)]
    #[test]
    fn a_directory_another_flock_holds_times_out_and_then_opens() {
        let dir = tempfile::tempdir().unwrap();
        let held = hold_flock(dir.path());
        let err = DirLock::acquire_within(dir.path(), Duration::from_millis(100))
            .err()
            .expect("the directory is held");
        assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
        let timeout = LockTimeout::of(&err).expect("a LockTimeout");
        assert_eq!(timeout.dir, dir.path().canonicalize().unwrap());
        drop(held);
        DirLock::acquire_within(dir.path(), Duration::from_secs(5)).unwrap();
    }

    #[test]
    fn a_directory_this_process_holds_times_out_and_then_opens() {
        let dir = tempfile::tempdir().unwrap();
        let held = DirLock::acquire(dir.path()).unwrap();
        let err = DirLock::acquire_within(dir.path(), Duration::from_millis(100))
            .err()
            .expect("the directory is held");
        assert!(LockTimeout::of(&err).is_some());
        drop(held);
        DirLock::acquire_within(dir.path(), Duration::from_secs(5)).unwrap();
    }

    #[test]
    fn other_errors_are_not_a_timeout() {
        let err = std::io::Error::other("x");
        assert!(LockTimeout::of(&err).is_none());
    }
}
