//! XJP Deploy Agent - 私有云部署代理
//!
//! 运行在私有云上，接收部署中心的触发请求，执行构建脚本并流式传输日志。

use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse,
    },
    routing::{get, post},
    Json, Router,
};
use futures::stream::Stream;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    convert::Infallible,
    env,
    process::Stdio,
    sync::Arc,
};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::Command,
    sync::{broadcast, RwLock},
};
use tokio_util::sync::CancellationToken;
use tower_http::{cors::CorsLayer, trace::TraceLayer};
use tracing::{error, info, warn};
use uuid::Uuid;

/// 部署任务状态
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DeployStatus {
    Running,
    Success,
    Failed,
}

/// 部署任务信息
#[derive(Clone, Debug, Serialize)]
pub struct DeployTask {
    pub id: String,
    pub project: String,
    pub status: DeployStatus,
    pub started_at: chrono::DateTime<chrono::Utc>,
    pub finished_at: Option<chrono::DateTime<chrono::Utc>>,
    pub exit_code: Option<i32>,
}

/// 日志行
#[derive(Clone, Debug, Serialize)]
pub struct LogLine {
    pub timestamp: chrono::DateTime<chrono::Utc>,
    pub stream: String, // stdout | stderr
    pub content: String,
}

/// 项目配置
#[derive(Clone, Debug)]
pub struct ProjectConfig {
    pub work_dir: String,
    pub script: String,
}

/// 运行中的部署进程信息
pub struct RunningDeploy {
    pub task_id: String,
    pub cancel_token: CancellationToken,
}

/// 应用状态
pub struct AppState {
    /// API 密钥（用于验证请求）
    api_key: String,
    /// 项目配置
    projects: HashMap<String, ProjectConfig>,
    /// 活跃的部署任务
    tasks: RwLock<HashMap<String, DeployTask>>,
    /// 日志广播通道 (task_id -> sender)
    log_channels: RwLock<HashMap<String, broadcast::Sender<LogLine>>>,
    /// 回调 URL（可选，用于通知部署中心）
    callback_url: Option<String>,
    /// 每个项目当前运行中的部署 (project -> RunningDeploy)
    running_deploys: RwLock<HashMap<String, RunningDeploy>>,
}

impl AppState {
    fn new() -> Self {
        let api_key = env::var("DEPLOY_AGENT_API_KEY")
            .unwrap_or_else(|_| "change-me-in-production".to_string());

        let callback_url = env::var("DEPLOY_CENTER_CALLBACK_URL").ok();

        // 配置项目
        let mut projects = HashMap::new();

        // xiaojinpro-backend (只在 BACKEND_WORK_DIR 存在时添加)
        if let Ok(work_dir) = env::var("BACKEND_WORK_DIR") {
            projects.insert(
                "xiaojinpro-backend".to_string(),
                ProjectConfig {
                    work_dir,
                    script: "./build-and-push-backend.sh".to_string(),
                },
            );
        }

        // 从环境变量添加更多项目
        // 格式: PROJECT_<NAME>_DIR, PROJECT_<NAME>_SCRIPT
        for (key, value) in env::vars() {
            if key.starts_with("PROJECT_") && key.ends_with("_DIR") {
                let name = key
                    .strip_prefix("PROJECT_")
                    .unwrap()
                    .strip_suffix("_DIR")
                    .unwrap()
                    .to_lowercase()
                    .replace('_', "-");

                let script_key = format!("PROJECT_{}_SCRIPT", name.to_uppercase().replace('-', "_"));
                let script = env::var(&script_key).unwrap_or_else(|_| "./deploy.sh".to_string());

                projects.insert(
                    name.clone(),
                    ProjectConfig {
                        work_dir: value,
                        script,
                    },
                );
                info!("Loaded project config: {}", name);
            }
        }

        Self {
            api_key,
            projects,
            tasks: RwLock::new(HashMap::new()),
            log_channels: RwLock::new(HashMap::new()),
            callback_url,
            running_deploys: RwLock::new(HashMap::new()),
        }
    }

