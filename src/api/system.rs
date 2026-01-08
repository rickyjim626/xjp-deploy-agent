//! 系统信息 API
//!
//! 包含 /system/info, /system/stats 端点

use axum::{
    extract::State,
    response::{IntoResponse, sse::{Event, Sse}},
    routing::{get, post},
    Json, Router,
};
use futures::stream::Stream;
use serde::Deserialize;
use tokio::process::Command;
use tokio::io::{AsyncBufReadExt, BufReader};
use std::convert::Infallible;
use std::sync::Arc;
use sysinfo::{CpuRefreshKind, Disks, MemoryRefreshKind, RefreshKind, System};

use crate::config::env::constants::VERSION;
use crate::domain::system::{DiskInfo, LoadAverage, SystemInfo, SystemStats};
use crate::error::{ApiError, ApiResult};
use crate::middleware::RequireApiKey;
use crate::state::AppState;

/// 创建系统信息路由
pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/system/info", get(get_system_info))
        .route("/system/stats", get(get_system_stats))
        .route("/exec", post(exec_command))
        .route("/update", post(force_update))
        .route("/service-logs", get(get_service_logs))
        .route("/logs/agent", get(get_agent_logs))  // 无需认证的 Agent 日志端点
        .route("/logs/agent/stream", get(stream_agent_logs))  // SSE 实时日志流
        .route("/config", get(get_config))
}

/// 获取系统硬件信息
///
/// GET /system/info
/// 无需认证
async fn get_system_info(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let mut sys = System::new_with_specifics(
        RefreshKind::new()
            .with_cpu(CpuRefreshKind::everything())
            .with_memory(MemoryRefreshKind::everything()),
    );
    sys.refresh_cpu_all();

    let cpu_brand = sys
        .cpus()
        .first()
        .map(|c| c.brand().to_string())
        .unwrap_or_else(|| "Unknown".to_string());

    let info = SystemInfo {
        hostname: System::host_name().unwrap_or_else(|| "unknown".to_string()),
        os_name: System::name().unwrap_or_else(|| "unknown".to_string()),
        os_version: System::os_version().unwrap_or_else(|| "unknown".to_string()),
        kernel_version: System::kernel_version().unwrap_or_else(|| "unknown".to_string()),
        cpu_arch: std::env::consts::ARCH.to_string(),
        cpu_count: sys.cpus().len(),
        cpu_brand,
        total_memory_gb: sys.total_memory() as f64 / 1024.0 / 1024.0 / 1024.0,
        total_swap_gb: sys.total_swap() as f64 / 1024.0 / 1024.0 / 1024.0,
    };

    // 计算运行时间
    let uptime_secs = (chrono::Utc::now() - state.started_at).num_seconds();
    let uptime_str = format!(
        "{}d {}h {}m {}s",
        uptime_secs / 86400,
        (uptime_secs % 86400) / 3600,
        (uptime_secs % 3600) / 60,
        uptime_secs % 60
    );

    Json(serde_json::json!({
        "system": info,
        "agent": {
            "version": VERSION,
            "started_at": state.started_at.to_rfc3339(),
            "uptime": uptime_str,
            "projects": state.projects.keys().collect::<Vec<_>>(),
        }
    }))
}

/// 获取系统负载统计
///
/// GET /system/stats
/// 无需认证
async fn get_system_stats() -> impl IntoResponse {
    // 创建 System 实例并刷新所有需要的数据
    let mut sys = System::new_with_specifics(
        RefreshKind::new()
            .with_cpu(CpuRefreshKind::everything())
            .with_memory(MemoryRefreshKind::everything()),
    );

    // 需要等待一小段时间让 CPU 使用率计算准确
    sys.refresh_cpu_all();
    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
    sys.refresh_cpu_all();

    // 获取磁盘信息
    let disks = Disks::new_with_refreshed_list();
    let disk_info: Vec<DiskInfo> = disks
        .iter()
        .map(|disk| {
            let total = disk.total_space() as f64 / 1024.0 / 1024.0 / 1024.0;
            let available = disk.available_space() as f64 / 1024.0 / 1024.0 / 1024.0;
            let used = total - available;
            DiskInfo {
                name: disk.name().to_string_lossy().to_string(),
                mount_point: disk.mount_point().to_string_lossy().to_string(),
                total_gb: total,
                used_gb: used,
                available_gb: available,
                usage_percent: if total > 0.0 {
                    (used / total) * 100.0
                } else {
                    0.0
                },
            }
        })
        .collect();

    // 计算 CPU 平均使用率
    let cpu_usage: f64 = if sys.cpus().is_empty() {
        0.0
    } else {
        sys.cpus().iter().map(|c| c.cpu_usage() as f64).sum::<f64>() / sys.cpus().len() as f64
    };

    let memory_total = sys.total_memory() as f64 / 1024.0 / 1024.0 / 1024.0;
    let memory_used = sys.used_memory() as f64 / 1024.0 / 1024.0 / 1024.0;
    let swap_total = sys.total_swap() as f64 / 1024.0 / 1024.0 / 1024.0;
    let swap_used = sys.used_swap() as f64 / 1024.0 / 1024.0 / 1024.0;

    // 获取负载平均值 (仅 Unix)
    let load_avg = System::load_average();

    let stats = SystemStats {
        timestamp: chrono::Utc::now(),
        cpu_usage_percent: cpu_usage,
        memory_used_gb: memory_used,
        memory_total_gb: memory_total,
        memory_usage_percent: if memory_total > 0.0 {
            (memory_used / memory_total) * 100.0
        } else {
            0.0
        },
        swap_used_gb: swap_used,
        swap_total_gb: swap_total,
        swap_usage_percent: if swap_total > 0.0 {
            (swap_used / swap_total) * 100.0
        } else {
            0.0
        },
        disks: disk_info,
        load_average: LoadAverage::new(load_avg.one, load_avg.five, load_avg.fifteen),
    };

    Json(stats)
}

