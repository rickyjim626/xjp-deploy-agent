//! 隧道 (FRP) API
//!
//! 包含 /tunnel/* 端点

use axum::{
    extract::{
        ws::{WebSocket, WebSocketUpgrade},
        State,
    },
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::get,
    Json, Router,
};
use std::sync::Arc;

use crate::domain::tunnel::{
    PortMappingStatus, TunnelClientStatus, TunnelMode, TunnelServerStatus, TunnelStatusResponse,
};
use crate::middleware::RequireApiKey;
use crate::state::AppState;

/// 创建隧道管理路由
pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/tunnel/status", get(get_tunnel_status))
        .route("/tunnel/ws", get(tunnel_websocket_handler))
}

/// 获取隧道状态
///
/// GET /tunnel/status
/// 需要 API Key
async fn get_tunnel_status(
    _auth: RequireApiKey,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    // If we are using managed FRP (frpc), expose status via the same tunnel endpoint for
    // backward compatibility.
    if let Some(frp) = &state.frp {
        let frp_status = frp.status().await;
        return Json(TunnelStatusResponse::Client(TunnelClientStatus {
            connected: frp_status.running,
            server_url: format!("{}:{}", frp_status.server_addr, frp_status.server_port),
            connected_at: frp_status.started_at,
            last_error: frp_status.last_error,
            reconnect_count: frp_status.restart_count,
            mappings: frp_status.mappings,
        }))
        .into_response();
    }

    let response = match &state.tunnel_mode {
        TunnelMode::Server => {
            let server_state = &state.tunnel_server_state;
            let client_connected = *server_state.client_connected.read().await;
            let client_addr = server_state.client_addr.read().await.clone();
            let client_connected_at = *server_state.client_connected_at.read().await;
            let client_mappings = server_state.client_mappings.read().await.clone();
            let port_listeners = server_state.port_listeners.read().await;

            let port_mappings: Vec<PortMappingStatus> = client_mappings
                .iter()
                .map(|m| PortMappingStatus {
                    mapping: m.clone(),
                    status: if port_listeners.contains_key(&m.remote_port) {
                        "listening".to_string()
                    } else {
                        "pending".to_string()
                    },
                    active_connections: 0,
                })
                .collect();

            TunnelStatusResponse::Server(TunnelServerStatus {
                listening: true,
                listen_port: state.config.port,
                client_connected,
                client_addr,
                client_connected_at,
                port_mappings,
            })
        }
        TunnelMode::Client => {
            let client_state = &state.tunnel_client_state;
            let connected = *client_state.connected.read().await;
            let connected_at = *client_state.connected_at.read().await;
            let last_error = client_state.last_error.read().await.clone();
            let reconnect_count = *client_state.reconnect_count.read().await;

            let mappings: Vec<PortMappingStatus> = state
                .tunnel_port_mappings
                .iter()
                .map(|m| PortMappingStatus {
                    mapping: m.clone(),
                    status: if connected {
                        "active".to_string()
                    } else {
                        "disconnected".to_string()
                    },
                    active_connections: 0,
                })
                .collect();

            TunnelStatusResponse::Client(TunnelClientStatus {
                connected,
                server_url: state.tunnel_server_url.clone(),
                connected_at,
                last_error,
                reconnect_count,
                mappings,
            })
        }
        TunnelMode::Disabled => TunnelStatusResponse::Disabled,
    };

    Json(response).into_response()
}

/// WebSocket 隧道端点 (Server 模式)
///
/// GET /tunnel/ws
/// 使用 x-tunnel-token header 认证
async fn tunnel_websocket_handler(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    // 验证 token
    let token = headers
        .get("x-tunnel-token")
        .and_then(|v| v.to_str().ok())
        .or_else(|| {
            // 也检查 query parameter
            headers
                .get("sec-websocket-protocol")
                .and_then(|v| v.to_str().ok())
        });

    if token != Some(&state.tunnel_auth_token) {
        return (StatusCode::UNAUTHORIZED, "Invalid tunnel token").into_response();
    }

    if state.tunnel_mode != TunnelMode::Server {
        return (StatusCode::BAD_REQUEST, "Tunnel server mode not enabled").into_response();
    }

    // 获取客户端地址
    let client_addr = headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("unknown")
        .to_string();

    ws.on_upgrade(move |socket| handle_tunnel_server_connection(socket, state, client_addr))
}

/// 处理 Server 端的隧道连接
///
/// TODO: 完整实现移至 services/tunnel/server.rs
async fn handle_tunnel_server_connection(
    _socket: WebSocket,
    _state: Arc<AppState>,
    _client_addr: String,
) {
    // TODO: 实现完整的隧道服务端逻辑
    // 当前只是占位符，完整实现将在 services 层
    tracing::info!("Tunnel connection handler - implementation pending in services layer");
}
