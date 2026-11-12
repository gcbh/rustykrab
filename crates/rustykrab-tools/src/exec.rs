use async_trait::async_trait;
use rustykrab_core::types::ToolSchema;
use rustykrab_core::{Result, SandboxRequirements, Tool, ToolError};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::time::timeout;

const MAX_OUTPUT_BYTES: usize = 100 * 1024; // 100KB

/// A command's time limit when the call names none: long enough for a
/// test suite or a build step, as feather-harness's 120 s default is.
pub const DEFAULT_TIMEOUT_SECS: u64 = 120;
/// The longest a call may ask for. The runner's own limit on an exec call
/// sits just above it, so the command's limit is the one that applies.
pub const MAX_TIMEOUT_SECS: u64 = 600;
/// The shortest limit a command that runs Cargo gets, whatever it asks for.
/// A build stopped after a few seconds finishes nothing worth keeping, and a
/// model that picked 15 s relaunched one 156 times in a live run.
pub const MIN_CARGO_TIMEOUT_SECS: u64 = 120;

/// The Cargo home every worker's builds share, beside the repositories'
/// build caches under the daemon's build-cache directory: one copy of each
/// downloaded dependency's sources and registry index for every worker, kept
/// across reboots. A Cargo home under the isolated `/tmp` HOME was wiped on
/// reboot, and re-extracted sources carry new modification times, which
/// invalidate every dependency already built in the shared target directory.
pub const SHARED_CARGO_HOME: &str = "cargo-home";

/// Rustc jobs one Cargo build may run on this machine. Without a limit each
/// build ran as many as the machine has cores, beside a local model that
/// holds most of its memory.
pub const CARGO_BUILD_JOBS: &str = "6";

/// An external run's temporary directory is `<RUN_TMP_PREFIX><12 hex>`
/// directly under the system's temporary directory (`/private/tmp` on
/// macOS); see `rustykrab_agent`'s `RunTmp`. A confined shell
/// ([`Confinement`]) may not reach into one.
pub const RUN_TMP_PREFIX: &str = "rk-";

/// The hex digits after [`RUN_TMP_PREFIX`] in a run's temporary directory.
pub const RUN_TMP_ID_LEN: usize = 12;

/// The shared Cargo home for a builder's build cache `cache` in a
/// repository (`<data>/build-cache/<repo>/<builder>`, see
/// `rustykrab_control::workspace::Workspace::build_cache`):
/// `<data>/build-cache/cargo-home`, the same for every repository and
/// builder.
pub fn shared_cargo_home(cache: &std::path::Path) -> Option<std::path::PathBuf> {
    cache
        .parent()
        .and_then(std::path::Path::parent)
        .map(|root| root.join(SHARED_CARGO_HOME))
}

/// Entries in a Cargo home that would change how every later build runs
/// (configuration: `rustc-wrapper`, source replacement, runners, aliases;
/// `bin/`, where Cargo looks for `cargo-<name>` subcommands before the
/// PATH) or hold registry tokens. The shared home is for what Cargo
/// downloads; none of these belongs in it, since one run could otherwise
/// set them for every worker's builds.
pub const CARGO_HOME_CONTROL_FILES: [&str; 5] = [
    "config",
    "config.toml",
    "credentials",
    "credentials.toml",
    "bin",
];

/// The directories of the shared Cargo home a build writes: downloaded
/// registry indexes, crate archives and their extracted sources, and git
/// dependencies. Created by [`prepare_shared_cargo_home`]; the only
/// directories of that home a confined shell may write.
pub const CARGO_HOME_DOWNLOAD_DIRS: [&str; 2] = ["registry", "git"];

/// The files at the top of the shared Cargo home a build writes: Cargo's
/// package-cache locks and its global cache tracker (an SQLite database
/// with its journal files). The only files there a confined shell may write.
pub const CARGO_HOME_LOCK_FILES: [&str; 6] = [
    ".package-cache",
    ".package-cache-mutate",
    ".global-cache",
    ".global-cache-journal",
    ".global-cache-wal",
    ".global-cache-shm",
];

/// The shared Cargo home for `cache` ([`shared_cargo_home`]), created with
/// its [`CARGO_HOME_DOWNLOAD_DIRS`], and without any of
/// [`CARGO_HOME_CONTROL_FILES`] an earlier run left there. Called as each
/// run starts, for local and external workers alike.
pub fn prepare_shared_cargo_home(cache: &std::path::Path) -> Option<std::path::PathBuf> {
    let home = shared_cargo_home(cache)?;
    if let Err(e) = CARGO_HOME_DOWNLOAD_DIRS
        .iter()
        .try_for_each(|dir| std::fs::create_dir_all(home.join(dir)))
    {
        tracing::warn!(
            cargo_home = %home.display(),
            error = %e,
            "shared Cargo home unavailable"
        );
        return None;
    }
    for name in CARGO_HOME_CONTROL_FILES {
        let path = home.join(name);
        let Ok(meta) = path.symlink_metadata() else {
            continue;
        };
        let removed = if meta.is_dir() {
            std::fs::remove_dir_all(&path)
        } else {
            std::fs::remove_file(&path)
        };
        match removed {
            Ok(()) => tracing::warn!(
                path = %path.display(),
                "removed a Cargo configuration, credentials or bin entry from the shared Cargo home"
            ),
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "cannot clear the shared Cargo home");
                return None;
            }
        }
    }
    Some(home)
}

// Dropping wait_with_output kills only the direct child. A timed-out shell can
// otherwise leave its actual command running after the observation is gone.
#[cfg(unix)]
struct ProcessGroupGuard(u32);
#[cfg(unix)]
impl Drop for ProcessGroupGuard {
    fn drop(&mut self) {
        if let Ok(pid) = libc::pid_t::try_from(self.0) {
            if pid > 0 {
                // SAFETY: this is the positive child pid used to create a fresh
                // process group below; its negative identifies only that group.
                unsafe {
                    libc::kill(-pid, libc::SIGKILL);
                }
            }
        }
    }
}

/// Commands that are explicitly allowed for execution.
/// All other commands are rejected to prevent arbitrary command injection.
const ALLOWED_COMMANDS: &[&str] = &[
    "ls",
    "cd",
    "rm",
    "rg",
    "cat",
    "printf",
    "read",
    "[",
    ":",
    "od",
    "xxd",
    "head",
    "tail",
    "wc",
    "grep",
    "find",
    "sort",
    "uniq",
    "diff",
    // Text and data utilities: they read, compare or reshape what they are
    // given, as sort and cut already do.
    "paste",
    "comm",
    "join",
    "column",
    "nl",
    "fold",
    "rev",
    "tac",
    "seq",
    "expr",
    "bc",
    "strings",
    "hexdump",
    "base64",
    "shasum",
    "sha256sum",
    "md5",
    "md5sum",
    "cmp",
    "jq",
    "tree",
    "realpath",
    "zcat",
    "echo",
    "pwd",
    "whoami",
    "date",
    "env",
    "which",
    "file",
    "stat",
    "du",
    "df",
    "git",
    "cargo",
    "rustc",
    "python3",
    "python",
    "node",
    "npm",
    "npx",
    "make",
    "cmake",
    "gcc",
    "g++",
    "clang",
    "go",
    "java",
    "javac",
    "curl",
    "wget",
    "ssh",
    "scp",
    "rsync",
    "tar",
    "zip",
    "unzip",
    "gzip",
    "sed",
    "awk",
    "cut",
    "tr",
    "tee",
    "xargs",
    "mkdir",
    "rmdir",
    "cp",
    "mv",
    "touch",
    "chmod",
    "chown",
    "ln",
    "readlink",
    "basename",
    "dirname",
    "ps",
    "top",
    "htop",
    "kill",
    "pgrep",
    "lsof",
    "netstat",
    "ss",
    "docker",
    "docker-compose",
    "kubectl",
    "pip",
    "pip3",
    "poetry",
    "pipenv",
    "uv",
    "ruby",
    "gem",
    "bundle",
    "rake",
    "test",
    "true",
    "false",
    "sleep",
    "crontab",
    "at",
    // Network diagnostics (private-LAN use; safety enforced by skill guidance)
    "dig",
    "nslookup",
    "host",
    "ping",
    "ping6",
    "traceroute",
    "tracepath",
    "mtr",
    "nmap",
    "arp",
    "arp-scan",
    "ip",
    "ifconfig",
    "avahi-browse",
    "avahi-resolve",
    "openssl",
    "ssh-keyscan",
    "ssh-copy-id",
    "wakeonlan",
    "etherwake",
    "mosquitto_sub",
    "mosquitto_pub",
];

/// A built-in tool that executes shell commands and returns their output.
///
/// Security: Commands are validated against an allowlist to prevent
/// arbitrary command injection. Shell metacharacters in arguments are
/// handled safely by parsing the command into parts.
///
/// Commands run in the daemon's working directory, or in `dir` for an
/// instance a worker run built with [`ExecTool::in_dir`]: a `code` run's
/// isolated worktree (control-layer plan section 5). A [`confined`]
/// instance also runs them in the OS sandbox: see [`Confinement`].
///
/// [`confined`]: ExecTool::confined
pub struct ExecTool {
    dir: Option<std::path::PathBuf>,
    confinement: Option<Confinement>,
    /// Where Cargo builds go instead of the directory's own `target/`
    /// (see [`ExecTool::with_build_cache`]).
    build_cache: Option<std::path::PathBuf>,
    /// The Cargo home shared beside the build cache ([`shared_cargo_home`]).
    cargo_home: Option<std::path::PathBuf>,
}

/// macOS's sandbox runner (Seatbelt).
const SANDBOX: &str = "/usr/bin/sandbox-exec";

impl ExecTool {
    pub fn new() -> Self {
        Self {
            dir: None,
            confinement: None,
            build_cache: None,
            cargo_home: None,
        }
    }

    /// An `exec` whose commands run in `dir`.
    pub fn in_dir(dir: impl Into<std::path::PathBuf>) -> Self {
        Self {
            dir: Some(dir.into()),
            confinement: None,
            build_cache: None,
            cargo_home: None,
        }
    }

    /// Build Cargo projects into `cache/target` rather than a `target/`
    /// of the directory's own, and let the confined shell write there
    /// (and no other builder's directory). Every run of a repository is a
    /// fresh worktree; with a target directory each, every run compiles all
    /// dependencies from nothing (minutes, past this tool's time limit)
    /// and keeps gigabytes of output. One directory per repository and
    /// builder (`<data>/build-cache/<repo>/<builder>`) keeps the
    /// dependencies built across that builder's runs, and no other
    /// builder's build ever holds its lock. Only for an instance with a
    /// directory; the directory is created.
    ///
    /// Cargo also gets the persistent Cargo home shared by every worker
    /// ([`shared_cargo_home`], created here and writable in the sandbox)
    /// and at most [`CARGO_BUILD_JOBS`] jobs.
    pub fn with_build_cache(mut self, cache: impl Into<std::path::PathBuf>) -> Self {
        let cache = cache.into();
        if self.dir.is_some() && cache.is_absolute() {
            match std::fs::create_dir_all(cache.join("target")) {
                Ok(()) => self.build_cache = Some(cache.clone()),
                Err(e) => tracing::warn!(
                    cache = %cache.display(),
                    error = %e,
                    "build cache unavailable; builds use the directory's own target"
                ),
            }
        }
        if self.build_cache.is_some() {
            self.cargo_home = prepare_shared_cargo_home(&cache);
        }
        self
    }

    /// The directory and confinement a command runs under, when it runs in
    /// the OS sandbox: a confined instance on a platform that has one.
    fn sandbox(&self) -> Option<(&std::path::Path, &Confinement)> {
        let (Some(dir), Some(confinement)) = (&self.dir, &self.confinement) else {
            return None;
        };
        if !cfg!(target_os = "macos") || !std::path::Path::new(SANDBOX).exists() {
            static WARNED: std::sync::Once = std::sync::Once::new();
            WARNED.call_once(|| {
                tracing::warn!(
                    "no OS sandbox on this platform; a worker's exec runs unconfined, \
                     checked against the allowlist"
                )
            });
            return None;
        }
        Some((dir, confinement))
    }

    /// `sh`, inside the sandbox when this instance runs in one.
    fn shell(&self) -> tokio::process::Command {
        let Some((dir, confinement)) = self.sandbox() else {
            return tokio::process::Command::new("sh");
        };
        let mut sandboxed = tokio::process::Command::new(SANDBOX);
        // The build cache, and of the shared Cargo home only what a build
        // downloads into it and its lock files: not its configuration,
        // credentials or `bin/`, which every later build would use.
        let cache: Vec<&std::path::Path> = self.build_cache.iter().map(|p| p.as_path()).collect();
        sandboxed
            .arg("-p")
            .arg(confinement.profile(dir, &cache, self.cargo_home.as_deref()))
            .arg("sh");
        sandboxed
    }

    /// Run commands confined to `dir` (see [`Confinement`]), with `private`
    /// unreadable on top of [`Confinement::default_private`]. Only for an
    /// instance with a directory.
    pub fn confined(mut self, private: Vec<std::path::PathBuf>) -> Self {
        if self.dir.is_some() {
            let mut all = Confinement::default_private();
            all.extend(private);
            self.confinement = Some(Confinement {
                private: all,
                ports: Confinement::default_private_ports(),
            });
        }
        self
    }

    /// Also refuse the confined shell connections to these loopback ports
    /// (the daemon's own gateway), on top of
    /// [`Confinement::default_private_ports`].
    pub fn deny_local_ports(mut self, ports: Vec<u16>) -> Self {
        if let Some(confinement) = &mut self.confinement {
            confinement.ports.extend(ports);
        }
        self
    }
}

/// How a worker run's shell is confined, in the OS sandbox (macOS Seatbelt,
/// as Claude Code's own sandbox is): it writes only in its directory, the
/// git directory a worktree commits into, and temporary space, and it reads
/// everything but `private` (credential stores, the daemon's data, where
/// its secrets file is), its own directory always excepted. External runs'
/// temporary directories in temporary space ([`RUN_TMP_PREFIX`]) it may
/// neither read, write nor connect to a socket in. Ancestors of
/// the working and git directories expose metadata only for path resolution;
/// their contents remain private. Reads elsewhere
/// stay open, as tools need the system, their caches and the network.
/// Before this a worker's shell could write anywhere the daemon could and
/// read its secrets; a run in the 2026-10-14 reminder eval searched the
/// whole disk for a passphrase. Other platforms run unconfined, said once.
///
/// Inside the sandbox any program may run, as in Codex and Claude Code: the
/// sandbox is the boundary, not a list of program names (python3, node and
/// xargs run anything anyway). So it also closes what would let a command
/// act outside it later or elsewhere: writes to the git directory's hooks
/// and config (which git runs or reads outside the sandbox), connections to
/// Docker's socket (host access), and connections to loopback `ports` (the
/// daemon's API and the model server). Signals may target only processes in
/// the same sandbox invocation, so one command can manage its children but
/// cannot stop another worker's jobs or the daemon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Confinement {
    pub private: Vec<std::path::PathBuf>,
    pub ports: Vec<u16>,
}