/// 执行命令请求
#[derive(Debug, Deserialize)]
pub struct ExecRequest {
    /// 要执行的命令
    pub command: String,
    /// 工作目录（可选）
    pub cwd: Option<String>,
    /// 超时时间（秒，默认 60）
    #[serde(default = "default_timeout")]
    pub timeout: u64,
}

fn default_timeout() -> u64 {
    60
}

/// 执行命令响应
#[derive(serde::Serialize)]
pub struct ExecResponse {
    pub success: bool,
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

/// 执行 shell 命令
///
/// POST /exec
/// 需要 API Key
///
/// 安全提示：此端点允许执行任意命令，仅应在受信任的网络中使用
async fn exec_command(
    _auth: RequireApiKey,
    State(_state): State<Arc<AppState>>,
    Json(request): Json<ExecRequest>,
) -> ApiResult<Json<ExecResponse>> {
    tracing::info!(command = %request.command, cwd = ?request.cwd, "Executing command");

    let mut cmd = Command::new("sh");
    cmd.args(["-c", &request.command]);

    // 设置工作目录
    if let Some(ref cwd) = request.cwd {
        cmd.current_dir(cwd);
    }

    // 执行命令带超时
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(request.timeout),
        cmd.output(),
    )
    .await;

    match result {
        Ok(Ok(output)) => {
            let stdout = String::from_utf8_lossy(&output.stdout).to_string();
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();
            let exit_code = output.status.code();

            tracing::info!(
                exit_code = ?exit_code,
                stdout_len = stdout.len(),
                stderr_len = stderr.len(),
                "Command completed"
            );

            Ok(Json(ExecResponse {
                success: output.status.success(),
                exit_code,
                stdout,
                stderr,
            }))
        }
        Ok(Err(e)) => {
            tracing::error!(error = %e, "Failed to execute command");
            Err(ApiError::internal(format!("Failed to execute command: {}", e)))
        }
        Err(_) => {
            tracing::error!(timeout = request.timeout, "Command timed out");
            Err(ApiError::internal(format!(
                "Command timed out after {} seconds",
                request.timeout
            )))
        }
    }
}

// ========== 新增端点 ==========

/// 强制更新请求
#[derive(Debug, Deserialize)]
pub struct ForceUpdateRequest {
    /// 目标版本 (可选，不指定则拉取最新)
    pub version: Option<String>,
}

/// 强制触发更新
///
/// POST /update
/// 需要 API Key
///
/// 手动触发自动更新流程
async fn force_update(
    _auth: RequireApiKey,
    State(state): State<Arc<AppState>>,
    Json(request): Json<ForceUpdateRequest>,
) -> ApiResult<impl IntoResponse> {
    let config = state.auto_update_config.as_ref().ok_or_else(|| {
        ApiError::bad_request("Auto-update is not configured on this agent")
    })?;

    tracing::info!(version = ?request.version, "Force update triggered");

    // 更新状态
    *state.auto_update_state.update_progress.write().await = "checking".to_string();

    // 获取元数据
    let metadata_url = config.metadata_url();
    let response = reqwest::get(&metadata_url).await.map_err(|e| {
        ApiError::internal(format!("Failed to fetch update metadata: {}", e))
    })?;

    let metadata: crate::config::UpdateMetadata = response.json().await.map_err(|e| {
        ApiError::internal(format!("Failed to parse update metadata: {}", e))
    })?;

    // 如果指定了版本，检查是否匹配
    if let Some(ref target_version) = request.version {
        if &metadata.version != target_version {
            return Err(ApiError::bad_request(format!(
                "Requested version {} not found. Latest available: {}",
                target_version, metadata.version
            )));
        }
    }

    // 更新状态
    *state.auto_update_state.latest_version.write().await = Some(metadata.version.clone());
    *state.auto_update_state.update_available.write().await = true;

    // 在后台执行更新
    let state_clone = state.clone();
    let config_clone = config.clone();
    tokio::spawn(async move {
        if let Err(e) = crate::services::autoupdate::trigger_update(state_clone, config_clone, metadata).await {
            tracing::error!(error = %e, "Force update failed");
        }
    });

    Ok(Json(serde_json::json!({
        "status": "update_started",
        "target_version": request.version.unwrap_or_else(|| "latest".to_string()),
        "message": "Update process started in background. The agent will restart if successful."
    })))
}