    /// 验证 API 密钥
    fn verify_api_key(&self, headers: &HeaderMap) -> bool {
        headers
            .get("x-api-key")
            .and_then(|v| v.to_str().ok())
            .map(|k| k == self.api_key)
            .unwrap_or(false)
    }
}

/// 触发部署请求
#[derive(Debug, Deserialize)]
pub struct TriggerRequest {
    /// 部署日志 ID（来自部署中心）
    pub deploy_log_id: Option<String>,
    /// 提交哈希
    pub commit_hash: Option<String>,
    /// 分支
    pub branch: Option<String>,
}

/// 触发部署响应
#[derive(Debug, Serialize)]
pub struct TriggerResponse {
    pub task_id: String,
    pub project: String,
    pub status: String,
    pub stream_url: String,
}

/// 错误响应
#[derive(Debug, Serialize)]
pub struct ErrorResponse {
    pub error: String,
    pub message: String,
}

/// 版本信息
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// 重启响应
#[derive(Debug, Serialize)]
pub struct RestartResponse {
    pub success: bool,
    pub message: String,
}

/// 健康检查 - 返回状态、版本、运行时间等信息
async fn health_check(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let (active_count, active_projects) = {
        let running_deploys = state.running_deploys.read().await;
        let count = running_deploys.len();
        let projects: Vec<String> = running_deploys.keys().cloned().collect();
        (count, projects)
    };

    let projects: Vec<String> = state.projects.keys().cloned().collect();

    Json(serde_json::json!({
        "status": "ok",
        "service": "xjp-deploy-agent",
        "version": VERSION,
        "timestamp": chrono::Utc::now().to_rfc3339(),
        "projects": projects,
        "active_deploys": active_count,
        "active_projects": active_projects
    }))
}

/// 列出支持的项目
async fn list_projects(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, (StatusCode, Json<ErrorResponse>)> {
    if !state.verify_api_key(&headers) {
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(ErrorResponse {
                error: "unauthorized".to_string(),
                message: "Invalid or missing API key".to_string(),
            }),
        ));
    }

    let projects: Vec<&String> = state.projects.keys().collect();
    Ok(Json(serde_json::json!({ "projects": projects })))
}

/// 重启服务 (通过 systemd-run 延迟重启)
///
/// POST /restart
async fn restart_service(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<RestartResponse>, (StatusCode, Json<ErrorResponse>)> {
    if !state.verify_api_key(&headers) {
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(ErrorResponse {
                error: "unauthorized".to_string(),
                message: "Invalid or missing API key".to_string(),
            }),
        ));
    }

    info!("Restart requested, scheduling service restart in 2 seconds...");

    // 使用 systemd-run 延迟 2 秒重启服务
    let result = Command::new("sudo")
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
                info!("Service restart scheduled successfully");
                Ok(Json(RestartResponse {
                    success: true,
                    message: "Service restart scheduled in 2 seconds".to_string(),
                }))
            } else {
                let stderr = String::from_utf8_lossy(&output.stderr);
                error!("Failed to schedule restart: {}", stderr);
                Ok(Json(RestartResponse {
                    success: false,
                    message: format!("Failed to schedule restart: {}", stderr),
                }))
            }
        }
        Err(e) => {
            error!("Failed to run systemd-run: {}", e);
            Ok(Json(RestartResponse {
                success: false,
                message: format!("Failed to run restart command: {}", e),
            }))
        }
    }
}

