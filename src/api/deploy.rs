//! 部署管理 API
//!
//! 包含 /trigger/:project, /tasks/*, /logs/* 端点

use axum::{
    extract::{Path, Query, State},
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse,
    },
    routing::{get, post},
    Json, Router,
};
use futures::stream::Stream;
use serde::{Deserialize, Serialize};
use std::{convert::Infallible, sync::Arc};
use tokio::sync::broadcast;
use tracing::warn;

use chrono::Utc;

use crate::config::env::constants::MAX_QUEUE_SIZE;
use crate::domain::deploy::DeployTask;
use crate::error::{ApiError, ApiResult};
use crate::middleware::RequireApiKey;
use crate::services;
use crate::state::app_state::QueuedDeploy;
use crate::state::AppState;

/// 触发部署请求
#[derive(Debug, Clone, Deserialize)]
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
    /// 队列位置（仅当 status="queued" 时有值）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub queue_position: Option<usize>,
    pub stream_url: String,
}

/// 任务历史查询参数
#[derive(Debug, Deserialize)]
pub struct TaskHistoryQuery {
    /// 返回数量限制，默认 20
    #[serde(default = "default_limit")]
    pub limit: usize,
    /// 项目名称过滤
    pub project: Option<String>,
    /// 状态过滤 (success, failed, running)
    pub status: Option<String>,
}

fn default_limit() -> usize {
    20
}

/// 任务历史响应
#[derive(Debug, Serialize)]
pub struct TaskHistoryResponse {
    pub tasks: Vec<DeployTask>,
    pub total: usize,
}

/// 队列状态响应
#[derive(Debug, Serialize)]
pub struct QueueStatusResponse {
    pub project: String,
    /// 当前运行中的任务 ID
    pub running_task_id: Option<String>,
    /// 队列中等待的任务数
    pub queue_length: usize,
}

/// 创建部署管理路由
pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/trigger/:project", post(trigger_deploy))
        .route("/tasks/:task_id", get(get_task_status))
        .route("/tasks/recent", get(get_recent_tasks))
        .route("/logs/:task_id/stream", get(stream_logs))
        .route("/projects/:name/config", get(get_project_config))
        .route("/queue/:project", get(get_queue_status))
}

/// 触发部署
///
/// POST /trigger/:project
/// 需要 API Key
///
/// 注意：实际的部署逻辑在 services/deploy 模块中
/// 此 handler 仅负责请求验证和任务创建
async fn trigger_deploy(
    _auth: RequireApiKey,
    State(state): State<Arc<AppState>>,
    Path(project): Path<String>,
    Json(request): Json<TriggerRequest>,
) -> ApiResult<impl IntoResponse> {
    // 检查项目是否存在
    // 优先从本地配置，然后尝试从部署中心获取
    let project_config = if let Some(config) = state.get_project(&project) {
        // 本地配置存在，克隆使用
        config.clone()
    } else {
        // 尝试从部署中心获取配置
        let remote_config = state
            .deploy_center
            .fetch_config(&project)
            .await
            .ok_or_else(|| ApiError::not_found(format!("Project '{}'", project)))?;

        // 转换为本地 ProjectConfig
        remote_config
            .to_project_config()
            .ok_or_else(|| ApiError::bad_request("Invalid remote config: missing required fields"))?
    };

    // 使用传入的 deploy_log_id 作为 task_id（如果有的话），否则生成新的
    let task_id = request
        .deploy_log_id
        .clone()
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

    // 检查是否已有部署在运行
    if state.has_running_deploy(&project).await {
        // 检查队列是否已满
        let queue_len = state.get_queue_length(&project).await;
        if queue_len >= MAX_QUEUE_SIZE {
            return Err(ApiError::conflict(format!(
                "Deployment queue for '{}' is full (max {})",
                project, MAX_QUEUE_SIZE
            )));
        }

        // 创建排队中的任务
        let task = DeployTask::new_queued(task_id.clone(), project.clone());
        state.task_store.create(task).await;

        // 加入队列
        let position = state
            .enqueue_deploy(
                &project,
                QueuedDeploy {
                    task_id: task_id.clone(),
                    project_config,
                    request,
                    queued_at: Utc::now(),
                },
            )
            .await;

        tracing::info!(
            task_id = %task_id,
            project = %project,
            queue_position = position,
            "Deployment queued"
        );

        // 返回排队响应
        return Ok(Json(TriggerResponse {
            task_id,
            project,
            status: "queued".to_string(),
            queue_position: Some(position),
            stream_url: String::new(), // 排队中暂无日志流
        }));
    }

    // 没有运行中的部署，直接执行
    // 创建任务
    let task = DeployTask::new(task_id.clone(), project.clone());
    state.task_store.create(task).await;

    // 创建日志通道
    let _log_tx = state.log_hub.create(&task_id).await;

    // 注册运行中的部署
    let _cancel_token = state.register_running_deploy(&project, &task_id).await;

    // 构建响应数据
    let response = TriggerResponse {
        task_id: task_id.clone(),
        project: project.clone(),
        status: "running".to_string(),
        queue_position: None,
        stream_url: format!("/logs/{}/stream", task_id),
    };

    // 在后台执行部署（非阻塞）
    let state_clone = state.clone();
    let task_id_clone = task_id.clone();
    let project_clone = project.clone();
    tokio::spawn(async move {
        services::deploy::execute(
            state_clone,
            task_id_clone,
            project_clone,
            project_config,
            request,
        )
        .await;
    });

    // 返回响应
    Ok(Json(response))
}

