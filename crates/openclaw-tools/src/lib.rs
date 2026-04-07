// Security utilities
pub mod sanitize;
pub mod security;

// Filesystem tools
mod apply_patch;
mod edit;
mod read;
mod write;

// Runtime tools
mod code_execution;
mod exec;
mod process;

// Web tools
mod web_fetch;
mod web_search;
mod x_search;

// Session tools
pub mod session_manager;
mod agents_list;
mod session_status;
mod sessions_history;
mod sessions_list;
mod sessions_send;
mod sessions_spawn;
mod sessions_yield;
mod subagents;

// Memory tools
pub mod memory_backend;
mod memory_delete;
mod memory_get;
mod memory_save;
mod memory_search;

// Messaging tools
pub mod message_backend;
mod message;

// Automation tools
pub mod cron_backend;
mod cron;
pub mod gateway_backend;
mod gateway;

// Media tools
mod image;
mod image_generate;
mod pdf;
mod tts;

// UI tools
mod browser;
mod canvas;

// Device tools
mod nodes;

// HTTP
mod http_request;
mod http_session;

// Email tools
mod gmail;

// Credentials (from main)
mod credential_read;
mod credential_write;

// Skill tools
mod skill_create;

// --- Public re-exports ---

// Filesystem
pub use apply_patch::ApplyPatchTool;
pub use edit::EditTool;
pub use read::ReadTool;
pub use write::WriteTool;

// Runtime
pub use code_execution::CodeExecutionTool;
pub use exec::ExecTool;
pub use process::ProcessTool;

// Web
pub use web_fetch::WebFetchTool;
pub use web_search::WebSearchTool;
pub use x_search::XSearchTool;

// Session
pub use agents_list::AgentsListTool;
pub use session_manager::SessionManager;
pub use session_status::SessionStatusTool;
pub use sessions_history::SessionsHistoryTool;
pub use sessions_list::SessionsListTool;
pub use sessions_send::SessionsSendTool;
pub use sessions_spawn::SessionsSpawnTool;
pub use sessions_yield::SessionsYieldTool;
pub use subagents::SubagentsTool;

// Memory
pub use memory_backend::MemoryBackend;
pub use memory_delete::MemoryDeleteTool;
pub use memory_get::MemoryGetTool;
pub use memory_save::MemorySaveTool;
pub use memory_search::MemorySearchTool;

// Messaging
pub use message::MessageTool;
pub use message_backend::MessageBackend;

// Automation
pub use cron::CronTool;
pub use cron_backend::CronBackend;
pub use gateway::GatewayTool;
pub use gateway_backend::GatewayBackend;

// Media
pub use self::image::ImageTool;
pub use image_generate::ImageGenerateTool;
pub use pdf::PdfTool;
pub use tts::TtsTool;

// UI
pub use browser::BrowserTool;
pub use canvas::CanvasTool;

// Devices
pub use nodes::NodesTool;

// HTTP
pub use http_request::HttpRequestTool;
pub use http_session::HttpSessionTool;

// Email
pub use gmail::GmailTool;

// Credentials
pub use credential_read::CredentialReadTool;
pub use credential_write::CredentialWriteTool;

// Skills
pub use skill_create::SkillCreateTool;

/// Collect all built-in tools that require no external backend into a Vec.
///
/// Tools that need access to the secret store (credential_read, credential_write)
/// require a `SecretStore` handle. The remaining tools are stateless or self-contained.
pub fn builtin_tools(
    secrets: openclaw_store::SecretStore,
) -> Vec<std::sync::Arc<dyn openclaw_core::Tool>> {
    vec![
        // HTTP
        std::sync::Arc::new(HttpRequestTool::new()),
        std::sync::Arc::new(HttpSessionTool::new()),
        // Filesystem
        std::sync::Arc::new(ReadTool::new()),
        std::sync::Arc::new(WriteTool::new()),
        std::sync::Arc::new(EditTool::new()),
        std::sync::Arc::new(ApplyPatchTool::new()),
        // Runtime
        std::sync::Arc::new(ExecTool::new()),
        std::sync::Arc::new(ProcessTool::new()),
        std::sync::Arc::new(CodeExecutionTool::new()),
        // Web
        std::sync::Arc::new(WebFetchTool::new()),
        std::sync::Arc::new(WebSearchTool::new()),
        std::sync::Arc::new(XSearchTool::new()),
        // Media
        std::sync::Arc::new(ImageTool::new()),
        std::sync::Arc::new(ImageGenerateTool::new()),
        std::sync::Arc::new(TtsTool::new()),
        std::sync::Arc::new(PdfTool::new()),
        // UI
        std::sync::Arc::new(BrowserTool::new()),
        std::sync::Arc::new(CanvasTool::new()),
        // Devices
        std::sync::Arc::new(NodesTool::new()),
        // Email
        std::sync::Arc::new(GmailTool::new(secrets.clone())),
        // Credentials
        std::sync::Arc::new(CredentialReadTool::new(secrets.clone())),
        std::sync::Arc::new(CredentialWriteTool::new(secrets)),
    ]
}

/// Collect session/agent tools that require a SessionManager into a Vec.
pub fn session_tools(
    manager: std::sync::Arc<dyn SessionManager>,
) -> Vec<std::sync::Arc<dyn openclaw_core::Tool>> {
    vec![
        std::sync::Arc::new(SessionsListTool::new(manager.clone())),
        std::sync::Arc::new(SessionsHistoryTool::new(manager.clone())),
        std::sync::Arc::new(SessionsSendTool::new(manager.clone())),
        std::sync::Arc::new(SessionsSpawnTool::new(manager.clone())),
        std::sync::Arc::new(SessionsYieldTool::new(manager.clone())),
        std::sync::Arc::new(SessionStatusTool::new(manager.clone())),
        std::sync::Arc::new(AgentsListTool::new(manager.clone())),
        std::sync::Arc::new(SubagentsTool::new(manager)),
    ]
}

/// Collect memory tools that require a MemoryBackend into a Vec.
pub fn memory_tools(
    backend: std::sync::Arc<dyn MemoryBackend>,
) -> Vec<std::sync::Arc<dyn openclaw_core::Tool>> {
    vec![
        std::sync::Arc::new(MemorySaveTool::new(backend.clone())),
        std::sync::Arc::new(MemorySearchTool::new(backend.clone())),
        std::sync::Arc::new(MemoryGetTool::new(backend.clone())),
        std::sync::Arc::new(MemoryDeleteTool::new(backend)),
    ]
}

/// Collect messaging tools that require a MessageBackend into a Vec.
pub fn message_tools(
    backend: std::sync::Arc<dyn MessageBackend>,
) -> Vec<std::sync::Arc<dyn openclaw_core::Tool>> {
    vec![std::sync::Arc::new(MessageTool::new(backend))]
}

/// Collect skill tools that require a skills directory into a Vec.
pub fn skill_tools(
    skills_dir: std::path::PathBuf,
) -> Vec<std::sync::Arc<dyn openclaw_core::Tool>> {
    vec![std::sync::Arc::new(SkillCreateTool::new(skills_dir))]
}

/// Collect automation tools that require backends into a Vec.
pub fn automation_tools(
    cron_backend: std::sync::Arc<dyn CronBackend>,
    gateway_backend: std::sync::Arc<dyn GatewayBackend>,
) -> Vec<std::sync::Arc<dyn openclaw_core::Tool>> {
    vec![
        std::sync::Arc::new(CronTool::new(cron_backend)),
        std::sync::Arc::new(GatewayTool::new(gateway_backend)),
    ]
}