/// 触发部署
async fn trigger_deploy(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(project): Path<String>,
    Json(request): Json<TriggerRequest>,
) -> Result<impl IntoResponse, (StatusCode, Json<ErrorResponse>)> {
    // 验证 API 密钥
    if !state.verify_api_key(&headers) {
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(ErrorResponse {
                error: "unauthorized".to_string(),
                message: "Invalid or missing API key".to_string(),
            }),
        ));
    }

    // 获取项目配置
    let config = state.projects.get(&project).ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: "not_found".to_string(),
                message: format!("Project '{}' not configured", project),
            }),
        )
    })?;

    let task_id = request
        .deploy_log_id
        .clone()
        .unwrap_or_else(|| Uuid::new_v4().to_string());

    // 检查是否有该项目正在运行的部署，如果有则取消它
    {
        let mut running = state.running_deploys.write().await;
        if let Some(old_deploy) = running.remove(&project) {
            warn!(
                project = %project,
                old_task_id = %old_deploy.task_id,
                new_task_id = %task_id,
                "Cancelling previous deployment for new commit"
            );
            // 触发取消
            old_deploy.cancel_token.cancel();

            // 更新旧任务状态为 cancelled
            let mut tasks = state.tasks.write().await;
            if let Some(old_task) = tasks.get_mut(&old_deploy.task_id) {
                old_task.status = DeployStatus::Failed;
                old_task.finished_at = Some(chrono::Utc::now());
                old_task.exit_code = Some(-2); // -2 表示被取消
            }

            // 通知 deploy-center 旧任务被取消
            if let Some(ref callback_url) = state.callback_url {
                let client = reqwest::Client::new();
                let url = format!("{}/api/deploy/logs/{}", callback_url, old_deploy.task_id);
                let _ = client
                    .patch(&url)
                    .json(&serde_json::json!({
                        "status": "failed",
                        "error_message": "Cancelled: superseded by newer commit"
                    }))
                    .send()
                    .await;
            }
        }
    }

    // 创建取消令牌
    let cancel_token = CancellationToken::new();

    // 创建日志广播通道
    let (log_tx, _) = broadcast::channel::<LogLine>(1000);

    // 创建任务
    let task = DeployTask {
        id: task_id.clone(),
        project: project.clone(),
        status: DeployStatus::Running,
        started_at: chrono::Utc::now(),
        finished_at: None,
        exit_code: None,
    };

    // 保存任务和通道
    {
        let mut tasks = state.tasks.write().await;
        tasks.insert(task_id.clone(), task);
    }
    {
        let mut channels = state.log_channels.write().await;
        channels.insert(task_id.clone(), log_tx.clone());
    }
    // 记录当前运行的部署
    {
        let mut running = state.running_deploys.write().await;
        running.insert(project.clone(), RunningDeploy {
            task_id: task_id.clone(),
            cancel_token: cancel_token.clone(),
        });
    }

    info!(
        task_id = %task_id,
        project = %project,
        commit = ?request.commit_hash,
        branch = ?request.branch,
        "Starting deployment"
    );

    // 启动部署进程
    let work_dir = config.work_dir.clone();
    let script = config.script.clone();
    let state_clone = state.clone();
    let task_id_clone = task_id.clone();
    let project_clone = project.clone();

    tokio::spawn(async move {
        run_deploy(
            state_clone,
            task_id_clone,
            project_clone,
            work_dir,
            script,
            request.commit_hash,
            request.branch,
            log_tx,
            cancel_token,
        )
        .await;
    });

    Ok((
        StatusCode::ACCEPTED,
        Json(TriggerResponse {
            task_id: task_id.clone(),
            project,
            status: "running".to_string(),
            stream_url: format!("/logs/{}/stream", task_id),
        }),
    ))
}