impl Confinement {
    /// The daemon user's credential stores.
    /// Loopback ports a confined shell never reaches: Ollama's default.
    pub fn default_private_ports() -> Vec<u16> {
        vec![11434]
    }

    /// Docker's sockets, wherever the usual runtimes put them.
    fn docker_sockets() -> Vec<std::path::PathBuf> {
        let mut sockets = vec![std::path::PathBuf::from("/var/run/docker.sock")];
        if let Some(home) = std::env::var_os("HOME").map(std::path::PathBuf::from) {
            for rel in [
                ".docker/run/docker.sock",
                ".colima/default/docker.sock",
                ".colima/docker.sock",
                ".orbstack/run/docker.sock",
                ".rd/docker.sock",
            ] {
                sockets.push(home.join(rel));
            }
        }
        sockets
    }

    pub fn default_private() -> Vec<std::path::PathBuf> {
        let Some(home) = std::env::var_os("HOME").map(std::path::PathBuf::from) else {
            return Vec::new();
        };
        [
            ".ssh",
            ".aws",
            ".gnupg",
            ".netrc",
            ".docker",
            ".kube",
            ".config/gh",
            ".claude",
            ".claude.json",
            ".codex",
            "Library/Keychains",
        ]
        .iter()
        .map(|p| home.join(p))
        .collect()
    }

    /// The Seatbelt profile for commands run in `dir`. Paths are resolved,
    /// since the sandbox matches real paths (`/tmp` is `/private/tmp`);
    /// a private path that does not exist is left out.
    fn profile(
        &self,
        dir: &std::path::Path,
        build_dirs: &[&std::path::Path],
        cargo_home: Option<&std::path::Path>,
    ) -> String {
        let real = |p: &std::path::Path| std::fs::canonicalize(p).ok();
        let quoted = |p: &std::path::Path| {
            let s = p
                .to_string_lossy()
                .replace('\\', "\\\\")
                .replace('"', "\\\"");
            format!("(subpath \"{s}\")")
        };
        let dir = real(dir).unwrap_or_else(|| dir.to_path_buf());
        let mut own = vec![dir.clone()];
        // The git directories whose hooks and config git runs or reads later,
        // outside this sandbox: a repository's own `.git`, or the common one
        // a linked worktree commits into.
        let mut git_dirs = Vec::new();
        if dir.join(".git").is_dir() {
            git_dirs.push(dir.join(".git"));
        }
        if let Some(common) = git_common_dir(&dir) {
            own.push(common.clone());
            git_dirs.push(common);
        }
        // The build cache and the shared Cargo home's download directories
        // are the run's to read and write, as its own `target/` and Cargo
        // home would be.
        let build_dirs: Vec<std::path::PathBuf> = build_dirs
            .iter()
            .map(|d| real(d).unwrap_or_else(|| d.to_path_buf()))
            .collect();
        own.extend(build_dirs.iter().cloned());
        // Other builders' directories beside it
        // (`<data>/build-cache/<repo>/<builder>`) are not, wherever they
        // lie (temporary space is writable): a build there would take that
        // builder's lock and could change what its next build reuses.
        let cache_roots: Vec<std::path::PathBuf> = build_dirs
            .iter()
            .filter_map(|d| d.parent().and_then(std::path::Path::parent))
            .map(std::path::Path::to_path_buf)
            .collect();
        // The shared Cargo home is read; of it, only what a build downloads
        // and Cargo's lock files are written (see the rule further down).
        let cargo_home = cargo_home.map(|d| real(d).unwrap_or_else(|| d.to_path_buf()));
        let read_only: Vec<std::path::PathBuf> = cargo_home.iter().cloned().collect();
        // Git resolves every directory component even when the worktree itself
        // is readable. A private ancestor (the daemon's data directory) must
        // expose its directory metadata, but not its entries or file contents.
        let ancestors: std::collections::BTreeSet<String> = own
            .iter()
            .chain(&read_only)
            .flat_map(|p| p.ancestors().skip(1))
            .map(|p| {
                let path = p
                    .to_string_lossy()
                    .replace('\\', "\\\\")
                    .replace('"', "\\\"");
                format!("(literal \"{path}\")")
            })
            .collect();
        let readable: Vec<String> = own.iter().chain(&read_only).map(|p| quoted(p)).collect();
        let own: Vec<String> = own.iter().map(|p| quoted(p)).collect();
        let private: Vec<String> = self
            .private
            .iter()
            .filter_map(|p| real(p))
            .map(|p| quoted(&p))
            .collect();
        let mut profile = format!(
            "(version 1)(allow default)\
             (deny signal)(allow signal (target same-sandbox))\
             (deny file-write*)\
             (allow file-write* {} (subpath \"/private/tmp\") \
             (subpath \"/private/var/folders\") (subpath \"/dev\"))",
            own.join(" ")
        );
        if !private.is_empty() {
            profile.push_str(&format!("(deny file-read* {})", private.join(" ")));
            profile.push_str(&format!("(allow file-read* {})", readable.join(" ")));
            if !ancestors.is_empty() {
                profile.push_str(&format!(
                    "(allow file-read-metadata {})",
                    ancestors.into_iter().collect::<Vec<_>>().join(" ")
                ));
            }
        }
        // Later rules win, so these hold inside the writable git directory.
        let git: Vec<String> = git_dirs
            .iter()
            .flat_map(|g| {
                let literal = |p: std::path::PathBuf| {
                    format!("(literal \"{}\")", p.to_string_lossy().replace('"', "\\\""))
                };
                [quoted(&g.join("hooks")), literal(g.join("config"))]
            })
            .collect();
        if !git.is_empty() {
            profile.push_str(&format!("(deny file-write* {})", git.join(" ")));
        }
        if !cache_roots.is_empty() {
            let roots: Vec<String> = cache_roots.iter().map(|p| quoted(p)).collect();
            let mine: Vec<String> = build_dirs.iter().map(|p| quoted(p)).collect();
            profile.push_str(&format!(
                "(deny file-write* {})(allow file-write* {})",
                roots.join(" "),
                mine.join(" ")
            ));
        }
        // The shared Cargo home, wherever it lies (temporary space is
        // writable): its configuration, credentials and `bin/` would change
        // every later build of every worker, so only its download
        // directories and lock files are writable.
        if let Some(home) = &cargo_home {
            let literal = |p: std::path::PathBuf| {
                format!(
                    "(literal \"{}\")",
                    p.to_string_lossy()
                        .replace('\\', "\\\\")
                        .replace('"', "\\\"")
                )
            };
            let allowed: Vec<String> = CARGO_HOME_DOWNLOAD_DIRS
                .iter()
                .map(|d| quoted(&home.join(d)))
                .chain(CARGO_HOME_LOCK_FILES.iter().map(|f| literal(home.join(f))))
                .collect();
            profile.push_str(&format!(
                "(deny file-write* {})(allow file-write* {})",
                quoted(home),
                allowed.join(" ")
            ));
        }
        // External runs' temporary directories (`/private/tmp/rk-<12 hex>`,
        // their `TMPDIR`) lie in temporary space but are not this shell's:
        // no reading, writing or connecting to a socket in one. Seatbelt's
        // regular expressions have no `{n}`, so the digits are spelled out.
        let run_tmp = format!(
            "^/private/tmp/{RUN_TMP_PREFIX}{}",
            "[0-9a-f]".repeat(RUN_TMP_ID_LEN)
        );
        profile.push_str(&format!(
            "(deny file-read* file-write* (regex #\"{run_tmp}(/|$)\"))\
             (deny network-outbound (remote unix-socket (path-regex #\"{run_tmp}/\")))"
        ));
        let sockets: Vec<String> = Self::docker_sockets()
            .iter()
            .filter_map(|p| real(p))
            .map(|p| {
                format!(
                    "(remote unix-socket (path-literal \"{}\"))",
                    p.to_string_lossy().replace('"', "\\\"")
                )
            })
            .collect();
        if !sockets.is_empty() {
            profile.push_str(&format!("(deny network-outbound {})", sockets.join(" ")));
        }
        if !self.ports.is_empty() {
            let ports: Vec<String> = self
                .ports
                .iter()
                .map(|p| format!("(remote tcp \"localhost:{p}\")"))
                .collect();
            profile.push_str(&format!("(deny network-outbound {})", ports.join(" ")));
        }
        profile
    }
}

/// The git directory a worktree at `dir` commits into (its repository's
/// common `.git`), when `dir` is a linked worktree.
fn git_common_dir(dir: &std::path::Path) -> Option<std::path::PathBuf> {
    let pointer = std::fs::read_to_string(dir.join(".git")).ok()?;
    let gitdir = std::path::PathBuf::from(pointer.strip_prefix("gitdir:")?.trim());
    let gitdir = if gitdir.is_absolute() {
        gitdir
    } else {
        dir.join(gitdir)
    };
    // `<repo>/.git/worktrees/<name>`: the common directory is two up.
    let common = gitdir.parent()?.parent()?;
    std::fs::canonicalize(common).ok()
}

impl Default for ExecTool {
    fn default() -> Self {
        Self::new()
    }
}

/// Rustup's shims find the installed toolchains here, independently of the
/// command's isolated HOME. Resolve relative host configuration before binding
/// the command to a different worktree. Cargo configuration/credentials and
/// other ambient environment variables are deliberately not forwarded.
/// External workers get the same location beside the shared Cargo home.
pub fn rustup_home() -> Option<std::path::PathBuf> {
    let home = std::env::var_os("RUSTUP_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".rustup"))
        })?;
    if home.is_absolute() {
        Some(home)
    } else {
        std::env::current_dir().ok().map(|cwd| cwd.join(home))
    }
}

/// The `bin/` of the Cargo home the daemon's user has (`$CARGO_HOME`, else
/// `~/.cargo`), when it exists: where Rustup's proxies and installed Cargo
/// subcommands live. A worker given the shared Cargo home keeps finding
/// them on its PATH. Nothing else of that home (its configuration or
/// credentials) is used.
pub fn host_cargo_bin() -> Option<std::path::PathBuf> {
    let home = std::env::var_os("CARGO_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".cargo")))?;
    let bin = home.join("bin");
    (bin.is_absolute() && bin.is_dir()).then_some(bin)
}

/// `path` with [`host_cargo_bin`] appended when it exists and is not on it
/// already: a shell or worker given the shared Cargo home keeps finding
/// Rustup's proxies and installed Cargo subcommands.
pub fn path_with_host_cargo_bin(path: &std::ffi::OsStr) -> std::ffi::OsString {
    let mut dirs: Vec<std::path::PathBuf> = std::env::split_paths(path).collect();
    match host_cargo_bin() {
        Some(bin) if !dirs.contains(&bin) => {
            dirs.push(bin);
            std::env::join_paths(dirs).unwrap_or_else(|_| path.to_os_string())
        }
        _ => path.to_os_string(),
    }
}

fn truncate_output(s: String) -> String {
    if s.len() > MAX_OUTPUT_BYTES {
        // Use floor_char_boundary to avoid panicking when the truncation
        // point falls within a multi-byte UTF-8 character.
        let end = s.floor_char_boundary(MAX_OUTPUT_BYTES);
        let mut truncated = s[..end].to_string();
        truncated.push_str("\n... [truncated]");
        truncated
    } else {
        s
    }
}

/// Refuse forced removal — `rm` with `-f` or `--force`, alone or in a flag
/// group such as `-rf` — wherever a command runs it: in any segment, inside
/// a substitution, behind a wrapper (`sudo`, `env`, `command`, `xargs` …)
/// or after `find -exec`. Codex flags the same; an unattended run has no
/// one to approve it, so it is refused with what to do instead. The sandbox
/// still lets a command delete its worktree's uncommitted work and the
/// repository's shared `.git`. An interpreter (`python3 -c`) can still
/// delete; this catches the shell's way, which is the one models reach for.
fn refuse_forced_removal(command: &str) -> std::result::Result<(), String> {
    forced_removal_nested(command, 0)
}

fn forced_removal_nested(command: &str, depth: usize) -> std::result::Result<(), String> {
    if depth > MAX_SUBSTITUTION_DEPTH {
        return Err("command substitutions nest too deeply".into());
    }
    let (command, inner) = substitutions(command)?;
    for body in &inner {
        forced_removal_nested(body, depth + 1)?;
    }
    for segment in command_segments(&command)? {
        if runs_forced_rm(segment) {
            return Err(
                "forced removal (rm -f, rm -rf, rm --force) is refused: remove the \
                        files you mean without -f, or use git rm or git clean -n first"
                    .into(),
            );
        }
    }
    Ok(())
}

/// Refuse a command that runs work in the background with a control `&`
/// (`sleep 30 &`, `nohup cargo test > log & disown`), wherever it does: in
/// any segment, inside a substitution, or in the script of `sh -c`,
/// `bash -c` or `eval`. Every process the command starts is killed when
/// the call returns, so the work can never survive it, and a backgrounded
/// process holding the output open makes the call wait for its whole time
/// limit anyway. `&&`, redirections (`2>&1`, `&>file`), a quoted `&` and
/// the `&` of arithmetic (`$((a & b))`, `(( x & 1 ))`) or of a parameter
/// expansion (`${v//&/and}`) are not backgrounding. A `wait` does not
/// excuse it: `disown`, a subshell or a later `&` all escape the wait.
fn refuse_background(command: &str) -> std::result::Result<(), String> {
    background_nested(command, 0)
}

const BACKGROUND_REFUSAL: &str =
    "running work in the background (a control `&`, as with `nohup … &` or `disown`) is \
     refused: every process this command starts is killed when the call returns, so \
     background work can never survive it and its output is lost. Run the command in the \
     foreground and raise timeout_secs (up to 600) if it needs longer; a cargo build or test \
     keeps the steps it finished, so running it again continues where it stopped.";

fn background_nested(command: &str, depth: usize) -> std::result::Result<(), String> {
    if depth > MAX_SUBSTITUTION_DEPTH {
        return Err("command substitutions nest too deeply".into());
    }
    let (command, inner) = substitutions(command)?;
    for body in &inner {
        background_nested(body, depth + 1)?;
    }
    let masked = mask_expansions(&command);
    let (segments, background) = scan_segments(&masked)?;
    if background {
        return Err(BACKGROUND_REFUSAL.into());
    }
    for segment in segments {
        for script in shell_scripts(segment) {
            background_nested(&script, depth + 1)?;
        }
    }
    Ok(())
}

/// `line` with every `&` inside arithmetic (`$((…))`, a `((…))` command)
/// and parameter expansions (`${…}`) replaced by a space, so the segment
/// scan sees only the shell's control operators. Quoted text is left as it
/// is (the scan already skips it); the length never changes.
fn mask_expansions(line: &str) -> String {
    let chars: Vec<char> = line.chars().collect();
    let mut out = chars.clone();
    let mut quote: Option<char> = None;
    let mut i = 0;
    // The end (exclusive) of the region at `from` whose `close` balances
    // its `open`, starting at depth `depth`.
    let close_of = |from: usize, open: char, close: char, mut depth: usize| {
        let mut k = from;
        while k < chars.len() {
            if chars[k] == open {
                depth += 1;
            } else if chars[k] == close {
                depth -= 1;
                if depth == 0 {
                    return k + 1;
                }
            }
            k += 1;
        }
        chars.len()
    };
    while i < chars.len() {
        let c = chars[i];
        if quote == Some('\'') {
            if c == '\'' {
                quote = None;
            }
            i += 1;
            continue;
        }
        if c == '\\' {
            i += 2;
            continue;
        }
        let at = |k: usize| chars.get(k).copied();
        let region = if c == '$' && at(i + 1) == Some('(') && at(i + 2) == Some('(') {
            // Arithmetic only when the inner `(` closes right before the
            // outer one (see [`arithmetic_body`]).
            let inner = close_of(i + 3, '(', ')', 1);
            (at(inner) == Some(')')).then_some(inner + 1)
        } else if c == '$' && at(i + 1) == Some('{') {
            Some(close_of(i + 2, '{', '}', 1))
        } else if quote.is_none()
            && c == '('
            && at(i + 1) == Some('(')
            && opens_command(&chars[..i])
        {
            // A `((…))` command only when the inner `(` closes right
            // before the outer one: `((sleep 2 &) ; echo ran)` is a
            // subshell holding a subshell, and its `&` is a real job.
            let inner = close_of(i + 2, '(', ')', 1);
            (at(inner) == Some(')')).then_some(inner + 1)
        } else {
            None
        };
        if let Some(end) = region {
            for slot in &mut out[i..end] {
                if *slot == '&' {
                    *slot = ' ';
                }
            }
            i = end;
            continue;
        }
        match (quote, c) {
            (None, '\'' | '"') => quote = Some(c),
            (Some('"'), '"') => quote = None,
            _ => {}
        }
        i += 1;
    }
    out.into_iter().collect()
}

/// Whether a command starts after `before`: at the start of the line,
/// after a control operator, `(`, `!` or a newline, or after a keyword
/// that a command follows (`if (( x & 1 ))`, `while (( … ))`).
fn opens_command(before: &[char]) -> bool {
    let text: String = before.iter().collect();
    let text = text.trim_end_matches([' ', '\t']);
    match text.chars().last() {
        None => true,
        Some(';' | '&' | '|' | '(' | '!' | '\n') => true,
        Some(_) => {
            let word = text
                .rsplit(|c: char| c.is_whitespace() || matches!(c, ';' | '&' | '|' | '('))
                .next()
                .unwrap_or_default();
            matches!(
                word,
                "if" | "elif" | "while" | "until" | "then" | "do" | "else" | "time"
            )
        }
    }
}

/// Whether `$(` + `body` + `)` is an arithmetic expansion `$((…))`: `body`
/// opens with `(` and that parenthesis closes only at its very end.
/// Otherwise (`$((sleep 2 &) ; echo ran)`) it is a command substitution
/// whose command starts with a subshell, as the shell reads it.
fn arithmetic_body(body: &str) -> bool {
    let count = body.chars().count();
    let mut depth = 0usize;
    for (k, c) in body.chars().enumerate() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return k + 1 == count;
                }
            }
            _ if k == 0 => return false,
            _ => {}
        }
    }
    false
}

