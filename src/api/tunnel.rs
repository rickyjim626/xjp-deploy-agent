//! 隧道 (FRP) API
//!
//! 包含 /tunnel/* 端点

use axum::{
    body::Body,
    extract::{
        ws::{WebSocket, WebSocketUpgrade},
        Path, State,
    },
    http::{HeaderMap, Method, Request, StatusCode},
    response::{IntoResponse, Response},
    routing::{any, get},
    Json, Router,
};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::oneshot;
use tracing::{debug, error, warn};

use crate::domain::tunnel::{
    ConnectedClientInfo, PortMappingStatus, TunnelClientStatus, TunnelMessage, TunnelMode,
    TunnelServerStatus, TunnelStatusResponse,
};
use crate::middleware::RequireApiKey;
use crate::state::AppState;

/// 创建隧道管理路由
pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/tunnel/status", get(get_tunnel_status))
        .route("/tunnel/ws", get(tunnel_websocket_handler))
        .route("/tunnel/proxy/{client_id}", any(tunnel_proxy_handler))
        .route("/tunnel/proxy/{client_id}/", any(tunnel_proxy_handler))
        .route("/tunnel/proxy/{client_id}/*path", any(tunnel_proxy_handler))
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
            let clients_guard = server_state.clients.read().await;

            // 收集所有连接的客户端信息
            let connected_clients: Vec<ConnectedClientInfo> = clients_guard
                .values()
                .map(|client| {
                    let port_mappings: Vec<PortMappingStatus> = client
                        .mappings
                        .iter()
                        .map(|m| PortMappingStatus {
                            mapping: m.clone(),
                            status: if client.port_listeners.contains_key(&m.remote_port) {
                                "listening".to_string()
                            } else {
                                "pending".to_string()
                            },
                            active_connections: 0,
                        })
                        .collect();

                    ConnectedClientInfo {
                        client_id: client.client_id.clone(),
                        client_addr: client.client_addr.clone(),
                        connected_at: client.connected_at,
                        port_mappings,
                    }
                })
                .collect();

            let client_count = connected_clients.len();

            TunnelStatusResponse::Server(TunnelServerStatus {
                listening: true,
                listen_port: state.config.port,
                client_count,
                clients: connected_clients,
                client_connected: client_count > 0, // 向后兼容
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
async fn handle_tunnel_server_connection(
    socket: WebSocket,
    state: Arc<AppState>,
    client_addr: String,
) {
    crate::services::tunnel::server::handle_connection(socket, state, client_addr).await;
}

/// HTTP 代理处理器
///
/// ANY /tunnel/proxy/{client_id}/*
/// 将 HTTP 请求通过隧道转发到指定客户端
async fn tunnel_proxy_handler(
    State(state): State<Arc<AppState>>,
    Path(params): Path<ProxyParams>,
    method: Method,
    headers: HeaderMap,
    request: Request<Body>,
) -> impl IntoResponse {
    // 检查是否为 Server 模式
    if state.tunnel_mode != TunnelMode::Server {
        return (StatusCode::BAD_REQUEST, "Tunnel server mode not enabled").into_response();
    }

    let client_id = &params.client_id;
    let path = params.path.as_deref().unwrap_or("");
    let full_path = if path.is_empty() {
        "/".to_string()
    } else {
        format!("/{}", path)
    };

    debug!(client_id = %client_id, path = %full_path, method = %method, "Proxy request");

    // 查找客户端
    let server_state = &state.tunnel_server_state;
    let clients_guard = server_state.clients.read().await;

    let client = match clients_guard.get(client_id) {
        Some(c) => c,
        None => {
            warn!(client_id = %client_id, "Client not found for proxy request");
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({
                    "error": "client_not_found",
                    "message": format!("Client '{}' is not connected", client_id)
                })),
            )
                .into_response();
        }
    };

    // 生成请求 ID
    let request_id = uuid::Uuid::new_v4().to_string();

    // 收集 headers
    let header_vec: Vec<(String, String)> = headers
        .iter()
        .filter(|(name, _)| {
            // 过滤掉 hop-by-hop headers
            let name_str = name.as_str().to_lowercase();
            !matches!(
                name_str.as_str(),
                "host" | "connection" | "keep-alive" | "transfer-encoding" | "upgrade"
            )
        })
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|v| (name.as_str().to_string(), v.to_string()))
        })
        .collect();

    // 读取 body
    let body_bytes = match axum::body::to_bytes(request.into_body(), 10 * 1024 * 1024).await {
        Ok(bytes) => {
            if bytes.is_empty() {
                None
            } else {
                Some(bytes.to_vec())
            }
        }
        Err(e) => {
            error!(error = %e, "Failed to read request body");
            return (StatusCode::BAD_REQUEST, "Failed to read request body").into_response();
        }
    };

    // 创建响应通道
    let (tx, rx) = oneshot::channel();

    // 注册等待响应
    {
        let mut pending = server_state.pending_http_requests.write().await;
        pending.insert(request_id.clone(), tx);
    }

    // 发送 HTTP 请求消息到客户端
    let http_request = TunnelMessage::HttpRequest {
        request_id: request_id.clone(),
        method: method.to_string(),
        path: full_path.clone(),
        headers: header_vec,
        body: body_bytes,
    };

    if let Err(e) = client.ws_tx.send(http_request).await {
        error!(error = %e, "Failed to send HTTP request to client");
        // 清理
        let mut pending = server_state.pending_http_requests.write().await;
        pending.remove(&request_id);
        return (StatusCode::BAD_GATEWAY, "Failed to send request to client").into_response();
    }

    drop(clients_guard); // 释放锁

    // 等待响应 (超时 30 秒)
    let result = tokio::time::timeout(Duration::from_secs(30), rx).await;

    match result {
        Ok(Ok(response)) => response,
        Ok(Err(_)) => {
            error!(request_id = %request_id, "Response channel closed");
            (StatusCode::BAD_GATEWAY, "Response channel closed").into_response()
        }
        Err(_) => {
            // 超时，清理
            let mut pending = server_state.pending_http_requests.write().await;
            pending.remove(&request_id);
            error!(request_id = %request_id, "Proxy request timeout");
            (StatusCode::GATEWAY_TIMEOUT, "Request timeout").into_response()
        }
    }
}

/// 代理路径参数
#[derive(serde::Deserialize)]
struct ProxyParams {
    client_id: String,
    path: Option<String>,
}