/// 执行部署脚本
async fn run_deploy(
    state: Arc<AppState>,
    task_id: String,
    project: String,
    work_dir: String,
    script: String,
    commit_hash: Option<String>,
    branch: Option<String>,
    log_tx: broadcast::Sender<LogLine>,
    cancel_token: CancellationToken,
) {
    // 创建 HTTP client 用于实时发送日志
    let http_client = reqwest::Client::new();
    let callback_url = state.callback_url.clone();
    let task_id_for_log = task_id.clone();

    let send_log = |stream: &str, content: String| {
        let line = LogLine {
            timestamp: chrono::Utc::now(),
            stream: stream.to_string(),
            content,
        };
        let _ = log_tx.send(line);
    };

    // 实时发送日志到 deploy-center
    let send_log_to_center = {
        let http_client = http_client.clone();
        let callback_url = callback_url.clone();
        let task_id = task_id_for_log.clone();
        move |stream: String, content: String| {
            let http_client = http_client.clone();
            let callback_url = callback_url.clone();
            let task_id = task_id.clone();
            tokio::spawn(async move {
                if let Some(ref url) = callback_url {
                    let append_url = format!("{}/api/deploy/logs/{}/append", url, task_id);
                    let _ = http_client
                        .post(&append_url)
                        .json(&serde_json::json!({
                            "line": content,
                            "stream": stream
                        }))
                        .send()
                        .await;
                }
            });
        }
    };

    // 辅助宏：同时发送到本地和 deploy-center
    macro_rules! log_line {
        ($stream:expr, $content:expr) => {{
            let content = $content;
            send_log($stream, content.clone());
            send_log_to_center($stream.to_string(), content);
        }};
    }

    log_line!("stdout", format!("=== Starting deployment for {} ===", project));
    log_line!("stdout", format!("Working directory: {}", work_dir));
    log_line!("stdout", format!("Script: {}", script));

    if let Some(ref hash) = commit_hash {
        log_line!("stdout", format!("Commit: {}", hash));
    }
    if let Some(ref br) = branch {
        log_line!("stdout", format!("Branch: {}", br));
    }

    // 先 git pull
    log_line!("stdout", ">>> git pull".to_string());

    let git_result = Command::new("git")
        .arg("pull")
        .current_dir(&work_dir)
        .output()
        .await;

    match git_result {
        Ok(output) => {
            if !output.stdout.is_empty() {
                log_line!("stdout", String::from_utf8_lossy(&output.stdout).to_string());
            }
            if !output.stderr.is_empty() {
                log_line!("stderr", String::from_utf8_lossy(&output.stderr).to_string());
            }
            if !output.status.success() {
                log_line!("stderr", "git pull failed, continuing anyway...".to_string());
            }
        }
        Err(e) => {
            log_line!("stderr", format!("Failed to run git pull: {}", e));
        }
    }

    // 执行构建脚本
    log_line!("stdout", format!(">>> {}", script));

    let mut child = match Command::new("bash")
        .arg("-c")
        .arg(&script)
        .current_dir(&work_dir)
        .env("COMMIT_HASH", commit_hash.as_deref().unwrap_or(""))
        .env("BRANCH", branch.as_deref().unwrap_or(""))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(e) => {
            error!(task_id = %task_id, error = %e, "Failed to spawn process");
            log_line!("stderr", format!("Failed to start script: {}", e));
            update_task_status(&state, &task_id, DeployStatus::Failed, Some(-1)).await;
            // Notify deploy-center about the failure
            if let Some(ref callback_url) = state.callback_url {
                let _ = notify_deploy_center(callback_url, &task_id, &project, &DeployStatus::Failed, -1).await;
            }
            return;
        }
    };

    // 流式读取 stdout
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();

    let log_tx_stdout = log_tx.clone();
    let log_tx_stderr = log_tx.clone();

    // 为 stdout/stderr 任务准备发送到 deploy-center 的能力
    let http_client_stdout = http_client.clone();
    let http_client_stderr = http_client.clone();
    let callback_url_stdout = callback_url.clone();
    let callback_url_stderr = callback_url.clone();
    let task_id_stdout = task_id.clone();
    let task_id_stderr = task_id.clone();

    // 过滤不重要的日志行
    fn should_send_to_center(line: &str) -> bool {
        let line = line.trim();
        // 过滤空行
        if line.is_empty() {
            return false;
        }
        // Docker buildx 输出 - 保留关键信息
        if line.starts_with('#') && line.chars().nth(1).map(|c| c.is_ascii_digit()).unwrap_or(false) {
            // 保留: DONE, ERROR, Compiling, Building, 阶段名称
            return line.contains("DONE")
                || line.contains("ERROR")
                || line.contains("error:")
                || line.contains("Compiling")
                || line.contains("Building")
                || line.contains("[stage")
                || line.contains("[builder")
                || line.contains("FROM");
        }
        // 过滤重复的 resolve/sha256 行
        if line.contains("sha256:") && line.contains("resolve") {
            return false;
        }
        true
    }

    let stdout_task = tokio::spawn(async move {
        let reader = BufReader::new(stdout);
        let mut lines = reader.lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let _ = log_tx_stdout.send(LogLine {
                timestamp: chrono::Utc::now(),
                stream: "stdout".to_string(),
                content: line.clone(),
            });
            // 只发送重要日志到 deploy-center
            if let Some(ref url) = callback_url_stdout {
                if should_send_to_center(&line) {
                    let append_url = format!("{}/api/deploy/logs/{}/append", url, task_id_stdout);
                    let _ = http_client_stdout
                        .post(&append_url)
                        .json(&serde_json::json!({
                            "line": line,
                            "stream": "stdout"
                        }))
                        .send()
                        .await;
                }
            }
        }
    });

    let stderr_task = tokio::spawn(async move {
        let reader = BufReader::new(stderr);
        let mut lines = reader.lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let _ = log_tx_stderr.send(LogLine {
                timestamp: chrono::Utc::now(),
                stream: "stderr".to_string(),
                content: line.clone(),
            });
            // stderr 大多是重要信息，但也过滤 buildx 进度
            if let Some(ref url) = callback_url_stderr {
                if should_send_to_center(&line) {
                    let append_url = format!("{}/api/deploy/logs/{}/append", url, task_id_stderr);
                    let _ = http_client_stderr
                        .post(&append_url)
                        .json(&serde_json::json!({
                            "line": line,
                            "stream": "stderr"
                        }))
                        .send()
                        .await;
                }
            }
        }
    });

    // 等待进程完成，同时监听取消信号
    let (status, exit_code) = tokio::select! {
        // 取消信号
        _ = cancel_token.cancelled() => {
            warn!(task_id = %task_id, project = %project, "Deployment cancelled by newer commit");
            log_line!("stderr", "=== Deployment CANCELLED: superseded by newer commit ===".to_string());

            // 尝试杀死子进程
            if let Err(e) = child.kill().await {
                warn!(task_id = %task_id, error = %e, "Failed to kill child process");
            }

            // 清理 stdout/stderr 任务
            stdout_task.abort();
            stderr_task.abort();

            // 从 running_deploys 中移除（如果还在）
            {
                let mut running = state.running_deploys.write().await;
                running.remove(&project);
            }

            // 取消时不需要再通知 deploy-center，因为 trigger_deploy 已经处理了
            return;
        }

        // 正常等待进程完成
        exit_status = child.wait() => {
            // 等待 stdout/stderr 读取完成
            let _ = stdout_task.await;
            let _ = stderr_task.await;

            match exit_status {
                Ok(status) => {
                    let code = status.code().unwrap_or(-1);
                    if status.success() {
                        log_line!("stdout", format!("=== Deployment completed successfully (exit code: {}) ===", code));
                        (DeployStatus::Success, code)
                    } else {
                        log_line!("stderr", format!("=== Deployment failed (exit code: {}) ===", code));
                        (DeployStatus::Failed, code)
                    }
                }
                Err(e) => {
                    log_line!("stderr", format!("=== Process error: {} ===", e));
                    (DeployStatus::Failed, -1)
                }
            }
        }
    };

    // 从 running_deploys 中移除
    {
        let mut running = state.running_deploys.write().await;
        // 只有当前任务才移除（可能已经被新任务替换）
        if let Some(current) = running.get(&project) {
            if current.task_id == task_id {
                running.remove(&project);
            }
        }
    }

    // 更新任务状态
    update_task_status(&state, &task_id, status.clone(), Some(exit_code)).await;

    // 回调通知部署中心
    if let Some(ref callback_url) = state.callback_url {
        let _ = notify_deploy_center(callback_url, &task_id, &project, &status, exit_code).await;
    }

    info!(
        task_id = %task_id,
        project = %project,
        exit_code = exit_code,
        "Deployment finished"
    );
}