/// A segment's words as the shell splits them: on unquoted blanks, with
/// quotes removed and backslash escapes resolved.
fn shell_words(segment: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut quote: Option<char> = None;
    let mut chars = segment.chars();
    while let Some(c) = chars.next() {
        match quote {
            Some(q) if c == q => quote = None,
            Some('"') if c == '\\' => {
                if let Some(n) = chars.next() {
                    word.push(n);
                }
            }
            Some(_) => word.push(c),
            None if c == '\'' || c == '"' => {
                quote = Some(c);
                in_word = true;
            }
            None if c == '\\' => {
                if let Some(n) = chars.next() {
                    word.push(n);
                }
                in_word = true;
            }
            None if c.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut word));
                    in_word = false;
                }
            }
            None => {
                word.push(c);
                in_word = true;
            }
        }
    }
    if in_word {
        words.push(word);
    }
    words
}

/// Whether option `opt` of wrapper `wrapper` takes the next word as its
/// value (`timeout -s KILL`, `env -u NAME`, `sudo -u user`).
fn wrapper_option_takes_value(wrapper: &str, opt: &str) -> bool {
    match wrapper {
        "timeout" => matches!(opt, "-s" | "-k" | "--signal" | "--kill-after"),
        "env" => matches!(opt, "-u" | "-C" | "--unset" | "--chdir"),
        "sudo" | "doas" => matches!(
            opt,
            "-u" | "-g" | "-h" | "-p" | "-C" | "-D" | "-r" | "-t" | "-U" | "--user" | "--group"
        ),
        "nice" => matches!(opt, "-n" | "--adjustment"),
        "stdbuf" => matches!(opt, "-i" | "-o" | "-e"),
        "xargs" => matches!(opt, "-I" | "-n" | "-P" | "-L" | "-d" | "-E" | "-s" | "-a"),
        _ => false,
    }
}

/// The index in `words` of the program a segment runs, past assignments,
/// redirects and their targets, the shell's own words, and wrappers with
/// their options and values.
fn program_index(words: &[String]) -> Option<usize> {
    let mut i = 0;
    let mut target_next = false;
    while i < words.len() {
        let token = words[i]
            .trim_start_matches(['(', '{'])
            .trim_end_matches([')', '}']);
        if std::mem::take(&mut target_next) {
            i += 1;
            continue;
        }
        if let Some(needs_target) = redirect(token) {
            target_next = needs_target;
            i += 1;
            continue;
        }
        if token.is_empty()
            || (token.contains('=') && !token.starts_with('='))
            || SHELL_WORDS.contains(&token)
        {
            i += 1;
            continue;
        }
        let name = token.rsplit('/').next().unwrap_or(token);
        if WRAPPERS.contains(&name) {
            i += 1;
            while i < words.len() {
                let word = words[i].as_str();
                if wrapper_option_takes_value(name, word) {
                    i += 2;
                } else if word.starts_with('-')
                    || word.contains('=')
                    || (matches!(name, "timeout" | "nice")
                        && word.chars().next().is_some_and(|c| c.is_ascii_digit()))
                {
                    i += 1;
                } else {
                    break;
                }
            }
            continue;
        }
        return Some(i);
    }
    None
}

/// The name of the program a segment runs (see [`program_index`]).
fn segment_program(segment: &str) -> Option<String> {
    let words = shell_words(segment);
    let i = program_index(&words)?;
    let token = words[i]
        .trim_start_matches(['(', '{'])
        .trim_end_matches([')', '}']);
    Some(token.rsplit('/').next().unwrap_or(token).to_string())
}

/// The scripts a segment hands to another shell to run: the argument of
/// `sh -c` (or `bash`, `zsh`, `dash`, `ksh`, with `-c` alone or in a flag
/// group such as `-ec`), or the words of `eval`.
fn shell_scripts(segment: &str) -> Vec<String> {
    let words = shell_words(segment);
    let Some(i) = program_index(&words) else {
        return Vec::new();
    };
    let token = words[i]
        .trim_start_matches(['(', '{'])
        .trim_end_matches([')', '}']);
    let name = token.rsplit('/').next().unwrap_or(token);
    match name {
        "eval" => vec![words[i + 1..].join(" ")],
        "sh" | "bash" | "zsh" | "dash" | "ksh" => words[i + 1..]
            .iter()
            .position(|w| w.starts_with('-') && !w.starts_with("--") && w.contains('c'))
            .and_then(|k| words.get(i + 2 + k))
            .cloned()
            .into_iter()
            .collect(),
        _ => Vec::new(),
    }
}

/// Whether any command the line runs, in any segment, substitution or
/// `sh -c`/`eval` script, is Cargo (past assignments, redirects, the
/// shell's own words and wrappers such as `env` or `nohup`). A line that
/// cannot be parsed is not one.
fn runs_cargo(command: &str) -> bool {
    fn nested(command: &str, depth: usize) -> bool {
        if depth > MAX_SUBSTITUTION_DEPTH {
            return false;
        }
        let Ok((command, inner)) = substitutions(command) else {
            return false;
        };
        inner.iter().any(|body| nested(body, depth + 1))
            || command_segments(&command)
                .unwrap_or_default()
                .into_iter()
                .any(|segment| {
                    segment_program(segment).as_deref() == Some("cargo")
                        || shell_scripts(segment)
                            .iter()
                            .any(|script| nested(script, depth + 1))
                })
    }
    nested(command, 0)
}

/// The time limit a call gets: what it asks for (or the default), at most
/// [`MAX_TIMEOUT_SECS`], and at least [`MIN_CARGO_TIMEOUT_SECS`] when the
/// command runs Cargo.
fn resolve_timeout(requested: Option<u64>, cargo: bool) -> u64 {
    let secs = requested
        .unwrap_or(DEFAULT_TIMEOUT_SECS)
        .min(MAX_TIMEOUT_SECS);
    if cargo {
        secs.max(MIN_CARGO_TIMEOUT_SECS)
    } else {
        secs
    }
}

/// How [`timeout_message`] begins for a Cargo command stopped at a limit.
const CARGO_TIMEOUT_KEEPS: &str = "and was stopped. Cargo keeps the build steps it finished";

/// Whether `message`, an `exec` timeout's text, says a Cargo command was
/// stopped at the longest limit a call may ask for ([`MAX_TIMEOUT_SECS`]):
/// its build steps are kept, and running the same command again continues
/// it, which is what the message tells the model to do. A worker's loop
/// rule does not count such a timeout as a repeated failure. A Cargo
/// command stopped earlier, at a limit it chose, is not one.
pub fn is_resumable_build_timeout(message: &str) -> bool {
    message.starts_with(&format!(
        "command timed out after {MAX_TIMEOUT_SECS}s {CARGO_TIMEOUT_KEEPS}"
    ))
}

/// What a timed-out call tells the model: that its processes were stopped,
/// that backgrounding cannot outlive the call, and for Cargo, that the work
/// already done is kept and how to continue it.
fn timeout_message(secs: u64, cargo: bool, shared_cache: bool) -> String {
    let background = "Background processes (`&`, `nohup`, `disown`) are killed when the call \
                      returns, so backgrounding cannot make a command outlive it.";
    if cargo {
        let kept = if shared_cache {
            "in the shared build cache"
        } else {
            "in the target directory"
        };
        format!(
            "command timed out after {secs}s {CARGO_TIMEOUT_KEEPS} {kept}, so nothing done so \
             far is lost: run the same command again in the foreground with \
             timeout_secs={MAX_TIMEOUT_SECS} and it continues where it stopped. {background}"
        )
    } else {
        format!(
            "command timed out after {secs}s and was stopped. {background} Run it in the \
             foreground with a larger timeout_secs (up to {MAX_TIMEOUT_SECS}), or narrow the \
             work."
        )
    }
}

/// Programs that run the command that follows them.
const WRAPPERS: &[&str] = &[
    "sudo", "doas", "env", "command", "exec", "nice", "nohup", "time", "timeout", "stdbuf", "xargs",
];

/// Whether a segment's command — past assignments, redirects, the shell's
/// own words and wrappers, or in a `find -exec` — is `rm` with a force flag.
fn runs_forced_rm(segment: &str) -> bool {
    let unquote = |t: &str| t.trim_matches(|c| c == '"' || c == '\'').to_string();
    let tokens: Vec<String> = segment.split_whitespace().map(unquote).collect();
    let forced = |args: &[String]| {
        args.iter()
            .take_while(|a| !matches!(a.as_str(), "--" | ";" | "\\;" | "+"))
            .any(|a| {
                a == "--force" || (a.starts_with('-') && !a.starts_with("--") && a.contains('f'))
            })
    };
    let mut i = 0;
    let mut target_next = false;
    let mut in_command = false;
    while i < tokens.len() {
        let token = tokens[i].trim_start_matches('(').trim_end_matches(')');
        if std::mem::take(&mut target_next) {
            i += 1;
            continue;
        }
        if !in_command {
            if let Some(needs_target) = redirect(token) {
                target_next = needs_target;
                i += 1;
                continue;
            }
            if (token.contains('=') && !token.starts_with('=')) || SHELL_WORDS.contains(&token) {
                i += 1;
                continue;
            }
        }
        let name = token.rsplit('/').next().unwrap_or(token);
        if name == "rm" {
            return forced(&tokens[i + 1..]);
        }
        if WRAPPERS.contains(&name) {
            // The wrapper's own flags and settings come before its command.
            in_command = true;
            i += 1;
            while i < tokens.len()
                && (tokens[i].starts_with('-')
                    || tokens[i].contains('=')
                    || (matches!(name, "timeout" | "nice")
                        && tokens[i].chars().next().is_some_and(|c| c.is_ascii_digit())))
            {
                i += 1;
            }
            continue;
        }
        if name == "find" {
            return tokens[i + 1..].iter().enumerate().any(|(k, t)| {
                matches!(t.as_str(), "-exec" | "-execdir" | "-ok" | "-okdir")
                    && tokens
                        .get(i + 2 + k)
                        .is_some_and(|n| n.rsplit('/').next() == Some("rm"))
                    && forced(&tokens[i + 3 + k..])
            });
        }
        return false;
    }
    false
}

