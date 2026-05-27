use std::fmt;

use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

/// Categorized tool execution error.
///
/// Carries both a human-readable message and a machine-readable kind so the
/// runner can make smart retry decisions (e.g. don't retry `NotFound`).
#[derive(Debug, Clone)]
pub struct ToolError {
    pub kind: ToolErrorKind,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolErrorKind {
    /// Bad or missing arguments from the model.
    InvalidInput,
    /// Target resource doesn't exist.
    NotFound,
    /// Missing credentials or insufficient access.
    PermissionDenied,
    /// Exceeded time limit.
    Timeout,
    /// Quota exceeded — retriable after delay.
    RateLimited,
    /// Network/IO — worth retrying.
    Transient,
    /// Catch-all (default for untyped errors).
    Internal,
}

impl ToolError {
    pub fn invalid_input(msg: impl Into<String>) -> Self {
        Self {
            kind: ToolErrorKind::InvalidInput,
            message: msg.into(),
        }
    }
    pub fn not_found(msg: impl Into<String>) -> Self {
        Self {
            kind: ToolErrorKind::NotFound,
            message: msg.into(),
        }
    }
    pub fn permission_denied(msg: impl Into<String>) -> Self {
        Self {
            kind: ToolErrorKind::PermissionDenied,
            message: msg.into(),
        }
    }
    pub fn timeout(msg: impl Into<String>) -> Self {
        Self {
            kind: ToolErrorKind::Timeout,
            message: msg.into(),
        }
    }
    pub fn rate_limited(msg: impl Into<String>) -> Self {
        Self {
            kind: ToolErrorKind::RateLimited,
            message: msg.into(),
        }
    }
    pub fn transient(msg: impl Into<String>) -> Self {
        Self {
            kind: ToolErrorKind::Transient,
            message: msg.into(),
        }
    }
    pub fn internal(msg: impl Into<String>) -> Self {
        Self {
            kind: ToolErrorKind::Internal,
            message: msg.into(),
        }
    }
}

impl ToolErrorKind {
    /// Stable machine-readable identifier, forwarded to the model so it can
    /// react to the failure category rather than parsing the message text.
    pub fn as_str(&self) -> &'static str {
        match self {
            ToolErrorKind::InvalidInput => "invalid_input",
            ToolErrorKind::NotFound => "not_found",
            ToolErrorKind::PermissionDenied => "permission_denied",
            ToolErrorKind::Timeout => "timeout",
            ToolErrorKind::RateLimited => "rate_limited",
            ToolErrorKind::Transient => "transient",
            ToolErrorKind::Internal => "internal",
        }
    }

    /// Whether retrying the *same* call unchanged might succeed. Distinct from
    /// the runner's retry policy: this is a hint to the model. `InvalidInput`
    /// and `NotFound` are false because the model must change something first.
    pub fn retryable(&self) -> bool {
        matches!(
            self,
            ToolErrorKind::Timeout | ToolErrorKind::RateLimited | ToolErrorKind::Transient
        )
    }
}

impl fmt::Display for ToolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl From<String> for ToolError {
    fn from(s: String) -> Self {
        Self {
            kind: ToolErrorKind::Internal,
            message: s,
        }
    }
}

impl From<&str> for ToolError {
    fn from(s: &str) -> Self {
        Self {
            kind: ToolErrorKind::Internal,
            message: s.to_string(),
        }
    }
}

#[derive(Debug, Error)]
pub enum Error {
    #[error("model provider error: {0}")]
    ModelProvider(String),

    #[error("model provider rate limited: {0}")]
    ModelRateLimit(String),

    #[error("model provider authentication failed: {0}")]
    ModelAuthError(String),

    #[error("model provider bad request: {0}")]
    ModelBadRequest(String),

    #[error("model provider overloaded: {0}")]
    ModelOverloaded(String),

    #[error("model refused to respond due to content policy")]
    ContentPolicy,

    #[error("tool execution error: {0}")]
    ToolExecution(ToolError),

    #[error("configuration error: {0}")]
    Config(String),

    #[error("storage error: {0}")]
    Storage(String),

    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    #[error("channel error: {0}")]
    Channel(String),

    #[error("authentication error: {0}")]
    Auth(String),

    #[error("not found: {0}")]
    NotFound(String),

    #[error("{0}")]
    Internal(String),
}

impl Error {
    /// Best-effort categorization for surfacing the failure to the agent.
    ///
    /// Tool errors carry their own kind; other variants are mapped to the
    /// closest category so the model always receives a structured signal.
    pub fn kind(&self) -> ToolErrorKind {
        match self {
            Error::ToolExecution(te) => te.kind,
            Error::ModelRateLimit(_) | Error::ModelOverloaded(_) => ToolErrorKind::RateLimited,
            Error::ModelAuthError(_) | Error::Auth(_) => ToolErrorKind::PermissionDenied,
            Error::ModelBadRequest(_) => ToolErrorKind::InvalidInput,
            Error::NotFound(_) => ToolErrorKind::NotFound,
            Error::ModelProvider(_) | Error::Channel(_) => ToolErrorKind::Transient,
            Error::Config(_)
            | Error::Storage(_)
            | Error::Serialization(_)
            | Error::ContentPolicy
            | Error::Internal(_) => ToolErrorKind::Internal,
        }
    }
}
