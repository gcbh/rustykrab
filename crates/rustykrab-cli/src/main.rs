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
use rustykrab_agent::{AgentEvent, HarnessProfile, HarnessRouter};
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
mod media_delivery;

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
    let profile = load_harness_profile(&data_dir)?;
    tracing::info!(profile = %profile.name, "harness profile loaded");

    // --- Master key for credential encryption ---
    // On macOS: stored in the system Keychain (Secure Enclave / Touch ID protected).
    // On Linux: falls back to RUSTYKRAB_MASTER_KEY env var or ephemeral key.
    let master_key = rustykrab_store::keychain::resolve_master_key()
        .expect("failed to resolve master encryption key");

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
                eprintln!(
                    "      keychain:   rustykrab-cli keychain set {} <value>",
                    m.spec.keychain_account
                );
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
            tracing::info!(%model, %base_url, num_ctx = config.num_ctx, "using Ollama provider");
            let p = rustykrab_providers::OllamaProvider::new(model)
                .with_base_url(base_url)
                .with_config(config);
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

include!("main_init.rs");
include!("main_server.rs");
include!("main_tg.rs");
include!("main_helpers.rs");
include!("main_cli.rs");
