use async_trait::async_trait;
use rustykrab_core::types::ToolSchema;
use rustykrab_core::{Result, SandboxRequirements, Tool, ToolError};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::time::timeout;

const MAX_OUTPUT_BYTES: usize = 100 * 1024; // 100KB

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
/// isolated worktree (control-layer plan section 5).
pub struct ExecTool {
    dir: Option<std::path::PathBuf>,
}

impl ExecTool {
    pub fn new() -> Self {
        Self { dir: None }
    }

    /// An `exec` whose commands run in `dir`.
    pub fn in_dir(dir: impl Into<std::path::PathBuf>) -> Self {
        Self {
            dir: Some(dir.into()),
        }
    }
}

impl Default for ExecTool {
    fn default() -> Self {
        Self::new()
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
                    let (delimiter, strip_tabs, after) = heredoc_delimiter(&chars, i + 2)?;
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
/// word, bare or quoted. Returns the word, the tab flag, and the index after.
fn heredoc_delimiter(
    chars: &[(usize, char)],
    mut at: usize,
) -> std::result::Result<(String, bool, usize), String> {
    let strip_tabs = chars.get(at).is_some_and(|&(_, c)| c == '-');
    if strip_tabs {
        at += 1;
    }
    while chars.get(at).is_some_and(|&(_, c)| c == ' ' || c == '\t') {
        at += 1;
    }
    let mut word = String::new();
    let mut quote: Option<char> = None;
    while let Some(&(_, c)) = chars.get(at) {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => word.push(c),
            None if c == '\'' || c == '"' => quote = Some(c),
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
    Ok((word, strip_tabs, at))
}

/// Validate that all commands in a potentially piped command are allowed.
///
/// The allowlist prevents arbitrary binary execution. Variable expansion,
/// redirects, and other shell features are permitted since commands run
/// via `sh -c` and the agent needs these for real-world tasks (e.g.
/// `echo $HOME`, `python3 script.py > output.txt`).
fn validate_command(command: &str) -> std::result::Result<(), String> {
    // Newlines separate commands, and each is checked below. A carriage
    // return separates nothing and has no use here.
    if command.contains('\r') {
        return Err("command contains carriage return characters".into());
    }

    // Block shell substitution patterns that bypass the allowlist.
    // Since the command runs via `sh -c`, constructs like `$(cmd)`, `\`cmd\``,
    // and `${var}` can execute arbitrary commands even if the outer command
    // is in the allowlist (e.g. `echo $(rm -rf /)`).
    if command.contains("$(") || command.contains('`') {
        return Err(
            "command contains shell substitution ($() or backticks) which is not allowed".into(),
        );
    }

    // Split on unquoted shell control operators and validate every command
    // segment. Empty segments account for the second byte in && and ||.
    for segment in command_segments(command)? {
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
        "Execute a shell command and return its output. Commands are validated against an allowlist."
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
                        "description": "Timeout in seconds (default: 30, max: 120)"
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

        // Validate command against allowlist
        validate_command(command).map_err(|e| {
            rustykrab_core::Error::ToolExecution(ToolError::invalid_input(format!(
                "command rejected: {e}"
            )))
        })?;

        let timeout_secs = args["timeout_secs"].as_u64().unwrap_or(30).min(120);

        // Inherit the user's PATH so tools installed via homebrew, cargo,
        // pip, nvm, etc. are available. Fall back to a sensible default.
        let path = std::env::var("PATH")
            .unwrap_or_else(|_| "/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin".to_string());

        let mut process = tokio::process::Command::new("sh");
        process
            .arg("-c")
            .arg(command)
            .env_clear()
            .env("PATH", &path)
            .env("HOME", "/tmp/rustykrab-home")
            .env("LANG", "C.UTF-8");
        if let Some(dir) = &self.dir {
            process.current_dir(dir);
        }
        let future = process.output();

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
        // Substitution stays blocked, in a body as anywhere else.
        assert!(validate_command("cat <<EOF\n$(definitely-not-allowed)\nEOF").is_err());
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

    #[tokio::test]
    async fn command_timeouts_are_typed_as_timeout() {
        let error = ExecTool::new()
            .execute(json!({"command": "sleep 1", "timeout_secs": 0}))
            .await
            .unwrap_err();

        assert_eq!(error.kind(), ToolErrorKind::Timeout);
    }
}
