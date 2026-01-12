//! Image Management API
//!
//! Phase 4.4: Pre-fetch images to reduce deployment cold start time
//!
//! Endpoints:
//! - POST /images/prefetch - Pre-fetch an image in the background
//! - GET /images/prefetch/:task_id - Get prefetch task status
//! - GET /images/list - List locally available images

use axum::{
    extract::{Path, State},
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, OnceLock};
use std::time::Instant;
use tokio::process::Command;
use tokio::sync::RwLock;
use uuid::Uuid;

use crate::error::{ApiError, ApiResult};
use crate::middleware::RequireApiKey;
use crate::state::AppState;

/// Pre-fetch request
#[derive(Debug, Clone, Deserialize)]
pub struct PrefetchRequest {
    /// Docker image to prefetch (e.g., "ghcr.io/org/image:latest")
    pub image: String,
    /// Optional: project name for tracking
    pub project: Option<String>,
    /// Optional: source deploy log ID that triggered this prefetch
    pub source_deploy_log_id: Option<String>,
}

/// Pre-fetch response
#[derive(Debug, Clone, Serialize)]
pub struct PrefetchResponse {
    /// Prefetch task ID
    pub task_id: String,
    /// Image being prefetched
    pub image: String,
    /// Status: "started", "running", "completed", "failed"
    pub status: String,
    /// Message (error message if failed)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// Pre-fetch task status
#[derive(Debug, Clone, Serialize)]
pub struct PrefetchTaskStatus {
    pub task_id: String,
    pub image: String,
    pub project: Option<String>,
    pub status: String,
    /// Duration in milliseconds (if completed)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    /// Error message (if failed)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Created timestamp
    pub created_at: chrono::DateTime<chrono::Utc>,
    /// Completed timestamp
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// Internal prefetch task tracking
#[derive(Debug, Clone)]
struct PrefetchTask {
    task_id: String,
    image: String,
    project: Option<String>,
    status: String,
    duration_ms: Option<u64>,
    error: Option<String>,
    created_at: chrono::DateTime<chrono::Utc>,
    completed_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// Global prefetch task store (in-memory, limited size)
static PREFETCH_TASKS: OnceLock<RwLock<HashMap<String, PrefetchTask>>> = OnceLock::new();

/// Get or initialize the prefetch tasks store
fn get_prefetch_tasks() -> &'static RwLock<HashMap<String, PrefetchTask>> {
    PREFETCH_TASKS.get_or_init(|| RwLock::new(HashMap::new()))
}

/// Maximum number of prefetch tasks to keep in memory
const MAX_PREFETCH_TASKS: usize = 100;
/// Prefetch task retention duration
const TASK_RETENTION_SECS: u64 = 3600; // 1 hour

/// Create image management router
pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/images/prefetch", post(prefetch_image))
        .route("/images/prefetch/:task_id", get(get_prefetch_status))
        .route("/images/list", get(list_images))
}

/// Pre-fetch an image in the background
///
/// POST /images/prefetch
/// Requires API Key
///
/// This endpoint is called by Deploy Center after a build completes
/// to warm up the image cache on downstream deployment targets.
async fn prefetch_image(
    _auth: RequireApiKey,
    State(_state): State<Arc<AppState>>,
    Json(request): Json<PrefetchRequest>,
) -> ApiResult<impl IntoResponse> {
    let task_id = Uuid::new_v4().to_string();
    let image = request.image.clone();

    // Validate image name (basic check)
    if image.is_empty() || image.contains(' ') {
        return Err(ApiError::bad_request("Invalid image name"));
    }

    // Create task record
    let task = PrefetchTask {
        task_id: task_id.clone(),
        image: image.clone(),
        project: request.project.clone(),
        status: "running".to_string(),
        duration_ms: None,
        error: None,
        created_at: chrono::Utc::now(),
        completed_at: None,
    };

    // Store task
    {
        let mut tasks = get_prefetch_tasks().write().await;

        // Cleanup old tasks if needed
        cleanup_old_tasks(&mut tasks);

        tasks.insert(task_id.clone(), task);
    }

    tracing::info!(
        task_id = %task_id,
        image = %image,
        project = ?request.project,
        source_deploy_log_id = ?request.source_deploy_log_id,
        "Starting image prefetch"
    );

    // Spawn background prefetch task
    let task_id_clone = task_id.clone();
    let image_clone = image.clone();
    tokio::spawn(async move {
        let start = Instant::now();

        // Execute docker pull
        let result = Command::new("docker")
            .args(["pull", &image_clone])
            .output()
            .await;

        let elapsed = start.elapsed();
        let duration_ms = elapsed.as_millis() as u64;

        // Update task status
        let mut tasks = get_prefetch_tasks().write().await;
        if let Some(task) = tasks.get_mut(&task_id_clone) {
            task.duration_ms = Some(duration_ms);
            task.completed_at = Some(chrono::Utc::now());

            match result {
                Ok(output) if output.status.success() => {
                    task.status = "completed".to_string();
                    tracing::info!(
                        task_id = %task_id_clone,
                        image = %image_clone,
                        duration_ms = duration_ms,
                        "Image prefetch completed successfully"
                    );
                }
                Ok(output) => {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    task.status = "failed".to_string();
                    task.error = Some(stderr.to_string());
                    tracing::warn!(
                        task_id = %task_id_clone,
                        image = %image_clone,
                        error = %stderr,
                        "Image prefetch failed"
                    );
                }
                Err(e) => {
                    task.status = "failed".to_string();
                    task.error = Some(e.to_string());
                    tracing::error!(
                        task_id = %task_id_clone,
                        image = %image_clone,
                        error = %e,
                        "Image prefetch execution error"
                    );
                }
            }
        }
    });

    Ok(Json(PrefetchResponse {
        task_id,
        image,
        status: "started".to_string(),
        message: None,
    }))
}

/// Get prefetch task status
///
/// GET /images/prefetch/:task_id
/// No authentication required
async fn get_prefetch_status(
    Path(task_id): Path<String>,
) -> ApiResult<impl IntoResponse> {
    let tasks = get_prefetch_tasks().read().await;

    let task = tasks
        .get(&task_id)
        .ok_or_else(|| ApiError::not_found(format!("Prefetch task '{}'", task_id)))?;

    Ok(Json(PrefetchTaskStatus {
        task_id: task.task_id.clone(),
        image: task.image.clone(),
        project: task.project.clone(),
        status: task.status.clone(),
        duration_ms: task.duration_ms,
        error: task.error.clone(),
        created_at: task.created_at,
        completed_at: task.completed_at,
    }))
}

/// List recently prefetched images
#[derive(Debug, Serialize)]
pub struct PrefetchListResponse {
    pub tasks: Vec<PrefetchTaskStatus>,
    pub total: usize,
}

/// List images (recent prefetch tasks)
///
/// GET /images/list
/// No authentication required
async fn list_images() -> impl IntoResponse {
    let tasks = get_prefetch_tasks().read().await;

    let mut task_list: Vec<PrefetchTaskStatus> = tasks
        .values()
        .map(|t| PrefetchTaskStatus {
            task_id: t.task_id.clone(),
            image: t.image.clone(),
            project: t.project.clone(),
            status: t.status.clone(),
            duration_ms: t.duration_ms,
            error: t.error.clone(),
            created_at: t.created_at,
            completed_at: t.completed_at,
        })
        .collect();

    // Sort by created_at descending
    task_list.sort_by(|a, b| b.created_at.cmp(&a.created_at));

    let total = task_list.len();

    Json(PrefetchListResponse {
        tasks: task_list,
        total,
    })
}

/// Cleanup old tasks to prevent memory leak
fn cleanup_old_tasks(tasks: &mut HashMap<String, PrefetchTask>) {
    let now = chrono::Utc::now();
    let retention = chrono::Duration::seconds(TASK_RETENTION_SECS as i64);

    // Remove tasks older than retention period
    tasks.retain(|_, task| {
        let age = now.signed_duration_since(task.created_at);
        age < retention
    });

    // If still too many, remove oldest completed tasks
    while tasks.len() >= MAX_PREFETCH_TASKS {
        // Find oldest completed task
        let oldest_completed = tasks
            .iter()
            .filter(|(_, t)| t.status == "completed" || t.status == "failed")
            .min_by_key(|(_, t)| t.created_at)
            .map(|(k, _)| k.clone());

        if let Some(key) = oldest_completed {
            tasks.remove(&key);
        } else {
            break;
        }
    }
}
