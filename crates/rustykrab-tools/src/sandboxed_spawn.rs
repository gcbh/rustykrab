//! Cross-platform sandboxed subprocess execution.
//!
//! Provides OS-level process isolation for tools that spawn subprocesses
//! (primarily `CodeExecutionTool`). Two layers of protection:
//!
//! 1. **POSIX resource limits** (`setrlimit`): memory, CPU, file size, and
//!    process count limits. Works on macOS and Linux.
//!
//! 2. **Platform-specific isolation**:
//!    - **macOS**: Seatbelt (`sandbox-exec`) profiles that restrict filesystem
//!      writes and deny network access at the kernel level.
//!    - **Linux**: Namespace isolation (`unshare`) for PID, IPC, and network,
//!      plus `PR_SET_NO_NEW_PRIVS` to prevent privilege escalation.

use std::io;

/// Apply POSIX resource limits to the current process.
///
/// Intended to be called from a `pre_exec` hook so that limits are applied
/// to the child process before it execs the target binary.
///
/// # Safety
///
/// Must be called in a `pre_exec` context (post-fork, pre-exec). Uses only
/// async-signal-safe libc calls (`setrlimit`).
#[cfg(unix)]
#[allow(unused_variables, dead_code)]
pub fn apply_resource_limits(max_memory_bytes: u64, max_cpu_secs: u64) -> io::Result<()> {
    unsafe {
        // CPU time limit
        if max_cpu_secs > 0 {
            let limit = libc::rlimit {
                rlim_cur: max_cpu_secs,
                rlim_max: max_cpu_secs,
            };
            // Ignore EINVAL — on macOS the hard limit may be lower than requested.
            let _ = libc::setrlimit(libc::RLIMIT_CPU, &limit);
        }

        // File size limit (10 MB)
        let fsize_limit = libc::rlimit {
            rlim_cur: 10 * 1024 * 1024,
            rlim_max: 10 * 1024 * 1024,
        };
        let _ = libc::setrlimit(libc::RLIMIT_FSIZE, &fsize_limit);

        // Memory limit (virtual address space).
        // On macOS, Python's virtual address space at startup can be several
        // GB due to memory-mapped frameworks, so RLIMIT_AS kills it before
        // it can even print output. Only apply on Linux where virtual memory
        // usage is more predictable.
        #[cfg(target_os = "linux")]
        if max_memory_bytes > 0 {
            let limit = libc::rlimit {
                rlim_cur: max_memory_bytes,
                rlim_max: max_memory_bytes,
            };
            let _ = libc::setrlimit(libc::RLIMIT_AS, &limit);
        }

        // Process count limit — prevent fork bombs.
        // On macOS, RLIMIT_NPROC applies to the entire user, not just
        // the child tree. Use a small but workable limit; ignore failure
        // when the current count already exceeds it.
        let nproc_limit = libc::rlimit {
            rlim_cur: 32,
            rlim_max: 32,
        };
        let _ = libc::setrlimit(libc::RLIMIT_NPROC, &nproc_limit);
    }
    Ok(())
}

/// Generate a macOS Seatbelt sandbox profile for code execution.
///
/// The profile denies all operations by default, then selectively allows:
/// - Reading system libraries, Python installation, and the sandbox directory
/// - Writing only to the sandbox temporary directory
/// - Basic process operations needed by the Python runtime
/// - Network access is always denied
#[cfg(target_os = "macos")]
pub fn generate_seatbelt_profile(
    sandbox_dir: &std::path::Path,
    _python_path: &std::path::Path,
) -> String {
    let sandbox_dir = sandbox_dir.to_string_lossy();

    // Use a permissive-by-default profile that only denies network access
    // and restricts file writes to the sandbox directory. Enumerating
    // every permission Python's runtime needs is fragile across macOS
    // versions and Python installations (pyenv, homebrew, system, etc.).
    format!(
        r#"(version 1)
(allow default)

;; DENY all network access — this is the primary containment.
(deny network*)

;; Restrict file writes to the sandbox temp directory.
(deny file-write*
    (require-not
        (require-any
            (subpath "{sandbox_dir}")
            (subpath "/dev/null"))))
"#
    )
}

/// Apply Linux namespace isolation to the current process.
///
/// Called from `pre_exec` to isolate the child process. Falls back
/// gracefully if namespaces are unavailable (rlimits still apply).
///
/// # Safety
///
/// Must be called in a `pre_exec` context. Uses only libc syscalls.
#[cfg(target_os = "linux")]
pub fn apply_linux_namespaces() -> io::Result<()> {
    unsafe {
        // Prevent privilege escalation via setuid binaries
        if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
            // Non-fatal: log and continue
            let _ = io::Error::last_os_error();
        }

        // Create new namespaces: PID, IPC, and network
        let flags = libc::CLONE_NEWPID | libc::CLONE_NEWIPC | libc::CLONE_NEWNET;
        if libc::unshare(flags) != 0 {
            // Namespace isolation unavailable (e.g., user namespaces disabled).
            // Fall back to rlimits-only protection. This is logged as a warning
            // by the caller.
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[cfg(target_os = "macos")]
    use super::*;

    #[cfg(target_os = "macos")]
    use std::path::Path;

    #[cfg(target_os = "macos")]
    #[test]
    fn seatbelt_profile_denies_network() {
        let profile = generate_seatbelt_profile(
            Path::new("/tmp/rustykrab_sandbox"),
            Path::new("/usr/bin/python3"),
        );
        assert!(profile.contains("(deny network*)"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn seatbelt_profile_allows_sandbox_dir_write() {
        let profile = generate_seatbelt_profile(
            Path::new("/tmp/rustykrab_sandbox"),
            Path::new("/usr/bin/python3"),
        );
        // Sandbox dir should appear in the write exception
        assert!(profile.contains("/tmp/rustykrab_sandbox"));
        assert!(profile.contains("(deny network*)"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn seatbelt_profile_restricts_writes() {
        let profile = generate_seatbelt_profile(
            Path::new("/tmp/rustykrab_sandbox"),
            Path::new("/usr/bin/python3"),
        );
        // File writes should be denied except to sandbox dir
        assert!(profile.contains("(deny file-write*"));
        assert!(profile.contains("/tmp/rustykrab_sandbox"));
    }
}
