//! One controller per data directory. During a cutover the old and new
//! daemons may share a data directory for a while; only the one holding an
//! exclusive advisory lock on `controller.lock` runs the loop. The lock is
//! `flock(2)`, so the kernel drops it when its holder exits, however it
//! exits, and a crashed daemon never leaves a stale lock behind.

use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};

use crate::handle::LockState;

/// The lock file's name inside the data directory.
pub const LOCK_FILE: &str = "controller.lock";

/// An exclusive advisory lock on one file, held until dropped.
#[derive(Debug)]
pub struct ControllerLock {
    // Closing the descriptor releases the lock.
    _file: File,
}

impl ControllerLock {
    /// Take the lock on `path` without waiting, creating the file if it is
    /// missing. `Ok(None)` when another open file already holds it, in this
    /// process or another.
    pub fn try_acquire(path: &Path) -> io::Result<Option<ControllerLock>> {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path)?;
        if try_flock_exclusive(&file)? {
            Ok(Some(ControllerLock { _file: file }))
        } else {
            Ok(None)
        }
    }
}

#[cfg(unix)]
fn try_flock_exclusive(file: &File) -> io::Result<bool> {
    use std::os::unix::io::AsRawFd;

    extern "C" {
        fn flock(fd: i32, operation: i32) -> i32;
    }
    const LOCK_EX: i32 = 2;
    const LOCK_NB: i32 = 4;
    // SAFETY: flock(2) takes a descriptor we own for the length of the call
    // and an integer, and touches no memory of ours.
    if unsafe { flock(file.as_raw_fd(), LOCK_EX | LOCK_NB) } == 0 {
        return Ok(true);
    }
    let err = io::Error::last_os_error();
    if err.kind() == io::ErrorKind::WouldBlock {
        Ok(false)
    } else {
        Err(err)
    }
}

/// Without flock(2) there is nothing to exclude another daemon with; the
/// lock is always granted.
#[cfg(not(unix))]
fn try_flock_exclusive(_file: &File) -> io::Result<bool> {
    Ok(true)
}

/// The tick loop's side of the lock: tried before each tick until it is
/// taken, then kept for the life of the loop.
#[derive(Debug)]
pub struct LoopLock {
    path: PathBuf,
    held: Option<ControllerLock>,
}

impl LoopLock {
    /// A loop lock on `data_dir/controller.lock`, not yet tried.
    pub fn in_data_dir(data_dir: &Path) -> Self {
        LoopLock::at(data_dir.join(LOCK_FILE))
    }

    /// A loop lock on `path`, not yet tried.
    pub fn at(path: PathBuf) -> Self {
        LoopLock { path, held: None }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Take the lock if it is not held yet. `Held` means a tick may run.
    /// An error opening or locking the file is returned as is; the loop
    /// treats it as `Waiting` and tries again next tick.
    pub fn poll(&mut self) -> io::Result<LockState> {
        if self.held.is_none() {
            self.held = ControllerLock::try_acquire(&self.path)?;
        }
        Ok(if self.held.is_some() {
            LockState::Held
        } else {
            LockState::Waiting
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rk-controller-lock-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(LOCK_FILE)
    }

    #[test]
    fn a_second_lock_fails_while_the_first_is_held_and_succeeds_after() {
        let path = temp_path();
        let first = ControllerLock::try_acquire(&path).unwrap();
        assert!(first.is_some(), "the first lock is granted");
        assert!(
            ControllerLock::try_acquire(&path).unwrap().is_none(),
            "a second lock on the same path is refused while the first is held"
        );
        drop(first);
        // See the loop lock test below for why the release is awaited.
        let started = std::time::Instant::now();
        while ControllerLock::try_acquire(&path).unwrap().is_none() {
            assert!(
                started.elapsed() < std::time::Duration::from_secs(5),
                "the lock is granted once the first is released"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    #[test]
    fn a_loop_lock_waits_then_holds_and_keeps_holding() {
        let path = temp_path();
        let other = ControllerLock::try_acquire(&path).unwrap().unwrap();
        let mut ours = LoopLock::at(path.clone());
        assert_eq!(ours.poll().unwrap(), LockState::Waiting);
        assert_eq!(ours.poll().unwrap(), LockState::Waiting);
        drop(other);
        // A process another test forks in parallel shares the descriptor
        // until it execs (it is close-on-exec), and with it the lock, so the
        // release can take a moment to be seen.
        let started = std::time::Instant::now();
        while ours.poll().unwrap() == LockState::Waiting {
            assert!(
                started.elapsed() < std::time::Duration::from_secs(5),
                "the lock is granted once the other holder lets go"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(ours.poll().unwrap(), LockState::Held);
        assert!(
            ControllerLock::try_acquire(&path).unwrap().is_none(),
            "a held loop lock keeps others out"
        );
    }
}
