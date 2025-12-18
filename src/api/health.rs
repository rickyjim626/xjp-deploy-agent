//! 健康检查和系统状态 API
//!
//! 包含 /health, /status, /projects, /restart 端点

use axum::{
    extract::State,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use serde::Serialize;
use std::sync::Arc;

use crate::config::autoupdate::AutoUpdateStatus;
use crate::config::env::constants::VERSION;
use crate::error::ApiResult;
use crate::middleware::RequireApiKey;
use crate::state::AppState;

/// 健康检查响应
#[derive(Debug, Serialize)]
struct HealthResponse {
    status: &'static str,
    service: &'static str,
    version: &'static str,
    timestamp: String,
    projects: Vec<String>,
    active_deploys: usize,
    active_projects: Vec<String>,
    auto_update_enabled: bool,
    auto_update: AutoUpdateStatus,
}

/// 重启响应
#[derive(Debug, Serialize)]
pub struct RestartResponse {
    pub success: bool,
    pub message: String,
}

/// 创建健康检查路由
pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/health", get(health_check))
        .route("/status", get(health_check))
        .route("/projects", get(list_projects))
        .route("/restart", post(restart_service))
}

/// 健康检查 - 返回状态、版本、运行时间等信息
///
/// GET /health, GET /status
/// 无需认证
async fn health_check(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let (active_count, active_projects) = {
        let running_deploys = state.running_deploys.read().await;
        let count = running_deploys.len();
        let projects: Vec<String> = running_deploys.keys().cloned().collect();
        (count, projects)
    };

    let projects: Vec<String> = state.projects.keys().cloned().collect();

    // 构建自动更新状态
    let auto_update_status = if let Some(ref _config) = state.auto_update_config {
        let update_state = &state.auto_update_state;
        AutoUpdateStatus {
            enabled: true,
            current_version: VERSION.to_string(),
            latest_version: update_state.latest_version.read().await.clone(),
            update_available: *update_state.update_available.read().await,
            last_check: update_state.last_check.read().await.map(|t| t.to_rfc3339()),
            last_check_result: update_state.last_check_result.read().await.clone(),
            next_check: update_state.next_check.read().await.map(|t| t.to_rfc3339()),
            update_progress: update_state.update_progress.read().await.clone(),
        }
    } else {
        // 回退到旧的逻辑：检查是否配置了 xjp-deploy-agent 项目
        let legacy_enabled = state.projects.contains_key("xjp-deploy-agent");
        AutoUpdateStatus::disabled(VERSION.to_string())
            .tap_mut(|s| s.enabled = legacy_enabled)
    };

    Json(HealthResponse {
        status: "ok",
        service: "xjp-deploy-agent",
        version: VERSION,
        timestamp: chrono::Utc::now().to_rfc3339(),
        projects,
        active_deploys: active_count,
        active_projects,
        auto_update_enabled: auto_update_status.enabled,
        auto_update: auto_update_status,
    })
}

/// 列出支持的项目
///
/// GET /projects
/// 需要 API Key 认证
async fn list_projects(
    _auth: RequireApiKey, // 使用 extractor 自动验证
    State(state): State<Arc<AppState>>,
) -> ApiResult<impl IntoResponse> {
    let projects: Vec<&String> = state.projects.keys().collect();
    Ok(Json(serde_json::json!({ "projects": projects })))
}

/// 重启服务
///
/// POST /restart
/// 需要 API Key 认证
async fn restart_service(
    _auth: RequireApiKey,
    State(_state): State<Arc<AppState>>,
) -> ApiResult<Json<RestartResponse>> {
    tracing::info!("Restart requested, scheduling service restart in 2 seconds...");

    // 使用 systemd-run 延迟 2 秒重启服务
    let result = tokio::process::Command::new("sudo")
        .args([
            "/usr/bin/systemd-run",
            "--no-block",
            "--on-active=2s",
            "/bin/systemctl",
            "restart",
            "xjp-deploy-agent",
        ])
        .output()
        .await;

    match result {
        Ok(output) => {
            if output.status.success() {
                tracing::info!("Service restart scheduled successfully");
                Ok(Json(RestartResponse {
                    success: true,
                    message: "Service restart scheduled in 2 seconds".to_string(),
                }))
            } else {
                let stderr = String::from_utf8_lossy(&output.stderr);
                tracing::error!("Failed to schedule restart: {}", stderr);
                Ok(Json(RestartResponse {
                    success: false,
                    message: format!("Failed to schedule restart: {}", stderr),
                }))
            }
        }
        Err(e) => {
            tracing::error!("Failed to run systemd-run: {}", e);
            Ok(Json(RestartResponse {
                success: false,
                message: format!("Failed to run restart command: {}", e),
            }))
        }
    }
}

/// Helper trait for tap pattern
trait TapMut: Sized {
    fn tap_mut(mut self, f: impl FnOnce(&mut Self)) -> Self {
        f(&mut self);
        self
    }
}

impl<T> TapMut for T {}