/// 更新任务状态
async fn update_task_status(
    state: &Arc<AppState>,
    task_id: &str,
    status: DeployStatus,
    exit_code: Option<i32>,
) {
    let mut tasks = state.tasks.write().await;
    if let Some(task) = tasks.get_mut(task_id) {
        task.status = status;
        task.finished_at = Some(chrono::Utc::now());
        task.exit_code = exit_code;
    }
}

/// 通知部署中心
async fn notify_deploy_center(
    callback_url: &str,
    task_id: &str,
    project: &str,
    status: &DeployStatus,
    exit_code: i32,
) -> Result<(), reqwest::Error> {
    let client = reqwest::Client::new();
    let status_str = match status {
        DeployStatus::Success => "success",
        DeployStatus::Failed => "failed",
        DeployStatus::Running => "running",
    };

    let url = format!("{}/api/deploy/logs/{}", callback_url, task_id);

    client
        .patch(&url)
        .json(&serde_json::json!({
            "status": status_str,
            "exit_code": exit_code,
        }))
        .send()
        .await?;

    info!(task_id = %task_id, project = %project, status = %status_str, "Notified deploy center");
    Ok(())
}

/// 获取任务状态
async fn get_task_status(
    State(state): State<Arc<AppState>>,
    Path(task_id): Path<String>,
) -> Result<impl IntoResponse, (StatusCode, Json<ErrorResponse>)> {
    let tasks = state.tasks.read().await;
    let task = tasks.get(&task_id).ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: "not_found".to_string(),
                message: format!("Task '{}' not found", task_id),
            }),
        )
    })?;

    Ok(Json(task.clone()))
}

