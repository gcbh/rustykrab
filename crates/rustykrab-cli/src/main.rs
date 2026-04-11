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
use rustykrab_agent::{AgentEvent, HarnessProfile, HarnessRouter, OrchestrationPipeline};
use rustykrab_channels::telegram::ChannelMessage;
use rustykrab_channels::{TelegramChannel, VideoChannel, VideoConfig};
use rustykrab_core::model::ModelProvider;
use rustykrab_core::orchestration::OrchestrationConfig;
use rustykrab_core::types::MessageContent;
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
    ) -> rustykrab_core::Result<serde_json::Value> {
        let job = self
            .store
            .jobs()
            .create_job(schedule, task, channel, chat_id)?;
        Ok(serde_json::to_value(&job).expect("ScheduledJob is always serializable"))
    }

    async fn list_jobs(&self) -> rustykrab_core::Result<serde_json::Value> {
        let jobs = self.store.jobs().list_jobs()?;
        Ok(serde_json::to_value(&jobs).expect("Vec<ScheduledJob> is always serializable"))
    }

    async fn delete_job(&self, job_id: &str) -> rustykrab_core::Result<serde_json::Value> {
        let deleted = self.store.jobs().delete_job(job_id)?;
        Ok(serde_json::json!({ "deleted": deleted, "job_id": job_id }))
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
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
    let profile = load_harness_profile(&data_dir)?;
    tracing::info!(profile = %profile.name, "harness profile loaded");

    // --- Master key for credential encryption ---
    // On macOS: stored in the system Keychain (Secure Enclave / Touch ID protected).
    // On Linux: falls back to RUSTYKRAB_MASTER_KEY env var or ephemeral key.
    let master_key = rustykrab_store::keychain::resolve_master_key()
        .expect("failed to resolve master encryption key");

    let store = rustykrab_store::Store::open(data_dir.join("db"), master_key)?;

    // --- Auth token ---
    // Resolution order:
    // 1. RUSTYKRAB_AUTH_TOKEN env var (CI, Docker, explicit override)
    // 2. macOS Keychain (persists across restarts without env var)
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
            tracing::info!(%model, %base_url, "using Ollama provider");
            let p = rustykrab_providers::OllamaProvider::new(model).with_base_url(base_url);
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

    // --- Skills directory (needed by skill tools and skill loader) ---
    let skills_dir = data_dir.join("skills");
    std::fs::create_dir_all(&skills_dir)?;

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

    // --- Tools ---
    let mut tools = rustykrab_tools::builtin_tools(store.secrets());
    tools.extend(rustykrab_tools::memory_tools(memory_backend));
    tools.extend(rustykrab_tools::skill_tools(skills_dir.clone()));

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

    // --- Harness router (auto-selects profile per message) ---
    // Reuses the main provider for classification to avoid model swapping.
    // The classification prompt is ~50 tokens — negligible overhead on any model.
    let classifier: Arc<dyn ModelProvider> = provider.clone();

    let router = Arc::new(HarnessRouter::new(classifier).with_base(profile));

    // --- Orchestration pipeline (optional, enabled via RUSTYKRAB_ORCHESTRATION=true) ---
    let orchestration_config = load_orchestration_config(&data_dir)?;
    let orchestration_enabled = std::env::var("RUSTYKRAB_ORCHESTRATION")
        .map(|v| v == "true" || v == "1")
        .unwrap_or(false);

    let orchestration_pipeline = if orchestration_enabled {
        let sandbox = Arc::new(rustykrab_agent::ProcessSandbox::new());
        let pipeline = OrchestrationPipeline::new(
            provider.clone(),
            tools.clone(),
            sandbox,
            orchestration_config.clone(),
        );
        tracing::info!("orchestration pipeline enabled");
        Some(Arc::new(pipeline))
    } else {
        tracing::info!(
            "orchestration pipeline disabled (set RUSTYKRAB_ORCHESTRATION=true to enable)"
        );
        None
    };

    // --- Load SKILL.md skills ---
    let skills_dir = data_dir.join("skills");
    std::fs::create_dir_all(&skills_dir)?;
    let mut skill_registry = SkillRegistry::new();
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

    // --- Build gateway state ---
    // Clone store handle so we can flush it after the server shuts down.
    let store_handle = store.clone();
    let mut state = rustykrab_gateway::AppState::new(store, tools, provider, auth_token)
        .with_harness_router(router)
        .with_orchestration_config(orchestration_config)
        .with_skill_registry(Arc::new(skill_registry));

    if let Some(pipeline) = orchestration_pipeline {
        state = state.with_orchestration_pipeline(pipeline);
    }

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

    // --- Job executor (scheduled task runner) ---
    {
        let executor_store = store_handle.clone();
        let executor_state = state.clone();
        infra_handles.push(tokio::spawn(async move {
            job_executor_loop(executor_store, executor_state).await;
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

/// How long the agent can go without emitting any event (text delta,
/// tool start/end, etc.) before we consider it stalled.
const HEARTBEAT_TIMEOUT_SECS: u64 = 300; // 5 minutes

/// How often to resend the "typing" indicator while the agent is working.
/// Telegram's typing indicator expires after ~5 seconds.
const TYPING_INTERVAL_SECS: u64 = 4;

fn epoch_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Per-chat state for tracking conversations and preventing concurrent runs.
struct ChatState {
    conv_id: Uuid,
    /// True while an agent run is in progress for this chat.
    busy: bool,
}

/// Background task: consume inbound Telegram messages and run the agent.
///
/// Each Telegram chat_id gets its own persistent conversation. Messages
/// for different chats are processed concurrently so one slow agent run
/// doesn't block other users. Within a single chat, messages are serialized.
async fn telegram_agent_loop(
    mut rx: mpsc::Receiver<ChannelMessage>,
    tg: Arc<TelegramChannel>,
    state: AppState,
) {
    let chat_states: Arc<tokio::sync::Mutex<HashMap<i64, ChatState>>> =
        Arc::new(tokio::sync::Mutex::new(HashMap::new()));

    while let Some(channel_msg) = rx.recv().await {
        let chat_id = channel_msg.chat_id;
        let tg = tg.clone();
        let state = state.clone();
        let chat_states = chat_states.clone();

        tokio::spawn(async move {
            // Check if this chat already has an agent run in progress.
            {
                let states = chat_states.lock().await;
                if let Some(cs) = states.get(&chat_id) {
                    if cs.busy {
                        let _ = tg
                            .send_text(
                                chat_id,
                                "I'm still working on your previous message. Please wait.",
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
                    states.remove(&chat_id);
                }
                return;
            }

            let user_text = match &channel_msg.message.content {
                MessageContent::Text(t) => t.clone(),
                _ => return,
            };

            // Get or create conversation.
            let conv_id = {
                let mut states = chat_states.lock().await;
                match states.get(&chat_id) {
                    Some(cs) => cs.conv_id,
                    None => match state.store.conversations().create() {
                        Ok(conv) => {
                            let id = conv.id;
                            states.insert(
                                chat_id,
                                ChatState {
                                    conv_id: id,
                                    busy: false,
                                },
                            );
                            tracing::info!(chat_id, conv_id = %id, "created new conversation for Telegram chat");
                            id
                        }
                        Err(e) => {
                            tracing::error!(chat_id, "failed to create conversation: {e}");
                            let _ = tg
                                .send_text(chat_id, "Internal error — please try again.")
                                .await;
                            return;
                        }
                    },
                }
            };

            // Mark chat as busy.
            {
                let mut states = chat_states.lock().await;
                if let Some(cs) = states.get_mut(&chat_id) {
                    cs.busy = true;
                }
            }

            let reply = process_telegram_message(
                &state,
                &tg,
                chat_id,
                conv_id,
                channel_msg.message,
                &user_text,
            )
            .await;

            // Mark chat as no longer busy.
            {
                let mut states = chat_states.lock().await;
                if let Some(cs) = states.get_mut(&chat_id) {
                    cs.busy = false;
                }
            }

            // Send response back to Telegram.
            if let Err(e) = tg.send_text(chat_id, &reply).await {
                tracing::error!(chat_id, "failed to send Telegram reply: {e}");
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
    conv_id: Uuid,
    message: rustykrab_core::types::Message,
    user_text: &str,
) -> String {
    // Load the conversation.
    let mut conv = match state.store.conversations().get(conv_id) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(chat_id, %conv_id, "failed to load conversation: {e}");
            return "Internal error — please try again.".to_string();
        }
    };

    // Append user message.
    conv.messages.push(message);
    conv.updated_at = Utc::now();

    // Send initial typing indicator.
    let _ = tg.send_typing(chat_id).await;

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
            let _ = tg_typing.send_typing(chat_id).await;
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

async fn shutdown_signal() {
    tokio::signal::ctrl_c()
        .await
        .expect("failed to listen for ctrl+c");
    tracing::info!("shutdown signal received");
}

/// Background task: poll for due scheduled jobs and execute them.
///
/// Every 30 seconds, queries the job store for enabled jobs whose
/// `next_run_at` has passed. Each due job is spawned as an independent
/// task that runs the job's prompt through the agent pipeline and
/// delivers the response to the originating channel.
async fn job_executor_loop(store: rustykrab_store::Store, state: AppState) {
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
            let store = store.clone();
            let state = state.clone();

            tokio::spawn(async move {
                let job_id = job.id.clone();
                let task = job.task.clone();
                let channel = job.channel.clone();
                let chat_id = job.chat_id.clone();

                tracing::info!(
                    job_id = %job_id,
                    task = %task,
                    "executing scheduled job"
                );

                // Create a fresh conversation for this job execution.
                let mut conv = match state.store.conversations().create() {
                    Ok(c) => c,
                    Err(e) => {
                        tracing::error!(job_id = %job_id, "failed to create conversation for scheduled job: {e}");
                        return;
                    }
                };

                // Prefix the task so the agent knows this is a scheduled execution.
                let prompt = format!(
                    "[Scheduled task] The following task was scheduled by the user and is now due. \
                     Execute it and provide the result concisely.\n\nTask: {task}"
                );

                // Run the agent.
                let no_op_event = |_event: AgentEvent| {};
                let result = rustykrab_gateway::run_agent_streaming(
                    &state,
                    &mut conv,
                    &prompt,
                    &no_op_event,
                )
                .await;

                let response_text = match result {
                    Ok(msg) => match &msg.content {
                        MessageContent::Text(t) => t.clone(),
                        _ => "Scheduled task completed (no text response).".to_string(),
                    },
                    Err(_) => {
                        tracing::error!(job_id = %job_id, "agent error executing scheduled job");
                        "Sorry, the scheduled task encountered an error.".to_string()
                    }
                };

                // Route the response to the originating channel.
                match channel.as_deref() {
                    Some("telegram") => {
                        if let (Some(tg), Some(cid)) = (&state.telegram, &chat_id) {
                            if let Ok(chat_id_num) = cid.parse::<i64>() {
                                if let Err(e) = tg.send_text(chat_id_num, &response_text).await {
                                    tracing::error!(job_id = %job_id, "failed to send scheduled job result to Telegram: {e}");
                                }
                            } else {
                                tracing::error!(job_id = %job_id, chat_id = %cid, "invalid Telegram chat_id");
                            }
                        }
                    }
                    Some("signal") => {
                        if let (Some(sig), Some(number)) = (&state.signal, &chat_id) {
                            if let Err(e) = sig.send_text(number, &response_text).await {
                                tracing::error!(job_id = %job_id, "failed to send scheduled job result to Signal: {e}");
                            }
                        }
                    }
                    _ => {
                        tracing::info!(
                            job_id = %job_id,
                            "scheduled job completed (no channel routing): {response_text}"
                        );
                    }
                }

                // Mark the job as executed (advances next_run_at or disables one-shot).
                if let Err(e) = store.jobs().mark_executed(&job_id) {
                    tracing::error!(job_id = %job_id, "failed to mark scheduled job as executed: {e}");
                }

                // Clean up the ephemeral conversation.
                if let Err(e) = state.store.conversations().delete(conv.id) {
                    tracing::warn!(job_id = %job_id, "failed to clean up job conversation: {e}");
                }
            });
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
// These functions implement a multi-source lookup chain:
//   env var → macOS Keychain → SecretStore → generate/error
// When a credential is found or generated, it is persisted to the sources
// below so it survives future restarts without the env var.

/// Keychain service names for RustyKrab credentials.
const KEYCHAIN_SERVICE: &str = "com.rustykrab.credentials";
const KEYCHAIN_ACCOUNT_AUTH_TOKEN: &str = "auth-token";
const KEYCHAIN_ACCOUNT_API_KEY: &str = "anthropic-api-key";

/// Resolve the bearer auth token for the gateway.
///
/// Lookup chain: env var → Keychain → SecretStore → generate new.
/// A newly generated token is persisted to Keychain and SecretStore so
/// subsequent restarts pick it up automatically.
fn resolve_auth_token(store: &rustykrab_store::Store) -> String {
    // 1. Environment variable (highest priority — explicit override).
    if let Ok(token) = std::env::var("RUSTYKRAB_AUTH_TOKEN") {
        tracing::info!("auth token loaded from RUSTYKRAB_AUTH_TOKEN env var");
        // Persist downward so removing the env var still works next time.
        persist_credential(
            store,
            KEYCHAIN_ACCOUNT_AUTH_TOKEN,
            "rustykrab_auth_token",
            &token,
        );
        return token;
    }

    // 2. macOS Keychain.
    if rustykrab_store::keychain::keychain_available() {
        if let Ok(Some(cred)) =
            rustykrab_store::keychain::get_credential(KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT_AUTH_TOKEN)
        {
            tracing::info!("auth token loaded from macOS Keychain");
            // Also ensure it's in the SecretStore.
            let _ = store.secrets().set("rustykrab_auth_token", &cred.value);
            return cred.value;
        }
    }

    // 3. Encrypted SecretStore.
    if let Ok(token) = store.secrets().get("rustykrab_auth_token") {
        tracing::info!("auth token loaded from encrypted store");
        // Back-fill into Keychain if available.
        if rustykrab_store::keychain::keychain_available() {
            let _ = rustykrab_store::keychain::set_credential(
                KEYCHAIN_SERVICE,
                KEYCHAIN_ACCOUNT_AUTH_TOKEN,
                &token,
            );
        }
        return token;
    }

    // 4. Generate a new token and persist everywhere.
    let token = rustykrab_gateway::generate_token();
    tracing::info!("generated new auth token — persisting for future restarts");
    println!("\n  Auth token (also saved to Keychain/store): {token}\n");
    persist_credential(
        store,
        KEYCHAIN_ACCOUNT_AUTH_TOKEN,
        "rustykrab_auth_token",
        &token,
    );
    token
}

/// Resolve the Anthropic API key.
///
/// Lookup chain: env var → Keychain → SecretStore → empty (with error log).
fn resolve_api_key(store: &rustykrab_store::Store) -> String {
    // 1. Environment variable.
    if let Ok(key) = std::env::var("ANTHROPIC_API_KEY") {
        tracing::info!("API key loaded from ANTHROPIC_API_KEY env var");
        persist_credential(store, KEYCHAIN_ACCOUNT_API_KEY, "anthropic_api_key", &key);
        return key;
    }

    // 2. macOS Keychain.
    if rustykrab_store::keychain::keychain_available() {
        if let Ok(Some(cred)) =
            rustykrab_store::keychain::get_credential(KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT_API_KEY)
        {
            tracing::info!("API key loaded from macOS Keychain");
            let _ = store.secrets().set("anthropic_api_key", &cred.value);
            return cred.value;
        }
    }

    // 3. SecretStore.
    if let Ok(key) = store.secrets().get("anthropic_api_key") {
        tracing::info!("API key loaded from encrypted store");
        if rustykrab_store::keychain::keychain_available() {
            let _ = rustykrab_store::keychain::set_credential(
                KEYCHAIN_SERVICE,
                KEYCHAIN_ACCOUNT_API_KEY,
                &key,
            );
        }
        return key;
    }

    tracing::error!(
        "ANTHROPIC_API_KEY not set. Set the env var, store it via the secrets API, \
         or add it to macOS Keychain (service: {KEYCHAIN_SERVICE}, account: {KEYCHAIN_ACCOUNT_API_KEY})."
    );
    String::new()
}

/// Persist a credential to both the macOS Keychain and the encrypted
/// SecretStore. Errors are logged but not fatal — best-effort persistence.
fn persist_credential(
    store: &rustykrab_store::Store,
    keychain_account: &str,
    store_name: &str,
    value: &str,
) {
    if rustykrab_store::keychain::keychain_available() {
        if let Err(e) =
            rustykrab_store::keychain::set_credential(KEYCHAIN_SERVICE, keychain_account, value)
        {
            tracing::warn!("failed to persist credential to Keychain: {e}");
        }
    }
    if let Err(e) = store.secrets().set(store_name, value) {
        tracing::warn!("failed to persist credential to store: {e}");
    }
}

/// Handle `keychain status`, `keychain migrate`, and `keychain set` subcommands.
///
/// These let the user verify Keychain connectivity, migrate legacy keychain
/// items to the Data Protection Keychain, and manually seed credentials.
fn handle_keychain_subcommand(data_dir: &std::path::Path, args: &[String]) -> anyhow::Result<()> {
    let sub = args.first().map(|s| s.as_str()).unwrap_or("status");

    match sub {
        "status" => {
            println!(
                "macOS Keychain support: {}",
                if rustykrab_store::keychain::keychain_available() {
                    "available (Data Protection Keychain)"
                } else {
                    "not available (this platform does not support macOS Keychain)"
                }
            );

            if !rustykrab_store::keychain::keychain_available() {
                println!(
                    "\nOn non-macOS platforms, use environment variables or the encrypted store."
                );
                return Ok(());
            }

            // Check each known credential.
            let checks = [
                (
                    "Master key",
                    "com.rustykrab.master-key",
                    "rustykrab-encryption-key",
                ),
                ("Auth token", KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT_AUTH_TOKEN),
                (
                    "Anthropic API key",
                    KEYCHAIN_SERVICE,
                    KEYCHAIN_ACCOUNT_API_KEY,
                ),
            ];

            println!("\n{:<25} {:<40} STATUS", "CREDENTIAL", "SERVICE/ACCOUNT");
            println!("{}", "-".repeat(80));
            for (label, service, account) in &checks {
                let status = match rustykrab_store::keychain::get_credential(service, account) {
                    Ok(Some(_)) => "present",
                    Ok(None) => "not set",
                    Err(_) => "error",
                };
                println!("{:<25} {}/{:<14} {}", label, service, account, status);
            }
            println!();
            println!("All items use the Data Protection Keychain (no password prompts).");
        }

        "set" => {
            let name = args.get(1).ok_or_else(|| {
                anyhow::anyhow!(
                    "usage: rustykrab-cli keychain set <name> <value>\n  \
                 names: auth-token, api-key"
                )
            })?;
            let value = args.get(2).ok_or_else(|| {
                anyhow::anyhow!("usage: rustykrab-cli keychain set <name> <value>")
            })?;

            if !rustykrab_store::keychain::keychain_available() {
                anyhow::bail!("macOS Keychain is not available on this platform");
            }

            let (service, account) = match name.as_str() {
                "auth-token" => (KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT_AUTH_TOKEN),
                "api-key" => (KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT_API_KEY),
                other => {
                    // Allow arbitrary service/account if specified as service:account
                    if let Some((s, a)) = other.split_once(':') {
                        (s, a)
                    } else {
                        anyhow::bail!(
                            "unknown credential name '{other}'. \
                             Use: auth-token, api-key, or service:account"
                        );
                    }
                }
            };

            rustykrab_store::keychain::set_credential(service, account, value)
                .map_err(|e| anyhow::anyhow!("failed to store: {e}"))?;
            println!("Stored in Keychain: {service}/{account}");

            // Also persist to the encrypted store if the DB exists.
            let db_path = data_dir.join("db");
            if db_path.exists() {
                if let Ok(master_key) = rustykrab_store::keychain::resolve_master_key() {
                    if let Ok(store) = rustykrab_store::Store::open(&db_path, master_key) {
                        let store_name = match name.as_str() {
                            "auth-token" => "rustykrab_auth_token",
                            "api-key" => "anthropic_api_key",
                            _ => name.as_str(),
                        };
                        let _ = store.secrets().set(store_name, value);
                        println!("Also stored in encrypted store as '{store_name}'");
                    }
                }
            }
        }

        "migrate" => {
            if !rustykrab_store::keychain::keychain_available() {
                anyhow::bail!("macOS Keychain is not available on this platform");
            }

            println!("Migrating credentials to Data Protection Keychain...");
            println!(
                "(This re-creates items without per-app ACLs so no password prompts occur.)\n"
            );

            // Re-resolve master key — this migrates it to the DP keychain.
            match rustykrab_store::keychain::resolve_master_key() {
                Ok(_) => println!("  master key: OK"),
                Err(e) => println!("  master key: FAILED ({e})"),
            }

            // Migrate auth token and API key from env vars or existing store.
            let db_path = data_dir.join("db");
            if db_path.exists() {
                if let Ok(master_key) = rustykrab_store::keychain::resolve_master_key() {
                    if let Ok(store) = rustykrab_store::Store::open(&db_path, master_key) {
                        // Auth token
                        if let Ok(token) = store.secrets().get("rustykrab_auth_token") {
                            match rustykrab_store::keychain::set_credential(
                                KEYCHAIN_SERVICE,
                                KEYCHAIN_ACCOUNT_AUTH_TOKEN,
                                &token,
                            ) {
                                Ok(()) => println!("  auth token: migrated to DP Keychain"),
                                Err(e) => println!("  auth token: FAILED ({e})"),
                            }
                        } else {
                            println!(
                                "  auth token: not in store (will be generated on next start)"
                            );
                        }

                        // API key
                        if let Ok(key) = store.secrets().get("anthropic_api_key") {
                            match rustykrab_store::keychain::set_credential(
                                KEYCHAIN_SERVICE,
                                KEYCHAIN_ACCOUNT_API_KEY,
                                &key,
                            ) {
                                Ok(()) => println!("  API key: migrated to DP Keychain"),
                                Err(e) => println!("  API key: FAILED ({e})"),
                            }
                        } else {
                            println!("  API key: not in store (set via env var or 'keychain set api-key')");
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
            eprintln!(
                "  rustykrab-cli keychain status              Show Keychain credential status"
            );
            eprintln!("  rustykrab-cli keychain set <name> <value>  Store a credential");
            eprintln!(
                "  rustykrab-cli keychain migrate             Migrate to Data Protection Keychain"
            );
            eprintln!();
            eprintln!("Credential names: auth-token, api-key, or <service>:<account>");
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
