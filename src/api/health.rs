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
use crate::domain::ssh::SshHealthInfo;
use crate::domain::tunnel::{PortMapping, TunnelMode};
use crate::error::ApiResult;
use crate::middleware::RequireApiKey;
use crate::services::nfa::NfaStatus;
use crate::state::AppState;

/// 连接的客户端信息（用于 health 端点）
#[derive(Debug, Serialize)]
struct ConnectedClient {
    client_id: String,
    client_addr: String,
    connected_at: String,
    port_mappings: Vec<PortMapping>,
}

/// 隧道状态摘要（用于 health 端点）
#[derive(Debug, Serialize)]
struct TunnelStatusSummary {
    mode: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    server_url: Option<String>,
    /// 已连接的客户端数量 (Server 模式)
    #[serde(skip_serializing_if = "Option::is_none")]
    client_count: Option<usize>,
    /// 已连接的客户端列表 (Server 模式)
    #[serde(skip_serializing_if = "Vec::is_empty")]
    clients: Vec<ConnectedClient>,
    /// 向后兼容：是否有客户端连接
    #[serde(skip_serializing_if = "Option::is_none")]
    client_connected: Option<bool>,
    /// Client 模式下的端口映射
    #[serde(skip_serializing_if = "Vec::is_empty")]
    port_mappings: Vec<PortMapping>,
}

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
    #[serde(skip_serializing_if = "Option::is_none")]
    nfa: Option<NfaStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tunnel: Option<TunnelStatusSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ssh: Option<SshHealthInfo>,
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

    // 获取 NFA 状态（如果启用）
    let nfa_status = if let Some(nfa) = &state.nfa {
        Some(nfa.status().await)
    } else {
        None
    };

    // 获取 SSH 状态（如果启用）
    let ssh_status = {
        let ssh_guard = state.ssh_server.read().await;
        if let Some(ssh) = ssh_guard.as_ref() {
            Some(ssh.health_info().await)
        } else {
            None
        }
    };

    // 获取隧道状态
    let tunnel_status = match &state.tunnel_mode {
        TunnelMode::Server => {
            let server_state = &state.tunnel_server_state;
            let clients_guard = server_state.clients.read().await;

            // 收集所有连接的客户端信息
            let connected_clients: Vec<ConnectedClient> = clients_guard
                .values()
                .map(|client| ConnectedClient {
                    client_id: client.client_id.clone(),
                    client_addr: client.client_addr.clone(),
                    connected_at: client.connected_at.to_rfc3339(),
                    port_mappings: client.mappings.clone(),
                })
                .collect();

            let client_count = connected_clients.len();

            Some(TunnelStatusSummary {
                mode: "server".to_string(),
                server_url: None,
                client_count: Some(client_count),
                clients: connected_clients,
                client_connected: Some(client_count > 0), // 向后兼容
                port_mappings: Vec::new(),
            })
        }
        TunnelMode::Client => {
            let client_state = &state.tunnel_client_state;
            let connected = *client_state.connected.read().await;
            Some(TunnelStatusSummary {
                mode: "client".to_string(),
                server_url: Some(state.tunnel_server_url.clone()),
                client_count: None,
                clients: Vec::new(),
                client_connected: Some(connected),
                port_mappings: state.tunnel_port_mappings.clone(),
            })
        }
        TunnelMode::Disabled => None,
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
        nfa: nfa_status,
        tunnel: tunnel_status,
        ssh: ssh_status,
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