/// Split out the commands a line runs inside substitutions — `$(…)`,
/// backticks, `<(…)` and `>(…)` — returning the line with each replaced by
/// [`SUBSTITUTION`] and the commands found, so both can be checked.
///
/// The shell runs a substitution wherever it expands text: unquoted, inside
/// double quotes, and in the body of a here-document whose delimiter is
/// unquoted. Single quotes and a quoted delimiter's body are literal, so a
/// `$(` there is text. `$((…))` is arithmetic and runs nothing, unless it
/// holds a substitution, which is refused rather than parsed.
fn substitutions(command: &str) -> std::result::Result<(String, Vec<String>), String> {
    let chars: Vec<(usize, char)> = command.char_indices().collect();
    let mut outer = String::with_capacity(command.len());
    let mut inner = Vec::new();
    let mut quote: Option<char> = None;
    // Here-document delimiters opened on the current line, in order.
    let mut heredocs: Vec<(String, bool, bool)> = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i].1;
        if quote == Some('\'') {
            outer.push(c);
            if c == '\'' {
                quote = None;
            }
            i += 1;
            continue;
        }
        if c == '\\' {
            outer.extend(chars[i..(i + 2).min(chars.len())].iter().map(|&(_, c)| c));
            i += 2;
            continue;
        }
        if quote == Some('"') {
            if c == '"' {
                quote = None;
            } else if let Some(after) = expand_at(&chars, i, &mut outer, &mut inner)? {
                i = after;
                continue;
            }
            outer.push(c);
            i += 1;
            continue;
        }
        let next = chars.get(i + 1).map(|&(_, c)| c);
        match c {
            '\'' | '"' => quote = Some(c),
            '<' if next == Some('<') && chars.get(i + 2).map(|&(_, c)| c) != Some('<') => {
                let (word, strip_tabs, quoted, after) = heredoc_delimiter(&chars, i + 2)?;
                heredocs.push((word, strip_tabs, quoted));
                outer.extend(chars[i..after].iter().map(|&(_, c)| c));
                i = after;
                continue;
            }
            '<' | '>' if next == Some('(') => {
                let (body, after) = substitution_body(&chars, i + 2)?;
                inner.push(body);
                outer.push_str(SUBSTITUTION);
                i = after;
                continue;
            }
            '\n' if !heredocs.is_empty() => {
                outer.push('\n');
                i += 1;
                for (word, strip_tabs, literal) in std::mem::take(&mut heredocs) {
                    let (body_end, after) = heredoc_body(&chars, i, &word, strip_tabs);
                    let body = &chars[i..body_end];
                    if literal {
                        outer.extend(body.iter().map(|&(_, c)| c));
                    } else {
                        // An unquoted body expands `$(…)` and backticks, as
                        // double quotes do, but its quotes are text.
                        let mut k = 0;
                        while k < body.len() {
                            if body[k].1 == '\\' {
                                outer.extend(
                                    body[k..(k + 2).min(body.len())].iter().map(|&(_, c)| c),
                                );
                                k += 2;
                            } else if let Some(after) = expand_at(body, k, &mut outer, &mut inner)?
                            {
                                k = after;
                            } else {
                                outer.push(body[k].1);
                                k += 1;
                            }
                        }
                    }
                    outer.extend(chars[body_end..after].iter().map(|&(_, c)| c));
                    i = after;
                }
                continue;
            }
            _ => {
                if let Some(after) = expand_at(&chars, i, &mut outer, &mut inner)? {
                    i = after;
                    continue;
                }
            }
        }
        outer.push(c);
        i += 1;
    }
    Ok((outer, inner))
}

/// Take the substitution or arithmetic that starts at `chars[i]`, if one
/// does: push its command (if any) to `inner`, write what stands in for it
/// to `outer`, and return the index after it.
fn expand_at(
    chars: &[(usize, char)],
    i: usize,
    outer: &mut String,
    inner: &mut Vec<String>,
) -> std::result::Result<Option<usize>, String> {
    let at = |k: usize| chars.get(k).map(|&(_, c)| c);
    match (at(i), at(i + 1), at(i + 2)) {
        (Some('$'), Some('('), Some('(')) => {
            let (body, after) = substitution_body(chars, i + 2)?;
            if !arithmetic_body(&body) {
                // `$((sleep 2 &) ; echo ran)`: a command substitution.
                inner.push(body);
                outer.push_str(SUBSTITUTION);
                return Ok(Some(after));
            }
            if body.contains("$(") || body.contains('`') {
                return Err("an arithmetic expansion holds a command substitution; \
                            run that command on its own first"
                    .into());
            }
            outer.extend(chars[i..after].iter().map(|&(_, c)| c));
            Ok(Some(after))
        }
        (Some('$'), Some('('), _) => {
            let (body, after) = substitution_body(chars, i + 2)?;
            inner.push(body);
            outer.push_str(SUBSTITUTION);
            Ok(Some(after))
        }
        (Some('`'), _, _) => {
            let mut body = String::new();
            let mut k = i + 1;
            loop {
                match at(k) {
                    None => return Err("command contains an unterminated backtick".into()),
                    Some('`') => break,
                    Some('\\') if matches!(at(k + 1), Some('`' | '\\' | '$')) => {
                        body.extend(at(k + 1));
                        k += 2;
                    }
                    Some(c) => {
                        body.push(c);
                        k += 1;
                    }
                }
            }
            inner.push(body);
            outer.push_str(SUBSTITUTION);
            Ok(Some(k + 1))
        }
        _ => Ok(None),
    }
}

/// The text between an opening parenthesis (just before `chars[start]`)
/// and its match, and the index after the match. Quoted parentheses do not
/// count.
fn substitution_body(
    chars: &[(usize, char)],
    start: usize,
) -> std::result::Result<(String, usize), String> {
    let mut depth = 1;
    let mut quote: Option<char> = None;
    let mut k = start;
    while let Some(&(_, c)) = chars.get(k) {
        match quote {
            Some('\'') if c == '\'' => quote = None,
            Some('\'') => {}
            Some(_) => match c {
                '"' => quote = None,
                '\\' => k += 1,
                _ => {}
            },
            None => match c {
                '\'' | '"' => quote = Some(c),
                '\\' => k += 1,
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        let body = chars[start..k].iter().map(|&(_, c)| c).collect();
                        return Ok((body, k + 1));
                    }
                }
                _ => {}
            },
        }
        k += 1;
    }
    Err("command contains an unterminated substitution".into())
}

/// Where a here-document's body ends (the start of its delimiter line) and
/// the index after the delimiter line, for a body starting at `chars[start]`.
/// An unterminated body runs to the end.
fn heredoc_body(
    chars: &[(usize, char)],
    start: usize,
    word: &str,
    strip_tabs: bool,
) -> (usize, usize) {
    let mut line_start = start;
    while line_start < chars.len() {
        let line_end = (line_start..chars.len())
            .find(|&k| chars[k].1 == '\n')
            .unwrap_or(chars.len());
        let line: String = chars[line_start..line_end]
            .iter()
            .map(|&(_, c)| c)
            .collect();
        let line = if strip_tabs {
            line.trim_start_matches('\t')
        } else {
            line.as_str()
        };
        if line == word {
            return (line_start, (line_end + 1).min(chars.len()));
        }
        line_start = line_end + 1;
    }
    (chars.len(), chars.len())
}

/// Split a shell command into the commands it runs, on the control
/// operators and newlines that occur outside quotes.
///
/// This is intentionally narrower than a complete shell parser: command
/// execution still belongs to `sh -c`, while this pass only identifies the
/// command boundaries that must be checked against the allowlist. Treating
/// quoted operators as boundaries would turn a grep pattern such as
/// `'GMAIL|EMAIL|PASSWORD'` into three apparent commands. An `&` inside a
/// redirect (`2>&1`, `>&2`, `&>file`) is part of that redirect, not a
/// boundary, and a here-document's body (`<<EOF` … `EOF`) is the input of
/// its command, not commands of its own.
fn command_segments(command: &str) -> std::result::Result<Vec<&str>, String> {
    scan_segments(command).map(|(segments, _)| segments)
}

/// [`command_segments`], and whether any boundary was a control `&` that
/// runs the command before it in the background: a lone `&`, not either
/// half of `&&`, not part of a redirect, and not bash's `|&`.
fn scan_segments(command: &str) -> std::result::Result<(Vec<&str>, bool), String> {
    #[derive(Clone, Copy)]
    enum Quote {
        Single,
        Double,
    }
    let chars: Vec<(usize, char)> = command.char_indices().collect();
    let mut segments = Vec::new();
    let mut background = false;
    let mut start = 0;
    let mut quote = None;
    let mut escaped = false;
    // Here-document delimiters opened on the current line, in order.
    let mut heredocs: Vec<(String, bool)> = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let (index, ch) = chars[i];
        let prev = i.checked_sub(1).map(|p| chars[p].1);
        let next = chars.get(i + 1).map(|&(_, c)| c);
        if escaped {
            escaped = false;
            i += 1;
            continue;
        }
        match quote {
            Some(Quote::Single) => {
                if ch == '\'' {
                    quote = None;
                }
            }
            Some(Quote::Double) => match ch {
                '"' => quote = None,
                '\\' => escaped = true,
                _ => {}
            },
            None => match ch {
                '\'' => quote = Some(Quote::Single),
                '"' => quote = Some(Quote::Double),
                '\\' => escaped = true,
                '&' if matches!(prev, Some('>' | '<')) || next == Some('>') => {}
                '<' if next == Some('<') && chars.get(i + 2).map(|&(_, c)| c) != Some('<') => {
                    let (delimiter, strip_tabs, _, after) = heredoc_delimiter(&chars, i + 2)?;
                    heredocs.push((delimiter, strip_tabs));
                    i = after;
                    continue;
                }
                '|' | ';' | '&' => {
                    if ch == '&' && !matches!(prev, Some('&' | '|' | ';')) && next != Some('&') {
                        background = true;
                    }
                    segments.push(&command[start..index]);
                    start = index + ch.len_utf8();
                }
                '\n' => {
                    segments.push(&command[start..index]);
                    start = index + 1;
                    // The lines after a here-document's line are its body,
                    // up to the line that is exactly its delimiter.
                    for (delimiter, strip_tabs) in std::mem::take(&mut heredocs) {
                        loop {
                            let Some(&(line_start, _)) = chars.get(i + 1) else {
                                // Unterminated: the rest is input, not commands.
                                return Ok((segments, background));
                            };
                            let line_end = command[line_start..]
                                .find('\n')
                                .map_or(command.len(), |n| line_start + n);
                            let line = &command[line_start..line_end];
                            let line = if strip_tabs {
                                line.trim_start_matches('\t')
                            } else {
                                line
                            };
                            while chars.get(i + 1).is_some_and(|&(at, _)| at <= line_end) {
                                i += 1;
                            }
                            if line == delimiter {
                                break;
                            }
                        }
                        start = chars.get(i + 1).map_or(command.len(), |&(at, _)| at);
                    }
                }
                _ => {}
            },
        }
        i += 1;
    }
    if escaped {
        return Err("command ends with an incomplete escape".into());
    }
    if quote.is_some() {
        return Err("command contains an unterminated quote".into());
    }
    segments.push(&command[start..]);
    Ok((segments, background))
}

/// Read a here-document's delimiter word starting at `chars[at]` (just past
/// `<<`): an optional `-` (strip leading tabs), optional blanks, then the
/// word, bare or quoted. Returns the word, the tab flag, whether any of the
/// word was quoted (which makes the body literal), and the index after.
fn heredoc_delimiter(
    chars: &[(usize, char)],
    mut at: usize,
) -> std::result::Result<(String, bool, bool, usize), String> {
    let strip_tabs = chars.get(at).is_some_and(|&(_, c)| c == '-');
    if strip_tabs {
        at += 1;
    }
    while chars.get(at).is_some_and(|&(_, c)| c == ' ' || c == '\t') {
        at += 1;
    }
    let mut word = String::new();
    let mut quote: Option<char> = None;
    let mut quoted = false;
    while let Some(&(_, c)) = chars.get(at) {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => word.push(c),
            None if c == '\'' || c == '"' => {
                quote = Some(c);
                quoted = true;
            }
            None if c.is_whitespace() || matches!(c, ';' | '&' | '|' | '<' | '>' | '(' | ')') => {
                break
            }
            None => word.push(c),
        }
        at += 1;
    }
    if quote.is_some() {
        return Err("here-document delimiter has an unterminated quote".into());
    }
    if word.is_empty() {
        return Err("here-document has no delimiter".into());
    }
    Ok((word, strip_tabs, quoted, at))
}

/// Validate that all commands in a potentially piped command are allowed.
///
/// The allowlist prevents arbitrary binary execution. Variable expansion,
/// redirects, and other shell features are permitted since commands run
/// via `sh -c` and the agent needs these for real-world tasks (e.g.
/// `echo $HOME`, `python3 script.py > output.txt`). A command or process
/// substitution runs a command of its own, so that command is checked like
/// any other (`x=$(git rev-parse HEAD)` passes, `echo $(bash -c ...)` does
/// not).
fn validate_command(command: &str) -> std::result::Result<(), String> {
    // Newlines separate commands, and each is checked below. A carriage
    // return separates nothing and has no use here.
    if command.contains('\r') {
        return Err("command contains carriage return characters".into());
    }
    validate_nested(command, 0)
}

/// How deeply substitutions may nest before the check refuses the command.
const MAX_SUBSTITUTION_DEPTH: usize = 8;

/// What a checked substitution is replaced with, so the rest of its line
/// can be checked. It is not a command: a substitution in command position
/// would run whatever it printed, which no check can see.
const SUBSTITUTION: &str = "__substitution__";

fn validate_nested(command: &str, depth: usize) -> std::result::Result<(), String> {
    if depth > MAX_SUBSTITUTION_DEPTH {
        return Err("command substitutions nest too deeply".into());
    }
    let (command, inner) = substitutions(command)?;
    for body in &inner {
        validate_nested(body, depth + 1)?;
    }

    // Split on unquoted shell control operators and validate every command
    // segment. Empty segments account for the second byte in && and ||.
    for segment in command_segments(&command)? {
        let segment = segment.trim();
        if segment.is_empty() || segment.starts_with('#') {
            continue;
        }
        // Find the command the segment runs, past what is not one: variable
        // assignments (`FOO=bar python3 x.py`), redirects and their targets
        // (`> out.txt`, `2>&1`), a subshell's `(`, and the shell's own words
        // (`if`, `then`, `do`, `done` …). A `for` header names a variable
        // and words, and runs nothing; the commands of its body are segments
        // of their own and are checked. A segment of only these runs nothing.
        let mut target_next = false;
        for token in segment.split_whitespace() {
            if std::mem::take(&mut target_next) {
                continue;
            }
            if let Some(needs_target) = redirect(token) {
                target_next = needs_target;
                continue;
            }
            if token.contains('=') && !token.starts_with('=') {
                continue;
            }
            let token = token.trim_start_matches('(').trim_end_matches(')');
            if token.is_empty() || SHELL_WORDS.contains(&token) {
                continue;
            }
            if token == "for" {
                break;
            }
            if token.contains(SUBSTITUTION) {
                return Err(
                    "a substitution cannot be the command to run: its output would \
                            run unchecked; run the command directly"
                        .into(),
                );
            }
            let cmd_name = token.rsplit('/').next().unwrap_or(token);
            if ALLOWED_COMMANDS.contains(&cmd_name) {
                break;
            }
            return Err(format!(
                "command '{}' is not in the allowlist. Allowed commands: {}",
                cmd_name,
                ALLOWED_COMMANDS.join(", ")
            ));
        }
    }
    Ok(())
}

