//! 隧道服务端
//!
//! 接收多个客户端连接，使用 PortListenerManager 管理端口监听器。
//! 端口监听器的生命周期与客户端 WebSocket 连接解耦，从根本上消除端口竞争问题。

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::ws::{Message, WebSocket};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use crate::domain::tunnel::TunnelMessage;
use crate::state::tunnel_state::ClientState;
use crate::state::AppState;

const PING_INTERVAL_SECS: u64 = 30;

/// 处理隧道服务端连接
pub async fn handle_connection(socket: WebSocket, state: Arc<AppState>, client_addr: String) {
    info!(client_addr = %client_addr, "New tunnel client connection");

    if let Err(e) = run_server(socket, state.clone(), client_addr.clone()).await {
        error!(error = %e, client_addr = %client_addr, "Tunnel server error");
    }

    info!(client_addr = %client_addr, "Tunnel client disconnected");
}

/// 运行服务端逻辑
async fn run_server(
    socket: WebSocket,
    state: Arc<AppState>,
    client_addr: String,
) -> anyhow::Result<()> {
    let (mut ws_tx, mut ws_rx) = socket.split();
    let server_state = &state.tunnel_server_state;
    let port_manager = &state.port_listener_manager;

    // 用于接收要发送的消息
    let (msg_tx, mut msg_rx) = mpsc::channel::<TunnelMessage>(256);

    // 客户端 ID，等待 Config 消息获取
    let mut client_id: Option<String> = None;

    // 心跳定时器
    let mut ping_interval = tokio::time::interval(Duration::from_secs(PING_INTERVAL_SECS));

    loop {
        tokio::select! {
            // 处理来自客户端的消息
            msg = ws_rx.next() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        match serde_json::from_str::<TunnelMessage>(&text) {
                            Ok(tunnel_msg) => {
                                // 处理 Config 消息时初始化客户端
                                if let TunnelMessage::Config { client_id: cid, mappings } = &tunnel_msg {
                                    if client_id.is_none() {
                                        // 检查是否已有同 ID 的客户端连接
                                        // 注意：不需要等待端口释放，因为端口监听器是复用的
                                        if server_state.has_client(cid).await {
                                            warn!(client_id = %cid, "Client with same ID already connected, rebinding ports");
                                            // 只从客户端状态中移除，端口监听器会自动重新绑定
                                            server_state.remove_client(cid).await;
                                        }

                                        // 创建新的客户端状态
                                        let mut new_client = ClientState::new(
                                            cid.clone(),
                                            client_addr.clone(),
                                            msg_tx.clone(),
                                        );
                                        new_client.mappings = mappings.clone();

                                        // 添加到服务器状态
                                        server_state.add_client(new_client).await;
                                        client_id = Some(cid.clone());

                                        // 使用 PortListenerManager 绑定端口（瞬时完成，无端口竞争）
                                        if let Err(e) = port_manager
                                            .bind_ports_for_client(cid, mappings, msg_tx.clone())
                                            .await
                                        {
                                            error!(client_id = %cid, error = %e, "Failed to bind ports for client");
                                            // 清理客户端状态
                                            server_state.remove_client(cid).await;
                                            client_id = None;
                                            continue;
                                        }

                                        info!(
                                            client_id = %cid,
                                            client_addr = %client_addr,
                                            mappings = mappings.len(),
                                            "Client registered and ports bound"
                                        );
                                    }
                                } else if let Some(cid) = &client_id {
                                    // 将消息路由到 PortListenerManager 处理
                                    handle_client_message(
                                        tunnel_msg,
                                        cid,
                                        &state,
                                    ).await;
                                }
                            }
                            Err(e) => {
                                warn!(error = %e, "Failed to parse tunnel message");
                            }
                        }
                    }
                    Some(Ok(Message::Binary(data))) => {
                        if let Ok(tunnel_msg) = serde_json::from_slice::<TunnelMessage>(&data) {
                            if let Some(cid) = &client_id {
                                handle_client_message(
                                    tunnel_msg,
                                    cid,
                                    &state,
                                ).await;
                            }
                        }
                    }
                    Some(Ok(Message::Ping(data))) => {
                        let _ = ws_tx.send(Message::Pong(data)).await;
                    }
                    Some(Ok(Message::Pong(_))) => {
                        debug!("Received pong from client");
                    }
                    Some(Ok(Message::Close(_))) => {
                        info!("Client closed connection");
                        break;
                    }
                    Some(Err(e)) => {
                        error!(error = %e, "WebSocket error");
                        break;
                    }
                    None => {
                        info!("WebSocket stream ended");
                        break;
                    }
                }
            }

            // 发送消息到客户端
            Some(msg) = msg_rx.recv() => {
                let json = serde_json::to_string(&msg)?;
                if let Err(e) = ws_tx.send(Message::Text(json)).await {
                    error!(error = %e, "Failed to send message to client");
                    break;
                }
            }

            // 心跳
            _ = ping_interval.tick() => {
                let ping_msg = serde_json::to_string(&TunnelMessage::Ping)?;
                if let Err(e) = ws_tx.send(Message::Text(ping_msg)).await {
                    error!(error = %e, "Failed to send ping");
                    break;
                }
            }
        }
    }

    // 清理客户端
    if let Some(cid) = &client_id {
        info!(client_id = %cid, "Cleaning up client");
        // 从客户端状态中移除
        server_state.remove_client(cid).await;
        // 解绑端口（端口监听器继续存在，等待下次绑定）
        port_manager.unbind_client(cid).await;
    }

    Ok(())
}

