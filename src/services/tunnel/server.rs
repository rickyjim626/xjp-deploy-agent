//! 隧道服务端
//!
//! 接收多个客户端连接，监听远程端口，转发数据到对应客户端

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::ws::{Message, WebSocket};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use crate::domain::tunnel::{PortMapping, TunnelMessage};
use crate::state::tunnel_state::ClientState;
use crate::state::AppState;

const READ_BUFFER_SIZE: usize = 65536;
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

    // 用于接收要发送的消息
    let (msg_tx, mut msg_rx) = mpsc::channel::<TunnelMessage>(256);

    // 客户端 ID，等待 Config 消息获取
    let mut client_id: Option<String> = None;

    // 远程连接管理 (conn_id -> 发送数据到 TCP 的通道)
    let remote_connections: Arc<tokio::sync::RwLock<HashMap<String, mpsc::Sender<Vec<u8>>>>> =
        Arc::new(tokio::sync::RwLock::new(HashMap::new()));

    // 等待连接确认的 pending connections (conn_id -> oneshot sender)
    let pending_connections: Arc<tokio::sync::RwLock<HashMap<String, oneshot::Sender<()>>>> =
        Arc::new(tokio::sync::RwLock::new(HashMap::new()));

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
                                        if server_state.has_client(cid).await {
                                            warn!(client_id = %cid, "Client with same ID already connected, disconnecting old one");
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

                                        info!(
                                            client_id = %cid,
                                            client_addr = %client_addr,
                                            mappings = mappings.len(),
                                            "Client registered"
                                        );

                                        // 为每个映射启动端口监听器
                                        for mapping in mappings {
                                            start_port_listener(
                                                mapping.clone(),
                                                cid.clone(),
                                                state.clone(),
                                                msg_tx.clone(),
                                                remote_connections.clone(),
                                                pending_connections.clone(),
                                            ).await;
                                        }
                                    }
                                } else if let Some(cid) = &client_id {
                                    handle_client_message(
                                        tunnel_msg,
                                        cid.clone(),
                                        state.clone(),
                                        msg_tx.clone(),
                                        remote_connections.clone(),
                                        pending_connections.clone(),
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
                                    cid.clone(),
                                    state.clone(),
                                    msg_tx.clone(),
                                    remote_connections.clone(),
                                    pending_connections.clone(),
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
    if let Some(cid) = client_id {
        info!(client_id = %cid, "Cleaning up client");
        server_state.remove_client(&cid).await;
    }

    // 清理所有远程连接
    let mut conns = remote_connections.write().await;
    conns.clear();

    Ok(())
}

/// 处理来自客户端的消息
async fn handle_client_message(
    msg: TunnelMessage,
    client_id: String,
    state: Arc<AppState>,
    msg_tx: mpsc::Sender<TunnelMessage>,
    remote_connections: Arc<tokio::sync::RwLock<HashMap<String, mpsc::Sender<Vec<u8>>>>>,
    pending_connections: Arc<tokio::sync::RwLock<HashMap<String, oneshot::Sender<()>>>>,
) {
    let _server_state = &state.tunnel_server_state;

    match msg {
        TunnelMessage::Config { .. } => {
            // 已在上层处理
        }

        TunnelMessage::Connected { conn_id } => {
            debug!(client_id = %client_id, conn_id = %conn_id, "Client connected to local service");
            let mut pending = pending_connections.write().await;
            if let Some(tx) = pending.remove(&conn_id) {
                let _ = tx.send(());
                debug!(conn_id = %conn_id, "Signaled connection ready to forward data");
            } else {
                warn!(conn_id = %conn_id, "No pending connection found for Connected message");
            }
        }

        TunnelMessage::ConnectFailed { conn_id, error } => {
            warn!(client_id = %client_id, conn_id = %conn_id, error = %error, "Client failed to connect to local service");
            {
                let mut pending = pending_connections.write().await;
                pending.remove(&conn_id);
            }
            let mut conns = remote_connections.write().await;
            conns.remove(&conn_id);
        }

        TunnelMessage::Data { conn_id, data } => {
            let conns = remote_connections.read().await;
            if let Some(tx) = conns.get(&conn_id) {
                if let Err(e) = tx.send(data).await {
                    debug!(conn_id = %conn_id, error = %e, "Failed to forward data to remote connection");
                }
            }
        }

        TunnelMessage::Close { conn_id } => {
            debug!(conn_id = %conn_id, "Closing remote connection");
            let mut conns = remote_connections.write().await;
            conns.remove(&conn_id);
        }

        TunnelMessage::Ping => {
            let _ = msg_tx.send(TunnelMessage::Pong).await;
        }

        TunnelMessage::Pong => {
            debug!("Received pong from client");
        }

        TunnelMessage::Connect { .. } => {
            warn!("Unexpected Connect message from client");
        }

        TunnelMessage::HttpRequest { .. } => {
            warn!("Unexpected HttpRequest message from client");
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
                pending.remove(&request_id)
            };

            if let Some(tx) = sender {
                // 构建 HTTP 响应
                let mut response_builder = Response::builder().status(
                    StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                );

                for (name, value) in headers {
                    response_builder = response_builder.header(name, value);
                }

                let response = match body {
                    Some(data) => response_builder
                        .body(Body::from(data))
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
                pending.remove(&request_id)
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

/// 启动端口监听器
async fn start_port_listener(
    mapping: PortMapping,
    client_id: String,
    state: Arc<AppState>,
    msg_tx: mpsc::Sender<TunnelMessage>,
    remote_connections: Arc<tokio::sync::RwLock<HashMap<String, mpsc::Sender<Vec<u8>>>>>,
    pending_connections: Arc<tokio::sync::RwLock<HashMap<String, oneshot::Sender<()>>>>,
) {
    let port = mapping.remote_port;
    let addr = format!("0.0.0.0:{}", port);

    let cancel_token = CancellationToken::new();

    // 注册端口到客户端的映射
    state
        .tunnel_server_state
        .register_port(port, client_id.clone())
        .await;

    // 记录监听器到客户端状态
    {
        let mut clients = state.tunnel_server_state.clients.write().await;
        if let Some(client) = clients.get_mut(&client_id) {
            client.port_listeners.insert(port, cancel_token.clone());
        }
    }

    let cancel_token_clone = cancel_token.clone();
    let client_id_clone = client_id.clone();

    tokio::spawn(async move {
        match TcpListener::bind(&addr).await {
            Ok(listener) => {
                info!(
                    client_id = %client_id_clone,
                    port = port,
                    name = %mapping.name,
                    "Started port listener"
                );

                loop {
                    tokio::select! {
                        _ = cancel_token_clone.cancelled() => {
                            info!(port = port, "Port listener cancelled");
                            break;
                        }

                        result = listener.accept() => {
                            match result {
                                Ok((stream, peer_addr)) => {
                                    let conn_id = Uuid::new_v4().to_string();
                                    info!(
                                        conn_id = %conn_id,
                                        client_id = %client_id_clone,
                                        port = port,
                                        peer = %peer_addr,
                                        "New connection on tunnel port"
                                    );

                                    let (ready_tx, ready_rx) = oneshot::channel();

                                    {
                                        let mut pending = pending_connections.write().await;
                                        pending.insert(conn_id.clone(), ready_tx);
                                    }

                                    let msg_tx_clone = msg_tx.clone();
                                    let remote_connections_clone = remote_connections.clone();
                                    let pending_connections_clone = pending_connections.clone();

                                    if let Err(e) = msg_tx.send(TunnelMessage::Connect {
                                        conn_id: conn_id.clone(),
                                        remote_port: port,
                                    }).await {
                                        error!(error = %e, "Failed to send connect request");
                                        let mut pending = pending_connections.write().await;
                                        pending.remove(&conn_id);
                                        continue;
                                    }

                                    tokio::spawn(handle_remote_connection(
                                        conn_id,
                                        stream,
                                        msg_tx_clone,
                                        remote_connections_clone,
                                        pending_connections_clone,
                                        ready_rx,
                                    ));
                                }
                                Err(e) => {
                                    error!(port = port, error = %e, "Failed to accept connection");
                                }
                            }
                        }
                    }
                }
            }
            Err(e) => {
                error!(
                    client_id = %client_id_clone,
                    port = port,
                    error = %e,
                    "Failed to bind port listener"
                );
            }
        }
    });
}

/// 处理远程连接
async fn handle_remote_connection(
    conn_id: String,
    stream: TcpStream,
    msg_tx: mpsc::Sender<TunnelMessage>,
    remote_connections: Arc<tokio::sync::RwLock<HashMap<String, mpsc::Sender<Vec<u8>>>>>,
    pending_connections: Arc<tokio::sync::RwLock<HashMap<String, oneshot::Sender<()>>>>,
    ready_rx: oneshot::Receiver<()>,
) {
    let (mut read_half, mut write_half) = stream.into_split();

    let (data_tx, mut data_rx) = mpsc::channel::<Vec<u8>>(64);

    {
        let mut conns = remote_connections.write().await;
        conns.insert(conn_id.clone(), data_tx);
    }

    let conn_id_read = conn_id.clone();
    let conn_id_write = conn_id.clone();
    let conn_id_cleanup = conn_id.clone();
    let msg_tx_read = msg_tx.clone();
    let remote_connections_clone = remote_connections.clone();
    let pending_connections_clone = pending_connections.clone();

    // 读取任务
    let read_task = tokio::spawn(async move {
        match ready_rx.await {
            Ok(()) => {
                debug!(conn_id = %conn_id_read, "Received ready signal, starting to read TCP data");
            }
            Err(_) => {
                debug!(conn_id = %conn_id_read, "Ready signal channel closed, aborting read task");
                return;
            }
        }

        let mut buf = vec![0u8; READ_BUFFER_SIZE];
        loop {
            match read_half.read(&mut buf).await {
                Ok(0) => {
                    debug!(conn_id = %conn_id_read, "Remote connection closed");
                    break;
                }
                Ok(n) => {
                    let data = buf[..n].to_vec();
                    if let Err(e) = msg_tx_read
                        .send(TunnelMessage::Data {
                            conn_id: conn_id_read.clone(),
                            data,
                        })
                        .await
                    {
                        error!(error = %e, "Failed to send data");
                        break;
                    }
                }
                Err(e) => {
                    error!(conn_id = %conn_id_read, error = %e, "Error reading from remote connection");
                    break;
                }
            }
        }

        let _ = msg_tx_read
            .send(TunnelMessage::Close {
                conn_id: conn_id_read.clone(),
            })
            .await;

        let mut conns = remote_connections_clone.write().await;
        conns.remove(&conn_id_read);
    });

    // 写入任务
    let write_task = tokio::spawn(async move {
        while let Some(data) = data_rx.recv().await {
            if let Err(e) = write_half.write_all(&data).await {
                error!(conn_id = %conn_id_write, error = %e, "Error writing to remote connection");
                break;
            }
        }
    });

    tokio::select! {
        _ = read_task => {}
        _ = write_task => {}
    }

    {
        let mut pending = pending_connections_clone.write().await;
        pending.remove(&conn_id_cleanup);
    }
}
