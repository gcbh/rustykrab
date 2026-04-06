use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::Arc;

use openclaw_agent::{HarnessProfile, HarnessRouter, OrchestrationPipeline};
use openclaw_core::model::ModelProvider;
use openclaw_core::orchestration::OrchestrationConfig;
use openclaw_skills::SkillRegistry;
use tracing_subscriber::fmt;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // --- Data directory ---
    let data_dir = dirs::data_local_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join("openclaw");
    std::fs::create_dir_all(&data_dir)?;

    // --- Logging: stdout + rolling file ---
    let log_dir = data_dir.join("logs");
    std::fs::create_dir_all(&log_dir)?;

    let file_appender = tracing_appender::rolling::daily(&log_dir, "openclaw.log");
    let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);

    let env_filter = EnvFilter::from_default_env();

    tracing_subscriber::registry()
        .with(env_filter)
        .with(fmt::layer().with_writer(std::io::stdout))
        .with(fmt::layer().with_writer(non_blocking).with_ansi(false))
        .init();

    // --- CLI subcommands ---
    let args: Vec<String> = std::env::args().collect();
    if args.len() >= 2 && args[1] == "skill" {
        return handle_skill_subcommand(&data_dir, &args[2..]);
    }

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
    let provider_name = std::env::var("OPENCLAW_PROVIDER")
        .unwrap_or_else(|_| "anthropic".to_string());
    let provider_name = provider_name.trim().to_lowercase();
    let provider: Arc<dyn ModelProvider> = match provider_name.as_str() {
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

    // --- Skills directory (needed by skill tools and skill loader) ---
    let skills_dir = data_dir.join("skills");
    std::fs::create_dir_all(&skills_dir)?;

    // --- Tools ---
    let mut tools = openclaw_tools::builtin_tools(store.secrets());
    tools.extend(openclaw_tools::skill_tools(skills_dir.clone()));

    // --- Log provider status ---
    tracing::info!(provider = provider.name(), "model provider configured");

    // --- Harness router (auto-selects profile per message) ---
    // Reuses the main provider for classification to avoid model swapping.
    // The classification prompt is ~50 tokens — negligible overhead on any model.
    let classifier: Arc<dyn ModelProvider> = provider.clone();

    let router = Arc::new(HarnessRouter::new(classifier).with_base(profile));

    // --- Orchestration pipeline (optional, enabled via OPENCLAW_ORCHESTRATION=true) ---
    let orchestration_config = load_orchestration_config(&data_dir)?;
    let orchestration_enabled = std::env::var("OPENCLAW_ORCHESTRATION")
        .map(|v| v == "true" || v == "1")
        .unwrap_or(false);

    let orchestration_pipeline = if orchestration_enabled {
        let sandbox = Arc::new(openclaw_agent::ProcessSandbox::new());
        let pipeline = OrchestrationPipeline::new(
            provider.clone(),
            tools.clone(),
            sandbox,
            orchestration_config.clone(),
        );
        tracing::info!("orchestration pipeline enabled");
        Some(Arc::new(pipeline))
    } else {
        tracing::info!("orchestration pipeline disabled (set OPENCLAW_ORCHESTRATION=true to enable)");
        None
    };

    // --- Load SKILL.md skills ---
    let skills_dir = data_dir.join("skills");
    std::fs::create_dir_all(&skills_dir)?;
    let mut skill_registry = SkillRegistry::new();
    match openclaw_skills::load_skills_from_dir(&skills_dir) {
        Ok(loaded) => {
            let count = loaded.len();
            for s in loaded {
                skill_registry.register_md(Arc::new(s));
            }
            if count > 0 {
                tracing::info!(count, "SKILL.md skills loaded");
            }
        }
        Err(e) => tracing::warn!(error = %e, "failed to scan skills directory"),
    }

    // --- Build gateway state ---
    let mut state = openclaw_gateway::AppState::new(store, tools, provider, auth_token)
        .with_harness_router(router)
        .with_orchestration_config(orchestration_config)
        .with_skill_registry(Arc::new(skill_registry));

    if let Some(pipeline) = orchestration_pipeline {
        state = state.with_orchestration_pipeline(pipeline);
    }

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

/// Load orchestration config from file or defaults.
///
/// Priority:
/// 1. `data_dir/orchestration.toml` — full custom config
/// 2. Env vars for key settings
/// 3. Fallback to defaults
fn load_orchestration_config(data_dir: &std::path::Path) -> anyhow::Result<OrchestrationConfig> {
    let config_path = data_dir.join("orchestration.toml");
    if config_path.exists() {
        let contents = std::fs::read_to_string(&config_path)?;
        let config: OrchestrationConfig = toml::from_str(&contents)?;
        tracing::info!("loaded orchestration config from {}", config_path.display());
        return Ok(config);
    }

    let mut config = OrchestrationConfig::default();

    // Allow env var overrides for key settings.
    if let Ok(val) = std::env::var("ORCHESTRATION_MAX_RECURSION_DEPTH") {
        if let Ok(depth) = val.parse() {
            config.max_recursion_depth = depth;
        }
    }
    if let Ok(val) = std::env::var("ORCHESTRATION_CONSISTENCY_SAMPLES") {
        if let Ok(samples) = val.parse() {
            config.consistency_samples = samples;
        }
    }
    if let Ok(val) = std::env::var("ORCHESTRATION_MAX_REFINEMENT_ITERATIONS") {
        if let Ok(iters) = val.parse() {
            config.max_refinement_iterations = iters;
        }
    }
    if let Ok(model) = std::env::var("ORCHESTRATION_FALLBACK_MODEL") {
        config.fallback_model = Some(model);
    }
    if let Ok(model) = std::env::var("ORCHESTRATION_PRIMARY_MODEL") {
        config.primary_model = Some(model);
    }

    Ok(config)
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

/// Handle `skill list` and `skill install <path>` subcommands.
fn handle_skill_subcommand(data_dir: &std::path::Path, args: &[String]) -> anyhow::Result<()> {
    let skills_dir = data_dir.join("skills");
    std::fs::create_dir_all(&skills_dir)?;

    let sub = args.first().map(|s| s.as_str()).unwrap_or("list");
    match sub {
        "list" => {
            let skills = openclaw_skills::load_skills_from_dir(&skills_dir)?;
            if skills.is_empty() {
                println!("No skills installed.");
                println!("  Skills directory: {}", skills_dir.display());
                println!("  Place skill directories containing SKILL.md here.");
                return Ok(());
            }
            println!("{:<24} {:<10} {}", "NAME", "STATUS", "DESCRIPTION");
            println!("{}", "-".repeat(60));
            for s in &skills {
                let status = if s.validation.is_satisfied() {
                    "ready"
                } else {
                    "unmet"
                };
                println!(
                    "{:<24} {:<10} {}",
                    s.frontmatter.name, status, s.frontmatter.description
                );
                if !s.validation.missing_env.is_empty() {
                    println!("  missing env: {}", s.validation.missing_env.join(", "));
                }
                if !s.validation.missing_bins.is_empty() {
                    println!("  missing bins: {}", s.validation.missing_bins.join(", "));
                }
            }
        }
        "install" => {
            let src = args
                .get(1)
                .ok_or_else(|| anyhow::anyhow!("usage: skill install <path>"))?;
            let src_path = std::path::Path::new(src);
            if !src_path.is_dir() {
                anyhow::bail!("source path is not a directory: {}", src_path.display());
            }
            let skill_md = src_path.join("SKILL.md");
            if !skill_md.is_file() {
                anyhow::bail!("no SKILL.md found in {}", src_path.display());
            }
            let name = src_path
                .file_name()
                .ok_or_else(|| anyhow::anyhow!("cannot determine skill name from path"))?;
            let dest = skills_dir.join(name);
            copy_dir_recursive(src_path, &dest)?;
            println!("Installed skill to {}", dest.display());
        }
        _ => {
            eprintln!("Unknown skill subcommand: {sub}");
            eprintln!("Usage:");
            eprintln!("  openclaw-cli skill list              List installed skills");
            eprintln!("  openclaw-cli skill install <path>    Install a skill directory");
            std::process::exit(1);
        }
    }
    Ok(())
}

/// Recursively copy a directory.
fn copy_dir_recursive(src: &std::path::Path, dst: &std::path::Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let dest_path = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_dir_recursive(&entry.path(), &dest_path)?;
        } else {
            std::fs::copy(entry.path(), &dest_path)?;
        }
    }
    Ok(())
}
