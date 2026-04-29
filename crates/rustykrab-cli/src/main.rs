mod task_queue;

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use chrono::Utc;

const VERSION: &str = env!("CARGO_PKG_VERSION");
const GIT_HASH: &str = env!("RUSTYKRAB_GIT_HASH");
const GIT_DIRTY: &str = env!("RUSTYKRAB_GIT_DIRTY");
const BUILD_DATE: &str = env!("RUSTYKRAB_BUILD_DATE");

fn version_string() -> String {
    format!("{VERSION} ({GIT_HASH}{GIT_DIRTY}, {BUILD_DATE})")
}
use rustykrab_agent::{AgentEvent, HarnessProfile, HarnessRouter, ProcessSandbox, SubagentRunner};
use rustykrab_channels::slack::SlackInboundMessage;
use rustykrab_channels::telegram::ChannelMessage;
use rustykrab_channels::{SlackChannel, TelegramChannel, VideoChannel, VideoConfig};
use rustykrab_core::model::ModelProvider;
use rustykrab_core::orchestration::OrchestrationConfig;
use rustykrab_core::types::MessageContent;
use rustykrab_core::AgentRegistry;
use rustykrab_gateway::AppState;
use rustykrab_memory::backend::HybridMemoryBackend;
use rustykrab_memory::embedding::FastEmbedder;
use rustykrab_memory::storage::SqliteMemoryStorage;
use rustykrab_memory::{MemoryConfig, MemorySystem};
use rustykrab_skills::SkillRegistry;
use rustykrab_tools::{CronBackend, MemoryBackend};
use tokio::sync::mpsc;
use tracing_subscriber::fmt;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;
use uuid::Uuid;

/// Adapter bridging [HybridMemoryBackend] (rustykrab-memory) to the
/// [MemoryBackend] trait (rustykrab-tools) so the memory tools can use
/// the hybrid retrieval engine.
struct MemoryAdapter {
    inner: HybridMemoryBackend,
}

#[async_trait::async_trait]
impl MemoryBackend for MemoryAdapter {
    async fn search(
        &self,
        query: &str,
        tags: &[String],
        limit: usize,
    ) -> rustykrab_core::Result<serde_json::Value> {
        self.inner.search(query, tags, limit).await
    }
    async fn get(&self, memory_id: &str) -> rustykrab_core::Result<serde_json::Value> {
        self.inner.get(memory_id).await
    }
    async fn save(&self, fact: &str, tags: &[String]) -> rustykrab_core::Result<serde_json::Value> {
        self.inner.save(fact, tags).await
    }
    async fn delete(&self, memory_id: &str) -> rustykrab_core::Result<serde_json::Value> {
        self.inner.delete(memory_id).await
    }
    async fn list(&self) -> rustykrab_core::Result<serde_json::Value> {
        self.inner.list().await
    }
}

/// Adapter bridging [rustykrab_store::JobStore] to the [CronBackend] trait
/// (rustykrab-tools) so the cron tool can manage scheduled jobs.
struct CronAdapter {
    store: rustykrab_store::Store,
}

#[async_trait::async_trait]
impl CronBackend for CronAdapter {
    async fn create_job(
        &self,
        schedule: &str,
        task: &str,
        channel: Option<&str>,
        chat_id: Option<&str>,
        thread_id: Option<&str>,
    ) -> rustykrab_core::Result<serde_json::Value> {
        let job = self
            .store
            .jobs()
            .create_job(schedule, task, channel, chat_id, thread_id)?;
        Ok(serde_json::to_value(&job).expect("ScheduledJob is always serializable"))
    }

    async fn list_jobs(&self) -> rustykrab_core::Result<serde_json::Value> {
        let jobs = self.store.jobs().list_jobs()?;
        Ok(serde_json::to_value(&jobs).expect("Vec<ScheduledJob> is always serializable"))
    }

    async fn delete_job(&self, job_id: &str) -> rustykrab_core::Result<serde_json::Value> {
        // Grab the conversation id (if any) before the row goes away so we
        // can reap the associated persistent conversation below. Missing
        // jobs are fine; delete_job returns `false` without error.
        let conversation_id = match self.store.jobs().get_job(job_id) {
            Ok(job) => job.conversation_id,
            Err(rustykrab_core::Error::NotFound(_)) => None,
            Err(e) => return Err(e),
        };

        let deleted = self.store.jobs().delete_job(job_id)?;

        if deleted {
            if let Some(cid) = conversation_id {
                if let Ok(uuid) = uuid::Uuid::parse_str(&cid) {
                    // NotFound is fine — the conversation may already be gone.
                    match self.store.conversations().delete(uuid) {
                        Ok(()) | Err(rustykrab_core::Error::NotFound(_)) => {}
                        Err(e) => {
                            tracing::warn!(
                                job_id = %job_id,
                                conv_id = %cid,
                                "failed to reap conversation for deleted job: {e}"
                            );
                        }
                    }
                }
            }
        }

        Ok(serde_json::json!({ "deleted": deleted, "job_id": job_id }))
    }

    async fn list_runs(
        &self,
        job_id: &str,
        limit: u32,
    ) -> rustykrab_core::Result<serde_json::Value> {
        let runs = self.store.jobs().list_runs(job_id, limit)?;
        Ok(serde_json::to_value(&runs).expect("Vec<JobRun> is always serializable"))
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // --- TLS crypto provider (must be set before any rustls usage) ---
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Failed to install rustls crypto provider");

    // --- Data directory ---
    let data_dir = dirs::data_local_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join("rustykrab");
    std::fs::create_dir_all(&data_dir)?;

    // --- Logging: stdout + rolling file ---
    let log_dir = data_dir.join("logs");
    std::fs::create_dir_all(&log_dir)?;

    let file_appender = tracing_appender::rolling::daily(&log_dir, "rustykrab.log");
    let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);

    let env_filter = EnvFilter::from_default_env();

    tracing_subscriber::registry()
        .with(env_filter)
        .with(fmt::layer().with_writer(std::io::stdout))
        .with(fmt::layer().with_writer(non_blocking).with_ansi(false))
        .init();

    tracing::info!("rustykrab {}", version_string());

    // --- CLI subcommands ---
    let args: Vec<String> = std::env::args().collect();
    if args.len() >= 2 && (args[1] == "--version" || args[1] == "-V") {
        println!("rustykrab {}", version_string());
        return Ok(());
    }
    if args.len() >= 2 && args[1] == "skill" {
        return handle_skill_subcommand(&data_dir, &args[2..]);
    }
    if args.len() >= 2 && args[1] == "keychain" {
        return handle_keychain_subcommand(&data_dir, &args[2..]);
    }

    // --- Harness profile ---
    // Load from file or use a preset. Supported: default, coding, research, creative
    let mut profile = load_harness_profile(&data_dir)?;
    tracing::info!(profile = %profile.name, "harness profile loaded");

    // --- Master key for credential encryption ---
    // On macOS: stored in the Data Protection Keychain (loaded after first
    // unlock, no password prompt).
    // On Linux/Docker: must be supplied via RUSTYKRAB_MASTER_KEY — see the
    // "Linux / Docker setup" section of the README. The daemon refuses to
    // start without it rather than risk an ephemeral key that would leave
    // previously-stored secrets unrecoverable.
    let master_key = match rustykrab_store::keychain::resolve_master_key() {
        Ok(key) => key,
        Err(e) => {
            eprintln!();
            eprintln!("ERROR: {e}");
            eprintln!();
            std::process::exit(1);
        }
    };

