//! 隧道客户端
//!
//! 主动连接到服务端，接收连接请求，转发本地服务

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{debug, error, info, warn};

use crate::domain::tunnel::{PortMapping, TunnelMessage};
use crate::state::AppState;

const RECONNECT_DELAY_SECS: u64 = 5;
const PING_INTERVAL_SECS: u64 = 30;
const READ_BUFFER_SIZE: usize = 65536;

/// 启动隧道客户端
pub async fn start(state: Arc<AppState>) {
    let server_url = state.tunnel_server_url.clone();
    let auth_token = state.tunnel_auth_token.clone();
    let mappings = state.tunnel_port_mappings.clone();
    let client_state = state.tunnel_client_state.clone();

    if mappings.is_empty() {
        warn!("No tunnel port mappings configured, tunnel client will not start");
        return;
    }

    info!(
        server_url = %server_url,
        mappings = mappings.len(),
        "Starting tunnel client"
    );

    loop {
        // 重置状态
        *client_state.connected.write().await = false;
        *client_state.connected_at.write().await = None;

        match run_client(&server_url, &auth_token, &mappings, client_state.clone()).await {
            Ok(()) => {
                info!("Tunnel client disconnected normally");
            }
            Err(e) => {
                error!(error = %e, "Tunnel client error");
                *client_state.last_error.write().await = Some(e.to_string());
            }
        }

        // 增加重连计数
        let mut count = client_state.reconnect_count.write().await;
        *count += 1;
        drop(count);

        info!(delay_secs = RECONNECT_DELAY_SECS, "Reconnecting tunnel client");
        tokio::time::sleep(Duration::from_secs(RECONNECT_DELAY_SECS)).await;
    }
}

