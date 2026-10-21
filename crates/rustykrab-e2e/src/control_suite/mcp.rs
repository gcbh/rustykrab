//! A native agent reaches RustyKrab's own tools through the daemon binary's
//! `mcp-bridge` (`docs/plans/native-mcp-host-tools.md`, rollout step 2).
//!
//! Until the native worker mints grants (rollout step 3), no booted daemon
//! has a grant to give, so this scenario plays that worker's part: it
//! serves the gateway's real router in-process on a loopback port, mints a
//! run's grant over the real work tools, and starts `mcp-bridge` from the
//! daemon binary under test with the grant and the daemon's URL as its
//! whole environment, as the worker will. The scripted agent opens as
//! Claude Code 2.1.295 does (its `server/discover` probe, `initialize`,
//! `notifications/initialized`, `tools/list`), calls a granted tool and an
//! ungranted one, and closes. Step 3 makes this a booted daemon's scenario,
//! with a Claude Code stand-in that starts the bridge from its
//! `--mcp-config`.

use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, ensure, Context, Result};
use rustykrab_agent::mcp_grants::{self, CallObservation, CallOutcome, GrantSpec, McpGrants};
use rustykrab_core::work::{ArtifactRef, ResultReport};
use rustykrab_tools::work_backend::{StubWorkBackend, WorkCall};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout};

use crate::Ctx;

const ITEM: &str = "e2e-native-mcp";
const ACTOR: &str = "worker:e2e-native";

/// The scripted agent's side of one bridge process.
struct ScriptedAgent {
    child: Child,
    stdin: ChildStdin,
    stdout: Lines<BufReader<ChildStdout>>,
    /// Every line each way, for the evidence directory.
    session: Vec<String>,
}

impl ScriptedAgent {
    fn start(bin: &str, gateway: &str, grant: &str) -> Result<ScriptedAgent> {
        let mut child = tokio::process::Command::new(bin)
            .arg("mcp-bridge")
            .env_clear()
            .env("RUSTYKRAB_MCP_GRANT", grant)
            .env("RUSTYKRAB_GATEWAY_URL", gateway)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("starting {bin} mcp-bridge"))?;
        let stdin = child.stdin.take().context("bridge stdin")?;
        let stdout = BufReader::new(child.stdout.take().context("bridge stdout")?).lines();
        Ok(ScriptedAgent {
            child,
            stdin,
            stdout,
            session: Vec::new(),
        })
    }

    async fn notify(&mut self, message: Value) -> Result<()> {
        let line = message.to_string();
        self.stdin.write_all(format!("{line}\n").as_bytes()).await?;
        self.session.push(format!("> {line}"));
        Ok(())
    }

    /// Send a request and read its reply, which must answer its id.
    async fn request(&mut self, message: Value) -> Result<Value> {
        self.notify(message.clone()).await?;
        let line = tokio::time::timeout(Duration::from_secs(30), self.stdout.next_line())
            .await
            .context("the bridge did not reply within 30 s")??
            .context("the bridge closed stdout")?;
        self.session.push(format!("< {line}"));
        let reply: Value = serde_json::from_str(&line)?;
        ensure!(
            reply["id"] == message["id"],
            "the reply answers {} instead of {}",
            reply["id"],
            message["id"]
        );
        Ok(reply)
    }

    /// Close stdin, as a CLI ends its MCP server, and return what the
    /// bridge logged.
    async fn close(mut self) -> Result<(String, Vec<String>)> {
        drop(self.stdin);
        let status = tokio::time::timeout(Duration::from_secs(10), self.child.wait())
            .await
            .context("the bridge did not exit when its stdin closed")??;
        let mut log = String::new();
        if let Some(mut stderr) = self.child.stderr.take() {
            stderr.read_to_string(&mut log).await?;
        }
        ensure!(status.success(), "the bridge exited {status}: {log}");
        Ok((log, self.session))
    }
}

/// The gateway's real router, in this process, on a loopback port, with
/// `grants` as its grant store.
async fn serve_gateway(
    grants: Arc<McpGrants>,
    dir: &std::path::Path,
) -> Result<(String, tokio::task::JoinHandle<()>)> {
    let store = rustykrab_store::Store::open(dir, vec![7; 32])?;
    let provider = rustykrab_providers::ScriptedProvider::from_json("{}")?;
    let state = rustykrab_gateway::AppState::new(
        store,
        vec![],
        Arc::new(provider),
        uuid::Uuid::new_v4().simple().to_string(),
    )
    .with_mcp_grants(grants);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let base = format!("http://{}", listener.local_addr()?);
    let app = rustykrab_gateway::router(state);
    let task = tokio::spawn(async move {
        let _ = axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await;
    });
    Ok((base, task))
}