/// 服务日志查询参数
#[derive(Debug, Deserialize)]
pub struct ServiceLogsQuery {
    /// 返回行数，默认 100
    #[serde(default = "default_lines")]
    pub lines: u32,
    /// 时间范围 (e.g., "5m", "1h", "1d")
    pub since: Option<String>,
    /// 服务名称，默认 "xjp-deploy-agent"
    #[serde(default = "default_service_name")]
    pub service: String,
}

fn default_lines() -> u32 {
    100
}

fn default_service_name() -> String {
    "xjp-deploy-agent".to_string()
}

/// 获取 systemd 服务日志
///
/// GET /service-logs
/// 需要 API Key
///
/// 通过 journalctl 获取服务日志
async fn get_service_logs(
    _auth: RequireApiKey,
    axum::extract::Query(query): axum::extract::Query<ServiceLogsQuery>,
) -> ApiResult<impl IntoResponse> {
    tracing::info!(
        service = %query.service,
        lines = query.lines,
        since = ?query.since,
        "Fetching service logs"
    );

    let mut cmd = Command::new("journalctl");
    cmd.args(["-u", &query.service, "-n", &query.lines.to_string(), "--no-pager"]);

    if let Some(ref since) = query.since {
        cmd.args(["--since", since]);
    }

    let result = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        cmd.output(),
    ).await;

    match result {
        Ok(Ok(output)) => {
            let stdout = String::from_utf8_lossy(&output.stdout).to_string();
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();

            if !output.status.success() {
                return Err(ApiError::internal(format!(
                    "journalctl failed: {}",
                    if stderr.is_empty() { &stdout } else { &stderr }
                )));
            }

            Ok(Json(serde_json::json!({
                "service": query.service,
                "lines": query.lines,
                "since": query.since,
                "logs": stdout,
            })))
        }
        Ok(Err(e)) => {
            Err(ApiError::internal(format!("Failed to execute journalctl: {}", e)))
        }
        Err(_) => {
            Err(ApiError::internal("journalctl command timed out"))
        }
    }
}

/// 获取 Agent 配置
///
/// GET /config
/// 需要 API Key
///
/// 返回当前 Agent 的完整配置信息
async fn get_config(
    _auth: RequireApiKey,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    // 构建项目配置列表
    let projects: Vec<serde_json::Value> = state.projects.iter().map(|(name, config)| {
        serde_json::json!({
            "name": name,
            "work_dir": config.work_dir,
            "deploy_type": config.deploy_type.name(),
        })
    }).collect();

    // 构建自动更新配置
    let auto_update = state.auto_update_config.as_ref().map(|config| {
        serde_json::json!({
            "enabled": true,
            "endpoint": config.endpoint,
            "check_interval_secs": config.check_interval_secs,
            "metadata_path": config.metadata_path,
            "binary_path_template": config.binary_path_template,
        })
    });

    // 构建隧道配置
    let tunnel = serde_json::json!({
        "mode": format!("{:?}", state.tunnel_mode),
        "server_url": if state.tunnel_server_url.is_empty() { None } else { Some(&state.tunnel_server_url) },
        "port_mappings": state.tunnel_port_mappings.len(),
    });

    Json(serde_json::json!({
        "version": VERSION,
        "started_at": state.started_at.to_rfc3339(),
        "port": state.config.port,
        "callback_url": state.config.callback_url,
        "projects": projects,
        "auto_update": auto_update,
        "tunnel": tunnel,
        "api_key_configured": !state.api_key.is_empty(),
    }))
}

/// Agent 日志查询参数
#[derive(Debug, Deserialize)]
pub struct AgentLogsQuery {
    /// 返回行数，默认 100，最大 200
    #[serde(default = "default_agent_log_lines")]
    pub lines: u32,
}

fn default_agent_log_lines() -> u32 {
    100
}

