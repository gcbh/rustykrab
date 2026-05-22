//! `rustykrab-cli chat` — a small REPL that talks to a running daemon over
//! the loopback gateway.
//!
//! Two purposes:
//! 1. Have a conversation with the agent from the terminal (handy when
//!    the messaging channels aren't configured or you don't want to use
//!    one for ad-hoc tasks).
//! 2. Onboard credentials via the slash-command `/set <name>`, which
//!    prompts for the value with no echo and `POST`s it directly to
//!    `/api/secrets`. The value never enters the model's context, never
//!    transits a third-party messaging server, and is not persisted in
//!    the conversation history.
//!
//! The chat client always talks to a daemon that's already running. It
//! does not touch the SQLite store directly to avoid lock contention and
//! a double trust path. Secret onboarding without a running daemon is
//! still supported via the existing `keychain` subcommand or by writing
//! to the SecretStore directly from a one-off process.

use std::io::{self, Write};
use std::path::Path;
use std::time::Duration;

use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION};
use serde::{Deserialize, Serialize};
use serde_json::json;
use uuid::Uuid;

const DEFAULT_GATEWAY_URL: &str = "http://127.0.0.1:3000";

pub async fn run(data_dir: &Path, _args: &[String]) -> anyhow::Result<()> {
    let gateway_url =
        std::env::var("RUSTYKRAB_GATEWAY_URL").unwrap_or_else(|_| DEFAULT_GATEWAY_URL.to_string());

    let token = resolve_auth_token(data_dir)?;

    let mut auth_headers = HeaderMap::new();
    auth_headers.insert(
        AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {token}"))
            .map_err(|e| anyhow::anyhow!("invalid auth token: {e}"))?,
    );

    let client = reqwest::Client::builder()
        .default_headers(auth_headers)
        .timeout(Duration::from_secs(600))
        .build()?;

    // Probe the daemon and start a conversation before printing anything,
    // so a failure mode is obvious.
    health_check(&client, &gateway_url).await?;
    let conv_id = create_conversation(&client, &gateway_url).await?;

    print_banner(&gateway_url, conv_id);

    let stdin = io::stdin();
    loop {
        print!("> ");
        io::stdout().flush().ok();

        let mut line = String::new();
        let n = stdin.read_line(&mut line)?;
        if n == 0 {
            // EOF (e.g. Ctrl-D).
            println!();
            return Ok(());
        }
        let input = line.trim();
        if input.is_empty() {
            continue;
        }

        if let Some(rest) = input.strip_prefix('/') {
            match handle_slash(&client, &gateway_url, rest).await {
                Ok(ControlFlow::Continue) => continue,
                Ok(ControlFlow::Quit) => return Ok(()),
                Err(e) => {
                    eprintln!("  error: {e}");
                    continue;
                }
            }
        }

        match send_message(&client, &gateway_url, conv_id, input).await {
            Ok(reply) => println!("{reply}\n"),
            Err(e) => eprintln!("  error: {e}\n"),
        }
    }
}

enum ControlFlow {
    Continue,
    Quit,
}

async fn handle_slash(
    client: &reqwest::Client,
    base: &str,
    rest: &str,
) -> anyhow::Result<ControlFlow> {
    let mut parts = rest.split_whitespace();
    let cmd = parts.next().unwrap_or("");
    let rest_args: Vec<&str> = parts.collect();

    match cmd {
        "quit" | "exit" | "q" => Ok(ControlFlow::Quit),
        "help" | "?" => {
            print_help();
            Ok(ControlFlow::Continue)
        }
        "list" | "ls" => {
            list_secrets(client, base).await?;
            Ok(ControlFlow::Continue)
        }
        "set" => {
            let name = rest_args.first().ok_or_else(|| {
                anyhow::anyhow!("usage: /set <name> [--keychain <service>/<account>]")
            })?;
            let keychain_target = parse_keychain_flag(&rest_args[1..])?;
            set_secret_interactive(client, base, name, keychain_target).await?;
            Ok(ControlFlow::Continue)
        }
        "delete" | "rm" => {
            let name = rest_args
                .first()
                .ok_or_else(|| anyhow::anyhow!("usage: /delete <name>"))?;
            delete_secret(client, base, name).await?;
            Ok(ControlFlow::Continue)
        }
        other => {
            eprintln!("  unknown command `/{other}` (try /help)");
            Ok(ControlFlow::Continue)
        }
    }
}

