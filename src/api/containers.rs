//! 容器管理 API
//!
//! 包含 /containers/* 端点

use axum::{
    extract::{Path, Query, State},
    response::IntoResponse,
    routing::get,
    Json, Router,
};
use std::sync::Arc;
use tokio::process::Command;
use tracing::error;

use crate::domain::container::{
    ContainerEnvResponse, ContainerInfo, ContainerLogsQuery, ContainerLogsResponse,
    ContainersResponse, EnvVar,
};
use crate::error::{ApiError, ApiResult};
use crate::middleware::RequireApiKey;
use crate::state::AppState;

/// 创建容器管理路由
pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/containers", get(list_containers))
        .route("/containers/:name/logs", get(get_container_logs))
        .route("/containers/:name/env", get(get_container_env))
        .route("/containers/:name/env/full", get(get_container_env_full))
}

/// 列出所有容器
///
/// GET /containers
/// 需要 API Key
async fn list_containers(
    _auth: RequireApiKey,
    State(_state): State<Arc<AppState>>,
) -> ApiResult<impl IntoResponse> {
    // 执行 docker ps -a 获取容器列表
    let output = Command::new("docker")
        .args([
            "ps",
            "-a",
            "--format",
            "{{.ID}}|{{.Names}}|{{.Image}}|{{.Status}}|{{.State}}|{{.CreatedAt}}|{{.Ports}}",
        ])
        .output()
        .await
        .map_err(|e| {
            error!(error = %e, "Failed to run docker ps");
            ApiError::internal(format!("Failed to list containers: {}", e))
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(ApiError::internal(format!(
            "Docker command failed: {}",
            stderr
        )));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let containers: Vec<ContainerInfo> = stdout
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| {
            let parts: Vec<&str> = line.split('|').collect();
            ContainerInfo {
                id: parts.first().unwrap_or(&"").to_string(),
                name: parts.get(1).unwrap_or(&"").to_string(),
                image: parts.get(2).unwrap_or(&"").to_string(),
                status: parts.get(3).unwrap_or(&"").to_string(),
                state: parts.get(4).unwrap_or(&"").to_string(),
                created: parts.get(5).unwrap_or(&"").to_string(),
                ports: parts
                    .get(6)
                    .unwrap_or(&"")
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect(),
            }
        })
        .collect();

    Ok(Json(ContainersResponse { containers }))
}

/// 获取容器日志
///
/// GET /containers/:name/logs
/// 需要 API Key
async fn get_container_logs(
    _auth: RequireApiKey,
    State(_state): State<Arc<AppState>>,
    Path(container_name): Path<String>,
    Query(query): Query<ContainerLogsQuery>,
) -> ApiResult<impl IntoResponse> {
    let mut args = vec!["logs".to_string()];

    // 添加 tail 参数
    args.push("--tail".to_string());
    args.push(query.tail.to_string());

    // 添加 timestamps 参数
    if query.timestamps {
        args.push("--timestamps".to_string());
    }

    // 添加 since 参数
    if let Some(since) = &query.since {
        args.push("--since".to_string());
        args.push(since.clone());
    }

    args.push(container_name.clone());

    let output = Command::new("docker")
        .args(&args)
        .output()
        .await
        .map_err(|e| {
            error!(container = %container_name, error = %e, "Failed to get container logs");
            ApiError::internal(format!("Failed to get logs: {}", e))
        })?;

    // docker logs 输出可能同时在 stdout 和 stderr
    let mut logs: Vec<String> = Vec::new();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    // 检查是否是错误（容器不存在等）
    if !output.status.success() && stderr.contains("No such container") {
        return Err(ApiError::not_found(format!(
            "Container '{}'",
            container_name
        )));
    }

    // 合并 stdout 和 stderr（docker logs 通常输出到 stderr）
    for line in stdout.lines() {
        if !line.is_empty() {
            logs.push(line.to_string());
        }
    }
    for line in stderr.lines() {
        if !line.is_empty() {
            logs.push(line.to_string());
        }
    }

    let total_lines = logs.len();

    Ok(Json(ContainerLogsResponse {
        container: container_name,
        logs,
        total_lines,
    }))
}

/// 获取容器环境变量（敏感信息已脱敏）
///
/// GET /containers/:name/env
/// 需要 API Key
async fn get_container_env(
    _auth: RequireApiKey,
    State(_state): State<Arc<AppState>>,
    Path(container_name): Path<String>,
) -> ApiResult<impl IntoResponse> {
    get_container_env_internal(&container_name, true).await
}

/// 获取容器完整环境变量（包括敏感信息）
///
/// GET /containers/:name/env/full
/// 需要 API Key
async fn get_container_env_full(
    _auth: RequireApiKey,
    State(_state): State<Arc<AppState>>,
    Path(container_name): Path<String>,
) -> ApiResult<impl IntoResponse> {
    get_container_env_internal(&container_name, false).await
}

/// 内部：获取容器环境变量
async fn get_container_env_internal(
    container_name: &str,
    redact_sensitive: bool,
) -> ApiResult<Json<ContainerEnvResponse>> {
    // 使用 docker inspect 获取容器环境变量
    let output = Command::new("docker")
        .args([
            "inspect",
            "--format",
            "{{range .Config.Env}}{{.}}\n{{end}}",
            container_name,
        ])
        .output()
        .await
        .map_err(|e| {
            error!(container = %container_name, error = %e, "Failed to inspect container");
            ApiError::internal(format!("Failed to get environment: {}", e))
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("No such") {
            return Err(ApiError::not_found(format!(
                "Container '{}'",
                container_name
            )));
        }
        return Err(ApiError::internal(format!(
            "Docker inspect failed: {}",
            stderr
        )));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);

    let env_vars: Vec<EnvVar> = stdout
        .lines()
        .filter(|line| !line.is_empty() && line.contains('='))
        .map(|line| {
            let mut parts = line.splitn(2, '=');
            let key = parts.next().unwrap_or("").to_string();
            let value = parts.next().unwrap_or("").to_string();

            // 检查是否是敏感信息
            let sensitive = EnvVar::is_sensitive_key(&key);

            EnvVar::new(
                key,
                if redact_sensitive && sensitive {
                    "***REDACTED***".to_string()
                } else {
                    value
                },
                sensitive,
            )
        })
        .collect();

    Ok(Json(ContainerEnvResponse {
        container: container_name.to_string(),
        env_vars,
    }))
}