/// 运行客户端连接
async fn run_client(
    server_url: &str,
    auth_token: &str,
    mappings: &[PortMapping],
    client_state: Arc<crate::state::TunnelClientState>,
) -> anyhow::Result<()> {
    // 构建 WebSocket URL，添加认证 token 作为 header
    let url = url::Url::parse(server_url)?;

    // 创建请求，添加认证 header
    let request = tokio_tungstenite::tungstenite::http::Request::builder()
        .uri(server_url)
        .header("x-tunnel-token", auth_token)
        .header("Host", url.host_str().unwrap_or("localhost"))
        .header("Connection", "Upgrade")
        .header("Upgrade", "websocket")
        .header("Sec-WebSocket-Version", "13")
        .header("Sec-WebSocket-Key", tokio_tungstenite::tungstenite::handshake::client::generate_key())
        .body(())?;

    info!(url = %server_url, "Connecting to tunnel server");

    let (ws_stream, _) = connect_async(request).await?;
    let (mut ws_tx, mut ws_rx) = ws_stream.split();

    info!("Connected to tunnel server");

    // 更新状态
    *client_state.connected.write().await = true;
    *client_state.connected_at.write().await = Some(Utc::now());
    *client_state.last_error.write().await = None;

    // 发送配置
    let config_msg = TunnelMessage::Config {
        mappings: mappings.to_vec(),
    };
    let config_json = serde_json::to_string(&config_msg)?;
    ws_tx.send(Message::Text(config_json)).await?;
    info!(mappings = mappings.len(), "Sent tunnel configuration");

    // 本地连接管理
    let (local_tx, mut local_rx) = mpsc::channel::<TunnelMessage>(256);
    let local_connections: Arc<tokio::sync::RwLock<HashMap<String, mpsc::Sender<Vec<u8>>>>> =
        Arc::new(tokio::sync::RwLock::new(HashMap::new()));

    // 心跳定时器
    let mut ping_interval = tokio::time::interval(Duration::from_secs(PING_INTERVAL_SECS));

    loop {
        tokio::select! {
            // 处理来自服务端的消息
            msg = ws_rx.next() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        match serde_json::from_str::<TunnelMessage>(&text) {
                            Ok(tunnel_msg) => {
                                handle_server_message(
                                    tunnel_msg,
                                    mappings,
                                    local_tx.clone(),
                                    local_connections.clone(),
                                ).await;
                            }
                            Err(e) => {
                                warn!(error = %e, "Failed to parse tunnel message");
                            }
                        }
                    }
                    Some(Ok(Message::Binary(data))) => {
                        // 尝试解析为 TunnelMessage
                        if let Ok(tunnel_msg) = serde_json::from_slice::<TunnelMessage>(&data) {
                            handle_server_message(
                                tunnel_msg,
                                mappings,
                                local_tx.clone(),
                                local_connections.clone(),
                            ).await;
                        }
                    }
                    Some(Ok(Message::Ping(data))) => {
                        let _ = ws_tx.send(Message::Pong(data)).await;
                    }
                    Some(Ok(Message::Pong(_))) => {
                        debug!("Received pong");
                    }
                    Some(Ok(Message::Close(_))) => {
                        info!("Server closed connection");
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
                    _ => {}
                }
            }

            // 处理要发送给服务端的消息
            Some(msg) = local_rx.recv() => {
                let json = serde_json::to_string(&msg)?;
                if let Err(e) = ws_tx.send(Message::Text(json)).await {
                    error!(error = %e, "Failed to send message to server");
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

    // 清理所有本地连接
    let mut conns = local_connections.write().await;
    conns.clear();

    *client_state.connected.write().await = false;

    Ok(())
}

/// 处理来自服务端的消息
async fn handle_server_message(
    msg: TunnelMessage,
    mappings: &[PortMapping],
    ws_tx: mpsc::Sender<TunnelMessage>,
    local_connections: Arc<tokio::sync::RwLock<HashMap<String, mpsc::Sender<Vec<u8>>>>>,
) {
    match msg {
        TunnelMessage::Connect { conn_id, remote_port } => {
            debug!(conn_id = %conn_id, remote_port = remote_port, "Received connect request");

            // 查找对应的端口映射
            let mapping = mappings.iter().find(|m| m.remote_port == remote_port);

            match mapping {
                Some(m) => {
                    let local_addr = format!("{}:{}", m.local_host, m.local_port);
                    let conn_id_clone = conn_id.clone();
                    let ws_tx_clone = ws_tx.clone();
                    let local_connections_clone = local_connections.clone();

                    // 在后台任务中建立本地连接
                    tokio::spawn(async move {
                        match TcpStream::connect(&local_addr).await {
                            Ok(stream) => {
                                info!(conn_id = %conn_id_clone, local_addr = %local_addr, "Connected to local service");

                                // 发送连接成功
                                let _ = ws_tx_clone.send(TunnelMessage::Connected {
                                    conn_id: conn_id_clone.clone(),
                                }).await;

                                // 启动数据转发
                                handle_local_connection(
                                    conn_id_clone,
                                    stream,
                                    ws_tx_clone,
                                    local_connections_clone,
                                ).await;
                            }
                            Err(e) => {
                                error!(conn_id = %conn_id_clone, error = %e, "Failed to connect to local service");
                                let _ = ws_tx_clone.send(TunnelMessage::ConnectFailed {
                                    conn_id: conn_id_clone,
                                    error: e.to_string(),
                                }).await;
                            }
                        }
                    });
                }
                None => {
                    warn!(remote_port = remote_port, "No mapping found for port");
                    let _ = ws_tx.send(TunnelMessage::ConnectFailed {
                        conn_id,
                        error: format!("No mapping for port {}", remote_port),
                    }).await;
                }
            }
        }

        TunnelMessage::Data { conn_id, data } => {
            // 转发数据到本地连接
            let conns = local_connections.read().await;
            if let Some(tx) = conns.get(&conn_id) {
                if let Err(e) = tx.send(data).await {
                    debug!(conn_id = %conn_id, error = %e, "Failed to forward data to local connection");
                }
            } else {
                warn!(conn_id = %conn_id, data_len = data.len(), "Received data for unknown conn_id (connection not ready yet or already closed)");
            }
        }

        TunnelMessage::Close { conn_id } => {
            debug!(conn_id = %conn_id, "Closing local connection");
            let mut conns = local_connections.write().await;
            conns.remove(&conn_id);
        }

        TunnelMessage::Pong => {
            debug!("Received pong");
        }

        _ => {
            debug!(?msg, "Ignoring message");
        }
    }
}

/// 处理本地连接的数据转发
async fn handle_local_connection(
    conn_id: String,
    stream: TcpStream,
    ws_tx: mpsc::Sender<TunnelMessage>,
    local_connections: Arc<tokio::sync::RwLock<HashMap<String, mpsc::Sender<Vec<u8>>>>>,
) {
    let (mut read_half, mut write_half) = stream.into_split();

    // 创建写入通道
    let (data_tx, mut data_rx) = mpsc::channel::<Vec<u8>>(64);

    // 注册连接
    {
        let mut conns = local_connections.write().await;
        conns.insert(conn_id.clone(), data_tx);
    }

    let conn_id_read = conn_id.clone();
    let conn_id_write = conn_id.clone();
    let ws_tx_read = ws_tx.clone();
    let local_connections_clone = local_connections.clone();

    // 读取任务：从本地服务读取数据，发送到 WebSocket
    let read_task = tokio::spawn(async move {
        let mut buf = vec![0u8; READ_BUFFER_SIZE];
        loop {
            match read_half.read(&mut buf).await {
                Ok(0) => {
                    debug!(conn_id = %conn_id_read, "Local connection closed");
                    break;
                }
                Ok(n) => {
                    let data = buf[..n].to_vec();
                    if let Err(e) = ws_tx_read.send(TunnelMessage::Data {
                        conn_id: conn_id_read.clone(),
                        data,
                    }).await {
                        error!(error = %e, "Failed to send data to WebSocket");
                        break;
                    }
                }
                Err(e) => {
                    error!(conn_id = %conn_id_read, error = %e, "Error reading from local connection");
                    break;
                }
            }
        }

        // 发送关闭消息
        let _ = ws_tx_read.send(TunnelMessage::Close {
            conn_id: conn_id_read.clone(),
        }).await;

        // 移除连接
        let mut conns = local_connections_clone.write().await;
        conns.remove(&conn_id_read);
    });

    // 写入任务：从通道接收数据，写入本地服务
    let write_task = tokio::spawn(async move {
        while let Some(data) = data_rx.recv().await {
            if let Err(e) = write_half.write_all(&data).await {
                error!(conn_id = %conn_id_write, error = %e, "Error writing to local connection");
                break;
            }
        }
    });

    // 等待任一任务完成
    tokio::select! {
        _ = read_task => {}
        _ = write_task => {}
    }
}
