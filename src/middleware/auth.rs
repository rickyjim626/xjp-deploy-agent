//! API Key 认证中间件
//!
//! 提供 `RequireApiKey` extractor，替代每个 handler 中重复的 API key 校验逻辑

use axum::{
    async_trait,
    extract::FromRequestParts,
    http::{header::HeaderMap, request::Parts},
};
use std::sync::Arc;

use crate::error::ApiError;
use crate::state::AppState;

/// API Key 认证 Extractor
///
/// 在需要认证的 handler 中使用此 extractor，自动验证 `x-api-key` header
///
/// # Example
///
/// ```ignore
/// async fn protected_handler(
///     _auth: RequireApiKey,
///     State(state): State<Arc<AppState>>,
/// ) -> impl IntoResponse {
///     // handler 逻辑...
/// }
/// ```
#[derive(Debug, Clone)]
pub struct RequireApiKey;

#[async_trait]
impl FromRequestParts<Arc<AppState>> for RequireApiKey {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &Arc<AppState>,
    ) -> Result<Self, Self::Rejection> {
        verify_api_key(&parts.headers, &state.api_key)
    }
}

/// 验证 API Key
///
/// 检查 `x-api-key` header 是否与配置的 API key 匹配
pub fn verify_api_key(headers: &HeaderMap, expected_key: &str) -> Result<RequireApiKey, ApiError> {
    let provided_key = headers
        .get("x-api-key")
        .and_then(|v| v.to_str().ok());

    match provided_key {
        Some(key) if key == expected_key => Ok(RequireApiKey),
        Some(_) => {
            tracing::warn!("Invalid API key provided");
            Err(ApiError::unauthorized())
        }
        None => {
            tracing::warn!("Missing x-api-key header");
            Err(ApiError::unauthorized())
        }
    }
}

/// 可选 API Key 认证 Extractor
///
/// 不强制要求 API key，但如果提供了会验证是否正确
/// 用于部分需要区分认证/未认证访问的接口
#[derive(Debug, Clone)]
pub struct OptionalApiKey {
    pub authenticated: bool,
}

#[async_trait]
impl FromRequestParts<Arc<AppState>> for OptionalApiKey {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &Arc<AppState>,
    ) -> Result<Self, Self::Rejection> {
        let provided_key = parts
            .headers
            .get("x-api-key")
            .and_then(|v| v.to_str().ok());

        match provided_key {
            Some(key) if key == state.api_key => Ok(OptionalApiKey { authenticated: true }),
            Some(_) => {
                // 提供了 key 但不匹配，返回错误
                tracing::warn!("Invalid API key provided");
                Err(ApiError::unauthorized())
            }
            None => {
                // 未提供 key，允许通过但标记为未认证
                Ok(OptionalApiKey { authenticated: false })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn test_verify_api_key_success() {
        let mut headers = HeaderMap::new();
        headers.insert("x-api-key", HeaderValue::from_static("test-key"));

        let result = verify_api_key(&headers, "test-key");
        assert!(result.is_ok());
    }

    #[test]
    fn test_verify_api_key_wrong_key() {
        let mut headers = HeaderMap::new();
        headers.insert("x-api-key", HeaderValue::from_static("wrong-key"));

        let result = verify_api_key(&headers, "test-key");
        assert!(result.is_err());
    }

    #[test]
    fn test_verify_api_key_missing() {
        let headers = HeaderMap::new();

        let result = verify_api_key(&headers, "test-key");
        assert!(result.is_err());
    }
}
