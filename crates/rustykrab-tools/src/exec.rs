use async_trait::async_trait;
use rustykrab_core::types::ToolSchema;
use rustykrab_core::{Result, SandboxRequirements, Tool, ToolError};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::time::timeout;

/// The most of a command's output that goes back to the model, stdout and
/// stderr together: 30 KB, about what Claude Code's Bash tool keeps (30,000
/// characters). It was 100 KB per stream; one `unittest -v` run then put
/// 42K tokens into a qwen3.8 conversation in a single turn, and the next
/// request ran Ollama out of GPU memory at about 104K tokens of context
/// (long-context eval, 2026-10-16).
const MAX_OUTPUT_BYTES: usize = 30 * 1024;

/// A command's time limit when the call names none: long enough for a
/// test suite or a build step, as feather-harness's 120 s default is.
pub const DEFAULT_TIMEOUT_SECS: u64 = 120;
/// The longest a call may ask for. The runner's own limit on an exec call
/// sits just above it, so the command's limit is the one that applies.
pub const MAX_TIMEOUT_SECS: u64 = 600;

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
}

/// macOS's sandbox runner (Seatbelt).
const SANDBOX: &str = "/usr/bin/sandbox-exec";

impl ExecTool {
    pub fn new() -> Self {
        Self {
            dir: None,
            confinement: None,
        }
    }