    let store = rustykrab_store::Store::open(data_dir.join("db"), master_key)?;

    // --- Validate required secrets (central registry) ---
    // Every credential the app needs is declared in `registry::REGISTRY`.
    // Required secrets that cannot be resolved from any source (env var,
    // OS keychain, or encrypted store) cause a hard startup failure.
    {
        let missing = rustykrab_store::registry::validate(&store.secrets());
        let required_missing: Vec<_> = missing.iter().filter(|m| m.spec.required).collect();

        if !required_missing.is_empty() {
            eprintln!();
            eprintln!("ERROR: required secrets are missing — the application cannot start.");
            eprintln!();
            for m in &required_missing {
                eprintln!("  {} ({})", m.spec.description, m.spec.store_name);
                eprintln!("    Set via one of:");
                eprintln!("      env var:    export {}=<value>", m.spec.env_var);
                if cfg!(target_os = "macos") {
                    eprintln!(
                        "      keychain:   rustykrab-cli keychain set {} <value>",
                        m.spec.keychain_account
                    );
                }
                eprintln!(
                    "      store:      credential_write(action='set', name='{}', value='...')",
                    m.spec.store_name
                );
                eprintln!();
            }
            eprintln!("Tip: run `scripts/setup-secrets.sh` to store all required secrets at once.");
            std::process::exit(1);
        }

        // Warn about optional secrets that are absent.
        for m in missing.iter().filter(|m| !m.spec.required) {
            tracing::warn!(
                secret = m.spec.store_name,
                "optional secret '{}' not found — some features may be unavailable",
                m.spec.description,
            );
        }
    }

    // --- Auth token ---
    // Resolution order (via registry):
    // 1. Environment variable (CI, Docker, explicit override)
    // 2. OS credential store (persists across restarts without env var)
    // 3. Encrypted local SecretStore
    // 4. Generate a new token and persist it in Keychain + SecretStore
    let auth_token = resolve_auth_token(&store);

    // --- Model provider ---
    let provider_name =
        std::env::var("RUSTYKRAB_PROVIDER").unwrap_or_else(|_| "anthropic".to_string());
    let provider_name = provider_name.trim().to_lowercase();
    let provider: Arc<dyn ModelProvider> = match provider_name.as_str() {
        "ollama" => {
            let model = std::env::var("OLLAMA_MODEL").unwrap_or_else(|_| "gemma4:26b".to_string());
            let base_url = std::env::var("OLLAMA_BASE_URL")
                .unwrap_or_else(|_| "http://localhost:11434".to_string());
            let config = rustykrab_providers::OllamaConfig::default();
            tracing::info!(
                %model,
                %base_url,
                num_ctx = ?config.num_ctx,
                "using Ollama provider (num_ctx=None defers to server's OLLAMA_CONTEXT_LENGTH)"
            );
            let p = rustykrab_providers::OllamaProvider::new(model)
                .with_base_url(base_url)
                .with_config(config)
                .with_detected_context_window()
                .await;
            tracing::info!(
                num_ctx = ?p.num_ctx(),
                effective_ctx = ?p.effective_ctx(),
                "Ollama client-side context settings"
            );
            Arc::new(p)
        }
        _ => {
            let api_key = resolve_api_key(&store);
            let model = std::env::var("ANTHROPIC_MODEL")
                .unwrap_or_else(|_| "claude-sonnet-4-20250514".to_string());
            tracing::info!(%model, "using Anthropic provider");
            let p = rustykrab_providers::AnthropicProvider::new(api_key).with_model(model);
            Arc::new(p)
        }
    };

    // Override the harness profile's context budget with a
    // provider-aware default, or the RUSTYKRAB_MAX_CONTEXT_TOKENS env
    // var if set. Cloud models ship with large windows (Claude at 200k);
    // local Ollama deployments on consumer hardware can't chew through
    // anywhere near that before the HTTP timeout fires, so default to
    // 32k for Ollama and keep the 128k default for everything else.
    profile.max_context_tokens = resolve_max_context_tokens(&provider_name);
    tracing::info!(
        max_context_tokens = profile.max_context_tokens,
        provider = %provider_name,
        "compaction context budget configured"
    );

    // --- Skills directory (needed by skill tools and skill loader) ---
    let skills_dir = data_dir.join("skills");
    std::fs::create_dir_all(&skills_dir)?;

    // --- Soul file (default location, overridable via RUSTYKRAB_SOUL_PATH) ---
    if std::env::var_os(rustykrab_skills::prompt::SOUL_PATH_ENV).is_none() {
        let soul_path = data_dir.join("soul.md");
        std::env::set_var(rustykrab_skills::prompt::SOUL_PATH_ENV, &soul_path);
        tracing::info!(
            path = %soul_path.display(),
            "soul path defaulted (set RUSTYKRAB_SOUL_PATH to override)"
        );
    }

    // --- Memory system (hybrid retrieval: vector + BM25 + temporal + graph) ---
    let memory_db_path = data_dir.join("memory.db");
    let memory_storage = Arc::new(
        SqliteMemoryStorage::open(&memory_db_path).expect("failed to open memory database"),
    );
    let model_cache_dir = data_dir.join("models");
    std::fs::create_dir_all(&model_cache_dir)?;
    let embedder =
        Arc::new(FastEmbedder::new(model_cache_dir).expect("failed to initialize embedding model"));
    let memory_system = Arc::new(MemorySystem::new(
        MemoryConfig::default(),
        memory_storage,
        embedder,
    ));

    // Persist agent_id so memories survive restarts. Stored as a simple
    // file in the data directory; created once on first run.
    let agent_id_path = data_dir.join("agent_id");
    let agent_id = if agent_id_path.exists() {
        let raw = std::fs::read_to_string(&agent_id_path)?;
        Uuid::parse_str(raw.trim()).unwrap_or_else(|_| {
            tracing::warn!("corrupt agent_id file, generating new ID");
            let id = Uuid::new_v4();
            let _ = std::fs::write(&agent_id_path, id.to_string());
            id
        })
    } else {
        let id = Uuid::new_v4();
        std::fs::write(&agent_id_path, id.to_string())?;
        tracing::info!(%id, "generated new persistent agent_id");
        id
    };

    let session_id = Uuid::new_v4();

    // Rebuild FTS5 index from persisted memories (idempotent).
    let indexed = memory_system.rebuild_indexes(agent_id).await?;
    if indexed > 0 {
        tracing::info!(indexed, "FTS5 index rebuilt from stored memories");
    }

    let memory_backend: Arc<dyn MemoryBackend> = Arc::new(MemoryAdapter {
        inner: HybridMemoryBackend::new(Arc::clone(&memory_system), agent_id, session_id),
    });
    tracing::info!(%agent_id, "memory system initialized");