/// The shell's own words that come before a command, or end a construct,
/// and run nothing themselves. `case` is not among them: its patterns sit in
/// front of commands on the same segment, where this check would not see
/// them, so it stays refused.
const SHELL_WORDS: &[&str] = &[
    "if", "elif", "then", "else", "fi", "while", "until", "do", "done", "!", "{", "}",
];

/// Whether `token` is a redirect (`>`, `2>>`, `&>`, `<&3`, `2>&1` …), and if
/// so whether its target is the next token (`> out.txt`) rather than part of
/// this one (`>out.txt`, `2>&1`).
fn redirect(token: &str) -> Option<bool> {
    let rest = token.trim_start_matches(|c: char| c.is_ascii_digit());
    [
        "&>>", "&>", ">>", ">|", ">&", "<&", "<>", "<<<", "<<", ">", "<",
    ]
    .iter()
    .find_map(|op| rest.strip_prefix(op))
    .map(str::is_empty)
}

#[async_trait]
impl Tool for ExecTool {
    fn name(&self) -> &str {
        "exec"
    }

    fn description(&self) -> &str {
        if self.sandbox().is_some() {
            "Execute a shell command in your working directory and return its output. Any \
             program may run, in a sandbox that refuses writes outside the working directory, \
             its git directory and temporary space, writes to git hooks and config, reads of \
             credential stores, and connections to Docker and RustyKrab's own services. Forced \
             removal (rm -f, rm -rf) is refused: remove files without -f, or use git rm."
        } else if self.confinement.is_some() {
            "Execute a shell command in your working directory and return its output. Commands \
             are validated against an allowlist. Writes outside the working directory and \
             temporary space, and reads of credential stores, are refused."
        } else {
            "Execute a shell command and return its output. Commands are validated against an allowlist."
        }
    }

    fn sandbox_requirements(&self) -> SandboxRequirements {
        SandboxRequirements {
            needs_spawn: true,
            ..SandboxRequirements::default()
        }
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: self.name().to_string(),
            description: self.description().to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "The shell command to execute (must use allowed commands)"
                    },
                    "timeout_secs": {
                        "type": "integer",
                        "description": "Timeout in seconds (default: 120, max: 600; a cargo command gets at least 120). Raise it for a long build or test run: processes run in the background are killed when the call returns."
                    }
                },
                "required": ["command"]
            }),
        }
    }

    async fn execute(&self, args: Value) -> Result<Value> {
        let command = args["command"].as_str().ok_or_else(|| {
            rustykrab_core::Error::ToolExecution(ToolError::invalid_input("missing command"))
        })?;

        // In the sandbox, the sandbox is the boundary and any program runs;
        // unsandboxed, the allowlist still decides. Forced removal is refused
        // either way.
        // Backgrounded work never outlives the call, so it is refused too.
        let checked = if self.sandbox().is_some() {
            refuse_forced_removal(command)
        } else {
            validate_command(command).and_then(|()| refuse_forced_removal(command))
        };
        checked
            .and_then(|()| refuse_background(command))
            .map_err(|e| {
                rustykrab_core::Error::ToolExecution(ToolError::invalid_input(format!(
                    "command rejected: {e}"
                )))
            })?;

        let cargo = runs_cargo(command);
        let timeout_secs = resolve_timeout(args["timeout_secs"].as_u64(), cargo);

        // Inherit the user's PATH so tools installed via homebrew, cargo,
        // pip, nvm, etc. are available. Fall back to a sensible default.
        let path = std::env::var("PATH")
            .unwrap_or_else(|_| "/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin".to_string());

        let mut process = self.shell();
        process
            .kill_on_drop(true)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .arg("-c")
            .arg(command)
            .env_clear()
            .env("PATH", &path)
            .env("HOME", "/tmp/rustykrab-home")
            .env("LANG", "C.UTF-8")
            // Use installed toolchains; an ordinary Cargo invocation must not
            // implicitly download into the operator's shared Rustup store.
            .env("RUSTUP_AUTO_INSTALL", "0");
        if let Some(home) = rustup_home() {
            process.env("RUSTUP_HOME", home);
        }
        if let Some(cache) = &self.build_cache {
            process
                .env("CARGO_TARGET_DIR", cache.join("target"))
                .env("CARGO_BUILD_JOBS", CARGO_BUILD_JOBS);
        }
        if let Some(home) = &self.cargo_home {
            process.env("CARGO_HOME", home).env(
                "PATH",
                path_with_host_cargo_bin(std::ffi::OsStr::new(&path)),
            );
        }
        if let Some(dir) = &self.dir {
            process.current_dir(dir);
        }
        #[cfg(unix)]
        process.process_group(0);
        let child = process.spawn().map_err(|e| {
            rustykrab_core::Error::ToolExecution(ToolError::transient(e.to_string()))
        })?;
        #[cfg(unix)]
        let _group = child.id().map(ProcessGroupGuard);
        let future = child.wait_with_output();

        let output = timeout(Duration::from_secs(timeout_secs), future)
            .await
            .map_err(|_| {
                rustykrab_core::Error::ToolExecution(ToolError::timeout(timeout_message(
                    timeout_secs,
                    cargo,
                    self.build_cache.is_some(),
                )))
            })?
            .map_err(|e| {
                rustykrab_core::Error::ToolExecution(ToolError::transient(e.to_string()))
            })?;

        let stdout = truncate_output(String::from_utf8_lossy(&output.stdout).into_owned());
        let stderr = truncate_output(String::from_utf8_lossy(&output.stderr).into_owned());
        let exit_code = output.status.code().unwrap_or(-1);

        Ok(json!({
            "exit_code": exit_code,
            "stdout": stdout,
            "stderr": stderr,
        }))
    }
}

#[cfg(unix)]
#[cfg(test)]
mod environment_tests {
    use super::ExecTool;
    use rustykrab_core::Tool;
    use serde_json::json;
    use std::path::Path;

    // Separate processes give each case a private environment; tests never
    // mutate the workspace test process's environment or operator files.
    fn fixture(explicit: Option<&str>) {
        let root = tempfile::tempdir().unwrap();
        let daemon = root.path().join("daemon");
        let worker = root.path().join("worker");
        let host_home = root.path().join("host-home");
        for dir in [&daemon, &worker, &host_home] {
            std::fs::create_dir_all(dir).unwrap();
        }
        let explicit = explicit.map(|p| {
            if p == "absolute" {
                root.path().join("custom-rustup")
            } else {
                p.into()
            }
        });
        let expected = match &explicit {
            Some(p) if p.is_absolute() => p.clone(),
            Some(p) => daemon.join(p),
            None => host_home.join(".rustup"),
        };
        std::fs::create_dir_all(&expected).unwrap();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap());
        child
            .args([
                "--ignored",
                "--exact",
                "exec::environment_tests::child_exec_environment",
                "--nocapture",
            ])
            .current_dir(&daemon)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", &host_home)
            .env("RK_EXEC_EXPECT_RUSTUP_HOME", &expected)
            .env("RK_EXEC_WORKER_DIR", &worker)
            .env("CARGO_HOME", root.path().join("credential-canary"))
            .env("CARGO_REGISTRIES_CRATES_IO_TOKEN", "fixture-only-token")
            .env("RUSTUP_AUTO_INSTALL", "1")
            .env("ANTHROPIC_API_KEY", "fixture-only-key")
            .env("RK_EXEC_PARENT_SECRET", "fixture-only-secret");
        if let Some(explicit) = explicit {
            child.env("RUSTUP_HOME", explicit);
        }
        let output = child.output().unwrap();
        assert!(
            output.status.success(),
            "private environment fixture failed:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn rustup_default_location_survives_an_isolated_command_home() {
        fixture(None);
    }

    #[test]
    fn explicit_rustup_location_takes_precedence_over_host_home() {
        fixture(Some("absolute"));
    }

    #[test]
    fn relative_rustup_location_is_bound_before_changing_worktree() {
        fixture(Some("relative-rustup"));
    }

    #[test]
    #[ignore = "private subprocess fixture; parents provide the isolated environment"]
    fn child_exec_environment() {
        let expected = std::env::var_os("RK_EXEC_EXPECT_RUSTUP_HOME")
            .expect("parent must supply the expected installation path");
        let worker = std::env::var_os("RK_EXEC_WORKER_DIR");
        let tool = worker.as_ref().map_or_else(ExecTool::new, ExecTool::in_dir);
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let out = tool.execute(json!({"command":"env"})).await.unwrap();
                assert_eq!(out["exit_code"], 0);
                let vars: std::collections::BTreeMap<_, _> = out["stdout"]
                    .as_str()
                    .unwrap()
                    .lines()
                    .filter_map(|line| line.split_once('='))
                    .collect();
                assert_eq!(vars.get("HOME"), Some(&"/tmp/rustykrab-home"));
                assert_eq!(vars.get("RUSTUP_AUTO_INSTALL"), Some(&"0"));
                let home = vars.get("RUSTUP_HOME").expect("Rustup location is missing");
                assert_eq!(
                    Path::new(home).canonicalize().unwrap(),
                    Path::new(&expected).canonicalize().unwrap()
                );
                for key in [
                    "CARGO_HOME",
                    "CARGO_REGISTRIES_CRATES_IO_TOKEN",
                    "ANTHROPIC_API_KEY",
                    "RK_EXEC_PARENT_SECRET",
                    "RK_EXEC_EXPECT_RUSTUP_HOME",
                    "RK_EXEC_WORKER_DIR",
                ] {
                    assert!(!vars.contains_key(key), "parent variable leaked: {key}");
                }
                let pwd = tool.execute(json!({"command":"pwd"})).await.unwrap();
                assert_eq!(pwd["exit_code"], 0);
                if let Some(worker) = worker {
                    assert_eq!(
                        Path::new(pwd["stdout"].as_str().unwrap().trim())
                            .canonicalize()
                            .unwrap(),
                        Path::new(&worker).canonicalize().unwrap()
                    );
                }
                // An explicit opt-in verification run uses the real installed
                // Cargo through this same Tool boundary; portable fixtures do
                // not require their host to have installed Rust with Rustup.
                if std::env::var_os("RK_EXEC_VERIFY_REAL_CARGO").is_some() {
                    let cargo = tool
                        .execute(json!({"command":"cargo --version"}))
                        .await
                        .unwrap();
                    assert_eq!(cargo["exit_code"], 0, "{cargo}");
                    assert!(cargo["stdout"].as_str().unwrap().starts_with("cargo "));
                    let check = tool
                        .execute(json!({"command":"cargo check --offline"}))
                        .await
                        .unwrap();
                    assert_eq!(check["exit_code"], 0, "{check}");
                    println!(
                        "REAL_EXEC_RECEIPT {}",
                        json!({"cargo_version":cargo,"offline_check":check,
                            "isolated_home":vars.get("HOME"),
                            "rustup_location_forwarded":true,
                            "rustup_auto_install":vars.get("RUSTUP_AUTO_INSTALL"),
                            "cargo_home_or_credentials_forwarded":false})
                    );
                }
                if std::env::var_os("RK_EXEC_VERIFY_MISSING_CARGO").is_some() {
                    for command in ["cargo --version", "cargo +stable --version"] {
                        let cargo = tool.execute(json!({"command":command})).await.unwrap();
                        assert_ne!(cargo["exit_code"], 0, "{cargo}");
                        assert!(cargo["stderr"].as_str().unwrap().contains("not installed"));
                        println!(
                            "MISSING_EXEC_RECEIPT {}",
                            json!({"command":command,"result":cargo})
                        );
                    }
                }
            });
    }
}

#[cfg(test)]
mod background_and_timeout_tests {
    use super::*;

    #[test]
    fn backgrounded_work_is_refused_with_why_and_what_to_do() {
        for command in [
            "sleep 30 & echo x",
            "sleep 30 &",
            "nohup cargo test > test.log 2>&1 & disown",
            "cd sub && cargo build &",
            "(cargo test &)",
            "echo $(sleep 5 & echo started)",
            "cargo test 2>&1 & echo launched",
            // A wait does not excuse it: each of these leaves work behind.
            "sleep 1 & sleep 2 & wait",
            "nohup cargo test > log 2>&1 & disown; wait",
            "(cargo test &); wait",
            "sleep 1 & wait; sleep 100 &",
            // Nor does handing the line to another shell.
            "sh -c 'cargo test &'",
            "bash -c \"nohup cargo test &\"",
            "bash -ec 'sleep 9 & echo x'",
            "eval 'sleep 9 &'",
            // Arithmetic beside it does not hide it.
            "echo $((1 & 1)); sleep 9 &",
            // Nor does a substitution or subshell that only starts like
            // arithmetic: a subshell inside, closed before the end.
            "$((sleep 2 &) ; echo ran)",
            "echo \"$((sleep 2 &) ; echo ran)\"",
            "((sleep 2 &) ; echo ran)",
            "x; ((sleep 2 &) )",
        ] {
            let error = refuse_background(command).unwrap_err();
            assert!(
                error.contains("background")
                    && error.contains("killed when the call returns")
                    && error.contains("foreground")
                    && error.contains("timeout_secs"),
                "{command}: {error}"
            );
        }
    }

    #[test]
    fn and_lists_redirections_quotes_and_waited_jobs_are_not_backgrounding() {
        for command in [
            "a && b",
            "cargo test 2>&1",
            "cargo build &> build.log",
            "cargo build >& build.log",
            "echo 'a & b'",
            "echo \"a & b\"",
            r"echo a \& b",
            "make || echo failed",
            "echo $((5 & 3))",
            "x=$(( a & 1 ))",
            "(( x & 1 )) && echo odd",
            "echo ${v//&/and}",
            "echo \"${v//&/and}\"",
            "sh -c 'a && b'",
            "cat <<'EOF'\nrun & later\nEOF",
            "echo $(( x & 1 ))",
            "echo $(( (a + b) & 1 ))",
            "if (( x & 1 )); then echo odd; fi",
            "while (( n & 1 )); do n=0; done",
            "echo x\n(( y & 1 )) && echo odd",
            // What the workers run every day.
            "cargo test 2>&1 | tail -20",
            "cargo test -p x -- --test-threads=1 2>&1 | tail -n 50",
            "curl -s 'https://example.invalid/?a=1&b=2'",
            "git log --format='%h & %s' -5",
            "[[ -n a && -n b ]] && echo y",
            "git commit -m \"$(cat <<'EOF'\nfix a & b\nEOF\n)\"",
        ] {
            refuse_background(command).unwrap_or_else(|e| panic!("{command}: {e}"));
        }
    }

