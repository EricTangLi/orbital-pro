//! 统一错误码：**唯一来源**（审核 P2-2）。所有对外错误都从这里出。

use thiserror::Error;

#[derive(Debug, Error)]
pub enum OrbitalError {
    #[error("agent 身份不匹配（configured={configured} claimed={claimed}）")]
    AgentIdentityMismatch { configured: String, claimed: String },

    /// 请求没带令牌（审核 P1-12：身份不能靠"启动参数"自证）
    #[error("缺少 agent 令牌")]
    AgentTokenMissing,

    /// 令牌无效（不存在或与任何 agent 不匹配）
    #[error("agent 令牌无效")]
    AgentTokenInvalid,

    #[error("记忆不存在：{id}")]
    MemoryNotFound { id: String },

    #[error("无权访问该作用域的这条记忆")]
    ScopeForbidden,

    #[error("内容重复（已存在 id={existing_id}）")]
    DuplicateContent { existing_id: String },

    #[error("参数非法：{reason}")]
    InvalidArgument { reason: String },

    #[error("嵌入服务不可用（已降级到关键词检索）：{reason}")]
    EmbeddingUnavailable { reason: String },

    #[error("写队列繁忙（可重试）")]
    WriteBusy,

    #[error("超过限流阈值（可重试）")]
    RateLimited,

    #[error("数据库 schema 版本过旧，拒绝服务（数据已保留）")]
    SchemaTooOld,

    #[error("内部错误：{0}")]
    Internal(String),
}

impl OrbitalError {
    /// 稳定的错误码字符串：**改动会破坏客户端，必须走 ADR**。
    pub fn code(&self) -> &'static str {
        match self {
            Self::AgentIdentityMismatch { .. } => "AGENT_IDENTITY_MISMATCH",
            Self::AgentTokenMissing => "AGENT_TOKEN_MISSING",
            Self::AgentTokenInvalid => "AGENT_TOKEN_INVALID",
            Self::MemoryNotFound { .. } => "MEMORY_NOT_FOUND",
            Self::ScopeForbidden => "SCOPE_FORBIDDEN",
            Self::DuplicateContent { .. } => "DUPLICATE_CONTENT",
            Self::InvalidArgument { .. } => "INVALID_ARGUMENT",
            Self::EmbeddingUnavailable { .. } => "EMBEDDING_UNAVAILABLE",
            Self::WriteBusy => "WRITE_BUSY",
            Self::RateLimited => "RATE_LIMITED",
            Self::SchemaTooOld => "SCHEMA_TOO_OLD",
            Self::Internal(_) => "INTERNAL",
        }
    }

    /// HTTP 状态码（MCP 与 proxy 共用同一套映射）。
    pub fn http_status(&self) -> u16 {
        match self {
            Self::AgentIdentityMismatch { .. } | Self::ScopeForbidden => 403,
            Self::AgentTokenMissing => 401,
            Self::AgentTokenInvalid => 403,
            Self::MemoryNotFound { .. } => 404,
            Self::DuplicateContent { .. } => 200,
            Self::InvalidArgument { .. } => 400,
            Self::EmbeddingUnavailable { .. } => 503,
            Self::WriteBusy | Self::RateLimited => 429,
            Self::SchemaTooOld => 500,
            Self::Internal(_) => 500,
        }
    }
}

pub type Result<T> = std::result::Result<T, OrbitalError>;
