use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::Arc;

use openclaw_agent::{HarnessProfile, HarnessRouter};
use openclaw_core::model::ModelProvider;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    // --- Data directory ---
    let data_dir = dirs::data_local_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join("openclaw");
    std::fs::create_dir_all(&data_dir)?;

    // --- Harness profile ---
    // Load from file or use a preset. Supported: default, coding, research, creative
    let profile = load_harness_profile(&data_dir)?;
    tracing::info!(profile = %profile.name, "harness profile loaded");

    // --- Master key for credential encryption ---
    // On macOS: stored in the system Keychain (Secure Enclave / Touch ID protected).
    // On Linux: falls back to OPENCLAW_MASTER_KEY env var or ephemeral key.
    let master_key = openclaw_store::keychain::resolve_master_key()
        .expect("failed to resolve master encryption key");

    let store = openclaw_store::Store::open(data_dir.join("db"), master_key)?;

    // --- Auth token ---
    let auth_token = std::env::var("OPENCLAW_AUTH_TOKEN").unwrap_or_else(|_| {
        let token = openclaw_gateway::generate_token();
        tracing::info!("Generated auth token (set OPENCLAW_AUTH_TOKEN to persist):");
        println!("\n  OPENCLAW_AUTH_TOKEN={token}\n");
        token
    });

    // --- Model provider ---
    let provider: Arc<dyn ModelProvider> = match std::env::var("OPENCLAW_PROVIDER")
        .unwrap_or_else(|_| "anthropic".to_string())
        .to_lowercase()
        .as_str()
    {
        "ollama" => {
            let model = std::env::var("OLLAMA_MODEL").unwrap_or_else(|_| "qwen3:32b".to_string());
            let base_url = std::env::var("OLLAMA_BASE_URL")
                .unwrap_or_else(|_| "http://localhost:11434".to_string());
            tracing::info!(%model, %base_url, "using Ollama provider");
            let p = openclaw_providers::OllamaProvider::new(model).with_base_url(base_url);
            Arc::new(p)
        }
        _ => {
            let api_key = std::env::var("ANTHROPIC_API_KEY").unwrap_or_else(|_| {
                if let Ok(key) = store.secrets().get("anthropic_api_key") {
                    return key;
                }
                tracing::error!(
                    "ANTHROPIC_API_KEY not set. Set it or store via the secrets API."
                );
                String::new()
            });
            let model = std::env::var("ANTHROPIC_MODEL")
                .unwrap_or_else(|_| "claude-sonnet-4-20250514".to_string());
            tracing::info!(%model, "using Anthropic provider");
            let p = openclaw_providers::AnthropicProvider::new(api_key).with_model(model);
            Arc::new(p)
        }
    };

    // --- Tools ---
    let tools = openclaw_tools::builtin_tools(store.secrets());

    // --- Log provider status ---
    tracing::info!(provider = provider.name(), "model provider configured");

    // --- Harness router (auto-selects profile per message) ---
    // Reuses the main provider for classification to avoid model swapping.
    // The classification prompt is ~50 tokens — negligible overhead on any model.
    let classifier: Arc<dyn ModelProvider> = provider.clone();

    let router = Arc::new(HarnessRouter::new(classifier).with_base(profile));

    // --- Build gateway state ---
    let mut state = openclaw_gateway::AppState::new(store, tools, provider, auth_token)
        .with_harness_router(router);

    // --- Telegram channel (optional) ---
    if let Ok(bot_token) = std::env::var("TELEGRAM_BOT_TOKEN") {
        let allowed_chats: HashSet<i64> = std::env::var("TELEGRAM_ALLOWED_CHATS")
            .unwrap_or_default()
            .split(',')
            .filter_map(|s| s.trim().parse().ok())
            .collect();

        let webhook_secret = std::env::var("TELEGRAM_WEBHOOK_SECRET").ok();

        let mut tg = openclaw_channels::TelegramChannel::new(bot_token, allowed_chats.clone());
        if let Some(secret) = webhook_secret {
            tg = tg.with_webhook_secret(secret);
        }

        let tg = Arc::new(tg);
        state = state.with_telegram(tg.clone());

        if let Ok(webhook_url) = std::env::var("TELEGRAM_WEBHOOK_URL") {
            tg.set_webhook(&webhook_url).await?;
            tracing::info!("Telegram: webhook mode");
        } else {
            let tg_poll = tg.clone();
            tokio::spawn(async move {
                if let Err(e) = tg_poll.start_polling().await {
                    tracing::error!("Telegram polling error: {e}");
                }
            });
            tracing::info!("Telegram: long-polling mode");
        }

        if allowed_chats.is_empty() {
            tracing::warn!(
                "TELEGRAM_ALLOWED_CHATS not set — bot will deny all chats. \
                 Set it to a comma-separated list of chat IDs."
            );
        } else {
            tracing::info!(chats = ?allowed_chats, "Telegram allowed chats configured");
        }
    }

    // --- Signal channel (optional) ---
    if let Ok(account_number) = std::env::var("SIGNAL_ACCOUNT") {
        let base_url = std::env::var("SIGNAL_CLI_URL")
            .unwrap_or_else(|_| "http://localhost:8080".to_string());

        let allowed_numbers: HashSet<String> = std::env::var("SIGNAL_ALLOWED_NUMBERS")
            .unwrap_or_default()
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();

        let webhook_secret = std::env::var("SIGNAL_WEBHOOK_SECRET").ok();

        let mut sig =
            openclaw_channels::SignalChannel::new(base_url.clone(), account_number.clone(), allowed_numbers.clone());
        if let Some(secret) = webhook_secret {
            sig = sig.with_webhook_secret(secret);
        }

        let sig = Arc::new(sig);
        state = state.with_signal(sig.clone());

        // Health check — verify signal-cli-rest-api is running.
        match sig.health_check().await {
            Ok(()) => tracing::info!("signal-cli-rest-api connected"),
            Err(e) => tracing::error!("signal-cli-rest-api not reachable: {e}"),
        }

        // Webhook or polling mode.
        if let Ok(webhook_url) = std::env::var("SIGNAL_WEBHOOK_URL") {
            if let Err(e) = sig.register_webhook(&webhook_url).await {
                tracing::error!("failed to register Signal webhook: {e}");
            } else {
                tracing::info!("Signal: webhook mode");
            }
        } else {
            let sig_poll = sig.clone();
            tokio::spawn(async move {
                if let Err(e) = sig_poll.start_polling().await {
                    tracing::error!("Signal polling error: {e}");
                }
            });
            tracing::info!("Signal: polling mode");
        }

        if allowed_numbers.is_empty() {
            tracing::warn!(
                "SIGNAL_ALLOWED_NUMBERS not set — bot will deny all messages. \
                 Set it to a comma-separated list of E.164 phone numbers."
            );
        } else {
            tracing::info!(
                numbers = ?allowed_numbers,
                "Signal allowed numbers configured"
            );
        }
    }

    // --- Gateway with security middleware ---
    let app = openclaw_gateway::router(state);

    // Bind to loopback only — never 0.0.0.0.
    let addr = SocketAddr::from(([127, 0, 0, 1], 3000));
    tracing::info!(%addr, "OpenClaw gateway listening");

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await?;

    Ok(())
}

/// Load harness profile from file or env var preset.
///
/// Priority:
/// 1. `data_dir/harness.toml` — full custom profile
/// 2. `OPENCLAW_HARNESS` env var — one of: default, coding, research, creative
/// 3. Fallback to default profile
fn load_harness_profile(data_dir: &std::path::Path) -> anyhow::Result<HarnessProfile> {
    let profile_path = data_dir.join("harness.toml");
    if profile_path.exists() {
        let contents = std::fs::read_to_string(&profile_path)?;
        let profile: HarnessProfile = toml::from_str(&contents)?;
        return Ok(profile);
    }

    let preset = std::env::var("OPENCLAW_HARNESS").unwrap_or_else(|_| "default".to_string());
    let profile = match preset.to_lowercase().as_str() {
        "coding" => HarnessProfile::coding(),
        "research" => HarnessProfile::research(),
        "creative" => HarnessProfile::creative(),
        _ => HarnessProfile::default(),
    };

    Ok(profile)
}