    // --- Idle lifecycle sweep ---
    // Runs a lifecycle sweep when there is no activity for N minutes.
    // Resets the idle timer on each inbound request (signaled via Notify).
    let activity_signal = Arc::new(tokio::sync::Notify::new());
    let idle_sweep_handle = {
        let system = Arc::clone(&memory_system);
        let activity = Arc::clone(&activity_signal);
        let idle_secs = memory_system.config().sweep_idle_trigger_minutes as u64 * 60;
        tokio::spawn(async move {
            loop {
                let idle = tokio::time::sleep(std::time::Duration::from_secs(idle_secs));
                tokio::select! {
                    _ = idle => {
                        if let Err(e) = system.lifecycle_sweep(agent_id).await {
                            tracing::warn!(error = %e, "idle lifecycle sweep failed");
                        }
                    }
                    _ = activity.notified() => {
                        continue;
                    }
                }
            }
        })
    };

    // --- Video channel (optional, enabled via RUSTYKRAB_VIDEO=true) ---
    let video_enabled = std::env::var("RUSTYKRAB_VIDEO")
        .map(|v| v == "true" || v == "1")
        .unwrap_or(false);

    let video_channel: Option<Arc<VideoChannel>> = if video_enabled {
        let video_dir = data_dir.join("video");
        std::fs::create_dir_all(&video_dir)?;

        let mut video_config = VideoConfig {
            projects_dir: video_dir,
            ..Default::default()
        };

        // Allow custom npx path via env.
        if let Ok(npx) = std::env::var("RUSTYKRAB_NPX_PATH") {
            video_config.npx_path = npx;
        }

        let channel = Arc::new(VideoChannel::new(video_config));
        tracing::info!("video communication channel enabled");
        Some(channel)
    } else {
        tracing::info!("video channel disabled (set RUSTYKRAB_VIDEO=true to enable)");
        None
    };