/// 流式获取日志
async fn stream_logs(
    State(state): State<Arc<AppState>>,
    Path(task_id): Path<String>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, (StatusCode, Json<ErrorResponse>)> {
    // 获取日志通道
    let channels = state.log_channels.read().await;
    let tx = channels.get(&task_id).ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: "not_found".to_string(),
                message: format!("Task '{}' not found or already completed", task_id),
            }),
        )
    })?;

    let mut rx = tx.subscribe();
    drop(channels);

    let state_clone = state.clone();
    let task_id_clone = task_id.clone();

    let stream = async_stream::stream! {
        loop {
            match rx.recv().await {
                Ok(log_line) => {
                    let json = serde_json::to_string(&log_line).unwrap_or_default();
                    yield Ok(Event::default().data(json));
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    warn!(task_id = %task_id_clone, lagged = n, "Log subscriber lagged");
                    continue;
                }
                Err(broadcast::error::RecvError::Closed) => {
                    // 发送结束事件
                    let tasks = state_clone.tasks.read().await;
                    if let Some(task) = tasks.get(&task_id_clone) {
                        let status = match task.status {
                            DeployStatus::Success => "success",
                            DeployStatus::Failed => "failed",
                            DeployStatus::Running => "running",
                        };
                        yield Ok(Event::default().event("complete").data(
                            serde_json::json!({
                                "status": status,
                                "exit_code": task.exit_code
                            }).to_string()
                        ));
                    }
                    break;
                }
            }
        }
    };

    Ok(Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(std::time::Duration::from_secs(15))
            .text("keepalive"),
    ))
}

#[tokio::main]
async fn main() {
    // 初始化日志
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("xjp_deploy_agent=info".parse().unwrap()),
        )
        .json()
        .init();

    let state = Arc::new(AppState::new());

    info!(
        projects = ?state.projects.keys().collect::<Vec<_>>(),
        "XJP Deploy Agent starting"
    );

    let app = Router::new()
        .route("/health", get(health_check))
        .route("/status", get(health_check))
        .route("/projects", get(list_projects))
        .route("/restart", post(restart_service))
        .route("/trigger/:project", post(trigger_deploy))
        .route("/tasks/:task_id", get(get_task_status))
        .route("/logs/:task_id/stream", get(stream_logs))
        .layer(TraceLayer::new_for_http())
        .layer(CorsLayer::permissive())
        .with_state(state);

    let port = env::var("PORT").unwrap_or_else(|_| "9876".to_string());
    let addr = format!("0.0.0.0:{}", port);

    info!(addr = %addr, "Listening");

    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
