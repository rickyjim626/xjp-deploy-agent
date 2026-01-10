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
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::domain::tunnel::{PortMapping, TunnelMessage};
use crate::state::AppState;

const RECONNECT_DELAY_SECS: u64 = 5;
const PING_INTERVAL_SECS: u64 = 30;
const READ_BUFFER_SIZE: usize = 65536;

/// 客户端运行结果
#[derive(Debug)]
pub enum ClientResult {
    /// 正常断开
    Disconnected,
    /// 收到 shutdown 信号，优雅退出
    Shutdown,
    /// 收到 TakeoverComplete，旧进程应该退出
    TakeoverComplete,
    /// Takeover 模式成功接管
    TakeoverSuccess,
    /// Takeover 模式失败
    TakeoverFailed(String),
}

/// Takeover 模式配置
#[derive(Debug, Clone)]
pub struct TakeoverConfig {
    /// 新进程的 session ID
    pub session_id: String,
    /// 信号文件路径（用于通知旧进程）
    pub signal_file: Option<std::path::PathBuf>,
}

/// 启动隧道客户端
pub async fn start(state: Arc<AppState>, shutdown_token: CancellationToken) {
    let server_url = state.tunnel_server_url.clone();
    let client_id = state.tunnel_client_id.clone();
    let auth_token = state.tunnel_auth_token.clone();
    let mappings = state.tunnel_port_mappings.clone();
    let client_state = state.tunnel_client_state.clone();

    if mappings.is_empty() {
        warn!("No tunnel port mappings configured, tunnel client will not start");
        return;
    }

    info!(
        server_url = %server_url,
        client_id = %client_id,
        mappings = mappings.len(),
        "Starting tunnel client"
    );

    loop {
        // 检查 shutdown 信号
        if shutdown_token.is_cancelled() {
            info!("Tunnel client received shutdown signal before connect");
            break;
        }

        // 重置状态
        *client_state.connected.write().await = false;
        *client_state.connected_at.write().await = None;

        match run_client(
            &server_url,
            &client_id,
            &auth_token,
            &mappings,
            client_state.clone(),
            shutdown_token.clone(),
        )
        .await
        {
            Ok(ClientResult::Disconnected) => {
                info!("Tunnel client disconnected normally");
            }
            Ok(ClientResult::Shutdown) => {
                info!("Tunnel client shutdown gracefully");
                break;
            }
            Ok(ClientResult::TakeoverComplete) => {
                info!("Tunnel client received takeover complete, new process has taken over, exiting");
                // 新进程已接管，旧进程应该退出
                // 注意：这里不 break，而是直接退出进程
                // 因为自动更新场景下，新进程已经在运行了
                std::process::exit(0);
            }
            Ok(result) => {
                info!(?result, "Tunnel client returned unexpected result");
            }
            Err(e) => {
                error!(error = %e, "Tunnel client error");
                *client_state.last_error.write().await = Some(e.to_string());
            }
        }

        // 检查 shutdown 信号
        if shutdown_token.is_cancelled() {
            info!("Tunnel client received shutdown signal, not reconnecting");
            break;
        }

        // 增加重连计数
        let mut count = client_state.reconnect_count.write().await;
        *count += 1;
        drop(count);

        info!(delay_secs = RECONNECT_DELAY_SECS, "Reconnecting tunnel client");

        // 使用 select 等待重连延迟或 shutdown 信号
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(RECONNECT_DELAY_SECS)) => {}
            _ = shutdown_token.cancelled() => {
                info!("Tunnel client received shutdown signal during reconnect delay");
                break;
            }
        }
    }

    info!("Tunnel client stopped");
}