    // --- Skill registry (shared by hot-reload tools and the gateway state) ---
    let skill_registry = Arc::new(SkillRegistry::new());
    match rustykrab_skills::load_skills_from_dir(&skills_dir) {
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

    // --- Tools ---
    let mut tools = rustykrab_tools::builtin_tools(store.secrets());
    tools.extend(rustykrab_tools::memory_tools(memory_backend));
    tools.extend(rustykrab_tools::skill_tools(
        skills_dir.clone(),
        Some(skill_registry.clone()),
    ));

    // --- Cron tool (task scheduling) ---
    let cron_backend: Arc<dyn CronBackend> = Arc::new(CronAdapter {
        store: store.clone(),
    });
    tools.push(Arc::new(rustykrab_tools::CronTool::new(cron_backend)));
    tracing::info!("cron tool registered");

    // --- Video tool (if video channel enabled) ---
    if let Some(ref vc) = video_channel {
        let video_backend: Arc<dyn rustykrab_tools::VideoBackend> =
            Arc::new(rustykrab_tools::VideoChannelAdapter::new(vc.clone()));
        tools.extend(rustykrab_tools::video_tools(video_backend));
        tracing::info!("video tool registered");
    }

    // --- Log provider status ---
    tracing::info!(provider = provider.name(), "model provider configured");

    // --- Orchestration config (used by RLM module and subagent throttle) ---
    let orchestration_config = load_orchestration_config(&data_dir)?;

    // --- Sub-agent tools (subagents, agents_list) ---
    // Snapshot the tool list as it stands now so the sub-agent can call
    // every parent tool except the session/subagent meta-tools we are
    // about to add — that prevents a sub-agent from re-spawning itself
    // through the same registry. The per-tool depth guard inside
    // `SubagentsTool` is the second line of defence.
    let agent_registry = Arc::new(AgentRegistry::with_defaults());
    let subagent_runner: Arc<dyn rustykrab_tools::SessionManager> = Arc::new(SubagentRunner::new(
        provider.clone(),
        tools.clone(),
        Arc::new(ProcessSandbox::new()),
        agent_registry,
        orchestration_config.max_concurrent_tasks,
    ));
    tools.extend(rustykrab_tools::session_tools(subagent_runner));
    tracing::info!("subagent tools registered");

    // --- Recall tools (read compaction-displaced history) ---
    // The store backing these tools lives on AppState and is threaded
    // through the runner's SessionToolContext, so the tools resolve it
    // at execute() time — no construction-time store argument needed.
    tools.extend(rustykrab_agent::recall_tools(
        provider.clone(),
        orchestration_config.clone(),
    ));
    tracing::info!("recall tools registered");

    // --- Harness router (auto-selects profile per message) ---
    // Reuses the main provider for classification to avoid model swapping.
    // The classification prompt is ~50 tokens — negligible overhead on any model.
    let classifier: Arc<dyn ModelProvider> = provider.clone();

    let router = Arc::new(HarnessRouter::new(classifier).with_base(profile));

    // --- Build gateway state ---
    // Clone store handle so we can flush it after the server shuts down.
    let store_handle = store.clone();
    let mut state = rustykrab_gateway::AppState::new(store, tools, provider, auth_token)
        .with_harness_router(router)
        .with_orchestration_config(orchestration_config)
        .with_skill_registry(skill_registry)
        .with_memory(Arc::clone(&memory_system), agent_id);

    // --- Attach video channel to state ---
    if let Some(vc) = video_channel {
        state = state.with_video(vc);
    }

    // Track infrastructure task JoinHandles so panics are surfaced
    // instead of silently swallowed (fixes ASYNC-H4).
    let mut infra_handles: Vec<tokio::task::JoinHandle<()>> = Vec::new();

    // --- Telegram channel (optional) ---
    // We need to take the inbound_rx before wrapping in Arc, so build in stages.
    let mut telegram_rx: Option<mpsc::Receiver<ChannelMessage>> = None;
    let mut telegram_arc: Option<Arc<TelegramChannel>> = None;

    if let Ok(bot_token) = std::env::var("TELEGRAM_BOT_TOKEN") {
        let allowed_chats: HashSet<i64> = std::env::var("TELEGRAM_ALLOWED_CHATS")
            .unwrap_or_default()
            .split(',')
            .filter_map(|s| s.trim().parse().ok())
            .collect();

        let webhook_secret = std::env::var("TELEGRAM_WEBHOOK_SECRET").ok();

        let mut tg = TelegramChannel::new(bot_token, allowed_chats.clone());
        if let Some(secret) = webhook_secret {
            tg = tg.with_webhook_secret(secret);
        }

        // Take rx before wrapping in Arc (requires &mut self).
        telegram_rx = tg.take_inbound_rx();

        let tg = Arc::new(tg);
        telegram_arc = Some(tg.clone());
        state = state.with_telegram(tg.clone());

        if let Ok(webhook_url) = std::env::var("TELEGRAM_WEBHOOK_URL") {
            tg.set_webhook(&webhook_url).await?;
            tracing::info!("Telegram: webhook mode");
        } else {
            let tg_poll = tg.clone();
            // Store handle so panics are not silently swallowed (fixes ASYNC-H4).
            infra_handles.push(tokio::spawn(async move {
                if let Err(e) = tg_poll.start_polling().await {
                    tracing::error!("Telegram polling error: {e}");
                }
            }));
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
        let base_url =
            std::env::var("SIGNAL_CLI_URL").unwrap_or_else(|_| "http://localhost:8080".to_string());

        let allowed_numbers: HashSet<String> = std::env::var("SIGNAL_ALLOWED_NUMBERS")
            .unwrap_or_default()
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();

        let webhook_secret = std::env::var("SIGNAL_WEBHOOK_SECRET").ok();

        let mut sig = rustykrab_channels::SignalChannel::new(
            base_url.clone(),
            account_number.clone(),
            allowed_numbers.clone(),
        );
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
            // Store handle so panics are not silently swallowed (fixes ASYNC-H4).
            infra_handles.push(tokio::spawn(async move {
                if let Err(e) = sig_poll.start_polling().await {
                    tracing::error!("Signal polling error: {e}");
                }
            }));
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

    // --- Spawn Telegram agent loop (after state is fully built) ---
    if let (Some(rx), Some(tg)) = (telegram_rx, telegram_arc) {
        // Store handle so panics are not silently swallowed (fixes ASYNC-H4).
        infra_handles.push(tokio::spawn(telegram_agent_loop(rx, tg, state.clone())));
        tracing::info!("Telegram agent loop started");
    }

    // --- Slack channel (optional, Socket Mode) ---
    let mut slack_rx: Option<mpsc::Receiver<SlackInboundMessage>> = None;
    let mut slack_arc: Option<Arc<SlackChannel>> = None;

    if let (Ok(bot_token), Ok(app_token)) = (
        std::env::var("SLACK_BOT_TOKEN"),
        std::env::var("SLACK_APP_TOKEN"),
    ) {
        let allowed_channels: HashSet<String> = std::env::var("SLACK_ALLOWED_CHANNELS")
            .unwrap_or_default()
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        let allowed_teams: HashSet<String> = std::env::var("SLACK_ALLOWED_TEAMS")
            .unwrap_or_default()
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();

        let mut sl = SlackChannel::new(bot_token, app_token, allowed_channels.clone());
        if !allowed_teams.is_empty() {
            sl = sl.with_allowed_teams(allowed_teams.clone());
        }

        slack_rx = sl.take_inbound_rx();

        let sl = Arc::new(sl);
        slack_arc = Some(sl.clone());
        state = state.with_slack(sl.clone());

        let sl_socket = sl.clone();
        infra_handles.push(tokio::spawn(async move {
            if let Err(e) = sl_socket.start_socket_mode().await {
                tracing::error!("Slack Socket Mode error: {e}");
            }
        }));

        if allowed_channels.is_empty() {
            tracing::warn!(
                "SLACK_ALLOWED_CHANNELS not set — bot will deny all channels. \
                 Set it to a comma-separated list of Slack channel IDs (Cxxxxx)."
            );
        } else {
            tracing::info!(channels = ?allowed_channels, "Slack allowed channels configured");
        }
        if !allowed_teams.is_empty() {
            tracing::info!(teams = ?allowed_teams, "Slack allowed teams configured");
        }
    }

    // --- Spawn Slack agent loop (after state is fully built) ---
    if let (Some(rx), Some(sl)) = (slack_rx, slack_arc) {
        infra_handles.push(tokio::spawn(slack_agent_loop(rx, sl, state.clone())));
        tracing::info!("Slack agent loop started");
    }

    // --- Task queue (bounded concurrency for background work) ---
    let max_concurrent: usize = std::env::var("RUSTYKRAB_MAX_CONCURRENT_TASKS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(4);
    let (task_queue, queue_handle) = task_queue::TaskQueue::spawn(
        64, // buffer capacity
        max_concurrent,
        state.clone(),
        store_handle.clone(),
    );
    infra_handles.push(queue_handle);
    tracing::info!(max_concurrent, "task queue started");

    // --- Job executor (scheduled task runner) ---
    {
        let executor_store = store_handle.clone();
        let executor_queue = task_queue.clone();
        infra_handles.push(tokio::spawn(async move {
            job_executor_loop(executor_store, executor_queue).await;
        }));
        tracing::info!("job executor started (30s poll interval)");
    }

    // Save a reference to the video channel for shutdown.
    let video_shutdown_handle = state.video.clone();

    // --- Gateway with security middleware ---
    let app = rustykrab_gateway::router(state);

    // Bind to loopback only — never 0.0.0.0.
    let addr = SocketAddr::from(([127, 0, 0, 1], 3000));
    tracing::info!(%addr, "RustyKrab gateway listening");

    let listener = tokio::net::TcpListener::bind(addr).await?;
    let server = axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal());

    server.await?;

    // Abort infrastructure tasks and log any panics.
    for handle in &infra_handles {
        handle.abort();
    }
    for handle in infra_handles {
        if let Err(e) = handle.await {
            if e.is_panic() {
                tracing::error!("infrastructure task panicked during shutdown: {e}");
            }
        }
    }

    // Finalize memory session: Working → Episodic + lifecycle sweep.
    idle_sweep_handle.abort();
    tracing::info!("finalizing memory session...");
    if let Err(e) = memory_system.finalize_session(agent_id, session_id).await {
        tracing::warn!(error = %e, "failed to finalize memory session");
    }
    if let Err(e) = memory_system.lifecycle_sweep(agent_id).await {
        tracing::warn!(error = %e, "shutdown lifecycle sweep failed");
    }

    // Shut down video channel (MCP server).
    if let Some(ref vc) = video_shutdown_handle {
        tracing::info!("shutting down video channel...");
        vc.shutdown().await;
    }

    // Flush database before exit
    tracing::info!("flushing database...");
    store_handle
        .flush()
        .map_err(|e| anyhow::anyhow!("flush failed: {e}"))?;
    tracing::info!("shutdown complete");

    Ok(())
}

/// How long the Telegram agent can go without emitting any event (text delta,
/// tool start/end, etc.) before we consider it stalled. Longer than the web
/// frontend since Telegram execution is fully async.
const HEARTBEAT_TIMEOUT_SECS: u64 = 1800; // 30 minutes

/// How often to resend the "typing" indicator while the agent is working.
/// Telegram's typing indicator expires after ~5 seconds.
const TYPING_INTERVAL_SECS: u64 = 4;

fn epoch_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Per-chat (or per-thread in forum groups) state for tracking conversations
/// and preventing concurrent runs. Keyed by `(chat_id, thread_id)` where
/// `thread_id == 0` means a non-forum chat or the implicit "General" topic.
struct ChatState {
    conv_id: Uuid,
    /// True while an agent run is in progress for this chat/thread.
    busy: bool,
}

/// Background task: consume inbound Telegram messages and run the agent.
///
/// Each `(chat_id, thread_id)` pair gets its own persistent conversation.
/// Messages for different chats/threads are processed concurrently so one
/// slow agent run doesn't block other users or topics. Within a single
/// chat+thread, messages are serialized.
async fn telegram_agent_loop(
    mut rx: mpsc::Receiver<ChannelMessage>,
    tg: Arc<TelegramChannel>,
    state: AppState,
) {
    let chat_states: Arc<tokio::sync::Mutex<HashMap<(i64, i64), ChatState>>> =
        Arc::new(tokio::sync::Mutex::new(HashMap::new()));

    while let Some(channel_msg) = rx.recv().await {
        let chat_id = channel_msg.chat_id;
        let thread_id = channel_msg.thread_id;
        let key = (chat_id, thread_id);
        let tg = tg.clone();
        let state = state.clone();
        let chat_states = chat_states.clone();

        tokio::spawn(async move {
            // Check if this chat/thread already has an agent run in progress.
            {
                let states = chat_states.lock().await;
                if let Some(cs) = states.get(&key) {
                    if cs.busy {
                        let _ = tg
                            .send_text(
                                chat_id,
                                "I'm still working on your previous message. Please wait.",
                                thread_id,
                            )
                            .await;
                        return;
                    }
                }
            }

            // Handle conversation reset via structured flag (not sentinel string).
            if channel_msg.reset {
                {
                    let mut states = chat_states.lock().await;
                    states.remove(&key);
                }
                // Also clear the persisted mapping.
                if let Err(e) = state.store.chat_map().remove(chat_id, thread_id) {
                    tracing::warn!(chat_id, thread_id, "failed to remove chat map entry: {e}");
                }
                return;
            }

            let user_text = match &channel_msg.message.content {
                MessageContent::Text(t) => t.clone(),
                _ => return,
            };

            // Get or create conversation. Check in-memory first, then DB,
            // then create a brand new one.
            let conv_id = {
                let mut states = chat_states.lock().await;
                match states.get(&key) {
                    Some(cs) => cs.conv_id,
                    None => {
                        // Try the database (survives restarts).
                        let db_id = state
                            .store
                            .chat_map()
                            .lookup(chat_id, thread_id)
                            .ok()
                            .flatten();

                        match db_id {
                            Some(id) => {
                                states.insert(
                                    key,
                                    ChatState {
                                        conv_id: id,
                                        busy: false,
                                    },
                                );
                                tracing::info!(
                                    chat_id, thread_id, conv_id = %id,
                                    "restored conversation from database"
                                );
                                id
                            }
                            None => match state.store.conversations().create() {
                                Ok(mut conv) => {
                                    conv.channel_source = Some("telegram".to_string());
                                    conv.channel_id = Some(chat_id.to_string());
                                    if thread_id != 0 {
                                        conv.channel_thread_id = Some(thread_id.to_string());
                                    }
                                    if let Err(e) = state.store.conversations().save(&conv) {
                                        tracing::warn!(
                                            chat_id,
                                            "failed to persist channel metadata: {e}"
                                        );
                                    }
                                    let id = conv.id;
                                    states.insert(
                                        key,
                                        ChatState {
                                            conv_id: id,
                                            busy: false,
                                        },
                                    );
                                    // Persist the mapping so it survives restarts.
                                    if let Err(e) =
                                        state.store.chat_map().upsert(chat_id, thread_id, id)
                                    {
                                        tracing::warn!(
                                            chat_id,
                                            thread_id,
                                            "failed to persist chat map: {e}"
                                        );
                                    }
                                    tracing::info!(
                                        chat_id, thread_id, conv_id = %id,
                                        "created new conversation for Telegram chat/thread"
                                    );
                                    id
                                }
                                Err(e) => {
                                    tracing::error!(
                                        chat_id,
                                        thread_id,
                                        "failed to create conversation: {e}"
                                    );
                                    let _ = tg
                                        .send_text(
                                            chat_id,
                                            "Internal error — please try again.",
                                            thread_id,
                                        )
                                        .await;
                                    return;
                                }
                            },
                        }
                    }
                }
            };

            // Mark chat/thread as busy.
            {
                let mut states = chat_states.lock().await;
                if let Some(cs) = states.get_mut(&key) {
                    cs.busy = true;
                }
            }

            let reply = process_telegram_message(
                &state,
                &tg,
                chat_id,
                thread_id,
                conv_id,
                channel_msg.message,
                &user_text,
            )
            .await;

            // Mark chat/thread as no longer busy.
            {
                let mut states = chat_states.lock().await;
                if let Some(cs) = states.get_mut(&key) {
                    cs.busy = false;
                }
            }

            // Send response back to Telegram (in the correct thread).
            if let Err(e) = tg.send_text(chat_id, &reply, thread_id).await {
                tracing::error!(chat_id, thread_id, "failed to send Telegram reply: {e}");
            }
        });
    }

    tracing::warn!("Telegram agent loop exited — inbound channel closed");
}

/// Process a single Telegram message: load conversation, run agent, persist.
async fn process_telegram_message(
    state: &AppState,
    tg: &Arc<TelegramChannel>,
    chat_id: i64,
    thread_id: i64,
    conv_id: Uuid,
    message: rustykrab_core::types::Message,
    user_text: &str,
) -> String {
    // Load the conversation.
    let mut conv = match state.store.conversations().get(conv_id) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(chat_id, thread_id, %conv_id, "failed to load conversation: {e}");
            return "Internal error — please try again.".to_string();
        }
    };

    // Ensure channel metadata is present (backfills conversations created
    // before this field was populated).
    if conv.channel_source.is_none() {
        conv.channel_source = Some("telegram".to_string());
        conv.channel_id = Some(chat_id.to_string());
        if thread_id != 0 {
            conv.channel_thread_id = Some(thread_id.to_string());
        }
    }
    if conv.channel_thread_id.is_none() && thread_id != 0 {
        conv.channel_thread_id = Some(thread_id.to_string());
    }

    // Append user message.
    conv.messages.push(message);
    conv.updated_at = Utc::now();

    // Send initial typing indicator (scoped to forum thread if applicable).
    let _ = tg.send_typing(chat_id, thread_id).await;

    // Spawn a background task to keep re-sending typing indicators
    // while the agent is working.
    let typing_active = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let typing_flag = typing_active.clone();
    let tg_typing = tg.clone();
    let typing_task = tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(TYPING_INTERVAL_SECS)).await;
            if !typing_flag.load(std::sync::atomic::Ordering::Relaxed) {
                break;
            }
            let _ = tg_typing.send_typing(chat_id, thread_id).await;
        }
    });

    // Run agent with heartbeat-based timeout.
    let last_heartbeat = Arc::new(AtomicU64::new(epoch_millis()));
    let hb = last_heartbeat.clone();

    let on_event = move |_event: AgentEvent| {
        hb.store(epoch_millis(), Ordering::Relaxed);
    };

    let agent_fut = rustykrab_gateway::run_agent_streaming(state, &mut conv, user_text, &on_event);

    let timeout_millis = HEARTBEAT_TIMEOUT_SECS * 1000;
    let heartbeat_monitor = async {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            let last = last_heartbeat.load(Ordering::Relaxed);
            if epoch_millis() - last > timeout_millis {
                break;
            }
        }
    };

    let reply = tokio::select! {
        result = agent_fut => {
            match result {
                Ok(assistant_msg) => {
                    match &assistant_msg.content {
                        MessageContent::Text(t) => t.clone(),
                        _ => "I processed your message but have no text response.".to_string(),
                    }
                }
                Err(_status) => {
                    tracing::error!(chat_id, %conv_id, "agent returned error");
                    "Sorry, I encountered an error processing your message.".to_string()
                }
            }
        }
        _ = heartbeat_monitor => {
            tracing::error!(
                chat_id, %conv_id,
                "agent stalled — no activity for {HEARTBEAT_TIMEOUT_SECS}s"
            );
            "Sorry, the agent appears to have stalled. Please try again.".to_string()
        }
    };

    // Stop typing indicator.
    typing_active.store(false, std::sync::atomic::Ordering::Relaxed);
    typing_task.abort();

    // Persist conversation.
    if let Err(e) = state.store.conversations().save(&conv) {
        tracing::error!(chat_id, %conv_id, "failed to persist conversation: {e}");
    }

    reply
}

