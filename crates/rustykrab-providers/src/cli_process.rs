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
//!
//! Every start takes the caller's deadline, the one its own timeout ends
//! at: a wait that would end after it is not taken, so the waits never
//! stretch the caller's budget and nothing starts once it is spent. The
//! caller then bounds the child with `tokio::time::timeout_at(deadline, ..)`,
//! so the time spent waiting comes out of the child's share. Every try
//! checks the deadline first and returns `TimedOut` without starting once
//! it has come. Dropping the returned future during a wait starts nothing.

use std::io;
use std::process::Output;
use std::time::Duration;

use tokio::process::{Child, Command};
use tokio::time::Instant;

/// The waits between busy starts: about 0.6 s in all. A CLI still busy
/// after the last one, or at the deadline, fails to start as it always did.
const BUSY_WAITS: [Duration; 5] = [
    Duration::from_millis(20),
    Duration::from_millis(40),
    Duration::from_millis(80),
    Duration::from_millis(160),
    Duration::from_millis(320),
];

/// Whether `started` failed on a busy executable and the next wait fits
/// before `deadline`; if so, waits it.
async fn waited<T>(
    started: &io::Result<T>,
    waits: &mut impl Iterator<Item = Duration>,
    deadline: Instant,
) -> bool {
    if !matches!(started, Err(e) if e.kind() == io::ErrorKind::ExecutableFileBusy) {
        return false;
    }
    match waits.next() {
        Some(wait) if Instant::now() + wait <= deadline => {
            tokio::time::sleep(wait).await;
            true
        }
        _ => false,
    }
}

/// `TimedOut` once `deadline` has come: checked before every try, so a
/// deadline that had already passed, or a wait that woke late, starts
/// nothing.
fn expired(deadline: Instant) -> io::Result<()> {
    if Instant::now() >= deadline {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "deadline passed before the CLI started",
        ));
    }
    Ok(())
}

/// [`Command::spawn`], waiting out a busy executable until `deadline`.
pub async fn spawn(cmd: &mut Command, deadline: Instant) -> io::Result<Child> {
    let mut waits = BUSY_WAITS.into_iter();
    loop {
        expired(deadline)?;
        let started = cmd.spawn();
        if !waited(&started, &mut waits, deadline).await {
            return started;
        }
    }
}

/// [`Command::output`], waiting out a busy executable until `deadline`.
/// The caller still bounds the whole call by the same deadline.
pub async fn output(cmd: &mut Command, deadline: Instant) -> io::Result<Output> {
    let mut waits = BUSY_WAITS.into_iter();
    loop {
        expired(deadline)?;
        let finished = cmd.output().await;
        if !waited(&finished, &mut waits, deadline).await {
            return finished;
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    /// A script that leaves `ran` beside itself when it runs.
    fn script(dir: &Path) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("cli");
        std::fs::write(
            &path,
            format!(
                "#!/bin/sh\ntouch {}\necho started\n",
                dir.join("ran").display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    /// Holds `path` open for writing, and checks that this alone makes it
    /// busy, so a test's window is real before the code under test runs.
    fn hold_busy(path: &Path) -> std::fs::File {
        let writer = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        let err = std::process::Command::new(path).output().unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::ExecutableFileBusy, "{err}");
        writer
    }

    fn far() -> Instant {
        Instant::now() + Duration::from_secs(30)
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
        let writer = hold_busy(&path);
        let release = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            drop(writer);
        });
        let out = output(&mut Command::new(&path), far()).await;
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
        let _writer = hold_busy(&path);
        let err = spawn(&mut Command::new(&path), far()).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::ExecutableFileBusy);
        assert!(!dir.path().join("ran").exists());
    }

    /// A wait that would end after the deadline is not taken: with 50 ms
    /// left, the start waits 20 ms, finds 40 ms more would pass the
    /// deadline, and fails.
    #[tokio::test(start_paused = true)]
    #[cfg_attr(
        not(target_os = "linux"),
        ignore = "macOS executes a file open for writing"
    )]
    async fn a_busy_start_never_waits_past_its_deadline() {
        let dir = tempfile::tempdir().unwrap();
        let path = script(dir.path());
        let _writer = hold_busy(&path);
        let begun = Instant::now();
        let err = spawn(&mut Command::new(&path), begun + Duration::from_millis(50))
            .await
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::ExecutableFileBusy);
        assert_eq!(begun.elapsed(), Duration::from_millis(20));
    }

    /// A start dropped while it waits never runs the CLI, even once the
    /// executable is free.
    #[tokio::test(start_paused = true)]
    #[cfg_attr(
        not(target_os = "linux"),
        ignore = "macOS executes a file open for writing"
    )]
    async fn a_start_cancelled_while_waiting_starts_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = script(dir.path());
        let writer = hold_busy(&path);
        let cancelled = tokio::time::timeout(
            Duration::from_millis(30),
            spawn(&mut Command::new(&path), far()),
        )
        .await;
        assert!(cancelled.is_err(), "the start was still waiting");
        drop(writer);
        tokio::time::sleep(Duration::from_secs(1)).await;
        std::thread::sleep(Duration::from_millis(50));
        assert!(!dir.path().join("ran").exists());
    }

    /// A deadline that has already come starts nothing, through either
    /// entry point, even for a healthy executable.
    #[tokio::test]
    async fn an_expired_deadline_starts_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = script(dir.path());
        let now = Instant::now();
        let err = spawn(&mut Command::new(&path), now).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        let err = output(&mut Command::new(&path), now).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        std::thread::sleep(Duration::from_millis(50));
        assert!(!dir.path().join("ran").exists());
    }

    /// A wait that ends at the deadline does not try again, though the
    /// executable came free during it: the try would start after its
    /// budget.
    #[tokio::test(start_paused = true)]
    #[cfg_attr(
        not(target_os = "linux"),
        ignore = "macOS executes a file open for writing"
    )]
    async fn a_wait_that_ends_at_the_deadline_starts_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = script(dir.path());
        let writer = hold_busy(&path);
        let release = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            drop(writer);
        });
        let begun = Instant::now();
        let err = spawn(&mut Command::new(&path), begun + Duration::from_millis(20))
            .await
            .unwrap_err();
        release.await.unwrap();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        assert_eq!(begun.elapsed(), Duration::from_millis(20));
        std::thread::sleep(Duration::from_millis(50));
        assert!(!dir.path().join("ran").exists());
    }

    /// Any other failure returns at once, without a wait.
    #[tokio::test(start_paused = true)]
    async fn a_missing_executable_fails_without_waiting() {
        let dir = tempfile::tempdir().unwrap();
        let begun = Instant::now();
        let err = spawn(&mut Command::new(dir.path().join("absent")), far())
            .await
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        assert_eq!(begun.elapsed(), Duration::ZERO);
    }
}