fn parse_keychain_flag(args: &[&str]) -> anyhow::Result<Option<(String, String)>> {
    let mut iter = args.iter();
    while let Some(a) = iter.next() {
        if *a == "--keychain" {
            let target = iter
                .next()
                .ok_or_else(|| anyhow::anyhow!("--keychain requires <service>/<account>"))?;
            let (s, acc) = target.split_once('/').ok_or_else(|| {
                anyhow::anyhow!("--keychain target must be <service>/<account>, got `{target}`")
            })?;
            return Ok(Some((s.to_string(), acc.to_string())));
        }
    }
    Ok(None)
}

fn print_banner(gateway_url: &str, conv_id: Uuid) {
    println!();
    println!("rustykrab chat — connected to {gateway_url}");
    println!("conversation: {conv_id}");
    println!("type /help for slash commands, /quit to exit.");
    println!();
}

fn print_help() {
    println!("  /set <name>                    Store a secret in the encrypted local store.");
    println!("                                 Value is prompted for with no echo.");
    println!("                                 Convention: mcp.<server>.<field>");
    println!("  /set <name> --keychain S/A     Store in the macOS Keychain instead");
    println!("                                 (service=S, account=A).");
    println!("  /list                          List the names of stored secrets.");
    println!("  /delete <name>                 Delete a secret from the local store.");
    println!("  /help, /quit");
    println!();
    println!("  Anything else is sent to the agent as a chat message. Secrets pasted");
    println!("  via /set never enter the model's context.");
    println!("  MCP servers re-read their credentials at daemon startup — restart");
    println!("  rustykrab after onboarding a new MCP secret for it to take effect.");
}

// ---------------------------------------------------------------------------
// HTTP helpers
// ---------------------------------------------------------------------------

async fn health_check(client: &reqwest::Client, base: &str) -> anyhow::Result<()> {
    let url = format!("{base}/api/health");
    let resp = client.get(&url).send().await.map_err(|e| {
        anyhow::anyhow!(
            "could not reach daemon at {base}: {e}. \
             Is `rustykrab-cli` (the daemon) running?"
        )
    })?;
    if !resp.status().is_success() {
        anyhow::bail!("daemon health check failed: HTTP {}", resp.status());
    }
    Ok(())
}

#[derive(Deserialize)]
struct ConversationResp {
    id: Uuid,
}

async fn create_conversation(client: &reqwest::Client, base: &str) -> anyhow::Result<Uuid> {
    let url = format!("{base}/api/conversations");
    let resp = client.post(&url).send().await?;
    if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
        anyhow::bail!(
            "401 Unauthorized — check RUSTYKRAB_AUTH_TOKEN matches the running daemon. \
             You can rotate via POST /api/logout."
        );
    }
    let resp = resp.error_for_status()?;
    let conv: ConversationResp = resp.json().await?;
    Ok(conv.id)
}

#[derive(Deserialize)]
struct AssistantMessage {
    content: serde_json::Value,
}

async fn send_message(
    client: &reqwest::Client,
    base: &str,
    conv_id: Uuid,
    content: &str,
) -> anyhow::Result<String> {
    let url = format!("{base}/api/conversations/{conv_id}/messages");
    let resp = client
        .post(&url)
        .json(&json!({ "content": content }))
        .send()
        .await?
        .error_for_status()?;
    let msg: AssistantMessage = resp.json().await?;
    Ok(render_content(&msg.content))
}

/// Extract a human-readable string from a `MessageContent` JSON value.
/// `MessageContent::Text(s)` serialises as `{"Text": "..."}`; any other
/// variant is dumped as JSON.
fn render_content(value: &serde_json::Value) -> String {
    if let Some(text) = value.get("Text").and_then(|v| v.as_str()) {
        return text.to_string();
    }
    if let Some(text) = value.as_str() {
        return text.to_string();
    }
    serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
}

#[derive(Serialize)]
struct SetSecretBody<'a> {
    name: &'a str,
    value: &'a str,
    dest: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    service: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    account: Option<&'a str>,
}