/// Per-(team, channel, thread) Slack state, mirroring [`ChatState`] for
/// Telegram. Slack threads are keyed by their `thread_ts`; when the
/// inbound mention is at the top level, the bot auto-threads off the
/// user's message timestamp so the conversation key is the user's `ts`.
struct SlackChatState {
    conv_id: Uuid,
    busy: bool,
}

/// `(team_id, channel_id, effective_thread_ts)` → per-thread state. The
/// effective thread is `inbound.thread_ts` when the mention was already
/// inside a thread, and `inbound.message_ts` otherwise (auto-threading).
type SlackChatStateMap = HashMap<(String, String, String), SlackChatState>;

/// Background task: consume inbound Slack messages and run the agent.
///
/// Each `(team_id, channel_id, effective_thread_ts)` triple gets its own
/// persistent conversation. When a user `@`-mentions the bot at the top
/// level of a channel, the reply auto-threads off the user's message and
/// the conversation is keyed by that message's `ts` — every subsequent
/// reply in that thread joins the same conversation.
async fn slack_agent_loop(
    mut rx: mpsc::Receiver<SlackInboundMessage>,
    sl: Arc<SlackChannel>,
    state: AppState,
) {
    let chat_states: Arc<tokio::sync::Mutex<SlackChatStateMap>> =
        Arc::new(tokio::sync::Mutex::new(HashMap::new()));

    while let Some(inbound) = rx.recv().await {
        // Auto-thread: top-level mentions reply in a new thread off the
        // user's message; in-thread mentions stay in the same thread.
        let effective_thread_ts = inbound
            .thread_ts
            .clone()
            .unwrap_or_else(|| inbound.message_ts.clone());
        let key = (
            inbound.team_id.clone(),
            inbound.channel_id.clone(),
            effective_thread_ts.clone(),
        );
        let sl = sl.clone();
        let state = state.clone();
        let chat_states = chat_states.clone();

        tokio::spawn(async move {
            // Concurrency guard: serialize within a single thread.
            {
                let states = chat_states.lock().await;
                if let Some(cs) = states.get(&key) {
                    if cs.busy {
                        let _ = sl
                            .send_text(
                                &inbound.channel_id,
                                "I'm still working on your previous message. Please wait.",
                                Some(&effective_thread_ts),
                            )
                            .await;
                        return;
                    }
                }
            }

            if inbound.reset {
                {
                    let mut states = chat_states.lock().await;
                    states.remove(&key);
                }
                if let Err(e) = state.store.slack_chat_map().remove(
                    &inbound.team_id,
                    &inbound.channel_id,
                    &effective_thread_ts,
                ) {
                    tracing::warn!(
                        team_id = %inbound.team_id,
                        channel_id = %inbound.channel_id,
                        thread_ts = %effective_thread_ts,
                        "failed to remove Slack chat map entry: {e}"
                    );
                }
                return;
            }

            let user_text = match &inbound.message.content {
                MessageContent::Text(t) => t.clone(),
                _ => return,
            };

            // Resolve / create the conversation.
            let conv_id = {
                let mut states = chat_states.lock().await;
                match states.get(&key) {
                    Some(cs) => cs.conv_id,
                    None => {
                        let db_id = state
                            .store
                            .slack_chat_map()
                            .lookup(&inbound.team_id, &inbound.channel_id, &effective_thread_ts)
                            .ok()
                            .flatten();

                        match db_id {
                            Some(id) => {
                                states.insert(
                                    key.clone(),
                                    SlackChatState {
                                        conv_id: id,
                                        busy: false,
                                    },
                                );
                                tracing::info!(
                                    team_id = %inbound.team_id,
                                    channel_id = %inbound.channel_id,
                                    thread_ts = %effective_thread_ts,
                                    conv_id = %id,
                                    "restored Slack conversation from database"
                                );
                                id
                            }
                            None => match state.store.conversations().create() {
                                Ok(mut conv) => {
                                    conv.channel_source = Some("slack".to_string());
                                    conv.channel_id = Some(inbound.channel_id.clone());
                                    conv.channel_thread_id = Some(effective_thread_ts.clone());
                                    if let Err(e) = state.store.conversations().save(&conv) {
                                        tracing::warn!(
                                            channel_id = %inbound.channel_id,
                                            "failed to persist Slack channel metadata: {e}"
                                        );
                                    }
                                    let id = conv.id;
                                    states.insert(
                                        key.clone(),
                                        SlackChatState {
                                            conv_id: id,
                                            busy: false,
                                        },
                                    );
                                    if let Err(e) = state.store.slack_chat_map().upsert(
                                        &inbound.team_id,
                                        &inbound.channel_id,
                                        &effective_thread_ts,
                                        id,
                                    ) {
                                        tracing::warn!(
                                            team_id = %inbound.team_id,
                                            channel_id = %inbound.channel_id,
                                            thread_ts = %effective_thread_ts,
                                            "failed to persist Slack chat map: {e}"
                                        );
                                    }
                                    tracing::info!(
                                        team_id = %inbound.team_id,
                                        channel_id = %inbound.channel_id,
                                        thread_ts = %effective_thread_ts,
                                        conv_id = %id,
                                        "created new Slack conversation"
                                    );
                                    id
                                }
                                Err(e) => {
                                    tracing::error!(
                                        channel_id = %inbound.channel_id,
                                        "failed to create Slack conversation: {e}"
                                    );
                                    let _ = sl
                                        .send_text(
                                            &inbound.channel_id,
                                            "Internal error — please try again.",
                                            Some(&effective_thread_ts),
                                        )
                                        .await;
                                    return;
                                }
                            },
                        }
                    }
                }
            };

            // Mark busy.
            {
                let mut states = chat_states.lock().await;
                if let Some(cs) = states.get_mut(&key) {
                    cs.busy = true;
                }
            }

            let reply = process_slack_message(
                &state,
                conv_id,
                &inbound.channel_id,
                &effective_thread_ts,
                inbound.message,
                &user_text,
            )
            .await;

            // Clear busy.
            {
                let mut states = chat_states.lock().await;
                if let Some(cs) = states.get_mut(&key) {
                    cs.busy = false;
                }
            }

            if let Err(e) = sl
                .send_text(&inbound.channel_id, &reply, Some(&effective_thread_ts))
                .await
            {
                tracing::error!(
                    channel_id = %inbound.channel_id,
                    thread_ts = %effective_thread_ts,
                    "failed to send Slack reply: {e}"
                );
            }
        });
    }

    tracing::warn!("Slack agent loop exited — inbound channel closed");
}

