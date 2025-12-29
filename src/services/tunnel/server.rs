//! 隧道服务端
//!
//! 接收客户端连接，监听远程端口，转发数据到客户端

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket};
use chrono::Utc;
use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use crate::domain::tunnel::{PortMapping, TunnelMessage};
use crate::state::AppState;

const READ_BUFFER_SIZE: usize = 65536;
const PING_INTERVAL_SECS: u64 = 30;

/// 处理隧道服务端连接
pub async fn handle_connection(
    socket: WebSocket,
    state: Arc<AppState>,
    client_addr: String,
) {
    info!(client_addr = %client_addr, "Tunnel client connected");

    let server_state = &state.tunnel_server_state;

    // 更新状态
    *server_state.client_connected.write().await = true;
    *server_state.client_addr.write().await = Some(client_addr.clone());
    *server_state.client_connected_at.write().await = Some(Utc::now());

    if let Err(e) = run_server(socket, state.clone()).await {
        error!(error = %e, "Tunnel server error");
    }

    // 清理状态
    *server_state.client_connected.write().await = false;
    *server_state.client_addr.write().await = None;

    // 停止所有端口监听
    let mut listeners = server_state.port_listeners.write().await;
    for (port, cancel_token) in listeners.drain() {
        info!(port = port, "Stopping port listener");
        cancel_token.cancel();
    }

    // 清理映射
    server_state.client_mappings.write().await.clear();

    info!(client_addr = %client_addr, "Tunnel client disconnected");
}