    #[tokio::test]
    async fn a_backgrounding_command_is_refused_before_it_runs() {
        let dir = tempfile::tempdir().unwrap();
        let error = ExecTool::in_dir(dir.path())
            .execute(json!({"command": "touch ran & echo x"}))
            .await
            .unwrap_err();
        assert_eq!(error.kind(), rustykrab_core::ToolErrorKind::InvalidInput);
        assert!(error.to_string().contains("background"), "{error}");
        assert!(!dir.path().join("ran").exists(), "the command ran");
    }

    #[test]
    fn commands_that_run_cargo_are_recognised_wherever_cargo_runs() {
        for command in [
            "cargo test",
            "cd crates/x && cargo test -p x",
            "RUSTFLAGS=-Dwarnings cargo build",
            "timeout 300 cargo test",
            "env CARGO_TERM_COLOR=never /usr/bin/cargo check",
            "echo $(cargo metadata --format-version 1)",
            "nohup cargo test > log",
            "RUSTFLAGS=\"-D warnings\" cargo build",
            "RUSTFLAGS='-D warnings' cargo build",
            "timeout -s KILL 300 cargo test",
            "env -u FOO cargo test",
            "sudo -u bob cargo test",
            "bash -c 'cargo test'",
            "eval cargo test",
        ] {
            assert!(runs_cargo(command), "{command}");
        }
        for command in ["echo cargo", "grep cargo Cargo.toml", "ls target", "make"] {
            assert!(!runs_cargo(command), "{command}");
        }
    }

    #[test]
    fn a_cargo_command_gets_at_least_the_cargo_minimum() {
        assert_eq!(resolve_timeout(Some(5), true), MIN_CARGO_TIMEOUT_SECS);
        assert_eq!(resolve_timeout(Some(15), true), 120);
        assert_eq!(resolve_timeout(Some(300), true), 300);
        assert_eq!(resolve_timeout(Some(5000), true), MAX_TIMEOUT_SECS);
        assert_eq!(resolve_timeout(None, true), DEFAULT_TIMEOUT_SECS);
        // Other commands keep what they ask for.
        assert_eq!(resolve_timeout(Some(5), false), 5);
        assert_eq!(resolve_timeout(Some(0), false), 0);
        assert_eq!(resolve_timeout(Some(5000), false), MAX_TIMEOUT_SECS);
    }

    #[test]
    fn a_cargo_timeout_says_the_work_is_kept_and_how_to_continue() {
        let message = timeout_message(
            resolve_timeout(Some(5), runs_cargo("cargo test")),
            true,
            true,
        );
        for wanted in [
            "timed out after 120s",
            "shared build cache",
            "run the same command again in the foreground with timeout_secs=600",
            "Background processes",
            "killed when the call returns",
        ] {
            assert!(message.contains(wanted), "{wanted:?} missing: {message}");
        }
        let message = timeout_message(30, false, true);
        assert!(
            message.contains("killed when the call returns"),
            "{message}"
        );
        assert!(!message.contains("build cache"), "{message}");
    }