/// Process a single Slack message: load conversation, run agent, persist.
async fn process_slack_message(
    state: &AppState,
    conv_id: Uuid,
    channel_id: &str,
    thread_ts: &str,
    message: rustykrab_core::types::Message,
    user_text: &str,
) -> String {
    let mut conv = match state.store.conversations().get(conv_id) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(channel_id, thread_ts, %conv_id, "failed to load Slack conversation: {e}");
            return "Internal error — please try again.".to_string();
        }
    };

    if conv.channel_source.is_none() {
        conv.channel_source = Some("slack".to_string());
        conv.channel_id = Some(channel_id.to_string());
        conv.channel_thread_id = Some(thread_ts.to_string());
    }
    if conv.channel_thread_id.is_none() {
        conv.channel_thread_id = Some(thread_ts.to_string());
    }

    conv.messages.push(message);
    conv.updated_at = Utc::now();

    // Heartbeat-monitored agent run, mirroring the Telegram path.
    let last_heartbeat = Arc::new(AtomicU64::new(epoch_millis()));
    let hb = last_heartbeat.clone();
    let on_event = move |_event: AgentEvent| {
        hb.store(epoch_millis(), Ordering::Relaxed);
    };

    let agent_fut = rustykrab_gateway::run_agent_streaming(state, &mut conv, user_text, &on_event);

    let timeout_millis = HEARTBEAT_TIMEOUT_SECS * 1000;
    let heartbeat_monitor = async {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            let last = last_heartbeat.load(Ordering::Relaxed);
            if epoch_millis() - last > timeout_millis {
                break;
            }
        }
    };

    let reply = tokio::select! {
        result = agent_fut => {
            match result {
                Ok(assistant_msg) => match &assistant_msg.content {
                    MessageContent::Text(t) => t.clone(),
                    _ => "I processed your message but have no text response.".to_string(),
                },
                Err(_status) => {
                    tracing::error!(channel_id, %conv_id, "Slack agent returned error");
                    "Sorry, I encountered an error processing your message.".to_string()
                }
            }
        }
        _ = heartbeat_monitor => {
            tracing::error!(
                channel_id, %conv_id,
                "Slack agent stalled — no activity for {HEARTBEAT_TIMEOUT_SECS}s"
            );
            "Sorry, the agent appears to have stalled. Please try again.".to_string()
        }
    };

    if let Err(e) = state.store.conversations().save(&conv) {
        tracing::error!(channel_id, %conv_id, "failed to persist Slack conversation: {e}");
    }

    reply
}