/// 启动隧道客户端（Takeover 模式）
///
/// 用于零断线更新：新进程连接后发送 Takeover 请求，
/// 等待 Server 切换端口绑定后开始工作。
pub async fn start_takeover(
    state: Arc<AppState>,
    takeover_config: TakeoverConfig,
    shutdown_token: CancellationToken,
) -> ClientResult {
    let server_url = state.tunnel_server_url.clone();
    let client_id = state.tunnel_client_id.clone();
    let auth_token = state.tunnel_auth_token.clone();
    let mappings = state.tunnel_port_mappings.clone();
    let client_state = state.tunnel_client_state.clone();

    if mappings.is_empty() {
        warn!("No tunnel port mappings configured, takeover cannot proceed");
        return ClientResult::TakeoverFailed("No port mappings".to_string());
    }

    info!(
        server_url = %server_url,
        client_id = %client_id,
        session_id = %takeover_config.session_id,
        "Starting tunnel client in takeover mode"
    );

    // 运行 takeover 客户端
    match run_client_takeover(
        &server_url,
        &client_id,
        &auth_token,
        &mappings,
        client_state.clone(),
        shutdown_token.clone(),
        &takeover_config,
    )
    .await
    {
        Ok(result) => {
            match &result {
                ClientResult::TakeoverSuccess => {
                    info!("Takeover successful, writing signal file");
                    // 写入信号文件通知旧进程
                    if let Some(ref signal_path) = takeover_config.signal_file {
                        if let Err(e) = tokio::fs::write(signal_path, "ready").await {
                            warn!(error = %e, "Failed to write takeover signal file");
                        }
                    }
                }
                ClientResult::TakeoverFailed(e) => {
                    error!(error = %e, "Takeover failed");
                }
                _ => {}
            }
            result
        }
        Err(e) => {
            error!(error = %e, "Takeover connection error");
            ClientResult::TakeoverFailed(e.to_string())
        }
    }
}