/// 运行服务端逻辑
async fn run_server(
    socket: WebSocket,
    state: Arc<AppState>,
) -> anyhow::Result<()> {
    let (mut ws_tx, mut ws_rx) = socket.split();
    let _server_state = &state.tunnel_server_state;

    // 用于接收要发送的消息
    let (msg_tx, mut msg_rx) = mpsc::channel::<TunnelMessage>(256);

    // 远程连接管理 (conn_id -> 发送数据到 TCP 的通道)
    let remote_connections: Arc<tokio::sync::RwLock<HashMap<String, mpsc::Sender<Vec<u8>>>>> =
        Arc::new(tokio::sync::RwLock::new(HashMap::new()));

    // 等待连接确认的 pending connections (conn_id -> oneshot sender)
    // 当客户端发送 Connected 时，通过这个 channel 通知服务端可以开始读取数据
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
                                handle_client_message(
                                    tunnel_msg,
                                    state.clone(),
                                    msg_tx.clone(),
                                    remote_connections.clone(),
                                    pending_connections.clone(),
                                ).await;
                            }
                            Err(e) => {
                                warn!(error = %e, "Failed to parse tunnel message");
                            }
                        }
                    }
                    Some(Ok(Message::Binary(data))) => {
                        if let Ok(tunnel_msg) = serde_json::from_slice::<TunnelMessage>(&data) {
                            handle_client_message(
                                tunnel_msg,
                                state.clone(),
                                msg_tx.clone(),
                                remote_connections.clone(),
                                pending_connections.clone(),
                            ).await;
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

    // 清理所有远程连接
    let mut conns = remote_connections.write().await;
    conns.clear();

    Ok(())
}

/// 处理来自客户端的消息
async fn handle_client_message(
    msg: TunnelMessage,
    state: Arc<AppState>,
    msg_tx: mpsc::Sender<TunnelMessage>,
    remote_connections: Arc<tokio::sync::RwLock<HashMap<String, mpsc::Sender<Vec<u8>>>>>,
    pending_connections: Arc<tokio::sync::RwLock<HashMap<String, oneshot::Sender<()>>>>,
) {
    let server_state = &state.tunnel_server_state;

    match msg {
        TunnelMessage::Config { mappings } => {
            info!(mappings = mappings.len(), "Received client configuration");

            // 保存映射配置
            *server_state.client_mappings.write().await = mappings.clone();

            // 为每个映射启动 TCP 监听器
            for mapping in mappings {
                start_port_listener(
                    mapping,
                    state.clone(),
                    msg_tx.clone(),
                    remote_connections.clone(),
                    pending_connections.clone(),
                ).await;
            }
        }

        TunnelMessage::Connected { conn_id } => {
            debug!(conn_id = %conn_id, "Client connected to local service");
            // 通知 handle_remote_connection 可以开始读取数据了
            let mut pending = pending_connections.write().await;
            if let Some(tx) = pending.remove(&conn_id) {
                let _ = tx.send(());
                debug!(conn_id = %conn_id, "Signaled connection ready to forward data");
            } else {
                warn!(conn_id = %conn_id, "No pending connection found for Connected message");
            }
        }

        TunnelMessage::ConnectFailed { conn_id, error } => {
            warn!(conn_id = %conn_id, error = %error, "Client failed to connect to local service");
            // 清理 pending 连接
            {
                let mut pending = pending_connections.write().await;
                pending.remove(&conn_id);
            }
            // 清理远程连接
            let mut conns = remote_connections.write().await;
            conns.remove(&conn_id);
        }

        TunnelMessage::Data { conn_id, data } => {
            // 转发数据到远程连接
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

        // Server 不应该收到这些消息
        TunnelMessage::Connect { .. } => {
            warn!("Unexpected Connect message from client");
        }
    }
}

/// 启动端口监听器
async fn start_port_listener(
    mapping: PortMapping,
    state: Arc<AppState>,
    msg_tx: mpsc::Sender<TunnelMessage>,
    remote_connections: Arc<tokio::sync::RwLock<HashMap<String, mpsc::Sender<Vec<u8>>>>>,
    pending_connections: Arc<tokio::sync::RwLock<HashMap<String, oneshot::Sender<()>>>>,
) {
    let port = mapping.remote_port;
    let addr = format!("0.0.0.0:{}", port);

    let cancel_token = CancellationToken::new();

    // 记录监听器
    {
        let mut listeners = state.tunnel_server_state.port_listeners.write().await;
        listeners.insert(port, cancel_token.clone());
    }

    let cancel_token_clone = cancel_token.clone();

    tokio::spawn(async move {
        match TcpListener::bind(&addr).await {
            Ok(listener) => {
                info!(port = port, name = %mapping.name, "Started port listener");

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
                                        port = port,
                                        peer = %peer_addr,
                                        "New connection on tunnel port"
                                    );

                                    // 创建 oneshot channel 来等待客户端连接确认
                                    let (ready_tx, ready_rx) = oneshot::channel();

                                    // 注册 pending connection
                                    {
                                        let mut pending = pending_connections.write().await;
                                        pending.insert(conn_id.clone(), ready_tx);
                                    }

                                    // 发送连接请求到客户端
                                    let msg_tx_clone = msg_tx.clone();
                                    let remote_connections_clone = remote_connections.clone();
                                    let pending_connections_clone = pending_connections.clone();

                                    if let Err(e) = msg_tx.send(TunnelMessage::Connect {
                                        conn_id: conn_id.clone(),
                                        remote_port: port,
                                    }).await {
                                        error!(error = %e, "Failed to send connect request");
                                        // 清理 pending connection
                                        let mut pending = pending_connections.write().await;
                                        pending.remove(&conn_id);
                                        continue;
                                    }

                                    // 启动连接处理（会等待 ready_rx 信号后才开始读取数据）
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
                error!(port = port, error = %e, "Failed to bind port listener");
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

    // 创建写入通道
    let (data_tx, mut data_rx) = mpsc::channel::<Vec<u8>>(64);

    // 注册连接（用于接收从客户端返回的数据）
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

    // 读取任务：等待客户端连接确认后，从 TCP 连接读取数据，发送到 WebSocket
    let read_task = tokio::spawn(async move {
        // 等待客户端连接本地服务成功
        match ready_rx.await {
            Ok(()) => {
                debug!(conn_id = %conn_id_read, "Received ready signal, starting to read TCP data");
            }
            Err(_) => {
                // ready_tx 被 drop 了，说明连接失败或被取消
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
                    if let Err(e) = msg_tx_read.send(TunnelMessage::Data {
                        conn_id: conn_id_read.clone(),
                        data,
                    }).await {
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

        // 发送关闭消息
        let _ = msg_tx_read.send(TunnelMessage::Close {
            conn_id: conn_id_read.clone(),
        }).await;

        // 移除连接
        let mut conns = remote_connections_clone.write().await;
        conns.remove(&conn_id_read);
    });

    // 写入任务：从通道接收数据，写入 TCP 连接
    let write_task = tokio::spawn(async move {
        while let Some(data) = data_rx.recv().await {
            if let Err(e) = write_half.write_all(&data).await {
                error!(conn_id = %conn_id_write, error = %e, "Error writing to remote connection");
                break;
            }
        }
    });

    // 等待任一任务完成
    tokio::select! {
        _ = read_task => {}
        _ = write_task => {}
    }

    // 清理 pending connection（如果还存在）
    {
        let mut pending = pending_connections_clone.write().await;
        pending.remove(&conn_id_cleanup);
    }
}