async fn shutdown_signal() {
    tokio::signal::ctrl_c()
        .await
        .expect("failed to listen for ctrl+c");
    tracing::info!("shutdown signal received");
}

/// Background task: poll for due scheduled jobs and submit them to
/// the task queue.
///
/// Every 30 seconds, queries the job store for enabled jobs whose
/// `next_run_at` has passed. Each due job is submitted to the shared
/// task queue with a dedup key so a long-running job cannot be picked
/// up again on the next poll cycle.
async fn job_executor_loop(store: rustykrab_store::Store, queue: task_queue::TaskQueue) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));

    loop {
        interval.tick().await;

        let now = Utc::now();
        let due_jobs = match store.jobs().get_due_jobs(now) {
            Ok(jobs) => jobs,
            Err(e) => {
                tracing::warn!(error = %e, "failed to query due jobs");
                continue;
            }
        };

        for job in due_jobs {
            let request = task_queue::TaskRequest {
                prompt: job.task.clone(),
                source: task_queue::TaskSource::Cron {
                    job_id: job.id.clone(),
                    channel: job.channel.clone(),
                    chat_id: job.chat_id.clone(),
                    thread_id: job.thread_id.clone(),
                },
                dedupe_key: Some(format!("cron:{}", job.id)),
            };

            if let Err(e) = queue.submit(request).await {
                tracing::error!(job_id = %job.id, "failed to submit job to task queue: {e}");
            }
        }
    }
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
    if let Ok(val) = std::env::var("ORCHESTRATION_PIPELINE_TIMEOUT_SECS") {
        if let Ok(secs) = val.parse() {
            config.pipeline_timeout_secs = secs;
        }
    }
    if let Ok(val) = std::env::var("ORCHESTRATION_MODEL_CALL_TIMEOUT_SECS") {
        if let Ok(secs) = val.parse() {
            config.model_call_timeout_secs = secs;
        }
    }

    Ok(config)
}

/// Resolve the effective `max_context_tokens` value.
///
/// Priority:
/// 1. `RUSTYKRAB_MAX_CONTEXT_TOKENS` env var when set to a positive integer
/// 2. Provider-aware default: 32k for Ollama (local inference with
///    limited GPU memory), 128k for everything else (cloud models)
fn resolve_max_context_tokens(provider_name: &str) -> usize {
    if let Some(v) = std::env::var("RUSTYKRAB_MAX_CONTEXT_TOKENS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|&v| v > 0)
    {
        return v;
    }
    match provider_name {
        "ollama" => 32_000,
        _ => 128_000,
    }
}

/// Load harness profile from file or env var preset.
///
/// Priority:
/// 1. `data_dir/harness.toml` — full custom profile
/// 2. `RUSTYKRAB_HARNESS` env var — one of: default, coding, research, creative
/// 3. Fallback to default profile
fn load_harness_profile(data_dir: &std::path::Path) -> anyhow::Result<HarnessProfile> {
    let profile_path = data_dir.join("harness.toml");
    if profile_path.exists() {
        let contents = std::fs::read_to_string(&profile_path)?;
        let profile: HarnessProfile = toml::from_str(&contents)?;
        return Ok(profile);
    }

    let preset = std::env::var("RUSTYKRAB_HARNESS").unwrap_or_else(|_| "default".to_string());
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
            let skills = rustykrab_skills::load_skills_from_dir(&skills_dir)?;
            if skills.is_empty() {
                println!("No skills installed.");
                println!("  Skills directory: {}", skills_dir.display());
                println!("  Place skill directories containing SKILL.md here.");
                return Ok(());
            }
            println!("{:<24} {:<10} DESCRIPTION", "NAME", "STATUS");
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
            eprintln!("  rustykrab-cli skill list              List installed skills");
            eprintln!("  rustykrab-cli skill install <path>    Install a skill directory");
            std::process::exit(1);
        }
    }
    Ok(())
}

// --- Credential resolution helpers ---
// These use the central registry (rustykrab_store::registry) for the
// env-var → OS-keychain → SecretStore lookup chain.

/// Resolve the bearer auth token for the gateway.
///
/// Uses the registry to check env / keychain / store, then generates
/// a new token if none exists.
fn resolve_auth_token(store: &rustykrab_store::Store) -> String {
    let spec = rustykrab_store::registry::lookup("rustykrab_auth_token")
        .expect("rustykrab_auth_token must be in the registry");

    if let Some(token) = rustykrab_store::registry::resolve(spec, &store.secrets()) {
        tracing::info!("auth token resolved via registry");
        return token;
    }

    // Not found anywhere — generate a new token and persist it.
    let token = rustykrab_gateway::generate_token();
    tracing::info!("generated new auth token — persisting for future restarts");
    println!("\n  Auth token (also saved to credential store): {token}\n");

    let svc = rustykrab_store::registry::keychain_service();
    if rustykrab_store::keychain::keychain_available() {
        let _ = rustykrab_store::keychain::set_credential(svc, spec.keychain_account, &token);
    }
    let _ = store.secrets().set(spec.store_name, &token);
    token
}