/// A scripted native agent calls a granted work tool through the bridge,
/// and the call is observed on its run's grant and attests the run's
/// report; an ungranted tool is refused and never runs; the bridge
/// connected, and the grant appears in nothing the bridge wrote.
pub(super) async fn native_agent_through_the_bridge(ctx: &Ctx) -> Result<()> {
    let dir = tempfile::Builder::new()
        .prefix("rustykrab-e2e-mcp-")
        .tempdir()?;
    let grants = Arc::new(McpGrants::new());
    let (gateway, server) = serve_gateway(grants.clone(), dir.path()).await?;
    let backend = Arc::new(StubWorkBackend::new());
    let grant = grants.mint(GrantSpec {
        item: ITEM.into(),
        conversation: uuid::Uuid::new_v4(),
        actor: ACTOR.into(),
        read_only: false,
        tools: mcp_grants::work_tools(backend.clone()),
        deadline: tokio::time::Instant::now() + Duration::from_secs(120),
        quota: 16,
    });
    let token = grant.token().expose().to_string();
    let mut agent = ScriptedAgent::start(&ctx.bin, &gateway, &token)?;

    let discover = agent
        .request(json!({"jsonrpc":"2.0","id":"server-discover-probe-1","method":"server/discover","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28"}}}))
        .await?;
    ensure!(
        discover["error"]["code"] == -32601,
        "the discover probe must fall back to initialize: {discover}"
    );
    let init = agent
        .request(json!({"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{"roots":{"listChanged":true}},"clientInfo":{"name":"claude-code","version":"2.1.295"}},"jsonrpc":"2.0","id":0}))
        .await?;
    ensure!(
        init["result"]["protocolVersion"] == "2025-11-25"
            && init["result"]["capabilities"]["tools"].is_object(),
        "initialize: {init}"
    );
    agent
        .notify(json!({"jsonrpc":"2.0","method":"notifications/initialized"}))
        .await?;
    let listed = agent
        .request(json!({"method":"tools/list","jsonrpc":"2.0","id":1}))
        .await?;
    let mut names: Vec<&str> = listed["result"]["tools"]
        .as_array()
        .context("tools/list lists no tools")?
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .collect();
    names.sort_unstable();
    ensure!(
        names == ["ask_user", "capability_request", "work_file", "work_status"],
        "tools/list must list exactly the granted tools: {names:?}"
    );
    let granted = agent
        .request(json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"work_status","arguments":{"ids":[ITEM]}}}))
        .await?;
    ensure!(
        granted["result"]["isError"] == false,
        "work_status through the bridge: {granted}"
    );
    let ungranted = agent
        .request(json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"exec","arguments":{"command":"id"}}}))
        .await?;
    ensure!(
        ungranted["error"]["code"] == -32602,
        "an ungranted tool must be refused: {ungranted}"
    );
    let (log, session) = agent.close().await?;
    super::keep_evidence("mcp-bridge/session.txt", session.join("\n"))?;
    ensure!(
        log.contains("forwarding MCP to") && !log.contains(&token),
        "the bridge's log must name the daemon and never the grant: {log}"
    );
    ensure!(
        !session.iter().any(|line| line.contains(&token)),
        "the grant crossed the agent's pipes"
    );

    // The call ran in the daemon, bound to the grant's item.
    let calls = backend.calls();
    match calls.as_slice() {
        [WorkCall::Status { principal, .. }] if principal.item.as_deref() == Some(ITEM) => {}
        other => bail!("the backend saw {other:?}, not one status read bound to {ITEM}"),
    }

    // The run ends: its grant attests the run's report in place of what
    // the agent claimed, and its record says the bridge connected (which
    // proves no call) and what the grant observed.
    let mut report = ResultReport {
        summary: "Read the item's status.".into(),
        artifacts: vec![ArtifactRef {
            kind: mcp_grants::MCP_CALL.into(),
            value: r#"{"tool":"gmail","outcome":"ok"}"#.into(),
        }],
        ..ResultReport::default()
    };
    let record = grant.finish(Some(&mut report));
    server.abort();
    ensure!(
        record.bridge_connected.is_some(),
        "initialize and tools/list reached the route, so the bridge connected"
    );
    let observed = vec![
        CallObservation {
            tool: "work_status".into(),
            outcome: CallOutcome::Ok,
        },
        CallObservation {
            tool: "exec".into(),
            outcome: CallOutcome::Denied,
        },
    ];
    ensure!(record.calls == observed, "observed {:?}", record.calls);
    ensure!(record.ended.is_none(), "no call ended the run");
    let attested: Vec<&str> = report
        .artifacts
        .iter()
        .filter(|a| a.kind == mcp_grants::MCP_CALL)
        .map(|a| a.value.as_str())
        .collect();
    ensure!(
        attested
            == [
                r#"{"tool":"work_status","outcome":"ok"}"#,
                r#"{"tool":"exec","outcome":"denied"}"#
            ],
        "the attested report carries what the grant observed, not the agent's claim: {attested:?}"
    );
    ensure!(
        !grants.is_live(&token),
        "the grant is revoked when its run ends"
    );
    Ok(())
}