    /// An `exec` whose commands run in `dir`.
    pub fn in_dir(dir: impl Into<std::path::PathBuf>) -> Self {
        Self {
            dir: Some(dir.into()),
            confinement: None,
        }
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
        sandboxed.arg("-p").arg(confinement.profile(dir)).arg("sh");
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
/// its secrets file is), its own directory always excepted. Reads elsewhere
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
/// daemon's API and the model server).
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
    fn profile(&self, dir: &std::path::Path) -> String {
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
        let own: Vec<String> = own.iter().map(|p| quoted(p)).collect();
        let private: Vec<String> = self
            .private
            .iter()
            .filter_map(|p| real(p))
            .map(|p| quoted(&p))
            .collect();
        let mut profile = format!(
            "(version 1)(allow default)(deny file-write*)\
             (allow file-write* {} (subpath \"/private/tmp\") \
             (subpath \"/private/var/folders\") (subpath \"/dev\"))",
            own.join(" ")
        );
        if !private.is_empty() {
            profile.push_str(&format!("(deny file-read* {})", private.join(" ")));
            profile.push_str(&format!("(allow file-read* {})", own.join(" ")));
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
fn rustup_home() -> Option<std::path::PathBuf> {
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

/// What of a command's stdout and stderr goes back to the model: both whole
/// when together they fit [`MAX_OUTPUT_BYTES`], otherwise each stream's head
/// and tail, the larger stream giving way first. A cut stream is written
/// whole to a file under [`full_output_dir`] and its marker names the path,
/// so the model can read or grep the rest with another command.
fn bound_output(stdout: String, stderr: String) -> (String, String) {
    let (out_len, err_len) = (stdout.len(), stderr.len());
    if out_len + err_len <= MAX_OUTPUT_BYTES {
        return (stdout, stderr);
    }
    let half = MAX_OUTPUT_BYTES / 2;
    let err_budget = if err_len <= half {
        err_len
    } else if out_len <= half {
        MAX_OUTPUT_BYTES - out_len
    } else {
        half
    };
    let out_budget = MAX_OUTPUT_BYTES - err_budget;
    (
        head_and_tail(stdout, out_budget, "stdout"),
        head_and_tail(stderr, err_budget, "stderr"),
    )
}

/// `s` cut to its first and last `budget / 2` bytes around a marker that
/// names the file holding all of it; `s` itself when it fits.
fn head_and_tail(s: String, budget: usize, stream: &str) -> String {
    if s.len() <= budget {
        return s;
    }
    // Char boundaries, so a cut never splits a multi-byte UTF-8 character.
    let head_end = s.floor_char_boundary(budget / 2);
    let tail_start = s.ceil_char_boundary(s.len() - budget / 2);
    let omitted = tail_start - head_end;
    let saved = match save_full_output(&s, stream) {
        Ok(path) => format!("the whole {stream} is in {}", path.display()),
        Err(e) => format!("the whole {stream} could not be saved: {e}"),
    };
    format!(
        "{}\n\n... [{omitted} bytes omitted; {saved}] ...\n\n{}",
        &s[..head_end],
        &s[tail_start..]
    )
}

/// Where cut output is kept: the daemon's temporary directory, which a
/// sandboxed command may read and which the OS clears.
fn full_output_dir() -> std::path::PathBuf {
    std::env::temp_dir().join("rustykrab-exec-output")
}

/// How long a saved output is kept: long enough for any run that cut it.
const FULL_OUTPUT_KEEP: Duration = Duration::from_secs(24 * 60 * 60);

fn save_full_output(s: &str, stream: &str) -> std::io::Result<std::path::PathBuf> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let dir = full_output_dir();
    std::fs::create_dir_all(&dir)?;
    // Each save prunes what earlier runs left, so the directory stays small
    // without a sweeper of its own.
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let stale = entry
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.elapsed().ok())
                .is_some_and(|age| age > FULL_OUTPUT_KEEP);
            if stale {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis());
    let path = dir.join(format!(
        "{millis}-{}-{}.{stream}.txt",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(&path, s)?;
    Ok(path)
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
    #[derive(Clone, Copy)]
    enum Quote {
        Single,
        Double,
    }
    let chars: Vec<(usize, char)> = command.char_indices().collect();
    let mut segments = Vec::new();
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
                                return Ok(segments);
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
    Ok(segments)
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
                        "description": "Timeout in seconds (default: 120, max: 600). Raise it for a long build or test run."
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
        let checked = if self.sandbox().is_some() {
            refuse_forced_removal(command)
        } else {
            validate_command(command).and_then(|()| refuse_forced_removal(command))
        };
        checked.map_err(|e| {
            rustykrab_core::Error::ToolExecution(ToolError::invalid_input(format!(
                "command rejected: {e}"
            )))
        })?;

        let timeout_secs = args["timeout_secs"]
            .as_u64()
            .unwrap_or(DEFAULT_TIMEOUT_SECS)
            .min(MAX_TIMEOUT_SECS);

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
                rustykrab_core::Error::ToolExecution(ToolError::timeout(format!(
                    "command timed out after {timeout_secs}s"
                )))
            })?
            .map_err(|e| {
                rustykrab_core::Error::ToolExecution(ToolError::transient(e.to_string()))
            })?;

        let (stdout, stderr) = bound_output(
            String::from_utf8_lossy(&output.stdout).into_owned(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        );
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

#[cfg(test)]
mod output_bound_tests {
    use super::{bound_output, MAX_OUTPUT_BYTES};

    /// The path a cut stream's marker names.
    fn saved_path(cut: &str) -> std::path::PathBuf {
        let start = cut.find(" is in ").expect("marker names a file") + " is in ".len();
        let end = cut[start..].find(']').unwrap() + start;
        cut[start..end].into()
    }

    #[test]
    fn output_that_fits_comes_back_whole() {
        let (out, err) = bound_output("a".repeat(20_000), "b".repeat(10_000));
        assert_eq!(out, "a".repeat(20_000));
        assert_eq!(err, "b".repeat(10_000));
    }

    #[test]
    fn long_output_keeps_its_head_and_tail_and_saves_the_rest() {
        // A test run's first failures are at the top and its summary at the
        // bottom; both must survive the cut.
        let mut out = String::from("FIRST FAILURE\n");
        out.push_str(&"x".repeat(200_000));
        out.push_str("\nRan 74 tests: FAILED (failures=1)");
        let (cut, err) = bound_output(out.clone(), String::new());
        assert!(err.is_empty());
        assert!(cut.len() < MAX_OUTPUT_BYTES + 300, "{}", cut.len());
        assert!(cut.starts_with("FIRST FAILURE"));
        assert!(cut.ends_with("FAILED (failures=1)"));
        assert!(cut.contains("bytes omitted"));
        let path = saved_path(&cut);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), out);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn short_stderr_survives_long_stdout_whole() {
        let (out, err) = bound_output("o".repeat(100_000), "error: E306 at line 18".into());
        assert_eq!(err, "error: E306 at line 18");
        assert!(out.len() + err.len() < MAX_OUTPUT_BYTES + 300);
        let _ = std::fs::remove_file(saved_path(&out));
    }

    #[test]
    fn two_long_streams_share_the_budget() {
        let (out, err) = bound_output("o".repeat(80_000), "e".repeat(80_000));
        assert!(out.contains("stdout is in") && err.contains("stderr is in"));
        assert!(out.len() + err.len() < MAX_OUTPUT_BYTES + 600);
        for cut in [out, err] {
            let _ = std::fs::remove_file(saved_path(&cut));
        }
    }

    #[test]
    fn a_cut_never_splits_a_character() {
        // Four-byte characters put every cut point mid-character unless it
        // moves to a boundary.
        let (out, _) = bound_output("😀".repeat(20_000), String::new());
        assert!(out.starts_with('😀') && out.ends_with('😀'));
        let _ = std::fs::remove_file(saved_path(&out));
    }
}
