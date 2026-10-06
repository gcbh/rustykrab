//! `rustykrab workers` and `rustykrab worker ...`: the worker registry from
//! the terminal (`docs/plans/control-layer-and-worker-fleet.md`, sections 5
//! and 14).
//!
//! Like `rustykrab work`, every command is a call to the running daemon,
//! here its `/api/workers` routes, so a worker added from the terminal is
//! leasable at once without a restart.

use std::path::{Path, PathBuf};

use rustykrab_control::registry::{WorkerSpec, WorkerView};
use rustykrab_core::work::WorkerKind;
use rustykrab_gateway::worker_routes::{Removed, WorkerList};

use crate::work_cmd::Daemon;

const USAGE: &str = "\
usage: rustykrab workers
       rustykrab worker <command>

  workers                        every named worker: kind, health, cost tier,
                                 repositories and routing record
  worker show <name>             one worker
  worker add <claude_code|codex> [--name N] --repos PATH[,PATH...]
             [--command PATH] [--model M] [--max-turns N]
             [--permission-mode M] [--timeout SECONDS]
             [--allowed-tools T[,T...]] [--denied-tools T[,T...]]
             [--cost-tier N] [--env VAR[,VAR...]]
                                 add an external coding worker; the registry
                                 names it when --name is left out;
                                 --denied-tools adds to claude_code's deny
                                 list (codex has none, so it is refused there)
  worker add peer --url URL (--pairing-code CODE | --token TOKEN)
             [--name N] [--timeout SECONDS] [--concurrency N] [--cost-tier N]
                                 add a paired node on the tailnet: the code
                                 its `rustykrab-cli pair` printed is redeemed
                                 for a token of this daemon's own
  worker remove <name>           remove an external worker or a peer

