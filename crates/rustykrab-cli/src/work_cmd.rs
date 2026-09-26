//! `rustykrab work ...`: the control layer from the terminal
//! (`docs/plans/control-layer-and-worker-fleet.md`, section 14.2).
//!
//! Every subcommand is one or two calls to the running daemon's `/api/work`
//! routes, made the way `chat` makes its calls: the gateway URL from
//! `RUSTYKRAB_GATEWAY_URL`, the daemon's bearer token, and the gateway's
//! own origin on every request. Nothing here opens the store or builds a
//! controller; a command enters as a typed request the controller applies
//! (section 11). Rendering is pure and lives in [`render`].
//!
//! Ids print as `#` plus their first eight characters, and any unambiguous
//! prefix (with or without the `#`) is accepted back.

mod render;

use std::path::Path;
use std::time::Duration;

use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, ORIGIN};
use reqwest::{StatusCode, Url};
use serde::de::DeserializeOwned;
use serde_json::{json, Value};

use rustykrab_control::handle::TickReport;
use rustykrab_gateway::work_routes::{
    ApproveReply, ArchiveList, CancelReply, GraphReply, ItemDetail, PlanPreview, WorkList,
};
use rustykrab_store::ArchivedItem;

const DEFAULT_GATEWAY_URL: &str = "http://127.0.0.1:3000";

/// The length of a UUID, the store's id: anything this long is used as
/// given rather than resolved as a prefix.
const FULL_ID: usize = 36;

const USAGE: &str = "\
usage: rustykrab work <command>

  ready                          items ready to run
  list [--status S] [--kind K] [--parent ID] [--all]
                                 open items, each parent as one roll-up line;
                                 --all includes closed ones
  show <id> [--graph]            one item, or the tree under it
  plan <id>                      the plan awaiting approval under <id>
  approve <id>                   release the items the plan holds
  reject <id> [reason]           cancel the held items, with cascade
  cancel <id> [reason]           cancel an item and its open subtree
  archive [list] [--kind K] [--since DATE]
  archive show <id>
  archive search <text>          compacted items, one line each
  tick                           run one pass of the controller loop

Talks to the daemon at RUSTYKRAB_GATEWAY_URL (default http://127.0.0.1:3000).";

/// Entry point from `main`: `args` are the words after `work`.
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
    let client = Daemon::connect(data_dir).await?;
    print!("{}", execute(&client, command).await?);
    Ok(())
}

// ── parsing ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
enum Command {
    Help,
    Ready,
    List {
        status: Option<String>,
        kind: Option<String>,
        parent: Option<String>,
        all: bool,
    },
    Show {
        id: String,
        graph: bool,
    },
    Plan {
        id: String,
    },
    Approve {
        id: String,
    },
    Reject {
        id: String,
        reason: Option<String>,
    },
    Cancel {
        id: String,
        reason: Option<String>,
    },
    ArchiveList {
        kind: Option<String>,
        since: Option<String>,
    },
    ArchiveShow {
        id: String,
    },
    ArchiveSearch {
        text: String,
    },
    Tick,
}

fn parse(args: &[String]) -> Result<Command, String> {
    let words: Vec<&str> = args.iter().map(String::as_str).collect();
    let Some((&verb, rest)) = words.split_first() else {
        return Ok(Command::Help);
    };
    match verb {
        "help" | "-h" | "--help" => Ok(Command::Help),
        "ready" => no_more(rest, Command::Ready),
        "list" | "ls" => {
            let flags = Flags::parse(rest, &["status", "kind", "parent"], &["all"])?;
            flags.no_words()?;
            Ok(Command::List {
                status: flags.value("status"),
                kind: flags.value("kind"),
                parent: flags.value("parent"),
                all: flags.switch("all"),
            })
        }
        "show" => {
            let flags = Flags::parse(rest, &[], &["graph"])?;
            Ok(Command::Show {
                id: flags.one_word("show <id>")?,
                graph: flags.switch("graph"),
            })
        }
        "plan" => Ok(Command::Plan {
            id: one_id(rest, "plan <id>")?,
        }),
        "approve" => Ok(Command::Approve {
            id: one_id(rest, "approve <id>")?,
        }),
        "reject" => {
            let (id, reason) = id_and_reason(rest, "reject <id> [reason]")?;
            Ok(Command::Reject { id, reason })
        }
        "cancel" => {
            let (id, reason) = id_and_reason(rest, "cancel <id> [reason]")?;
            Ok(Command::Cancel { id, reason })
        }
        "archive" => parse_archive(rest),
        "tick" => no_more(rest, Command::Tick),
        other => Err(format!("unknown work command `{other}`")),
    }
}