/// 运行 Takeover 模式的客户端连接
async fn run_client_takeover(
    server_url: &str,
    client_id: &str,
    auth_token: &str,
    mappings: &[PortMapping],
    client_state: Arc<crate::state::TunnelClientState>,
    shutdown_token: CancellationToken,
    takeover_config: &TakeoverConfig,
) -> anyhow::Result<ClientResult> {
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
        .header(
            "Sec-WebSocket-Key",
            tokio_tungstenite::tungstenite::handshake::client::generate_key(),
        )
        .body(())?;

    info!(url = %server_url, "Connecting to tunnel server (takeover mode)");

    let (ws_stream, _) = connect_async(request).await?;
    let (mut ws_tx, mut ws_rx) = ws_stream.split();

    info!("Connected to tunnel server, sending takeover request");

    // 发送 Takeover 请求
    let takeover_msg = TunnelMessage::Takeover {
        client_id: client_id.to_string(),
        mappings: mappings.to_vec(),
        new_session_id: takeover_config.session_id.clone(),
    };
    let takeover_json = serde_json::to_string(&takeover_msg)?;
    ws_tx.send(Message::Text(takeover_json)).await?;
    info!(
        client_id = %client_id,
        session_id = %takeover_config.session_id,
        "Sent takeover request, waiting for TakeoverReady"
    );

    // 等待 TakeoverReady 响应（带超时）
    let takeover_timeout = Duration::from_secs(30);
    let takeover_result = tokio::time::timeout(takeover_timeout, async {
        while let Some(msg) = ws_rx.next().await {
            match msg {
                Ok(Message::Text(text)) => {
                    if let Ok(tunnel_msg) = serde_json::from_str::<TunnelMessage>(&text) {
                        match tunnel_msg {
                            TunnelMessage::TakeoverReady { session_id } => {
                                if session_id == takeover_config.session_id {
                                    info!(session_id = %session_id, "Received TakeoverReady");
                                    return Ok(true);
                                }
                            }
                            TunnelMessage::TakeoverFailed { error } => {
                                error!(error = %error, "Received TakeoverFailed");
                                return Err(error);
                            }
                            _ => {
                                debug!(?tunnel_msg, "Ignoring message while waiting for TakeoverReady");
                            }
                        }
                    }
                }
                Ok(Message::Ping(data)) => {
                    let _ = ws_tx.send(Message::Pong(data)).await;
                }
                Ok(Message::Close(_)) => {
                    return Err("Server closed connection".to_string());
                }
                Err(e) => {
                    return Err(format!("WebSocket error: {}", e));
                }
                _ => {}
            }
        }
        Err("WebSocket stream ended".to_string())
    })
    .await;

    match takeover_result {
        Ok(Ok(true)) => {
            // Takeover 成功，更新状态
            *client_state.connected.write().await = true;
            *client_state.connected_at.write().await = Some(Utc::now());
            *client_state.last_error.write().await = None;

            info!("Takeover successful, starting normal operation");

            // 继续正常的客户端循环
            let (local_tx, mut local_rx) = mpsc::channel::<TunnelMessage>(256);
            let local_connections: Arc<tokio::sync::RwLock<HashMap<String, mpsc::Sender<Vec<u8>>>>> =
                Arc::new(tokio::sync::RwLock::new(HashMap::new()));
            let mut ping_interval = tokio::time::interval(Duration::from_secs(PING_INTERVAL_SECS));
            let mut shutdown_triggered = false;

            loop {
                tokio::select! {
                    _ = shutdown_token.cancelled() => {
                        info!("Tunnel client received shutdown signal");
                        shutdown_triggered = true;
                        let _ = ws_tx.send(Message::Close(None)).await;
                        break;
                    }

                    msg = ws_rx.next() => {
                        match msg {
                            Some(Ok(Message::Text(text))) => {
                                if let Ok(tunnel_msg) = serde_json::from_str::<TunnelMessage>(&text) {
                                    handle_server_message(
                                        tunnel_msg,
                                        mappings,
                                        local_tx.clone(),
                                        local_connections.clone(),
                                    ).await;
                                }
                            }
                            Some(Ok(Message::Binary(data))) => {
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

                    Some(msg) = local_rx.recv() => {
                        let json = serde_json::to_string(&msg)?;
                        if let Err(e) = ws_tx.send(Message::Text(json)).await {
                            error!(error = %e, "Failed to send message to server");
                            break;
                        }
                    }

                    _ = ping_interval.tick() => {
                        let ping_msg = serde_json::to_string(&TunnelMessage::Ping)?;
                        if let Err(e) = ws_tx.send(Message::Text(ping_msg)).await {
                            error!(error = %e, "Failed to send ping");
                            break;
                        }
                    }
                }
            }

            // 清理
            let mut conns = local_connections.write().await;
            conns.clear();
            *client_state.connected.write().await = false;

            if shutdown_triggered {
                Ok(ClientResult::Shutdown)
            } else {
                Ok(ClientResult::TakeoverSuccess)
            }
        }
        Ok(Ok(false)) => {
            // 这个分支理论上不会发生，但编译器需要处理所有情况
            warn!("Unexpected Ok(false) from takeover result");
            Ok(ClientResult::TakeoverFailed("Unexpected takeover result".to_string()))
        }
        Ok(Err(e)) => Ok(ClientResult::TakeoverFailed(e)),
        Err(_) => Ok(ClientResult::TakeoverFailed("Takeover timeout".to_string())),
    }
}

/// 运行客户端连接
async fn run_client(
    server_url: &str,
    client_id: &str,
    auth_token: &str,
    mappings: &[PortMapping],
    client_state: Arc<crate::state::TunnelClientState>,
    shutdown_token: CancellationToken,
) -> anyhow::Result<ClientResult> {
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

    // 发送配置（包含客户端 ID）
    let config_msg = TunnelMessage::Config {
        client_id: client_id.to_string(),
        mappings: mappings.to_vec(),
    };
    let config_json = serde_json::to_string(&config_msg)?;
    ws_tx.send(Message::Text(config_json)).await?;
    info!(client_id = %client_id, mappings = mappings.len(), "Sent tunnel configuration");

    // 本地连接管理
    let (local_tx, mut local_rx) = mpsc::channel::<TunnelMessage>(256);
    let local_connections: Arc<tokio::sync::RwLock<HashMap<String, mpsc::Sender<Vec<u8>>>>> =
        Arc::new(tokio::sync::RwLock::new(HashMap::new()));

    // 心跳定时器
    let mut ping_interval = tokio::time::interval(Duration::from_secs(PING_INTERVAL_SECS));

    // 是否因 shutdown 退出
    let mut shutdown_triggered = false;

    loop {
        tokio::select! {
            // 优先处理 shutdown 信号
            _ = shutdown_token.cancelled() => {
                info!("Tunnel client received shutdown signal, closing connection gracefully");
                shutdown_triggered = true;
                // 发送 WebSocket Close 帧
                if let Err(e) = ws_tx.send(Message::Close(None)).await {
                    warn!(error = %e, "Failed to send WebSocket close frame");
                }
                break;
            }

            // 处理来自服务端的消息
            msg = ws_rx.next() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        match serde_json::from_str::<TunnelMessage>(&text) {
                            Ok(tunnel_msg) => {
                                // 检查是否收到 TakeoverComplete（新进程已接管）
                                if let TunnelMessage::TakeoverComplete { new_session_id } = &tunnel_msg {
                                    info!(
                                        new_session_id = %new_session_id,
                                        "Received TakeoverComplete, new process has taken over"
                                    );
                                    // 发送 Close 帧并退出
                                    let _ = ws_tx.send(Message::Close(None)).await;
                                    // 清理
                                    let mut conns = local_connections.write().await;
                                    conns.clear();
                                    *client_state.connected.write().await = false;
                                    return Ok(ClientResult::TakeoverComplete);
                                }

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

    if shutdown_triggered {
        Ok(ClientResult::Shutdown)
    } else {
        Ok(ClientResult::Disconnected)
    }
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

                                // 先注册连接，再发送 Connected
                                // 这样可以避免服务端发送数据时客户端还没注册连接的竞态条件
                                let (mut read_half, mut write_half) = stream.into_split();
                                let (data_tx, mut data_rx) = mpsc::channel::<Vec<u8>>(64);

                                // 先注册连接
                                {
                                    let mut conns = local_connections_clone.write().await;
                                    conns.insert(conn_id_clone.clone(), data_tx);
                                }

                                // 再发送连接成功
                                let _ = ws_tx_clone.send(TunnelMessage::Connected {
                                    conn_id: conn_id_clone.clone(),
                                }).await;

                                // 启动数据转发
                                let conn_id_read = conn_id_clone.clone();
                                let conn_id_write = conn_id_clone.clone();
                                let ws_tx_read = ws_tx_clone.clone();
                                let local_connections_read = local_connections_clone.clone();

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
                                    let mut conns = local_connections_read.write().await;
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

        TunnelMessage::HttpRequest {
            request_id,
            method,
            path,
            headers,
            body,
        } => {
            debug!(request_id = %request_id, method = %method, path = %path, "Received HTTP proxy request");

            // 查找 deploy-agent 映射的本地端口，默认 9876
            let local_port = mappings
                .iter()
                .find(|m| m.name == "deploy-agent")
                .map(|m| m.local_port)
                .unwrap_or(9876);

            let ws_tx_clone = ws_tx.clone();
            tokio::spawn(async move {
                // 代理到本地 deploy-agent API
                let local_url = format!("http://127.0.0.1:{}{}", local_port, path);

                let client = reqwest::Client::new();
                let method = match method.as_str() {
                    "GET" => reqwest::Method::GET,
                    "POST" => reqwest::Method::POST,
                    "PUT" => reqwest::Method::PUT,
                    "DELETE" => reqwest::Method::DELETE,
                    "PATCH" => reqwest::Method::PATCH,
                    "HEAD" => reqwest::Method::HEAD,
                    "OPTIONS" => reqwest::Method::OPTIONS,
                    _ => reqwest::Method::GET,
                };

                let mut request = client.request(method, &local_url);

                // 添加 headers
                for (name, value) in headers {
                    request = request.header(&name, &value);
                }

                // 添加 body
                if let Some(data) = body {
                    request = request.body(data);
                }

                // 发送请求
                match request.send().await {
                    Ok(response) => {
                        let status = response.status().as_u16();
                        let response_headers: Vec<(String, String)> = response
                            .headers()
                            .iter()
                            .filter_map(|(name, value)| {
                                value
                                    .to_str()
                                    .ok()
                                    .map(|v| (name.as_str().to_string(), v.to_string()))
                            })
                            .collect();

                        // 读取 body
                        match response.bytes().await {
                            Ok(bytes) => {
                                let body_data = if bytes.is_empty() {
                                    None
                                } else {
                                    Some(bytes.to_vec())
                                };

                                let _ = ws_tx_clone
                                    .send(TunnelMessage::HttpResponse {
                                        request_id,
                                        status,
                                        headers: response_headers,
                                        body: body_data,
                                    })
                                    .await;
                            }
                            Err(e) => {
                                error!(error = %e, "Failed to read response body");
                                let _ = ws_tx_clone
                                    .send(TunnelMessage::HttpError {
                                        request_id,
                                        error: e.to_string(),
                                    })
                                    .await;
                            }
                        }
                    }
                    Err(e) => {
                        error!(error = %e, local_url = %local_url, "Failed to make local HTTP request");
                        let _ = ws_tx_clone
                            .send(TunnelMessage::HttpError {
                                request_id,
                                error: e.to_string(),
                            })
                            .await;
                    }
                }
            });
        }

        _ => {
            debug!(?msg, "Ignoring message");
        }
    }
}