/// 处理来自客户端的消息
async fn handle_client_message(
    msg: TunnelMessage,
    client_id: &str,
    state: &Arc<AppState>,
) {
    let port_manager = &state.port_listener_manager;

    match &msg {
        TunnelMessage::Config { .. } => {
            // 已在上层处理
        }

        TunnelMessage::Connected { .. }
        | TunnelMessage::ConnectFailed { .. }
        | TunnelMessage::Data { .. }
        | TunnelMessage::Close { .. } => {
            // 路由到 PortListenerManager 处理
            port_manager.handle_client_message(client_id, msg).await;
        }

        TunnelMessage::Ping => {
            // Ping 由 PortListener 内部处理，这里只是记录
            debug!(client_id = %client_id, "Received ping from client");
        }

        TunnelMessage::Pong => {
            debug!(client_id = %client_id, "Received pong from client");
        }

        TunnelMessage::Connect { .. } => {
            warn!(client_id = %client_id, "Unexpected Connect message from client");
        }

        TunnelMessage::HttpRequest { .. } => {
            warn!(client_id = %client_id, "Unexpected HttpRequest message from client");
        }

        TunnelMessage::HttpResponse {
            request_id,
            status,
            headers,
            body,
        } => {
            debug!(request_id = %request_id, status = status, "Received HTTP response from client");

            // 查找等待的请求
            let sender = {
                let mut pending = state.tunnel_server_state.pending_http_requests.write().await;
                pending.remove(request_id)
            };

            if let Some(tx) = sender {
                // 构建 HTTP 响应
                let mut response_builder = Response::builder().status(
                    StatusCode::from_u16(*status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                );

                for (name, value) in headers {
                    response_builder = response_builder.header(name, value);
                }

                let response = match body {
                    Some(data) => response_builder
                        .body(Body::from(data.clone()))
                        .unwrap_or_else(|_| {
                            (StatusCode::INTERNAL_SERVER_ERROR, "Failed to build response")
                                .into_response()
                        }),
                    None => response_builder
                        .body(Body::empty())
                        .unwrap_or_else(|_| {
                            (StatusCode::INTERNAL_SERVER_ERROR, "Failed to build response")
                                .into_response()
                        }),
                };

                if tx.send(response).is_err() {
                    warn!(request_id = %request_id, "Failed to send HTTP response - receiver dropped");
                }
            } else {
                warn!(request_id = %request_id, "No pending request found for HTTP response");
            }
        }

        TunnelMessage::HttpError { request_id, error } => {
            warn!(request_id = %request_id, error = %error, "Received HTTP error from client");

            let sender = {
                let mut pending = state.tunnel_server_state.pending_http_requests.write().await;
                pending.remove(request_id)
            };

            if let Some(tx) = sender {
                let response = (
                    StatusCode::BAD_GATEWAY,
                    axum::Json(serde_json::json!({
                        "error": "proxy_error",
                        "message": error
                    })),
                )
                    .into_response();

                if tx.send(response).is_err() {
                    warn!(request_id = %request_id, "Failed to send HTTP error response");
                }
            }
        }
    }
}