fn parse_archive(rest: &[&str]) -> Result<Command, String> {
    match rest.split_first() {
        Some((&"show", tail)) => Ok(Command::ArchiveShow {
            id: one_id(tail, "archive show <id>")?,
        }),
        Some((&"search", tail)) => {
            let text = tail.join(" ");
            if text.trim().is_empty() {
                return Err("usage: work archive search <text>".into());
            }
            Ok(Command::ArchiveSearch { text })
        }
        Some((&"list", tail)) => archive_list(tail),
        _ => archive_list(rest),
    }
}

fn archive_list(rest: &[&str]) -> Result<Command, String> {
    let flags = Flags::parse(rest, &["kind", "since"], &[])?;
    flags.no_words()?;
    Ok(Command::ArchiveList {
        kind: flags.value("kind"),
        since: flags.value("since"),
    })
}

fn no_more(rest: &[&str], command: Command) -> Result<Command, String> {
    match rest.first() {
        None => Ok(command),
        Some(extra) => Err(format!("unexpected argument `{extra}`")),
    }
}

fn one_id(rest: &[&str], usage: &str) -> Result<String, String> {
    match rest {
        [id] => Ok((*id).to_string()),
        _ => Err(format!("usage: work {usage}")),
    }
}

fn id_and_reason(rest: &[&str], usage: &str) -> Result<(String, Option<String>), String> {
    let Some((id, reason)) = rest.split_first() else {
        return Err(format!("usage: work {usage}"));
    };
    let reason = reason.join(" ");
    let reason = (!reason.trim().is_empty()).then(|| reason.trim().to_string());
    Ok((id.to_string(), reason))
}

/// `--name value`, `--name=value` and bare `--switch` flags, plus the
/// positional words left over.
struct Flags {
    values: Vec<(String, String)>,
    switches: Vec<String>,
    words: Vec<String>,
}

impl Flags {
    fn parse(rest: &[&str], valued: &[&str], switches: &[&str]) -> Result<Flags, String> {
        let mut flags = Flags {
            values: Vec::new(),
            switches: Vec::new(),
            words: Vec::new(),
        };
        let mut iter = rest.iter();
        while let Some(&word) = iter.next() {
            let Some(flag) = word.strip_prefix("--") else {
                flags.words.push(word.to_string());
                continue;
            };
            let (name, inline) = match flag.split_once('=') {
                Some((name, value)) => (name, Some(value.to_string())),
                None => (flag, None),
            };
            if valued.contains(&name) {
                let value = match inline {
                    Some(value) => value,
                    None => iter
                        .next()
                        .map(|v| v.to_string())
                        .ok_or_else(|| format!("--{name} needs a value"))?,
                };
                flags.values.push((name.to_string(), value));
            } else if switches.contains(&name) && inline.is_none() {
                flags.switches.push(name.to_string());
            } else {
                return Err(format!("unknown flag `{word}`"));
            }
        }
        Ok(flags)
    }

    fn value(&self, name: &str) -> Option<String> {
        self.values
            .iter()
            .rev()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.clone())
    }

    fn switch(&self, name: &str) -> bool {
        self.switches.iter().any(|s| s == name)
    }

    fn no_words(&self) -> Result<(), String> {
        match self.words.first() {
            None => Ok(()),
            Some(extra) => Err(format!("unexpected argument `{extra}`")),
        }
    }

    fn one_word(&self, usage: &str) -> Result<String, String> {
        match self.words.as_slice() {
            [word] => Ok(word.clone()),
            _ => Err(format!("usage: work {usage}")),
        }
    }
}

