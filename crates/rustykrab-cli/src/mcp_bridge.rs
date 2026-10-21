//! `rustykrab-cli mcp-bridge`: RustyKrab's host tools for a native agent
//! (Claude Code or Codex), as a stdio MCP server
//! (`docs/plans/native-mcp-host-tools.md`, rollout step 2).
//!
//! The agent starts it and speaks MCP to it: one JSON-RPC message per
//! line, both ways (the stdio framing both CLIs use). Each message goes as
//! it is to the daemon's `POST /api/mcp` over loopback, and the daemon's
//! reply comes back as one line. The daemon answers every method and runs
//! every call under the run's grant, so the bridge holds no tools and
//! decides nothing.
//!
//! - The grant is read from `RUSTYKRAB_MCP_GRANT` alone, never from argv,
//!   which every process on the machine can read. The daemon is
//!   `RUSTYKRAB_GATEWAY_URL` (the same default as the other client
//!   subcommands) and must be a loopback address, and no proxy is used, so
//!   the grant never leaves this machine.
//! - Nothing it writes contains the grant: its log lines on stderr (which
//!   the agent keeps), the errors it answers with and the daemon's replies
//!   all pass through [`Redactor`].
//! - Tool calls go to the daemon one at a time, in the order they came: a
//!   grant runs one call at a time, so calls an agent sends together are
//!   served in turn rather than refused. Any other message goes at once,
//!   so a `ping` is answered during a long call.
//! - A request the daemon refused or could not be reached for is answered
//!   with a JSON-RPC error; a notification that met one is dropped.
//! - When the agent closes stdin, the bridge exits without waiting: no one
//!   is left to read a reply, and the daemon records a call it abandoned
//!   as ambiguous.

use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, ORIGIN};
use reqwest::{StatusCode, Url};
use rustykrab_gateway::mcp_route::{PATH, SESSION_HEADER};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

/// Where the grant is read from, and the only place.
pub(crate) const GRANT_ENV: &str = "RUSTYKRAB_MCP_GRANT";

/// What a request the daemon refused, or could not be reached for, is
/// answered with: JSON-RPC's code for a server error.
const SERVER_ERROR: i64 = -32000;
const PARSE_ERROR: i64 = -32700;

/// Run the bridge on this process's stdin, stdout and stderr.
pub(crate) async fn run(args: &[String]) -> anyhow::Result<()> {
    let grant = grant(args, std::env::var(GRANT_ENV).ok())?;
    let bridge = Bridge::new(
        &crate::daemon_client::gateway_url()?,
        &grant,
        Box::new(std::io::stderr()),
    )?;
    bridge.serve(tokio::io::stdin(), tokio::io::stdout()).await
}

/// The grant, from the environment; an argument is refused unread.
fn grant(args: &[String], env: Option<String>) -> anyhow::Result<String> {
    if !args.is_empty() {
        anyhow::bail!("mcp-bridge takes no arguments: its grant is read from {GRANT_ENV} alone");
    }
    env.map(|grant| grant.trim().to_string())
        .filter(|grant| !grant.is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "{GRANT_ENV} is not set: the bridge runs only for a native run the daemon granted host tools"
            )
        })
}

/// Keeps the grant out of a line of text.
#[derive(Clone)]
struct Redactor(Arc<str>);

impl Redactor {
    fn redact(&self, text: &str) -> String {
        text.replace(&*self.0, "[redacted]")
    }
}

struct Bridge {
    client: reqwest::Client,
    endpoint: Url,
    redactor: Redactor,
    log: Mutex<Box<dyn Write + Send>>,
}