Talks to the daemon at RUSTYKRAB_GATEWAY_URL (default http://127.0.0.1:3000).";

#[derive(Debug, Clone, PartialEq, Eq)]
enum Command {
    Help,
    List,
    Show(String),
    Add(Box<WorkerSpec>),
    Remove(String),
}

/// Entry point from `main`: `args` start at `workers` or `worker`.
pub async fn run(data_dir: &Path, args: &[String]) -> anyhow::Result<()> {
    let command = match parse(args) {
        Ok(Command::Help) => {
            println!("{USAGE}");
            return Ok(());
        }
        Ok(command) => command,
        Err(message) => {
            eprintln!("{message}\n\n{USAGE}");
            std::process::exit(2);
        }
    };
    let daemon = Daemon::connect(data_dir).await?;
    let out = match command {
        Command::Help => USAGE.to_string(),
        Command::List => {
            let list: WorkerList = daemon.get(&["api", "workers"], &[]).await?;
            render_list(&list.workers)
        }
        Command::Show(name) => {
            let view: WorkerView = daemon.get(&["api", "workers", &name], &[]).await?;
            render_one(&view)
        }
        Command::Add(spec) => {
            let mut body = serde_json::to_value(&spec)?;
            // A spec never serialises its secrets (they are kept out of the
            // stored spec and every view), so the request carries them by
            // hand, once, to the daemon that keeps them.
            if let Some(token) = &spec.token {
                body["token"] = serde_json::json!(token);
            }
            if let Some(code) = &spec.pairing_code {
                body["pairing_code"] = serde_json::json!(code);
            }
            let view: WorkerView = daemon.post(&["api", "workers"], Some(body)).await?;
            format!("added {}\n{}", view.name, render_one(&view))
        }
        Command::Remove(name) => {
            let removed: Removed = daemon.delete(&["api", "workers", &name]).await?;
            format!("removed {}\n", removed.name)
        }
    };
    print!("{out}");
    Ok(())
}

fn parse(args: &[String]) -> Result<Command, String> {
    let words: Vec<&str> = args.iter().map(String::as_str).collect();
    match words.as_slice() {
        ["workers"] | ["worker"] | ["worker", "list"] => Ok(Command::List),
        ["workers", "help" | "-h" | "--help"] | ["worker", "help" | "-h" | "--help"] => {
            Ok(Command::Help)
        }
        ["worker", "show", name] => Ok(Command::Show(name.to_string())),
        ["worker", "remove" | "rm", name] => Ok(Command::Remove(name.to_string())),
        ["worker", "add", kind, rest @ ..] => {
            parse_add(kind, rest).map(|spec| Command::Add(Box::new(spec)))
        }
        _ => Err(format!("unknown worker command: {}", words.join(" "))),
    }
}

/// `worker add <kind> [flags]`. Repository paths are made absolute here,
/// since the daemon runs elsewhere.
fn parse_add(kind: &str, rest: &[&str]) -> Result<WorkerSpec, String> {
    let kind = WorkerKind::parse(kind)
        .filter(|k| {
            matches!(
                k,
                WorkerKind::ClaudeCode | WorkerKind::Codex | WorkerKind::Peer
            )
        })
        .ok_or_else(|| format!("worker kind must be claude_code, codex or peer, not `{kind}`"))?;
    let mut spec = WorkerSpec {
        kind,
        ..WorkerSpec::default()
    };
    let mut i = 0;
    while i < rest.len() {
        let flag = rest[i];
        let value = |i: usize| -> Result<&str, String> {
            rest.get(i + 1)
                .copied()
                .filter(|v| !v.starts_with("--"))
                .ok_or_else(|| format!("{flag} needs a value"))
        };
        let number = |raw: &str| -> Result<u64, String> {
            raw.parse::<u64>()
                .map_err(|_| format!("{flag} takes a number, not `{raw}`"))
        };
        match flag {
            "--repos" | "--repo" => {
                // Paths until the next flag, each possibly comma separated.
                let mut j = i + 1;
                while j < rest.len() && !rest[j].starts_with("--") {
                    for part in rest[j].split(',').filter(|p| !p.trim().is_empty()) {
                        spec.repos.push(absolute(part.trim()));
                    }
                    j += 1;
                }
                if j == i + 1 {
                    return Err("--repos needs at least one path".into());
                }
                i = j;
                continue;
            }
            "--name" => spec.name = Some(value(i)?.to_string()),
            "--command" => spec.command = Some(absolute_if_path(value(i)?)),
            "--model" => spec.model = Some(value(i)?.to_string()),
            "--permission-mode" => spec.permission_mode = Some(value(i)?.to_string()),
            "--max-turns" => spec.max_turns = Some(number(value(i)?)? as u32),
            "--timeout" => spec.timeout_seconds = Some(number(value(i)?)?),
            "--cost-tier" => spec.cost_tier = Some(number(value(i)?)? as u32),
            "--allowed-tools" => {
                spec.allowed_tools = split_list(value(i)?);
            }
            "--denied-tools" => {
                spec.denied_tools = split_list(value(i)?);
            }
            "--env" => spec.env = split_list(value(i)?),
            "--url" => spec.base_url = Some(value(i)?.to_string()),
            "--token" => spec.token = Some(value(i)?.to_string()),
            "--pairing-code" => spec.pairing_code = Some(value(i)?.to_string()),
            "--concurrency" => spec.concurrency = Some(number(value(i)?)? as usize),
            other => return Err(format!("unknown flag {other}")),
        }
        i += 2;
    }
    if kind == WorkerKind::Peer {
        if spec.base_url.is_none() {
            return Err("worker add peer needs --url: its node's gateway".into());
        }
        if spec.token.is_none() && spec.pairing_code.is_none() {
            return Err("worker add peer needs --pairing-code or --token".into());
        }
        return Ok(spec);
    }
    if spec.repos.is_empty() {
        return Err("worker add needs --repos: the repositories it may work in".into());
    }
    if kind == WorkerKind::Codex && !spec.denied_tools.is_empty() {
        return Err(
            "codex has no deny list, so --denied-tools would not be enforced; \
             drop it or add a claude_code worker"
                .into(),
        );
    }
    Ok(spec)
}

fn split_list(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

fn absolute(path: &str) -> String {
    let p = PathBuf::from(path);
    let p = if p.is_absolute() {
        p
    } else {
        std::env::current_dir().map(|d| d.join(&p)).unwrap_or(p)
    };
    p.canonicalize().unwrap_or(p).display().to_string()
}

/// A command given as a path is made absolute; a bare name stays a name
/// for the daemon's `PATH`.
fn absolute_if_path(command: &str) -> String {
    if command.contains('/') {
        absolute(command)
    } else {
        command.to_string()
    }
}

fn render_list(workers: &[WorkerView]) -> String {
    if workers.is_empty() {
        return "no workers\n".to_string();
    }
    workers.iter().map(render_line).collect()
}

fn render_line(w: &WorkerView) -> String {
    let mut line = format!(
        "{:<12} {:<11} {:<10} tier {}",
        w.name,
        w.kind,
        if w.live {
            w.health.as_str()
        } else {
            "not running"
        },
        w.cost_tier
    );
    if !w.capabilities.repos.is_empty() {
        line.push_str(&format!("  repos: {}", w.capabilities.repos.join(", ")));
    }
    for (class, r) in &w.routing_record {
        line.push_str(&format!(
            "  {class}: {} verified, {} not verified{}",
            r.verified_done,
            r.claimed_not_verified,
            if r.probation { ", on probation" } else { "" }
        ));
    }
    line.push('\n');
    line
}

fn render_one(w: &WorkerView) -> String {
    let mut out = render_line(w);
    if let Some(seen) = w.last_seen {
        out.push_str(&format!(
            "  last seen {}\n",
            seen.format("%Y-%m-%d %H:%M UTC")
        ));
    }
    if let Some(spec) = &w.spec {
        if let Some(command) = &spec.command {
            out.push_str(&format!("  command {command}\n"));
        }
        if let Some(url) = &spec.base_url {
            out.push_str(&format!("  node {url}\n"));
        }
        if !spec.denied_tools.is_empty() {
            out.push_str(&format!("  denied {}\n", spec.denied_tools.join(", ")));
        }
    }
    if let Some(machine) = &w.capabilities.machine {
        out.push_str(&format!("  machine {machine}\n"));
    }
    if w.kind == WorkerKind::Peer.as_str() && !w.capabilities.tools.is_empty() {
        out.push_str(&format!("  tools {}\n", w.capabilities.tools.join(", ")));
    }
    for (class, r) in &w.routing_record {
        out.push_str(&format!(
            "  {class}: verified {} / claimed not verified {} / failed {} / repairs {} / runs {} ({}s, {} tokens)\n",
            r.verified_done,
            r.claimed_not_verified,
            r.failed,
            r.repairs,
            r.cost.runs,
            r.cost.wall_seconds,
            r.cost.tokens
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(line: &str) -> Vec<String> {
        line.split_whitespace().map(str::to_string).collect()
    }

    #[test]
    fn worker_add_reads_the_plan_s_example() {
        let repo = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let line = format!(
            "worker add claude_code --name pinch --repos {},{} --max-turns 12 --timeout 600",
            repo.path().display(),
            other.path().display()
        );
        let Command::Add(spec) = parse(&words(&line)).unwrap() else {
            panic!("not an add")
        };
        assert_eq!(spec.kind, WorkerKind::ClaudeCode);
        assert_eq!(spec.name.as_deref(), Some("pinch"));
        assert_eq!(spec.repos.len(), 2);
        assert_eq!(
            PathBuf::from(&spec.repos[0]),
            repo.path().canonicalize().unwrap()
        );
        assert_eq!(spec.max_turns, Some(12));
        assert_eq!(spec.timeout_seconds, Some(600));
        assert!(spec.denied_tools.is_empty());

        let Command::Add(spec) = parse(&words(&format!(
            "worker add claude_code --repos {} --denied-tools Read(~/.config/**),WebFetch",
            repo.path().display()
        )))
        .unwrap() else {
            panic!("not an add")
        };
        assert_eq!(spec.denied_tools, ["Read(~/.config/**)", "WebFetch"]);

        let Command::Add(spec) = parse(&words(&format!(
            "worker add codex --repos {} {}",
            repo.path().display(),
            other.path().display()
        )))
        .unwrap() else {
            panic!("not an add")
        };
        assert_eq!(spec.kind, WorkerKind::Codex);
        assert_eq!(spec.repos.len(), 2, "space separated too");
        assert!(spec.name.is_none(), "the registry names it");

        let refused = parse(&words(&format!(
            "worker add codex --repos {} --denied-tools WebFetch",
            repo.path().display()
        )));
        assert!(
            refused
                .as_ref()
                .is_err_and(|e| e.contains("--denied-tools")),
            "codex cannot enforce a deny list: {:?}",
            refused.err()
        );
    }

    #[test]
    fn worker_add_peer_takes_a_node_and_a_way_in() {
        let Command::Add(spec) = parse(&words(
            "worker add peer --name krabby --url https://m4.tailnet.ts.net --pairing-code ABCD2345",
        ))
        .unwrap() else {
            panic!("not an add")
        };
        assert_eq!(spec.kind, WorkerKind::Peer);
        assert_eq!(spec.base_url.as_deref(), Some("https://m4.tailnet.ts.net"));
        assert_eq!(spec.pairing_code.as_deref(), Some("ABCD2345"));
        assert!(spec.repos.is_empty(), "a peer needs no repositories here");
        assert!(
            parse(&words("worker add peer --token t")).is_err(),
            "no url"
        );
        assert!(
            parse(&words("worker add peer --url http://n")).is_err(),
            "no token or code"
        );
    }

    #[test]
    fn worker_commands_refuse_what_they_cannot_do() {
        assert_eq!(parse(&words("workers")).unwrap(), Command::List);
        assert_eq!(
            parse(&words("worker remove pinch")).unwrap(),
            Command::Remove("pinch".into())
        );
        assert!(parse(&words("worker add local --repos /tmp")).is_err());
        assert!(parse(&words("worker add claude_code")).is_err(), "no repos");
        assert!(parse(&words("worker add claude_code --repos /tmp --bogus")).is_err());
        assert!(parse(&words("worker add claude_code --repos /tmp --max-turns x")).is_err());
    }
}