/// The one id among `candidates` that `raw` names: an exact id, or a
/// unique prefix, with or without a leading `#`. `None` when nothing
/// matches, so the daemon can say whether the id is unknown or archived.
fn resolve_among<'a>(
    raw: &str,
    candidates: impl IntoIterator<Item = &'a str>,
) -> Result<Option<String>, String> {
    let wanted = raw.trim().trim_start_matches('#');
    let mut matches: Vec<&str> = Vec::new();
    for id in candidates {
        if id == wanted {
            return Ok(Some(id.to_string()));
        }
        if id.starts_with(wanted) && !matches.contains(&id) {
            matches.push(id);
        }
    }
    match matches.as_slice() {
        [] => Ok(None),
        [one] => Ok(Some(one.to_string())),
        many => Err(format!(
            "#{wanted} is ambiguous: {}",
            many.iter()
                .map(|id| format!("#{id}"))
                .collect::<Vec<_>>()
                .join(" ")
        )),
    }
}

// ── the daemon ─────────────────────────────────────────────────────────

struct Daemon {
    http: reqwest::Client,
    base: Url,
}

/// The daemon's refusal: its status and its own words.
#[derive(Debug)]
struct Refused {
    status: StatusCode,
    message: String,
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.message, self.status)
    }
}

impl std::error::Error for Refused {}

/// Whether the daemon found no live item: unknown, or aged into the
/// archive.
fn not_live(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<Refused>()
        .is_some_and(|r| matches!(r.status, StatusCode::NOT_FOUND | StatusCode::GONE))
}

impl Daemon {
    async fn connect(data_dir: &Path) -> anyhow::Result<Daemon> {
        let raw =
            std::env::var("RUSTYKRAB_GATEWAY_URL").unwrap_or_else(|_| DEFAULT_GATEWAY_URL.into());
        let base = Url::parse(&raw)
            .map_err(|e| anyhow::anyhow!("invalid RUSTYKRAB_GATEWAY_URL `{raw}`: {e}"))?;
        if !matches!(base.scheme(), "http" | "https") || base.host().is_none() {
            anyhow::bail!("RUSTYKRAB_GATEWAY_URL must be an http(s) URL with a host");
        }
        let token = resolve_auth_token(data_dir).await?;
        Daemon::new(base, &token)
    }

