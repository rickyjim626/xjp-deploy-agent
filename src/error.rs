//! 统一错误处理
//!
//! 提供 `ApiError` 枚举实现 `IntoResponse`，替代重复的 `(StatusCode, Json<ErrorResponse>)` 模式

use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::Serialize;

/// API 错误响应结构
#[derive(Debug, Serialize)]
pub struct ErrorResponse {
    pub error: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<String>,
}

impl ErrorResponse {
    pub fn new(error: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            error: error.into(),
            message: message.into(),
            details: None,
        }
    }

    pub fn with_details(mut self, details: impl Into<String>) -> Self {
        self.details = Some(details.into());
        self
    }
}

/// 统一 API 错误类型
#[derive(Debug)]
pub enum ApiError {
    /// 401 - 未授权（API Key 无效或缺失）
    Unauthorized,
    /// 404 - 资源未找到
    NotFound(String),
    /// 400 - 请求无效
    BadRequest(String),
    /// 409 - 冲突（如：部署已在进行）
    Conflict(String),
    /// 500 - 内部错误
    Internal(String),
    /// 503 - 服务不可用
    ServiceUnavailable(String),
}

impl ApiError {
    /// 创建未授权错误
    pub fn unauthorized() -> Self {
        Self::Unauthorized
    }

    /// 创建未找到错误
    pub fn not_found(resource: impl Into<String>) -> Self {
        Self::NotFound(resource.into())
    }

    /// 创建请求无效错误
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::BadRequest(message.into())
    }

    /// 创建冲突错误
    pub fn conflict(message: impl Into<String>) -> Self {
        Self::Conflict(message.into())
    }

    /// 创建内部错误
    pub fn internal(message: impl Into<String>) -> Self {
        Self::Internal(message.into())
    }

    /// 创建服务不可用错误
    pub fn service_unavailable(message: impl Into<String>) -> Self {
        Self::ServiceUnavailable(message.into())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, error_type, message) = match self {
            ApiError::Unauthorized => (
                StatusCode::UNAUTHORIZED,
                "unauthorized",
                "Invalid or missing API key".to_string(),
            ),
            ApiError::NotFound(resource) => (
                StatusCode::NOT_FOUND,
                "not_found",
                format!("{} not found", resource),
            ),
            ApiError::BadRequest(msg) => (StatusCode::BAD_REQUEST, "bad_request", msg),
            ApiError::Conflict(msg) => (StatusCode::CONFLICT, "conflict", msg),
            ApiError::Internal(msg) => (StatusCode::INTERNAL_SERVER_ERROR, "internal_error", msg),
            ApiError::ServiceUnavailable(msg) => {
                (StatusCode::SERVICE_UNAVAILABLE, "service_unavailable", msg)
            }
        };

        let body = ErrorResponse::new(error_type, message);
        (status, Json(body)).into_response()
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApiError::Unauthorized => write!(f, "Unauthorized"),
            ApiError::NotFound(r) => write!(f, "Not found: {}", r),
            ApiError::BadRequest(m) => write!(f, "Bad request: {}", m),
            ApiError::Conflict(m) => write!(f, "Conflict: {}", m),
            ApiError::Internal(m) => write!(f, "Internal error: {}", m),
            ApiError::ServiceUnavailable(m) => write!(f, "Service unavailable: {}", m),
        }
    }
}

impl std::error::Error for ApiError {}

/// 便捷类型别名
pub type ApiResult<T> = Result<T, ApiError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_error_response_new() {
        let resp = ErrorResponse::new("test_error", "Test message");
        assert_eq!(resp.error, "test_error");
        assert_eq!(resp.message, "Test message");
        assert!(resp.details.is_none());
    }

    #[test]
    fn test_error_response_with_details() {
        let resp = ErrorResponse::new("test_error", "Test message").with_details("Extra info");
        assert_eq!(resp.details, Some("Extra info".to_string()));
    }
}