async fn set_secret_interactive(
    client: &reqwest::Client,
    base: &str,
    name: &str,
    keychain_target: Option<(String, String)>,
) -> anyhow::Result<()> {
    // `rpassword` reads /dev/tty (or the Windows console) directly,
    // bypassing stdin redirection. The buffer is dropped before this
    // function returns; the network send moves it through a borrow only.
    let value = rpassword::prompt_password(format!("  value for {name} (hidden): "))?;
    if value.is_empty() {
        anyhow::bail!("empty value — not stored");
    }

    let body = match &keychain_target {
        Some((service, account)) => SetSecretBody {
            name,
            value: &value,
            dest: "keychain",
            service: Some(service.as_str()),
            account: Some(account.as_str()),
        },
        None => SetSecretBody {
            name,
            value: &value,
            dest: "store",
            service: None,
            account: None,
        },
    };

    let url = format!("{base}/api/secrets");
    let resp = client.post(&url).json(&body).send().await?;
    drop(value); // be explicit about it
    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        anyhow::bail!("set failed: HTTP {status} {text}");
    }

    match keychain_target {
        Some((s, a)) => println!("  ✓ stored in keychain (service={s}, account={a})"),
        None => println!("  ✓ stored in encrypted local store as `{name}`"),
    }
    println!("  (MCP servers pick up new credentials at next daemon restart.)");
    Ok(())
}

#[derive(Deserialize)]
struct ListSecretsResp {
    names: Vec<String>,
    keychain_available: bool,
}

async fn list_secrets(client: &reqwest::Client, base: &str) -> anyhow::Result<()> {
    let url = format!("{base}/api/secrets");
    let resp = client.get(&url).send().await?.error_for_status()?;
    let list: ListSecretsResp = resp.json().await?;
    if list.names.is_empty() {
        println!("  (no secrets stored)");
    } else {
        for n in &list.names {
            println!("  {n}");
        }
    }
    println!(
        "  keychain: {}",
        if list.keychain_available {
            "available"
        } else {
            "not available on this platform"
        }
    );
    Ok(())
}

async fn delete_secret(client: &reqwest::Client, base: &str, name: &str) -> anyhow::Result<()> {
    let url = format!("{base}/api/secrets/{name}");
    let resp = client.delete(&url).send().await?;
    if !resp.status().is_success() {
        anyhow::bail!("delete failed: HTTP {}", resp.status());
    }
    println!("  ✓ deleted `{name}`");
    Ok(())
}

// ---------------------------------------------------------------------------
// Auth-token resolution
// ---------------------------------------------------------------------------
//
// The chat client must hold the same bearer token the daemon accepts.
// We use the same registry chain the daemon does: env → keychain → store.
// If the store is locked (daemon holds it), we fall back to env/keychain
// only; the user can also set RUSTYKRAB_AUTH_TOKEN explicitly.

fn resolve_auth_token(data_dir: &Path) -> anyhow::Result<String> {
    if let Ok(v) = std::env::var("RUSTYKRAB_AUTH_TOKEN") {
        let v = v.trim();
        if !v.is_empty() {
            return Ok(v.to_string());
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

    // Last resort: try opening the store. Will fail if the daemon holds
    // an exclusive lock on it — that's fine, we already told the user
    // to set RUSTYKRAB_AUTH_TOKEN above.
    let db_path = data_dir.join("db");
    if db_path.exists() {
        if let Ok(master_key) = rustykrab_store::keychain::resolve_master_key() {
            if let Ok(store) = rustykrab_store::Store::open(&db_path, master_key) {
                if let Ok(v) = store.secrets().get(spec.store_name) {
                    return Ok(v);
                }
            }
        }
    }

    anyhow::bail!(
        "could not resolve auth token. Set RUSTYKRAB_AUTH_TOKEN to the value \
         the daemon printed at startup, or run `rustykrab-cli keychain status` \
         to inspect what's stored."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_keychain_flag_extracts_target() {
        let args = vec!["--keychain", "Foo/bar"];
        let got = parse_keychain_flag(&args).unwrap();
        assert_eq!(got, Some(("Foo".to_string(), "bar".to_string())));
    }

    #[test]
    fn parse_keychain_flag_returns_none_when_absent() {
        let args: Vec<&str> = vec![];
        assert!(parse_keychain_flag(&args).unwrap().is_none());
    }

    #[test]
    fn parse_keychain_flag_rejects_missing_target() {
        let args = vec!["--keychain"];
        assert!(parse_keychain_flag(&args).is_err());
    }

    #[test]
    fn parse_keychain_flag_rejects_missing_slash() {
        let args = vec!["--keychain", "no-slash"];
        assert!(parse_keychain_flag(&args).is_err());
    }

    #[test]
    fn render_content_extracts_text_variant() {
        let v = serde_json::json!({ "Text": "hello world" });
        assert_eq!(render_content(&v), "hello world");
    }

    #[test]
    fn render_content_handles_plain_string() {
        let v = serde_json::json!("hello");
        assert_eq!(render_content(&v), "hello");
    }
}