    fn new(base: Url, token: &str) -> anyhow::Result<Daemon> {
        let mut headers = HeaderMap::new();
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {token}"))
                .map_err(|e| anyhow::anyhow!("invalid auth token: {e}"))?,
        );
        // The gateway requires an Origin on every /api request; this
        // trusted loopback client supplies the gateway's own.
        headers.insert(
            ORIGIN,
            HeaderValue::from_str(&base.origin().ascii_serialization())
                .map_err(|e| anyhow::anyhow!("invalid gateway origin: {e}"))?,
        );
        let http = reqwest::Client::builder()
            .default_headers(headers)
            .timeout(Duration::from_secs(120))
            .build()?;
        Ok(Daemon { http, base })
    }

    fn url(&self, segments: &[&str], query: &[(&str, &str)]) -> Url {
        let mut url = self.base.clone();
        if let Ok(mut path) = url.path_segments_mut() {
            path.pop_if_empty().extend(segments);
        }
        if !query.is_empty() {
            url.query_pairs_mut().extend_pairs(query);
        }
        url
    }

    async fn get<T: DeserializeOwned>(
        &self,
        segments: &[&str],
        query: &[(&str, &str)],
    ) -> anyhow::Result<T> {
        let url = self.url(segments, query);
        self.read(self.http.get(url.clone()), &url).await
    }

    async fn post<T: DeserializeOwned>(
        &self,
        segments: &[&str],
        body: Option<Value>,
    ) -> anyhow::Result<T> {
        let url = self.url(segments, &[]);
        let mut request = self.http.post(url.clone());
        if let Some(body) = body {
            request = request.json(&body);
        }
        self.read(request, &url).await
    }

    async fn read<T: DeserializeOwned>(
        &self,
        request: reqwest::RequestBuilder,
        url: &Url,
    ) -> anyhow::Result<T> {
        let response = request.send().await.map_err(|e| {
            anyhow::anyhow!(
                "could not reach the daemon at {}: {e}. Is it running?",
                self.base
            )
        })?;
        let status = response.status();
        if status.is_success() {
            return Ok(response.json().await?);
        }
        if status == StatusCode::UNAUTHORIZED {
            anyhow::bail!(
                "401 Unauthorized: check RUSTYKRAB_AUTH_TOKEN matches the running daemon"
            );
        }
        let body: Value = response.json().await.unwrap_or(Value::Null);
        let message = match body["message"].as_str() {
            Some(message) if !message.is_empty() => message.to_string(),
            _ => url.path().to_string(),
        };
        Err(Refused { status, message }.into())
    }

    /// A full live id for `raw`: itself when the daemon knows it, else the
    /// unique live id it prefixes.
    async fn resolve(&self, raw: &str) -> anyhow::Result<String> {
        let wanted = raw.trim().trim_start_matches('#').to_string();
        if wanted.len() >= FULL_ID {
            return Ok(wanted);
        }
        let every: WorkList = self
            .get(&["api", "work"], &[("include_closed", "true")])
            .await?;
        let ids = every.items.iter().map(|row| row.item.id.as_str());
        Ok(resolve_among(&wanted, ids)
            .map_err(anyhow::Error::msg)?
            .unwrap_or(wanted))
    }

    /// The same for an archived id.
    async fn resolve_archived(&self, raw: &str) -> anyhow::Result<String> {
        let wanted = raw.trim().trim_start_matches('#').to_string();
        if wanted.len() >= FULL_ID {
            return Ok(wanted);
        }
        let every: ArchiveList = self.get(&["api", "work", "archive"], &[]).await?;
        let ids = every.archived.iter().map(|a| a.id.as_str());
        Ok(resolve_among(&wanted, ids)
            .map_err(anyhow::Error::msg)?
            .unwrap_or(wanted))
    }
}

