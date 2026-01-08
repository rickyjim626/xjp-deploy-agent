//! NFA node API
//!
//! Exposes a stable interface for backend services to call a Windows NFA node.

use axum::{
    extract::{Query, State},
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use std::sync::Arc;

use crate::{
    error::{ApiError, ApiResult},
    middleware::RequireApiKey,
    state::AppState,
};

#[derive(Debug, Deserialize)]
pub struct TailQuery {
    #[serde(default = "default_tail")]
    pub tail: usize,
}

fn default_tail() -> usize {
    200
}

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/nfa/status", get(get_status))
        .route("/nfa/start", post(start))
        .route("/nfa/stop", post(stop))
        .route("/nfa/restart", post(restart))
        .route("/nfa/logs", get(get_logs))
        .route("/nfa/jobs", get(get_jobs))
        .route("/nfa/align", post(align))
}

async fn get_status(_auth: RequireApiKey, State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let Some(nfa) = &state.nfa else {
        return Json(serde_json::json!({
            "enabled": false,
            "message": "NFA disabled"
        }))
        .into_response();
    };

    Json(nfa.status().await).into_response()
}

async fn start(
    _auth: RequireApiKey,
    State(state): State<Arc<AppState>>,
) -> ApiResult<Json<serde_json::Value>> {
    let nfa = state
        .nfa
        .as_ref()
        .ok_or_else(|| ApiError::service_unavailable("NFA disabled".to_string()))?;

    nfa.start()
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?;

    Ok(Json(serde_json::json!({ "ok": true })))
}

async fn stop(
    _auth: RequireApiKey,
    State(state): State<Arc<AppState>>,
) -> ApiResult<Json<serde_json::Value>> {
    let nfa = state
        .nfa
        .as_ref()
        .ok_or_else(|| ApiError::service_unavailable("NFA disabled".to_string()))?;

    nfa.stop()
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?;

    Ok(Json(serde_json::json!({ "ok": true })))
}

async fn restart(
    _auth: RequireApiKey,
    State(state): State<Arc<AppState>>,
) -> ApiResult<Json<serde_json::Value>> {
    let nfa = state
        .nfa
        .as_ref()
        .ok_or_else(|| ApiError::service_unavailable("NFA disabled".to_string()))?;

    nfa.restart()
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?;

    Ok(Json(serde_json::json!({ "ok": true })))
}

async fn get_logs(
    _auth: RequireApiKey,
    State(state): State<Arc<AppState>>,
    Query(q): Query<TailQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    let nfa = state
        .nfa
        .as_ref()
        .ok_or_else(|| ApiError::service_unavailable("NFA disabled".to_string()))?;

    let lines = nfa.tail_logs(q.tail).await;
    Ok(Json(serde_json::json!({ "lines": lines })))
}

async fn get_jobs(
    _auth: RequireApiKey,
    State(state): State<Arc<AppState>>,
) -> ApiResult<Json<serde_json::Value>> {
    let nfa = state
        .nfa
        .as_ref()
        .ok_or_else(|| ApiError::service_unavailable("NFA disabled".to_string()))?;

    let jobs = nfa
        .get_jobs()
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?;

    Ok(Json(jobs))
}

async fn align(
    _auth: RequireApiKey,
    State(state): State<Arc<AppState>>,
    Json(req): Json<crate::services::nfa::NfaAlignRequest>,
) -> ApiResult<Json<serde_json::Value>> {
    let nfa = state
        .nfa
        .as_ref()
        .ok_or_else(|| ApiError::service_unavailable("NFA disabled".to_string()))?;

    let result = nfa
        .align(req)
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?;

    Ok(Json(result))
}