/// Resolve the Anthropic API key.
///
/// Uses the registry to check env / keychain / store.
fn resolve_api_key(store: &rustykrab_store::Store) -> String {
    let spec = rustykrab_store::registry::lookup("anthropic_api_key")
        .expect("anthropic_api_key must be in the registry");

    if let Some(key) = rustykrab_store::registry::resolve(spec, &store.secrets()) {
        tracing::info!("API key resolved via registry");
        return key;
    }

    if cfg!(target_os = "macos") {
        tracing::error!(
            "ANTHROPIC_API_KEY not set. Set the env var, store it via the secrets API, \
             or add it to the OS credential store (rustykrab-cli keychain set {}).",
            spec.keychain_account,
        );
    } else {
        tracing::error!(
            "ANTHROPIC_API_KEY not set. Set the env var or store it via the \
             gateway secrets API."
        );
    }
    String::new()
}

/// Handle `keychain status`, `keychain migrate`, and `keychain set` subcommands.
///
/// These let the user verify Keychain connectivity, migrate legacy keychain
/// items to the Data Protection Keychain, and manually seed credentials.
fn handle_keychain_subcommand(data_dir: &std::path::Path, args: &[String]) -> anyhow::Result<()> {
    let sub = args.first().map(|s| s.as_str()).unwrap_or("status");

    match sub {
        "status" => {
            let available = rustykrab_store::keychain::keychain_available();
            println!(
                "OS credential store: {}",
                if available {
                    "available (macOS Data Protection Keychain)"
                } else {
                    "not supported on this platform"
                }
            );

            if !available {
                println!(
                    "\nThe `keychain` subcommand is macOS-only. On Linux/Docker, \
                     set RUSTYKRAB_MASTER_KEY and the per-credential RUSTYKRAB_* \
                     env vars (or write secrets to the encrypted store via the \
                     gateway secrets API). See the README for details."
                );
                return Ok(());
            }

            // Master key (separate service).
            let master_status = match rustykrab_store::keychain::get_credential(
                "com.rustykrab.master-key",
                "rustykrab-encryption-key",
            ) {
                Ok(Some(_)) => "present",
                Ok(None) => "not set",
                Err(_) => "error",
            };

            let svc = rustykrab_store::registry::keychain_service();
            println!(
                "\n{:<25} {:<25} {:<10} STATUS",
                "CREDENTIAL", "ACCOUNT", "REQUIRED"
            );
            println!("{}", "-".repeat(75));
            println!(
                "{:<25} {:<25} {:<10} {}",
                "Master key", "rustykrab-encryption-key", "-", master_status
            );

            // All registry entries.
            for spec in rustykrab_store::registry::REGISTRY {
                let status =
                    match rustykrab_store::keychain::get_credential(svc, spec.keychain_account) {
                        Ok(Some(_)) => "present",
                        Ok(None) => "not set",
                        Err(_) => "error",
                    };
                let req = if spec.required { "yes" } else { "no" };
                println!(
                    "{:<25} {:<25} {:<10} {}",
                    spec.description, spec.keychain_account, req, status
                );
            }
            println!();
        }

        "set" => {
            // Build the list of accepted names from the registry.
            let known_names: Vec<&str> = rustykrab_store::registry::REGISTRY
                .iter()
                .map(|s| s.keychain_account)
                .collect();
            let names_list = known_names.join(", ");

            let name = args.get(1).ok_or_else(|| {
                anyhow::anyhow!(
                    "usage: rustykrab-cli keychain set <name> <value>\n  \
                     names: {names_list}, or <service>:<account>"
                )
            })?;
            let value = args.get(2).ok_or_else(|| {
                anyhow::anyhow!("usage: rustykrab-cli keychain set <name> <value>")
            })?;

            if !rustykrab_store::keychain::keychain_available() {
                anyhow::bail!(
                    "the `keychain` subcommand is macOS-only. On Linux/Docker, \
                     set the credential's RUSTYKRAB_* env var or write to the \
                     encrypted store via the gateway secrets API."
                );
            }

            let svc = rustykrab_store::registry::keychain_service();

            // Look up the name in the registry first, then fall back to
            // arbitrary service:account pairs.
            let (service, account, store_name) =
                if let Some(spec) = rustykrab_store::registry::lookup_by_account(name) {
                    (svc, spec.keychain_account, Some(spec.store_name))
                } else if let Some((s, a)) = name.split_once(':') {
                    (s, a, None)
                } else {
                    anyhow::bail!(
                        "unknown credential name '{name}'. \
                         Use: {names_list}, or <service>:<account>"
                    );
                };

            rustykrab_store::keychain::set_credential(service, account, value)
                .map_err(|e| anyhow::anyhow!("failed to store: {e}"))?;
            println!("Stored in credential store: {service}/{account}");

            // Also persist to the encrypted store if the DB exists.
            if let Some(sn) = store_name {
                let db_path = data_dir.join("db");
                if db_path.exists() {
                    if let Ok(master_key) = rustykrab_store::keychain::resolve_master_key() {
                        if let Ok(store) = rustykrab_store::Store::open(&db_path, master_key) {
                            let _ = store.secrets().set(sn, value);
                            println!("Also stored in encrypted store as '{sn}'");
                        }
                    }
                }
            }
        }

        "migrate" => {
            if !rustykrab_store::keychain::keychain_available() {
                anyhow::bail!(
                    "the `keychain migrate` subcommand is macOS-only. On \
                     Linux/Docker, credentials live in env vars or the \
                     encrypted store — there is no OS keychain to migrate to."
                );
            }

            println!("Migrating credentials to OS credential store...\n");

            // Re-resolve master key — this ensures it is stored in the credential store.
            match rustykrab_store::keychain::resolve_master_key() {
                Ok(_) => println!("  master key: OK"),
                Err(e) => println!("  master key: FAILED ({e})"),
            }

            // Migrate all registry secrets from the encrypted store.
            let db_path = data_dir.join("db");
            if db_path.exists() {
                if let Ok(master_key) = rustykrab_store::keychain::resolve_master_key() {
                    if let Ok(store) = rustykrab_store::Store::open(&db_path, master_key) {
                        let svc = rustykrab_store::registry::keychain_service();
                        for spec in rustykrab_store::registry::REGISTRY {
                            if let Ok(val) = store.secrets().get(spec.store_name) {
                                match rustykrab_store::keychain::set_credential(
                                    svc,
                                    spec.keychain_account,
                                    &val,
                                ) {
                                    Ok(()) => {
                                        println!("  {}: migrated", spec.description)
                                    }
                                    Err(e) => {
                                        println!("  {}: FAILED ({e})", spec.description)
                                    }
                                }
                            } else {
                                println!(
                                    "  {}: not in store (set via 'keychain set {}')",
                                    spec.description, spec.keychain_account
                                );
                            }
                        }
                    }
                }
            } else {
                println!(
                    "  No database found at {} — skipping store migration",
                    db_path.display()
                );
            }

            println!("\nMigration complete. Restart rustykrab-cli to verify.");
        }

        _ => {
            eprintln!("Unknown keychain subcommand: {sub}");
            eprintln!("Usage:");
            eprintln!("  rustykrab-cli keychain status              Show credential store status");
            eprintln!("  rustykrab-cli keychain set <name> <value>  Store a credential");
            eprintln!(
                "  rustykrab-cli keychain migrate             Migrate store to OS credential store"
            );
            eprintln!();
            eprint!("Credential names: ");
            let names: Vec<&str> = rustykrab_store::registry::REGISTRY
                .iter()
                .map(|s| s.keychain_account)
                .collect();
            eprintln!("{}, or <service>:<account>", names.join(", "));
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