async fn execute(daemon: &Daemon, command: Command) -> anyhow::Result<String> {
    Ok(match command {
        Command::Help => USAGE.to_string(),
        Command::Ready => {
            let list: WorkList = daemon.get(&["api", "work", "ready"], &[]).await?;
            render::list(&list.items, false, "nothing is ready")
        }
        Command::List {
            status,
            kind,
            parent,
            all,
        } => {
            let parent = match parent {
                Some(raw) => Some(daemon.resolve(&raw).await?),
                None => None,
            };
            let mut query: Vec<(&str, &str)> = Vec::new();
            if let Some(status) = &status {
                query.push(("status", status));
            }
            if let Some(kind) = &kind {
                query.push(("kind", kind));
            }
            if let Some(parent) = &parent {
                query.push(("parent", parent));
            }
            if all {
                query.push(("include_closed", "true"));
            }
            let list: WorkList = daemon.get(&["api", "work"], &query).await?;
            render::list(&list.items, true, "no work items match")
        }
        Command::Show { id: raw, graph } => {
            let id = daemon.resolve(&raw).await?;
            let shown = if graph {
                daemon
                    .get::<GraphReply>(&["api", "work", &id, "graph"], &[])
                    .await
                    .map(|reply| render::tree(&reply.graph, &reply.rungs))
            } else {
                daemon
                    .get::<ItemDetail>(&["api", "work", &id], &[])
                    .await
                    .map(|detail| render::detail(&detail))
            };
            match shown {
                // An archived item shows as what compaction kept of it
                // (plan section 4.6).
                Err(error) if not_live(&error) => {
                    let archived_id = daemon.resolve_archived(&raw).await?;
                    match daemon
                        .get::<ArchivedItem>(&["api", "work", "archive", &archived_id], &[])
                        .await
                    {
                        Ok(item) => render::archived(&item),
                        Err(_) => return Err(error),
                    }
                }
                other => other?,
            }
        }
        Command::Plan { id } => {
            let id = daemon.resolve(&id).await?;
            let preview: PlanPreview = daemon.get(&["api", "work", &id, "plan"], &[]).await?;
            render::plan(&preview)
        }
        Command::Approve { id } => {
            let id = daemon.resolve(&id).await?;
            let reply: ApproveReply = daemon.post(&["api", "work", &id, "approve"], None).await?;
            render::approved(&reply)
        }
        Command::Reject { id, reason } => {
            let id = daemon.resolve(&id).await?;
            let reply: CancelReply = daemon
                .post(
                    &["api", "work", &id, "reject"],
                    Some(json!({ "reason": reason })),
                )
                .await?;
            render::rejected(&reply)
        }
        Command::Cancel { id, reason } => {
            let id = daemon.resolve(&id).await?;
            let reply: CancelReply = daemon
                .post(
                    &["api", "work", &id, "cancel"],
                    Some(json!({ "reason": reason })),
                )
                .await?;
            render::cancelled(&reply)
        }
        Command::ArchiveList { kind, since } => {
            let mut query: Vec<(&str, &str)> = Vec::new();
            if let Some(kind) = &kind {
                query.push(("kind", kind));
            }
            if let Some(since) = &since {
                query.push(("since", since));
            }
            let list: ArchiveList = daemon.get(&["api", "work", "archive"], &query).await?;
            render::archive_list(&list.archived)
        }
        Command::ArchiveShow { id } => {
            let id = daemon.resolve_archived(&id).await?;
            let item: ArchivedItem = daemon.get(&["api", "work", "archive", &id], &[]).await?;
            render::archived(&item)
        }
        Command::ArchiveSearch { text } => {
            let list: ArchiveList = daemon
                .get(&["api", "work", "archive"], &[("q", &text)])
                .await?;
            render::archive_list(&list.archived)
        }
        Command::Tick => {
            let report: TickReport = daemon.post(&["api", "work", "tick"], None).await?;
            render::tick(&report)
        }
    })
}

