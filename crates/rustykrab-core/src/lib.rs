/// The running RustyKrab version, for stamping persisted records so
/// behaviour can be attributed to a specific build.
///
/// Every workspace crate sets `version.workspace = true`, so this is the
/// workspace version and agrees across all of them. Read it from here
/// rather than calling `env!("CARGO_PKG_VERSION")` locally: in a crate
/// that ever stops inheriting the workspace version, a local `env!`
/// would silently start reporting something different.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub mod active_tools;
pub mod agent_def;
pub mod capability;
pub mod crypto;
pub mod error;
pub mod model;
pub mod orchestration;
pub mod outcome;
pub mod prompt_trace;
pub mod recall;
pub mod schema_validate;
pub mod session;
pub mod todo;
pub mod tool;
pub mod types;

pub use active_tools::{
    with_session_context, ActiveToolsRegistry, SessionToolContext, SESSION_TOOL_CONTEXT,
};
pub use agent_def::{AgentDefinition, AgentRegistry};
pub use capability::{is_subagent_tool, Capability, CapabilitySet, SUBAGENT_TOOL_NAMES};
pub use error::{Error, Result, ToolError, ToolErrorKind};
pub use model::ModelProvider;
pub use orchestration::{OrchestrationConfig, RecursiveCall, TaskComplexity, VoteResult};
pub use outcome::{
    classify_run, Attribution, AttributionKind, ExecutionCounters, OutcomeRecord, OutcomeSink,
    OutcomeTally, OutcomeVerdict, SignalClass,
};
pub use recall::RecallStore;
pub use schema_validate::validate_tool_args;
pub use session::Session;
pub use todo::{render_todos, TodoItem, TodoStatus, TodoStore};
pub use tool::{SandboxRequirements, Tool};
