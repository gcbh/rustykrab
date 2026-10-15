//! Starting an agent CLI (Claude Code, Codex) whose executable may be
//! momentarily busy.
//!
//! `execve` fails with `ETXTBSY` while anything holds the executable open
//! for writing. That passes in a moment when the CLI's updater is replacing
//! it, or when another thread of this process forked while the file was
//! open and the child has not exec'd yet. Failing the start would fail a
//! run, a login check or a quota read over it, so a busy start is tried
//! again after a short wait. A busy start ran nothing, so trying again
//! repeats nothing.

use std::io;
use std::process::Output;
use std::time::Duration;

use tokio::process::{Child, Command};

/// The waits between busy starts: about 0.6 s in all. A CLI still busy
/// after the last one fails to start as it always did.
const BUSY_WAITS: [Duration; 5] = [
    Duration::from_millis(20),
    Duration::from_millis(40),
    Duration::from_millis(80),
    Duration::from_millis(160),
    Duration::from_millis(320),
];

fn busy<T>(result: &io::Result<T>) -> bool {
    matches!(result, Err(e) if e.kind() == io::ErrorKind::ExecutableFileBusy)
}

/// [`Command::spawn`], waiting out a busy executable.
pub async fn spawn(cmd: &mut Command) -> io::Result<Child> {
    for wait in BUSY_WAITS {
        let started = cmd.spawn();
        if !busy(&started) {
            return started;
        }
        tokio::time::sleep(wait).await;
    }
    cmd.spawn()
}

/// [`Command::output`], waiting out a busy executable.
pub async fn output(cmd: &mut Command) -> io::Result<Output> {
    for wait in BUSY_WAITS {
        let finished = cmd.output().await;
        if !busy(&finished) {
            return finished;
        }
        tokio::time::sleep(wait).await;
    }
    cmd.output().await
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn script(dir: &std::path::Path) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("cli");
        std::fs::write(&path, "#!/bin/sh\necho started\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    /// Held open for writing a moment longer than the first try, the CLI
    /// still starts.
    #[tokio::test]
    #[cfg_attr(
        not(target_os = "linux"),
        ignore = "macOS executes a file open for writing"
    )]
    async fn a_briefly_busy_executable_starts() {
        let dir = tempfile::tempdir().unwrap();
        let path = script(dir.path());
        let writer = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        let release = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            drop(writer);
        });
        let out = output(&mut Command::new(&path)).await;
        release.join().unwrap();
        assert_eq!(out.unwrap().stdout, b"started\n");
    }

    /// Busy past every wait, the start fails with the busy error.
    #[tokio::test]
    #[cfg_attr(
        not(target_os = "linux"),
        ignore = "macOS executes a file open for writing"
    )]
    async fn an_executable_busy_past_every_wait_does_not_start() {
        let dir = tempfile::tempdir().unwrap();
        let path = script(dir.path());
        let _writer = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        let err = spawn(&mut Command::new(&path)).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::ExecutableFileBusy);
    }

    /// Any other failure returns at once, without waiting.
    #[tokio::test]
    async fn a_missing_executable_fails_at_once() {
        let dir = tempfile::tempdir().unwrap();
        let started = std::time::Instant::now();
        let err = spawn(&mut Command::new(dir.path().join("absent")))
            .await
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        assert!(started.elapsed() < BUSY_WAITS[0], "{:?}", started.elapsed());
    }
}