/// 获取任务状态
///
/// GET /tasks/:task_id
/// 无需认证
///
/// 注意：查询活跃任务和历史记录，已完成的任务也可以查到
async fn get_task_status(
    State(state): State<Arc<AppState>>,
    Path(task_id): Path<String>,
) -> ApiResult<impl IntoResponse> {
    // 使用 get_any 同时查询活跃任务和历史记录
    let task = state
        .task_store
        .get_any(&task_id)
        .await
        .ok_or_else(|| ApiError::not_found(format!("Task '{}'", task_id)))?;

    Ok(Json(task))
}

/// 获取最近的任务历史
///
/// GET /tasks/recent
/// 无需认证
async fn get_recent_tasks(
    State(state): State<Arc<AppState>>,
    Query(query): Query<TaskHistoryQuery>,
) -> impl IntoResponse {
    // 使用新的 TaskStore API
    let history = state
        .task_store
        .get_history(query.limit, query.project.as_deref(), query.status.as_deref())
        .await;

    // 获取运行中的任务
    let running_tasks = state.task_store.get_all().await;

    // 合并运行中的任务和历史任务
    let mut all_tasks: Vec<DeployTask> = running_tasks
        .into_iter()
        .filter(|task| !task.status.is_terminal())
        .collect();

    // 添加历史任务
    all_tasks.extend(history);

    // 应用过滤器
    let filtered: Vec<DeployTask> = all_tasks
        .into_iter()
        .filter(|task| {
            let project_match = query
                .project
                .as_ref()
                .map_or(true, |p| task.project == *p);
            let status_match = query
                .status
                .as_ref()
                .map_or(true, |s| task.status.as_str() == s);
            project_match && status_match
        })
        .take(query.limit)
        .collect();

    let total = filtered.len();

    Json(TaskHistoryResponse {
        tasks: filtered,
        total,
    })
}

/// 流式日志
///
/// GET /logs/:task_id/stream
/// 无需认证
async fn stream_logs(
    State(state): State<Arc<AppState>>,
    Path(task_id): Path<String>,
) -> ApiResult<Sse<impl Stream<Item = Result<Event, Infallible>>>> {
    // 获取日志通道
    let mut rx = state
        .log_hub
        .subscribe(&task_id)
        .await
        .ok_or_else(|| ApiError::not_found(format!("Task '{}' not found or already completed", task_id)))?;

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
                    if let Some(task) = state_clone.task_store.get(&task_id_clone).await {
                        let status = task.status.as_str();
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

/// 项目配置响应
#[derive(Debug, Serialize)]
pub struct ProjectConfigResponse {
    pub name: String,
    pub work_dir: String,
    pub deploy_type: String,
    pub config: serde_json::Value,
}

/// 获取项目配置
///
/// GET /projects/:name/config
/// 无需认证
async fn get_project_config(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> ApiResult<impl IntoResponse> {
    // 先查本地配置
    if let Some(config) = state.get_project(&name) {
        return Ok(Json(ProjectConfigResponse {
            name: name.clone(),
            work_dir: config.work_dir.clone(),
            deploy_type: config.deploy_type.name().to_string(),
            config: serde_json::to_value(&config.deploy_type).unwrap_or_default(),
        }));
    }

    // 尝试从部署中心获取
    if let Some(remote_config) = state.deploy_center.fetch_config(&name).await {
        if let Some(local_config) = remote_config.to_project_config() {
            return Ok(Json(ProjectConfigResponse {
                name: name.clone(),
                work_dir: local_config.work_dir,
                deploy_type: local_config.deploy_type.name().to_string(),
                config: serde_json::to_value(&local_config.deploy_type).unwrap_or_default(),
            }));
        }
    }

    Err(ApiError::not_found(format!("Project '{}'", name)))
}

/// 获取队列状态
///
/// GET /queue/:project
/// 无需认证
async fn get_queue_status(
    State(state): State<Arc<AppState>>,
    Path(project): Path<String>,
) -> impl IntoResponse {
    let running_task_id = state.get_running_deploy_task_id(&project).await;
    let queue_length = state.get_queue_length(&project).await;

    Json(QueueStatusResponse {
        project,
        running_task_id,
        queue_length,
    })
}