/// The daemon's bearer token: `RUSTYKRAB_AUTH_TOKEN`, then the keychain,
/// then the store, the chain `chat` uses.
async fn resolve_auth_token(data_dir: &Path) -> anyhow::Result<String> {
    if let Ok(v) = std::env::var("RUSTYKRAB_AUTH_TOKEN") {
        if !v.trim().is_empty() {
            return Ok(v.trim().to_string());
        }
    }
    let spec = rustykrab_store::registry::lookup("rustykrab_auth_token")
        .ok_or_else(|| anyhow::anyhow!("auth-token spec missing from registry"))?;
    if rustykrab_store::keychain::keychain_available() {
        if let Ok(Some(cred)) = rustykrab_store::keychain::get_credential(
            rustykrab_store::registry::keychain_service(),
            spec.keychain_account,
        ) {
            return Ok(cred.value);
        }
    }
    let db_path = data_dir.join("db");
    if db_path.exists() {
        if let Ok(master_key) = rustykrab_store::keychain::resolve_master_key() {
            if let Ok(store) = rustykrab_store::Store::open(&db_path, master_key) {
                if let Ok(v) = store.secrets().get(spec.store_name).await {
                    return Ok(v);
                }
            }
        }
    }
    anyhow::bail!(
        "could not resolve the auth token. Set RUSTYKRAB_AUTH_TOKEN to the value \
         the daemon printed at startup."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(line: &str) -> Vec<String> {
        line.split_whitespace().map(str::to_string).collect()
    }

    #[test]
    fn subcommands_parse() {
        assert_eq!(parse(&args("")), Ok(Command::Help));
        assert_eq!(parse(&args("ready")), Ok(Command::Ready));
        assert_eq!(
            parse(&args(
                "list --status blocked --kind=personal --parent #41 --all"
            )),
            Ok(Command::List {
                status: Some("blocked".into()),
                kind: Some("personal".into()),
                parent: Some("#41".into()),
                all: true,
            })
        );
        assert_eq!(
            parse(&args("show 41 --graph")),
            Ok(Command::Show {
                id: "41".into(),
                graph: true,
            })
        );
        assert_eq!(
            parse(&args("plan #41")),
            Ok(Command::Plan { id: "#41".into() })
        );
        assert_eq!(
            parse(&args("approve 41")),
            Ok(Command::Approve { id: "41".into() })
        );
        assert_eq!(
            parse(&args("reject 41 too  pricey")),
            Ok(Command::Reject {
                id: "41".into(),
                reason: Some("too pricey".into()),
            })
        );
        assert_eq!(
            parse(&args("cancel 41")),
            Ok(Command::Cancel {
                id: "41".into(),
                reason: None,
            })
        );
        assert_eq!(
            parse(&args("archive")),
            Ok(Command::ArchiveList {
                kind: None,
                since: None,
            })
        );
        assert_eq!(
            parse(&args("archive list --since 2026-09-01")),
            Ok(Command::ArchiveList {
                kind: None,
                since: Some("2026-09-01".into()),
            })
        );
        assert_eq!(
            parse(&args("archive show 7")),
            Ok(Command::ArchiveShow { id: "7".into() })
        );
        assert_eq!(
            parse(&args("archive search renew the passport")),
            Ok(Command::ArchiveSearch {
                text: "renew the passport".into(),
            })
        );
        assert_eq!(parse(&args("tick")), Ok(Command::Tick));
    }

    #[test]
    fn mistakes_are_reported_not_guessed() {
        for bad in [
            "frobnicate",
            "ready now",
            "list --colour red",
            "list --status",
            "list extra",
            "show",
            "show 1 2",
            "approve",
            "reject",
            "archive search",
            "archive show",
            "tick 3",
        ] {
            assert!(parse(&args(bad)).is_err(), "`{bad}` should not parse");
        }
    }

    #[test]
    fn ids_resolve_by_exact_id_or_unique_prefix() {
        let ids = [
            "3f2a9c1e-0000-4000-8000-000000000001",
            "3f2a9c1e-0000-4000-8000-000000000002",
            "77aa0000-0000-4000-8000-000000000003",
        ];
        assert_eq!(
            resolve_among("#77aa", ids),
            Ok(Some("77aa0000-0000-4000-8000-000000000003".into()))
        );
        assert_eq!(resolve_among(ids[0], ids), Ok(Some(ids[0].to_string())));
        assert!(resolve_among("3f2a9c1e", ids)
            .unwrap_err()
            .contains("ambiguous"));
        assert_eq!(resolve_among("ffff", ids), Ok(None));
    }

    // ── end to end: the real router, a stub controller ─────────────────

    use std::sync::Arc;

    use async_trait::async_trait;
    use chrono::Utc;
    use rustykrab_control::graph::FilingSource;
    use rustykrab_control::handle::{ControlHandle, GraphNode, GraphView};
    use rustykrab_core::model::{ModelProvider, ModelResponse};
    use rustykrab_core::types::{Message, ToolSchema};
    use rustykrab_core::work::{
        BlockedReason, Budget, Edge, EdgeKind, PlanOutcome, Status, Trigger, WorkItem, WorkItemId,
        WorkKind, WorkPlan, WorkerKind,
    };
    use rustykrab_core::Error;
    use rustykrab_store::Store;
    use rustykrab_tools::work_backend::Provenance;

    struct NoModel;

    #[async_trait]
    impl ModelProvider for NoModel {
        fn name(&self) -> &str {
            "none"
        }

        async fn chat(
            &self,
            _: &[Message],
            _: &[ToolSchema],
        ) -> rustykrab_core::Result<ModelResponse> {
            Err(Error::ModelProvider("not used".into()))
        }
    }

    /// Answers `graph` and `cancel` from the store; the rest is not used.
    struct StoreGraph(Store);

    #[async_trait]
    impl ControlHandle for StoreGraph {
        async fn file_plan(
            &self,
            _: WorkPlan,
            _: Provenance,
            _: FilingSource,
        ) -> Result<PlanOutcome, Error> {
            Err(Error::Internal("not used".into()))
        }

        async fn approve(&self, _: &str, _: &str) -> Result<Vec<WorkItemId>, Error> {
            Err(Error::Internal("not used".into()))
        }

        async fn reject(
            &self,
            _: &str,
            _: Option<String>,
            _: &str,
        ) -> Result<Vec<WorkItemId>, Error> {
            Err(Error::Internal("not used".into()))
        }

        async fn cancel(
            &self,
            item: &str,
            _: Option<String>,
            _: &str,
        ) -> Result<Vec<WorkItemId>, Error> {
            let view = self.graph(item).await?;
            Ok(view
                .nodes
                .into_iter()
                .filter(|n| !n.item.status.is_closed())
                .map(|n| n.item.id)
                .collect())
        }

        async fn tick(&self) -> Result<TickReport, Error> {
            Ok(TickReport::default())
        }

        async fn graph(&self, root: &str) -> Result<GraphView, Error> {
            let top = self
                .0
                .work_get(root)
                .await?
                .ok_or_else(|| Error::NotFound(root.into()))?;
            let mut nodes = Vec::new();
            let mut stack = vec![(top, 0u32)];
            while let Some((item, depth)) = stack.pop() {
                let children = self.0.work_children(&item.id).await?;
                let edges = self.0.work_edges_of(&item.id).await?;
                let done = children.iter().filter(|c| c.status == Status::Done).count();
                nodes.push(GraphNode {
                    rollup: (!children.is_empty()).then_some(item.status),
                    children_done: done as u32,
                    children_total: children.len() as u32,
                    depth,
                    edges,
                    archived_summary: None,
                    item,
                });
                for child in children.into_iter().rev() {
                    stack.push((child, depth + 1));
                }
            }
            Ok(GraphView {
                root: root.into(),
                nodes,
            })
        }
    }

    fn stored(n: i64, id: &str, title: &str, status: Status, parent: Option<&str>) -> WorkItem {
        let at = Utc::now() - chrono::TimeDelta::hours(1) + chrono::TimeDelta::seconds(n);
        WorkItem {
            id: id.into(),
            kind: WorkKind::Personal,
            title: title.into(),
            objective: "objective".into(),
            done_when: "done".into(),
            constraints: vec![],
            decisions_made: vec![],
            artifact_refs: vec![],
            required_tools: vec![],
            required_mcp_servers: vec![],
            worker_kind: WorkerKind::Any,
            writable_resources: vec![],
            parent: parent.map(str::to_string),
            inputs_from: vec![],
            origin_conversation_id: None,
            trigger: Trigger::Now,
            preconditions: vec![],
            expires_at: None,
            budget: Budget::default(),
            priority: 0,
            status,
            status_origin: None,
            plan_id: None,
            held_by: None,
            created_at: at,
            updated_at: at,
            closed_at: status.is_closed().then_some(at),
        }
    }

    #[tokio::test]
    async fn commands_reach_the_daemon_and_render() {
        const TOKEN: &str = "work-cmd-test-token";
        let dir = std::env::temp_dir().join(format!("rk-work-cmd-{}", uuid::Uuid::new_v4()));
        let store = Store::open(&dir, vec![5u8; 32]).expect("store opens");
        let held = Status::Blocked(BlockedReason::UpstreamFailed);
        let mut trip = stored(0, "aa11bb22-trip", "Plan the trip", held, None);
        trip.status_origin = Some("cc33dd44-hotel".into());
        let mut book = stored(2, "ee55ff66-book", "Book it", held, Some("aa11bb22-trip"));
        book.status_origin = Some("cc33dd44-hotel".into());
        let items = vec![
            trip,
            stored(
                1,
                "cc33dd44-hotel",
                "Find a hotel",
                Status::Failed,
                Some("aa11bb22-trip"),
            ),
            book,
            stored(3, "99887766-errand", "Post the letter", Status::Ready, None),
        ];
        let edges = vec![Edge {
            item: "ee55ff66-book".into(),
            depends_on: "cc33dd44-hotel".into(),
            kind: EdgeKind::Blocks,
        }];
        store.work_insert_graph(&items, &edges, None).await.unwrap();
        let passport = stored(
            4,
            "0a0b0c0d-passport",
            "Renew the passport",
            Status::Done,
            None,
        );
        store
            .work_insert_graph(&[passport], &[], None)
            .await
            .unwrap();
        store
            .work_archive_compact(&["0a0b0c0d-passport".into()], Utc::now())
            .await
            .unwrap();

        let state = rustykrab_gateway::AppState::new(
            store.clone(),
            vec![],
            Arc::new(NoModel),
            TOKEN.into(),
        )
        .with_control(Arc::new(StoreGraph(store)));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = rustykrab_gateway::router(state);
        tokio::spawn(async move {
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
            )
            .await
            .unwrap();
        });
        let daemon = Daemon::new(Url::parse(&format!("http://{addr}")).unwrap(), TOKEN).unwrap();
        let run = |line: &str| {
            let command = parse(&args(line)).unwrap();
            let daemon = &daemon;
            async move { execute(daemon, command).await }
        };

        assert_eq!(
            run("list").await.unwrap(),
            "\
#aa11bb22 Plan the trip    personal  blocked   0/2 done; origin #cc33dd44
#99887766 Post the letter  personal  ready
"
        );
        assert_eq!(
            run("ready").await.unwrap(),
            "#99887766 Post the letter  personal  ready\n"
        );
        assert_eq!(
            run("show #aa11 --graph").await.unwrap(),
            "\
#aa11bb22 Plan the trip    personal  blocked   0/2 done; origin #cc33dd44
├─ #cc33dd44 Find a hotel  personal  failed
└─ #ee55ff66 Book it       personal  blocked(upstream_failed) <- #cc33dd44
             blocked by #cc33dd44
"
        );
        let shown = run("show ee55").await.unwrap();
        assert!(
            shown.contains("  status     blocked(upstream_failed) <- #cc33dd44\n"),
            "{shown}"
        );
        let hotel = run("show cc33").await.unwrap();
        assert!(
            hotel.contains("  needed by  #ee55ff66 (blocks)\n"),
            "{hotel}"
        );
        assert_eq!(
            run("cancel aa11 no longer going").await.unwrap(),
            "cancelled #aa11bb22 #ee55ff66\n\
             already finished: #cc33dd44 Find a hotel (failed)\n"
        );
        assert_eq!(
            run("tick").await.unwrap(),
            "tick: 0 transitions, 0 notices\n"
        );
        assert!(run("archive search passport")
            .await
            .unwrap()
            .contains("  Renew the passport  personal  done\n"));
        assert!(run("show #0a0b")
            .await
            .unwrap()
            .starts_with("#0a0b0c0d Renew the passport (archived "));
        assert_eq!(
            run("archive list --kind research").await.unwrap(),
            "no archived items\n"
        );

        // The daemon's own words come back as the error.
        let missing = run("show 12345678").await.unwrap_err().to_string();
        assert!(missing.contains("no work item 12345678"), "{missing}");
        let bad = run("list --status bogus").await.unwrap_err().to_string();
        assert!(bad.contains("unknown status"), "{bad}");
        let wrong = Daemon::new(Url::parse(&format!("http://{addr}")).unwrap(), "nope").unwrap();
        let refused = execute(&wrong, Command::Tick)
            .await
            .unwrap_err()
            .to_string();
        assert!(refused.contains("401"), "{refused}");
    }
}