impl Bridge {
    /// A bridge to the daemon at `base`, which must be loopback, logging to
    /// `log`.
    fn new(base: &Url, grant: &str, log: Box<dyn Write + Send>) -> anyhow::Result<Self> {
        if !loopback(base) {
            anyhow::bail!(
                "the MCP bridge sends its grant only to a daemon on this machine, and {base} is \
                 not a loopback address (use http://127.0.0.1:<port>)"
            );
        }
        let mut bearer = HeaderValue::from_str(&format!("Bearer {grant}"))
            .map_err(|_| anyhow::anyhow!("{GRANT_ENV} is not a valid HTTP header value"))?;
        bearer.set_sensitive(true);
        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, bearer);
        headers.insert(ORIGIN, crate::daemon_client::gateway_origin(base.as_str())?);
        // A JSON-RPC id names one call within this session; a bridge the
        // agent restarts starts a new one.
        headers.insert(
            SESSION_HEADER,
            HeaderValue::from_str(&uuid::Uuid::new_v4().to_string())?,
        );
        // No overall timeout: the daemon clamps every call to the run's
        // deadline and the tool's own cap.
        let client = reqwest::Client::builder()
            .default_headers(headers)
            .connect_timeout(Duration::from_secs(10))
            .no_proxy()
            .build()?;
        Ok(Self {
            client,
            endpoint: base.join(PATH)?,
            redactor: Redactor(grant.into()),
            log: Mutex::new(log),
        })
    }

    /// Serve the agent on `input` and `output` until it closes `input`.
    async fn serve<R, W>(self, input: R, output: W) -> anyhow::Result<()>
    where
        R: AsyncRead + Unpin,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let bridge = Arc::new(self);
        bridge.log(&format!("forwarding MCP to {}", bridge.endpoint));
        let (replies, mut outgoing) = mpsc::unbounded_channel::<Value>();
        let writer = tokio::spawn({
            let redactor = bridge.redactor.clone();
            let mut output = output;
            async move {
                while let Some(reply) = outgoing.recv().await {
                    let line = redactor.redact(&reply.to_string());
                    output.write_all(line.as_bytes()).await?;
                    output.write_all(b"\n").await?;
                    output.flush().await?;
                }
                Ok::<_, std::io::Error>(())
            }
        });
        let (calls, mut queued) = mpsc::unbounded_channel::<Value>();
        let serial = tokio::spawn({
            let (bridge, replies) = (bridge.clone(), replies.clone());
            async move {
                while let Some(call) = queued.recv().await {
                    if let Some(reply) = bridge.forward(&call).await {
                        let _ = replies.send(reply);
                    }
                }
            }
        });
        let mut lines = BufReader::new(input).lines();
        while let Some(line) = lines.next_line().await? {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let Ok(message) = serde_json::from_str::<Value>(line) else {
                let _ = replies.send(error(Value::Null, PARSE_ERROR, "not a JSON-RPC message"));
                continue;
            };
            if message.get("method").and_then(Value::as_str) == Some("tools/call") {
                let _ = calls.send(message);
            } else {
                let (bridge, replies) = (bridge.clone(), replies.clone());
                tokio::spawn(async move {
                    if let Some(reply) = bridge.forward(&message).await {
                        let _ = replies.send(reply);
                    }
                });
            }
        }
        bridge.log("the agent closed stdin; stopping");
        serial.abort();
        writer.abort();
        Ok(())
    }

    /// Post `message` to the daemon: its reply, or an error answering it,
    /// or `None` for a notification.
    async fn forward(&self, message: &Value) -> Option<Value> {
        let id = message.get("id").cloned();
        let method = message
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or("a message");
        let sent = self
            .client
            .post(self.endpoint.clone())
            .json(message)
            .send()
            .await;
        let failure = match sent {
            Ok(response) if response.status() == StatusCode::ACCEPTED => return None,
            Ok(response) if response.status().is_success() => {
                match response.json::<Value>().await {
                    Ok(reply) => return Some(reply),
                    Err(e) => format!("the daemon's reply to `{method}` was not JSON: {e}"),
                }
            }
            Ok(response) => {
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                format!(
                    "the daemon refused `{method}` (HTTP {status}): {}",
                    body.trim()
                )
            }
            Err(e) => format!("the daemon could not be reached for `{method}`: {e}"),
        };
        let failure = self.redactor.redact(&failure);
        self.log(&failure);
        id.map(|id| error(id, SERVER_ERROR, &failure))
    }

    fn log(&self, line: &str) {
        let line = self.redactor.redact(line);
        let mut log = self.log.lock().unwrap_or_else(|e| e.into_inner());
        let _ = writeln!(log, "rustykrab mcp-bridge: {line}");
    }
}

/// Whether `url` names this machine by a loopback address.
fn loopback(url: &Url) -> bool {
    url.host_str()
        .map(|host| host.trim_start_matches('[').trim_end_matches(']'))
        .and_then(|host| host.parse::<std::net::IpAddr>().ok())
        .is_some_and(|ip| ip.is_loopback())
}

fn error(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

#[cfg(test)]
mod tests;