/// 获取 Agent 运行日志（无需认证）
///
/// GET /logs/agent
/// 无需 API Key
///
/// 返回最近的 xjp-deploy-agent 服务日志
/// 限制最多 200 行，用于前端监控面板显示
async fn get_agent_logs(
    axum::extract::Query(query): axum::extract::Query<AgentLogsQuery>,
) -> ApiResult<impl IntoResponse> {
    // 限制最大行数为 200
    let lines = query.lines.min(200);

    tracing::debug!(lines = lines, "Fetching agent logs (public endpoint)");

    let mut cmd = Command::new("journalctl");
    cmd.args([
        "-u", "xjp-deploy-agent",
        "-n", &lines.to_string(),
        "--no-pager",
        "-o", "short-iso",  // 使用 ISO 时间格式便于阅读
    ]);

    let result = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        cmd.output(),
    ).await;

    match result {
        Ok(Ok(output)) => {
            let stdout = String::from_utf8_lossy(&output.stdout).to_string();
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();

            if !output.status.success() {
                // 可能是 journalctl 没有权限或服务不存在，返回空日志
                return Ok(Json(serde_json::json!({
                    "lines": 0,
                    "logs": [],
                    "error": if stderr.is_empty() { stdout } else { stderr },
                })));
            }

            // 将日志按行分割
            let log_lines: Vec<&str> = stdout.lines().collect();

            Ok(Json(serde_json::json!({
                "lines": log_lines.len(),
                "logs": log_lines,
            })))
        }
        Ok(Err(e)) => {
            Ok(Json(serde_json::json!({
                "lines": 0,
                "logs": [],
                "error": format!("Failed to execute journalctl: {}", e),
            })))
        }
        Err(_) => {
            Ok(Json(serde_json::json!({
                "lines": 0,
                "logs": [],
                "error": "journalctl command timed out",
            })))
        }
    }
}

/// SSE 流式日志查询参数
#[derive(Debug, Deserialize)]
pub struct StreamLogsQuery {
    /// 初始返回行数，默认 50
    #[serde(default = "default_initial_lines")]
    pub initial: u32,
}

fn default_initial_lines() -> u32 {
    50
}

/// 流式获取 Agent 运行日志（SSE）
///
/// GET /logs/agent/stream
/// 无需 API Key
///
/// 通过 journalctl -f 实时流式传输 xjp-deploy-agent 服务日志
async fn stream_agent_logs(
    axum::extract::Query(query): axum::extract::Query<StreamLogsQuery>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let initial_lines = query.initial.min(100);

    tracing::info!(initial_lines = initial_lines, "Starting SSE log stream");

    let stream = async_stream::stream! {
        // 先获取历史日志
        let mut cmd = Command::new("journalctl");
        cmd.args([
            "-u", "xjp-deploy-agent",
            "-n", &initial_lines.to_string(),
            "--no-pager",
            "-o", "short-iso",
        ]);

        if let Ok(output) = cmd.output().await {
            let stdout = String::from_utf8_lossy(&output.stdout);
            for line in stdout.lines() {
                yield Ok(Event::default().data(line));
            }
        }

        // 发送分隔符表示历史日志结束
        yield Ok(Event::default().event("history_end").data("---"));

        // 使用 journalctl -f 实时追踪日志
        let mut follow_cmd = Command::new("journalctl");
        follow_cmd.args([
            "-u", "xjp-deploy-agent",
            "-f",
            "--no-pager",
            "-o", "short-iso",
        ]);
        follow_cmd.stdout(std::process::Stdio::piped());
        follow_cmd.stderr(std::process::Stdio::null());

        match follow_cmd.spawn() {
            Ok(mut child) => {
                if let Some(stdout) = child.stdout.take() {
                    let reader = BufReader::new(stdout);
                    let mut lines = reader.lines();

                    // 持续读取日志行
                    loop {
                        tokio::select! {
                            result = lines.next_line() => {
                                match result {
                                    Ok(Some(line)) => {
                                        yield Ok(Event::default().data(line));
                                    }
                                    Ok(None) => {
                                        // journalctl 进程结束
                                        tracing::info!("journalctl process ended");
                                        break;
                                    }
                                    Err(e) => {
                                        tracing::error!(error = %e, "Error reading journalctl output");
                                        break;
                                    }
                                }
                            }
                        }
                    }
                }

                // 确保子进程被正确清理
                let _ = child.kill().await;
            }
            Err(e) => {
                tracing::error!(error = %e, "Failed to spawn journalctl");
                yield Ok(Event::default().event("error").data(format!("Failed to start log stream: {}", e)));
            }
        }

        // 发送结束事件
        yield Ok(Event::default().event("end").data("Stream ended"));
    };

    Sse::new(stream).keep_alive(
        axum::response::sse::KeepAlive::new()
            .interval(std::time::Duration::from_secs(15))
            .text("ping")
    )
}
