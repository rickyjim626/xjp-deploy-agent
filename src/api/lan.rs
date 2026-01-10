//! LAN Discovery API
//!
//! 提供 LAN 发现服务的 HTTP API 端点

use axum::{
    extract::State,
    http::StatusCode,
    response::IntoResponse,
    routing::get,
    Json, Router,
};
use serde::Serialize;
use std::sync::Arc;

use crate::state::AppState;
use crate::services::lan_discovery::{DiscoveredPeer, LanDiscoveryStats};
use crate::services::service_router::ServiceEndpoint;

/// 创建 LAN API 路由
pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/peers", get(get_peers))
        .route("/peers/healthy", get(get_healthy_peers))
        .route("/stats", get(get_stats))
        .route("/service/:service_id", get(get_service_endpoint))
        .route("/services", get(get_all_services))
}

/// 获取所有发现的 peers
async fn get_peers(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    match &state.lan_discovery {
        Some(discovery) => {
            let peers = discovery.get_peers().await;
            Json(PeersResponse { peers }).into_response()
        }
        None => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ErrorResponse {
                error: "LAN discovery not enabled".to_string(),
            }),
        )
            .into_response(),
    }
}

/// 获取健康的 peers
async fn get_healthy_peers(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    match &state.lan_discovery {
        Some(discovery) => {
            let peers = discovery.get_healthy_peers().await;
            Json(PeersResponse { peers }).into_response()
        }
        None => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ErrorResponse {
                error: "LAN discovery not enabled".to_string(),
            }),
        )
            .into_response(),
    }
}

/// 获取 LAN 发现统计
async fn get_stats(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    match &state.lan_discovery {
        Some(discovery) => {
            let stats = discovery.get_stats().await;
            Json(stats).into_response()
        }
        None => Json(LanDiscoveryStats {
            enabled: false,
            local_ip: None,
            agent_id: String::new(),
            broadcast_port: 0,
            total_peers: 0,
            healthy_peers: 0,
            total_services: 0,
            local_services: 0,
        })
        .into_response(),
    }
}

/// 获取服务端点
async fn get_service_endpoint(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(service_id): axum::extract::Path<String>,
) -> impl IntoResponse {
    match &state.service_router {
        Some(router) => {
            let endpoint = router.get_endpoint(&service_id).await;
            Json(ServiceEndpointResponse {
                service_id,
                endpoint,
            })
            .into_response()
        }
        None => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ErrorResponse {
                error: "Service router not available".to_string(),
            }),
        )
            .into_response(),
    }
}

/// 获取所有已知服务
async fn get_all_services(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    match &state.service_router {
        Some(router) => {
            let endpoints = router.get_all_endpoints().await;
            let services: Vec<ServiceInfo> = endpoints
                .into_iter()
                .map(|(service_id, endpoint)| ServiceInfo {
                    service_id,
                    url: endpoint.url,
                    source: format!("{:?}", endpoint.source),
                    latency_ms: endpoint.latency_ms,
                    healthy: endpoint.healthy,
                })
                .collect();
            Json(ServicesResponse { services }).into_response()
        }
        None => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ErrorResponse {
                error: "Service router not available".to_string(),
            }),
        )
            .into_response(),
    }
}

// ========== Response Types ==========

#[derive(Serialize)]
struct PeersResponse {
    peers: Vec<DiscoveredPeer>,
}

#[derive(Serialize)]
struct ErrorResponse {
    error: String,
}

#[derive(Serialize)]
struct ServiceEndpointResponse {
    service_id: String,
    endpoint: ServiceEndpoint,
}

#[derive(Serialize)]
struct ServicesResponse {
    services: Vec<ServiceInfo>,
}

#[derive(Serialize)]
struct ServiceInfo {
    service_id: String,
    url: String,
    source: String,
    latency_ms: Option<f64>,
    healthy: bool,
}