    #[tokio::test]
    async fn a_timed_out_command_is_told_backgrounding_cannot_outlive_the_call() {
        let error = ExecTool::new()
            .execute(json!({"command": "sleep 1", "timeout_secs": 0}))
            .await
            .unwrap_err();
        assert_eq!(error.kind(), rustykrab_core::ToolErrorKind::Timeout);
        let message = error.to_string();
        assert!(message.contains("timed out after 0s"), "{message}");
        assert!(
            message.contains("killed when the call returns"),
            "{message}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn builds_share_a_persistent_cargo_home_beside_the_cache_with_limited_jobs() {
        let data = tempfile::tempdir().unwrap();
        let work = data.path().join("worktrees/run-1");
        std::fs::create_dir_all(&work).unwrap();
        let cache = data.path().join("build-cache/source/worker-w");
        let tool = ExecTool::in_dir(&work).with_build_cache(&cache);
        let out = tool.execute(json!({"command": "env"})).await.unwrap();
        assert_eq!(out["exit_code"], 0, "{out}");
        let vars: std::collections::BTreeMap<_, _> = out["stdout"]
            .as_str()
            .unwrap()
            .lines()
            .filter_map(|line| line.split_once('='))
            .collect();
        let home = vars.get("CARGO_HOME").expect("CARGO_HOME is set");
        let shared = data.path().join("build-cache").join(SHARED_CARGO_HOME);
        assert_eq!(std::path::Path::new(home), shared);
        assert!(shared.is_dir(), "the shared Cargo home is created");
        assert!(
            !home.starts_with(vars.get("HOME").unwrap()),
            "Cargo's home is not under the isolated, temporary HOME: {home}"
        );
        assert_eq!(vars.get("CARGO_BUILD_JOBS"), Some(&CARGO_BUILD_JOBS));
        assert_eq!(
            vars.get("CARGO_TARGET_DIR").map(std::path::Path::new),
            Some(cache.join("target").as_path())
        );
    }

    /// Opt-in, with the host's real Cargo and network: two worktrees of one
    /// repository build through `exec`, and the second compiles none of the
    /// dependencies the first fetched into the shared Cargo home and built.
    #[cfg(unix)]
    #[tokio::test]
    #[ignore = "needs a real Cargo toolchain and crates.io; run explicitly"]
    async fn real_cargo_reuses_the_shared_home_and_cache_across_worktrees() {
        let data = tempfile::tempdir().unwrap();
        let cache = data.path().join("build-cache/source/worker-w");
        let mut outputs = Vec::new();
        for run in ["run-1", "run-2"] {
            let work = data.path().join("worktrees").join(run);
            std::fs::create_dir_all(work.join("src")).unwrap();
            std::fs::write(
                work.join("Cargo.toml"),
                "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n\
                 [dependencies]\nitoa = \"=1.0.11\"\n",
            )
            .unwrap();
            std::fs::write(
                work.join("src/lib.rs"),
                "pub fn f() -> String { itoa::Buffer::new().format(1).into() }\n",
            )
            .unwrap();
            let tool = ExecTool::in_dir(&work).with_build_cache(&cache);
            let out = tool
                .execute(json!({"command": "cargo build 2>&1", "timeout_secs": 600}))
                .await
                .unwrap();
            assert_eq!(out["exit_code"], 0, "{out}");
            outputs.push(out["stdout"].as_str().unwrap().to_string());
        }
        println!("REAL_SHARED_HOME_RECEIPT {}", json!({"builds": outputs}));
        assert!(outputs[0].contains("Compiling itoa"), "{}", outputs[0]);
        assert!(!outputs[1].contains("Compiling itoa"), "{}", outputs[1]);
        let home = data.path().join("build-cache/cargo-home");
        assert!(
            home.join("registry").is_dir(),
            "sources live in the shared home"
        );
    }

    /// Opt-in, with the host's real Cargo and network, macOS only: a build
    /// with one dependency succeeds in the confined shell, which may write
    /// only the shared Cargo home's download directories and lock files.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    #[ignore = "needs a real Cargo toolchain and crates.io; run explicitly"]
    async fn real_cargo_builds_in_the_confined_shell_with_a_narrowed_home() {
        let data = tempfile::tempdir().unwrap();
        let cache = data.path().join("build-cache/source/worker-w");
        let work = data.path().join("worktrees/run-1");
        std::fs::create_dir_all(work.join("src")).unwrap();
        std::fs::write(
            work.join("Cargo.toml"),
            "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n\
             [dependencies]\nitoa = \"=1.0.11\"\n",
        )
        .unwrap();
        std::fs::write(
            work.join("src/lib.rs"),
            "pub fn f() -> String { itoa::Buffer::new().format(1).into() }\n",
        )
        .unwrap();
        let tool = ExecTool::in_dir(&work)
            .confined(vec![data.path().to_path_buf()])
            .with_build_cache(&cache);
        let out = tool
            .execute(json!({"command": "cargo build 2>&1", "timeout_secs": 600}))
            .await
            .unwrap();
        println!("REAL_CONFINED_RECEIPT {out}");
        assert_eq!(out["exit_code"], 0, "{out}");
        let stdout = out["stdout"].as_str().unwrap();
        assert!(stdout.contains("Compiling itoa"), "{stdout}");
        let home = data.path().join("build-cache/cargo-home");
        assert!(home.join("registry/src").is_dir(), "sources extracted");
        assert!(!home.join("bin").exists());
    }

    #[test]
    fn each_run_clears_configuration_and_credentials_from_the_shared_home() {
        let data = tempfile::tempdir().unwrap();
        let cache = data.path().join("build-cache/source/worker-w");
        let home = prepare_shared_cargo_home(&cache).unwrap();
        std::fs::create_dir_all(home.join("registry/src")).unwrap();
        std::fs::write(home.join("registry/src/kept"), "kept").unwrap();
        std::fs::write(home.join("config.toml"), "[alias]\nplanted = \"version\"\n").unwrap();
        std::fs::write(home.join("credentials.toml"), "token").unwrap();
        std::fs::create_dir_all(home.join("config")).unwrap();
        std::fs::create_dir_all(home.join("bin")).unwrap();
        std::fs::write(home.join("bin/cargo-fmt"), "#!/bin/sh\necho planted\n").unwrap();
        assert_eq!(prepare_shared_cargo_home(&cache), Some(home.clone()));
        for dir in CARGO_HOME_DOWNLOAD_DIRS {
            assert!(home.join(dir).is_dir(), "{dir} is created");
        }
        for name in CARGO_HOME_CONTROL_FILES {
            assert!(!home.join(name).exists(), "{name} survived");
        }
        assert!(home.join("registry/src/kept").is_file(), "downloads stay");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_shell_without_a_build_cache_sets_no_cargo_home_or_jobs() {
        let dir = tempfile::tempdir().unwrap();
        let out = ExecTool::in_dir(dir.path())
            .execute(json!({"command": "printf '%s/%s' \"${CARGO_HOME:-unset}\" \"${CARGO_BUILD_JOBS:-unset}\""}))
            .await
            .unwrap();
        assert_eq!(out["stdout"], "unset/unset");
    }
}

#[cfg(test)]
mod forced_removal_tests {
    use super::refuse_forced_removal;

    #[test]
    fn forced_removal_is_refused_wherever_it_runs() {
        for command in [
            "rm -rf build",
            "rm -f a.txt",
            "rm --force a.txt",
            "rm -fr build",
            "rm -r -f build",
            "/bin/rm -rf build",
            "ls && rm -rf build",
            "cd sub; rm -rf .",
            "echo $(rm -rf build)",
            "find . -name '*.o' | xargs rm -f",
            "find . -name '*.o' -exec rm -f {} \\;",
            "env FOO=1 rm -rf build",
            "sudo rm -rf /",
            "timeout 5 rm -rf build",
            "nice -n 10 rm -rf build",
            "if true; then rm -rf build; fi",
            // A command substitution that only starts like arithmetic.
            "echo $((rm -rf build) )",
            "echo \"$((rm -rf build); echo done)\"",
        ] {
            let error = refuse_forced_removal(command).unwrap_err();
            assert!(error.contains("forced removal"), "{command}: {error}");
        }
    }

    #[test]
    fn plain_removal_and_mentions_of_rm_run() {
        for command in [
            "rm a.txt",
            "rm -r build",
            "rm -i a.txt",
            "git rm -f old.txt",
            "grep -n 'rm -rf' notes.md",
            "echo rm -rf is refused",
            "find . -name '*.pyc' -delete",
            "python3 -c 'print(1)'",
        ] {
            refuse_forced_removal(command).unwrap_or_else(|e| panic!("{command}: {e}"));
        }
    }
}

#[cfg(test)]
mod validation_tests {
    use super::validate_command;

    #[test]
    fn permits_control_operators_inside_quotes() {
        validate_command("grep -E 'GMAIL|EMAIL|PASSWORD' config.txt").unwrap();
        validate_command("echo \"one; two && three || four | five\"").unwrap();
    }

    #[test]
    fn permits_escaped_control_operators() {
        validate_command(r"echo one\|two\;three\&four").unwrap();
    }

    #[test]
    fn validates_each_unquoted_command_segment() {
        validate_command("grep -E 'one|two' file | sort").unwrap();

        let error = validate_command("echo safe | definitely-not-allowed").unwrap_err();
        assert!(error.contains("definitely-not-allowed"));

        let error = validate_command("echo safe & definitely-not-allowed").unwrap_err();
        assert!(error.contains("definitely-not-allowed"));
    }

    #[test]
    fn permits_directory_changes_removal_and_ripgrep() {
        validate_command("cd src && ls").unwrap();
        validate_command("rm -f build/out.txt").unwrap();
        validate_command("rg -n 'fn main' src").unwrap();
    }

    #[test]
    fn an_ampersand_in_a_redirect_is_not_a_boundary() {
        validate_command("cargo test 2>&1 | tail -20").unwrap();
        validate_command("echo warning >&2").unwrap();
        validate_command("python3 run.py &> out.log").unwrap();
        validate_command("python3 run.py &>> out.log").unwrap();
        validate_command("cat <&3").unwrap();
        // Background and `&&` still split.
        let error = validate_command("sleep 1 & definitely-not-allowed").unwrap_err();
        assert!(error.contains("definitely-not-allowed"));
        let error = validate_command("ls && definitely-not-allowed").unwrap_err();
        assert!(error.contains("definitely-not-allowed"));
    }

    #[test]
    fn newlines_separate_commands_that_are_each_checked() {
        validate_command("cd src\nls -la\n# a comment\npwd").unwrap();
        validate_command("python3 -c \"import sys\nprint(sys.version)\"").unwrap();
        let error = validate_command("ls\ndefinitely-not-allowed").unwrap_err();
        assert!(error.contains("definitely-not-allowed"));
        assert!(validate_command("ls\r\npwd").is_err());
    }

    #[test]
    fn a_heredoc_body_is_input_not_commands() {
        validate_command("cat > f.py <<'EOF'\nimport os\nnot-a-command here\nEOF\npython3 f.py")
            .unwrap();
        validate_command("cat <<EOF > notes.md\n# Title\nbody\nEOF").unwrap();
        validate_command("cat <<-\"END\"\n\tindented\n\tEND\nls").unwrap();
        // The command after the body is still checked.
        let error = validate_command("cat <<EOF\nbody\nEOF\ndefinitely-not-allowed").unwrap_err();
        assert!(error.contains("definitely-not-allowed"));
        // The command that reads the body is checked too.
        let error = validate_command("bash <<EOF\nls\nEOF").unwrap_err();
        assert!(error.contains("bash"));
        // An unquoted body runs its substitutions, so they are checked; a
        // quoted delimiter makes the body text.
        assert!(validate_command("cat <<EOF\n$(definitely-not-allowed)\nEOF").is_err());
        assert!(validate_command("cat <<EOF\n`definitely-not-allowed`\nEOF").is_err());
        validate_command("cat <<EOF\nhead is $(git rev-parse HEAD)\nEOF").unwrap();
        validate_command("cat > f.sh <<'EOF'\necho $(definitely-not-allowed)\nEOF").unwrap();
        // A here-string is not a here-document.
        validate_command("grep x <<< 'x y'").unwrap();
    }

    #[test]
    fn loops_conditionals_and_subshells_check_the_commands_inside() {
        validate_command("for f in *.py; do wc -l $f; done").unwrap();
        validate_command("for f in src/*.py\ndo\n  grep -n def \"$f\"\ndone > defs.txt").unwrap();
        validate_command("while read line; do echo \"$line\"; done < list.txt").unwrap();
        validate_command("if [ -f setup.py ]; then python3 setup.py --version; else echo none; fi")
            .unwrap();
        validate_command("(grep -rn TODO src || true) | head").unwrap();
        validate_command("printf '%s\\n' a b | sort").unwrap();
        let error =
            validate_command("for f in a b; do definitely-not-allowed $f; done").unwrap_err();
        assert!(error.contains("definitely-not-allowed"));
        let error = validate_command("if true; then definitely-not-allowed; fi").unwrap_err();
        assert!(error.contains("definitely-not-allowed"));
        let error = validate_command("(definitely-not-allowed)").unwrap_err();
        assert!(error.contains("definitely-not-allowed"));
        // `case` puts commands behind patterns on one segment: refused.
        assert!(validate_command("case x in x) ls;; esac").is_err());
    }

    #[test]
    fn a_command_after_a_redirect_is_still_checked() {
        validate_command("> out.txt").unwrap();
        validate_command("2>/dev/null ls").unwrap();
        let error = validate_command("> out.txt definitely-not-allowed").unwrap_err();
        assert!(error.contains("definitely-not-allowed"));
        let error = validate_command("2> err.log definitely-not-allowed").unwrap_err();
        assert!(error.contains("definitely-not-allowed"));
    }

    #[test]
    fn substitutions_run_only_allowed_commands() {
        validate_command("head=$(git rev-parse HEAD); echo $head").unwrap();
        validate_command("echo \"today: $(date +%F)\"").unwrap();
        validate_command("cd \"$(git rev-parse --show-toplevel)\" && ls").unwrap();
        validate_command("echo `date`").unwrap();
        validate_command("echo $(basename $(pwd))").unwrap();
        validate_command("echo \"$(grep -c x \"$(ls | head -1)\")\"").unwrap();
        validate_command("diff <(sort a.txt) <(sort b.txt)").unwrap();
        validate_command("python3 gen.py | tee >(wc -l) > out.txt").unwrap();
        validate_command("echo $((1 + 2 * 3))").unwrap();
        // Single quotes are literal: nothing runs.
        validate_command("grep -n '$(definitely-not-allowed)' notes.txt").unwrap();
        validate_command("grep -n '`definitely-not-allowed`' notes.txt").unwrap();

        for command in [
            "echo $(definitely-not-allowed)",
            "echo \"$(definitely-not-allowed)\"",
            "echo `definitely-not-allowed`",
            "echo $(ls; definitely-not-allowed)",
            "echo $(echo $(definitely-not-allowed))",
            "cat <(definitely-not-allowed)",
            "ls | tee >(definitely-not-allowed)",
        ] {
            let error = validate_command(command).unwrap_err();
            assert!(
                error.contains("definitely-not-allowed"),
                "{command}: {error}"
            );
        }
        // What a substitution prints cannot be the command: no check sees it.
        assert!(validate_command("$(which python3) run.py").is_err());
        assert!(validate_command("\"$(which python3)\" run.py").is_err());
        assert!(validate_command("echo $(( $(definitely-not-allowed) ))").is_err());
        assert!(validate_command("echo $(ls").is_err());
        assert!(validate_command("echo `ls").is_err());
        let deep = format!("echo {}ls{}", "$(echo ".repeat(10), ")".repeat(10));
        assert!(validate_command(&deep).is_err());
    }

    #[test]
    fn text_and_data_utilities_are_allowed() {
        validate_command("grep x log.txt | paste - - | column -t").unwrap();
        validate_command("strings vault.bin | head").unwrap();
        validate_command("jq '.items[] | .id' data.json | sort | uniq -c").unwrap();
        validate_command("seq 1 5 | tac | nl").unwrap();
        validate_command("comm -12 <(sort a) <(sort b)").unwrap();
        validate_command("shasum -a 256 build.tar && cmp a.bin b.bin").unwrap();
    }

    #[test]
    fn rejects_incomplete_shell_quoting() {
        assert!(validate_command("echo 'unterminated").is_err());
        assert!(validate_command("echo incomplete\\").is_err());
    }
}

#[cfg(test)]
mod error_typing_tests {
    use super::ExecTool;
    use rustykrab_core::{Tool, ToolErrorKind};
    use serde_json::json;
    use std::time::Duration;

    #[tokio::test]
    async fn invalid_commands_are_typed_as_invalid_input() {
        let error = ExecTool::new()
            .execute(json!({"command": "definitely-not-allowed"}))
            .await
            .unwrap_err();

        assert_eq!(error.kind(), ToolErrorKind::InvalidInput);
    }

    #[tokio::test]
    async fn an_exec_bound_to_a_directory_runs_there() {
        let dir = tempfile::tempdir().unwrap();
        let out = ExecTool::in_dir(dir.path())
            .execute(json!({"command": "pwd"}))
            .await
            .unwrap();
        let pwd = std::path::PathBuf::from(out["stdout"].as_str().unwrap().trim());
        assert_eq!(
            pwd.canonicalize().unwrap(),
            dir.path().canonicalize().unwrap()
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancelling_exec_stops_the_command_and_its_descendant_group() {
        let dir = tempfile::tempdir().unwrap();
        let tool = ExecTool::in_dir(dir.path());
        let task = tokio::spawn(async move {
            tool.execute(json!({"command":"python3 -c \"import os,subprocess,time; p=subprocess.Popen(['sleep','30']); open('pids','w').write(str(os.getpid())+' '+str(p.pid)); time.sleep(30)\""})).await
        });
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while !dir.path().join("pids").exists() && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let pids = std::fs::read_to_string(dir.path().join("pids"));
        task.abort();
        let _ = task.await;
        let pids = pids.expect("fixture shell command should run");
        let mut survivors = Vec::new();
        for pid in pids.split_whitespace() {
            let mut stopped = false;
            for _ in 0..100 {
                let state = std::process::Command::new("ps")
                    .args(["-o", "stat=", "-p", pid])
                    .output()
                    .unwrap();
                let state = String::from_utf8_lossy(&state.stdout);
                if state.trim().is_empty() || state.trim().starts_with('Z') {
                    stopped = true;
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            if !stopped {
                let _ = std::process::Command::new("kill")
                    .args(["-9", pid])
                    .status();
            }
            if !stopped {
                survivors.push(pid.to_string());
            }
        }
        assert!(
            survivors.is_empty(),
            "cancelled exec left processes running: {survivors:?}"
        );
    }

    #[tokio::test]
    async fn multi_line_scripts_run_as_the_shell_reads_them() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        let out = ExecTool::in_dir(dir.path())
            .execute(json!({
                "command": "cd sub\ncat > note.txt <<'EOF'\nline $HOME\nEOF\ncat note.txt missing.txt 2>&1\nrm note.txt\nls"
            }))
            .await
            .unwrap();
        let stdout = out["stdout"].as_str().unwrap();
        assert!(
            stdout.contains("line $HOME"),
            "quoted body kept literal: {stdout}"
        );
        assert!(
            stdout.contains("missing.txt"),
            "stderr joined to stdout: {stdout}"
        );
        assert!(!dir.path().join("sub/note.txt").exists());
    }

    #[cfg(target_os = "macos")]
    async fn run_in(tool: &ExecTool, command: &str) -> (i64, String) {
        let out = tool.execute(json!({ "command": command })).await.unwrap();
        let text = format!(
            "{}{}",
            out["stdout"].as_str().unwrap_or_default(),
            out["stderr"].as_str().unwrap_or_default()
        );
        (out["exit_code"].as_i64().unwrap(), text)
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn a_confined_shell_cannot_signal_an_unconfined_peer() {
        let dir = tempfile::tempdir().unwrap();
        let mut peer = tokio::process::Command::new("/bin/sleep")
            .arg("30")
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let pid = peer.id().unwrap();
        let output = ExecTool::in_dir(dir.path())
            .confined(Vec::new())
            .execute(json!({"command": format!("kill -TERM {pid}")}))
            .await
            .unwrap();
        let alive = peer.try_wait().unwrap().is_none();
        let _ = peer.kill().await;
        let _ = peer.wait().await;
        assert_eq!(output["exit_code"], 1, "{output}");
        assert!(alive, "another host job must survive: {output}");
        assert!(
            output["stderr"].as_str().unwrap().contains("permitted"),
            "{output}"
        );
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn a_confined_shell_cannot_signal_another_sandbox_job() {
        let own = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        assert_other_sandbox_survives(own.path(), other.path()).await;
        // Equal profiles must not turn separate calls into one signal domain.
        assert_other_sandbox_survives(own.path(), own.path()).await;
    }

    #[cfg(target_os = "macos")]
    async fn assert_other_sandbox_survives(own: &std::path::Path, other: &std::path::Path) {
        let pid_file = other.join("peer.pid");
        let peer_tool = ExecTool::in_dir(other).confined(Vec::new());
        let peer = tokio::spawn(async move {
            peer_tool
                .execute(json!({"command": "echo $$ > peer.pid; exec sleep 30"}))
                .await
        });
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        let pid = loop {
            if let Ok(raw) = std::fs::read_to_string(&pid_file) {
                if let Ok(pid) = raw.trim().parse::<u32>() {
                    break pid;
                }
            }
            if tokio::time::Instant::now() >= deadline {
                peer.abort();
                let _ = peer.await;
                panic!("sandbox peer did not start");
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        };
        let result = ExecTool::in_dir(own)
            .confined(Vec::new())
            .execute(json!({"command": format!("kill -TERM {pid}")}))
            .await;
        let alive = !peer.is_finished();
        peer.abort();
        let _ = peer.await;
        let output = result.unwrap();
        assert_eq!(output["exit_code"], 1, "{output}");
        assert!(alive, "another sandbox's job must survive: {output}");
        assert!(
            output["stderr"].as_str().unwrap().contains("permitted"),
            "{output}"
        );
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn a_confined_shell_can_signal_its_own_child() {
        let dir = tempfile::tempdir().unwrap();
        let output = ExecTool::in_dir(dir.path())
            .confined(Vec::new())
            // The child is started without `&`, which exec refuses: a
            // process spawned inside the same sandbox invocation.
            .execute(json!({"command": "rk_owned_child=$(python3 -c \"import subprocess; print(subprocess.Popen(['sleep','30'], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL).pid)\"); kill -0 \"$rk_owned_child\" && kill -TERM \"$rk_owned_child\" || exit 1; for i in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20; do kill -0 \"$rk_owned_child\" 2>/dev/null || exit 0; sleep 0.1; done; exit 1"}))
            .await
            .unwrap();
        assert_eq!(output["exit_code"], 0, "{output}");
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn a_confined_shell_writes_only_in_its_directory() {
        let dir = tempfile::tempdir().unwrap();
        let tool = ExecTool::in_dir(dir.path()).confined(Vec::new());
        assert_eq!(
            run_in(&tool, "echo ok > here.txt && cat here.txt").await,
            (0, "ok\n".into())
        );
        let outside = std::path::PathBuf::from(std::env::var("HOME").unwrap())
            .join(format!(".rustykrab-confinement-{}", std::process::id()));
        let (code, text) = run_in(&tool, &format!("echo x > '{}'", outside.display())).await;
        let escaped = outside.exists();
        let _ = std::fs::remove_file(&outside);
        assert!(!escaped, "wrote outside its directory");
        assert_ne!(code, 0);
        assert!(text.contains("Operation not permitted"), "{text}");
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn a_confined_shell_cannot_read_private_paths_but_reads_its_own() {
        // The worktree sits inside the private directory, as a worker's
        // does inside the daemon's data directory.
        let data = tempfile::tempdir().unwrap();
        std::fs::write(data.path().join("service.json"), "s3cret").unwrap();
        let work = data.path().join("workers/pinch/run-1");
        std::fs::create_dir_all(&work).unwrap();
        std::fs::write(work.join("notes.txt"), "mine").unwrap();
        let tool = ExecTool::in_dir(&work).confined(vec![data.path().to_path_buf()]);
        assert_eq!(run_in(&tool, "cat notes.txt").await, (0, "mine".into()));
        let (code, text) = run_in(&tool, "cat ../../../service.json").await;
        assert_ne!(code, 0);
        assert!(!text.contains("s3cret"), "{text}");
    }

    /// 12 November 2026: external runs' temporary directories moved from
    /// the daemon's data directory, which a confined shell cannot read, to
    /// `/private/tmp/rk-<12 hex>`, which it could read and write as
    /// temporary space. They stay out of its reach there too: no reading,
    /// writing or connecting to a socket inside one. The rest of temporary
    /// space stays writable.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn a_confined_shell_cannot_reach_an_external_runs_temporary_directory() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let id = format!(
            "{:012x}",
            (nanos ^ u128::from(std::process::id())) & 0xffff_ffff_ffff
        );
        let theirs =
            std::path::Path::new("/private/tmp").join(format!("{}{id}", super::RUN_TMP_PREFIX));
        std::fs::create_dir(&theirs).unwrap();
        struct Gone(std::path::PathBuf);
        impl Drop for Gone {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _gone = Gone(theirs.clone());
        std::fs::write(theirs.join("secret"), "theirs").unwrap();
        let socket = theirs.join("s");
        let _listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let work = tempfile::tempdir().unwrap();
        let tool = ExecTool::in_dir(work.path()).confined(Vec::new());
        let dir = theirs.display();
        let (code, text) = run_in(&tool, &format!("cat {dir}/secret")).await;
        assert_ne!(code, 0, "{text}");
        assert!(!text.contains("theirs"), "{text}");
        let (code, text) = run_in(&tool, &format!("ls {dir}")).await;
        assert_ne!(code, 0, "{text}");
        let (code, text) = run_in(&tool, &format!("echo x > {dir}/planted")).await;
        assert_ne!(code, 0, "{text}");
        assert!(!theirs.join("planted").exists());
        let (code, text) = run_in(&tool, &format!("nc -U {} < /dev/null", socket.display())).await;
        assert_ne!(code, 0, "{text}");
        // Temporary space beside it stays writable, a longer name included.
        let own = format!("/private/tmp/{}{id}0", super::RUN_TMP_PREFIX);
        let _own = Gone(own.clone().into());
        let (code, text) = run_in(&tool, &format!("mkdir {own} && echo x > {own}/f")).await;
        assert_eq!(code, 0, "{text}");
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn a_confined_shell_builds_into_its_shared_cache_inside_private_data() {
        let data = tempfile::tempdir().unwrap();
        std::fs::write(data.path().join("service.json"), "s3cret").unwrap();
        let work = data.path().join("worktrees/run-1");
        std::fs::create_dir_all(&work).unwrap();
        let cache = data.path().join("build-cache/source/worker-w");
        let tool = ExecTool::in_dir(&work)
            .confined(vec![data.path().to_path_buf()])
            .with_build_cache(&cache);
        let target = std::fs::canonicalize(cache.join("target")).unwrap();
        let (code, text) = run_in(
            &tool,
            "echo built > \"$CARGO_TARGET_DIR/artifact\" && cat \"$CARGO_TARGET_DIR/artifact\" \
             && printf %s \"$CARGO_TARGET_DIR\"",
        )
        .await;
        assert_eq!(code, 0, "{text}");
        assert!(text.starts_with("built\n"), "{text}");
        assert_eq!(
            std::fs::canonicalize(text.trim_start_matches("built\n")).unwrap(),
            target
        );
        // So does the Cargo home every worker shares, beside the cache.
        let (code, text) = run_in(
            &tool,
            "mkdir -p \"$CARGO_HOME/registry\" && echo fetched > \"$CARGO_HOME/registry/entry\" \
             && cat \"$CARGO_HOME/registry/entry\"",
        )
        .await;
        assert_eq!((code, text.as_str()), (0, "fetched\n"));
        // Its git checkouts and lock files are writable too.
        for command in [
            "mkdir -p \"$CARGO_HOME/git/db\" && echo x > \"$CARGO_HOME/git/db/entry\"",
            "echo > \"$CARGO_HOME/.package-cache\" && echo > \"$CARGO_HOME/.global-cache\" \
             && echo > \"$CARGO_HOME/.global-cache-journal\"",
        ] {
            let (code, text) = run_in(&tool, command).await;
            assert_eq!(code, 0, "{command}: {text}");
        }
        // But not its configuration, credentials or `bin/`, which would
        // change every later build of every worker, nor anything else there.
        for name in super::CARGO_HOME_CONTROL_FILES {
            let (code, _) = run_in(&tool, &format!("echo planted > \"$CARGO_HOME/{name}\"")).await;
            assert_ne!(code, 0, "{name} was writable");
        }
        for command in [
            "mkdir \"$CARGO_HOME/bin\"",
            "mkdir \"$CARGO_HOME/other\"",
            "echo planted > \"$CARGO_HOME/env\"",
            "echo planted > \"$CARGO_HOME/registry/entry2\" \
             && mv \"$CARGO_HOME/registry/entry2\" \"$CARGO_HOME/config.toml\"",
            "ln -s registry \"$CARGO_HOME/bin\"",
        ] {
            let (code, _) = run_in(&tool, command).await;
            assert_ne!(code, 0, "{command} succeeded");
        }
        let home = data.path().join("build-cache/cargo-home");
        assert!(home.join("registry/entry").is_file());
        for name in super::CARGO_HOME_CONTROL_FILES
            .iter()
            .chain(&["other", "env"])
        {
            assert!(!home.join(name).exists(), "{name} was created");
        }
        // The cache opens nothing else in the private directory.
        let (code, text) = run_in(&tool, "cat ../../service.json").await;
        assert_ne!(code, 0);
        assert!(!text.contains("s3cret"), "{text}");
    }

    /// Each builder writes its own build directory and no other's, even
    /// where the data directory lies in writable temporary space.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn a_confined_shell_cannot_write_another_builders_target_directory() {
        let data = tempfile::tempdir().unwrap();
        let work = data.path().join("worktrees/run-1");
        std::fs::create_dir_all(&work).unwrap();
        let repo = data.path().join("build-cache/source");
        let other = repo.join("worker-codex-primary/target");
        std::fs::create_dir_all(&other).unwrap();
        let tool = ExecTool::in_dir(&work)
            .confined(vec![data.path().to_path_buf()])
            .with_build_cache(repo.join("slot-model-daemon"));
        let (code, text) = run_in(&tool, "echo built > \"$CARGO_TARGET_DIR/artifact\"").await;
        assert_eq!(code, 0, "its own target is writable: {text}");
        for command in [
            format!("echo planted > '{}/artifact'", other.display()),
            format!("mkdir '{}/debug'", other.display()),
            format!("mkdir '{}/worker-new'", repo.display()),
        ] {
            let (code, _) = run_in(&tool, &command).await;
            assert_ne!(code, 0, "{command} succeeded");
        }
        assert!(!other.join("artifact").exists());
        assert!(!other.join("debug").exists());
        assert!(repo.join("slot-model-daemon/target/artifact").is_file());
        // The shared Cargo home's download directories stay writable.
        let (code, text) = run_in(
            &tool,
            "echo fetched > \"$CARGO_HOME/registry/entry\" && echo > \"$CARGO_HOME/.package-cache\"",
        )
        .await;
        assert_eq!(code, 0, "{text}");
    }

    #[test]
    fn the_sandbox_profile_denies_other_builders_directories_and_allows_its_own() {
        let confinement = super::Confinement {
            private: Vec::new(),
            ports: Vec::new(),
        };
        let mine = std::path::Path::new("/nonexistent-data/build-cache/repo/worker-a");
        let profile = confinement.profile(
            std::path::Path::new("/nonexistent-data/worktrees/run"),
            &[mine],
            Some(std::path::Path::new(
                "/nonexistent-data/build-cache/cargo-home",
            )),
        );
        let deny = "(deny file-write* (subpath \"/nonexistent-data/build-cache\"))";
        let allow = "(allow file-write* (subpath \"/nonexistent-data/build-cache/repo/worker-a\"))";
        let home = "(deny file-write* (subpath \"/nonexistent-data/build-cache/cargo-home\"))";
        let (d, a, h) = (
            profile.find(deny).expect(deny),
            profile.find(allow).expect(allow),
            profile.find(home).expect(home),
        );
        assert!(d < a && a < h, "{profile}");
    }

    #[test]
    fn only_a_cargo_timeout_at_the_longest_limit_is_resumable() {
        assert!(super::is_resumable_build_timeout(&super::timeout_message(
            super::MAX_TIMEOUT_SECS,
            true,
            true
        )));
        assert!(super::is_resumable_build_timeout(&super::timeout_message(
            super::MAX_TIMEOUT_SECS,
            true,
            false
        )));
        assert!(!super::is_resumable_build_timeout(&super::timeout_message(
            super::MIN_CARGO_TIMEOUT_SECS,
            true,
            true
        )));
        assert!(!super::is_resumable_build_timeout(&super::timeout_message(
            super::MAX_TIMEOUT_SECS,
            false,
            true
        )));
    }

    #[tokio::test]
    async fn a_shell_without_a_directory_has_no_build_cache() {
        let cache = tempfile::tempdir().unwrap();
        let tool = ExecTool::new().with_build_cache(cache.path().join("c"));
        let out = tool
            .execute(json!({ "command": "printf %s \"${CARGO_TARGET_DIR:-unset}\"" }))
            .await
            .unwrap();
        assert_eq!(out["exit_code"], 0);
        assert_eq!(out["stdout"], "unset");
        assert!(!cache.path().join("c").exists());
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn a_confined_worktree_can_resolve_its_cwd_inside_private_data() {
        let data = tempfile::tempdir().unwrap();
        let repo = data.path().join("repos/source");
        let work = data.path().join("worktrees/repair");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::create_dir_all(work.parent().unwrap()).unwrap();
        let git = |dir: &std::path::Path, args: &[&str]| {
            let out = std::process::Command::new("git")
                .args([
                    "-c",
                    "core.hooksPath=/dev/null",
                    "-c",
                    "user.name=t",
                    "-c",
                    "user.email=t@x.invalid",
                ])
                .args(args)
                .current_dir(dir)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        git(&repo, &["init", "-q"]);
        git(&repo, &["commit", "-q", "--allow-empty", "-m", "init"]);
        git(
            &repo,
            &[
                "worktree",
                "add",
                "-q",
                work.to_str().unwrap(),
                "-b",
                "repair",
            ],
        );
        std::fs::write(
            data.path().join("service.json"),
            "protected-service-sentinel",
        )
        .unwrap();
        let tool = ExecTool::in_dir(&work).confined(vec![data.path().to_path_buf()]);
        let expected = std::fs::canonicalize(&work).unwrap();
        assert_eq!(
            run_in(&tool, "/bin/pwd -P").await,
            (0, format!("{}\n", expected.display()))
        );
        assert_eq!(
            run_in(&tool, &format!("cd '{}' && /bin/pwd -P", work.display())).await,
            (0, format!("{}\n", expected.display()))
        );
        assert_eq!(
            run_in(&tool, "git status --porcelain").await,
            (0, String::new())
        );
        assert_eq!(
            run_in(&tool, "git rev-parse --show-toplevel").await,
            (0, format!("{}\n", expected.display()))
        );
        assert_eq!(run_in(&tool, "echo repaired > result.txt && git add result.txt && git -c user.name=t -c user.email=t@x.invalid commit -q --no-gpg-sign -m repair").await,
                   (0, String::new()));
        let (code, text) = run_in(&tool, "cat ../../service.json").await;
        assert_ne!(code, 0, "private data became readable: {text}");
        assert!(!text.contains("protected-service-sentinel"), "{text}");
        assert_ne!(
            run_in(&tool, "ls ../..").await.0,
            0,
            "private directory became listable"
        );
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn a_confined_shell_commits_from_a_linked_worktree() {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("repo");
        let git = |dir: &std::path::Path, args: &[&str]| {
            assert!(std::process::Command::new("git")
                .args(["-c", "user.name=t", "-c", "user.email=t@x.invalid"])
                .args(args)
                .current_dir(dir)
                .status()
                .unwrap()
                .success());
        };
        std::fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "-q"]);
        git(&repo, &["commit", "-q", "--allow-empty", "-m", "init"]);
        git(&repo, &["worktree", "add", "-q", "../wt", "-b", "work"]);
        let tool = ExecTool::in_dir(root.path().join("wt")).confined(Vec::new());
        let (code, text) = run_in(
            &tool,
            "echo change > f.txt && git add f.txt && \
             git -c user.name=t -c user.email=t@x.invalid commit -q --no-gpg-sign -m change",
        )
        .await;
        assert_eq!(code, 0, "{text}");
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn a_sandboxed_shell_runs_any_program_but_forced_removal() {
        let dir = tempfile::tempdir().unwrap();
        // `id` is not on the allowlist: unsandboxed it is refused.
        assert!(ExecTool::in_dir(dir.path())
            .execute(json!({ "command": "id -u" }))
            .await
            .is_err());
        let tool = ExecTool::in_dir(dir.path()).confined(Vec::new());
        let (code, text) = run_in(&tool, "id -u | paste - -").await;
        assert_eq!(code, 0, "{text}");
        std::fs::create_dir(dir.path().join("build")).unwrap();
        let error = tool
            .execute(json!({ "command": "rm -rf build" }))
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("forced removal"), "{error}");
        assert!(dir.path().join("build").exists());
        assert_eq!(
            run_in(&tool, "rmdir build && echo gone").await,
            (0, "gone\n".into())
        );
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn a_confined_shell_cannot_touch_git_hooks_or_config() {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("repo");
        let git = |dir: &std::path::Path, args: &[&str]| {
            assert!(std::process::Command::new("git")
                .args(["-c", "user.name=t", "-c", "user.email=t@x.invalid"])
                .args(args)
                .current_dir(dir)
                .status()
                .unwrap()
                .success());
        };
        std::fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "-q"]);
        git(&repo, &["commit", "-q", "--allow-empty", "-m", "init"]);
        git(&repo, &["worktree", "add", "-q", "../wt", "-b", "work"]);
        let common = std::fs::canonicalize(repo.join(".git")).unwrap();
        let tool = ExecTool::in_dir(root.path().join("wt")).confined(Vec::new());
        let (code, text) = run_in(
            &tool,
            &format!(
                "echo 'echo pwned' > '{}/hooks/post-commit'",
                common.display()
            ),
        )
        .await;
        assert_ne!(code, 0, "wrote a hook: {text}");
        assert!(!common.join("hooks/post-commit").exists());
        let (code, text) = run_in(&tool, "git config core.pager 'sh -c id'").await;
        assert_ne!(code, 0, "set git config: {text}");
        // Committing, which writes objects, refs and the index, still works.
        let (code, text) = run_in(
            &tool,
            "echo change > f.txt && git add f.txt && \
             git -c user.name=t -c user.email=t@x.invalid commit -q --no-gpg-sign -m change",
        )
        .await;
        assert_eq!(code, 0, "{text}");
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn a_confined_shell_cannot_reach_denied_local_ports() {
        let denied = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let open = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let (denied_port, open_port) = (
            denied.local_addr().unwrap().port(),
            open.local_addr().unwrap().port(),
        );
        let dir = tempfile::tempdir().unwrap();
        let tool = ExecTool::in_dir(dir.path())
            .confined(Vec::new())
            .deny_local_ports(vec![denied_port]);
        let connect = |port: u16| {
            format!(
                "python3 -c \"import socket; socket.create_connection(('127.0.0.1', {port}), 2); \
                 print('connected')\""
            )
        };
        let (code, text) = run_in(&tool, &connect(denied_port)).await;
        assert_ne!(code, 0, "reached a denied port: {text}");
        assert_eq!(
            run_in(&tool, &connect(open_port)).await,
            (0, "connected\n".into())
        );
    }

    #[tokio::test]
    async fn command_timeouts_are_typed_as_timeout() {
        let error = ExecTool::new()
            .execute(json!({"command": "sleep 1", "timeout_secs": 0}))
            .await
            .unwrap_err();

        assert_eq!(error.kind(), ToolErrorKind::Timeout);
    }
}
