//! 端口监听器管理器
//!
//! 将端口监听器的生命周期从客户端 WebSocket 连接中解耦，
//! 作为独立的长期运行资源管理，从根本上消除端口竞争问题。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot, RwLock};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use crate::domain::tunnel::{PortMapping, TunnelMessage};

const READ_BUFFER_SIZE: usize = 65536;
const CONNECTION_TIMEOUT_SECS: u64 = 30;

/// 绑定的客户端信息
struct BoundClient {
    client_id: String,
    ws_tx: mpsc::Sender<TunnelMessage>,
    #[allow(dead_code)]
    bound_at: DateTime<Utc>,
}

/// 活跃连接信息
struct ActiveConnection {
    /// 发送数据到 TCP 的通道
    data_tx: mpsc::Sender<Vec<u8>>,
}

/// 单个端口监听器（长期运行）
pub struct PortListener {
    /// 监听端口
    pub port: u16,
    /// 服务名称
    pub name: String,
    /// 当前绑定的客户端（可为空）
    bound_client: RwLock<Option<BoundClient>>,
    /// 活跃连接 (conn_id -> connection info)
    active_connections: RwLock<HashMap<String, ActiveConnection>>,
    /// 待确认连接 (conn_id -> ready_tx)
    pending_connections: RwLock<HashMap<String, oneshot::Sender<()>>>,
    /// 取消令牌（仅用于完全关闭监听器）
    cancel_token: CancellationToken,
    /// 监听器是否已启动
    started: RwLock<bool>,
}

impl PortListener {
    /// 创建新的端口监听器
    pub fn new(port: u16, name: String) -> Self {
        Self {
            port,
            name,
            bound_client: RwLock::new(None),
            active_connections: RwLock::new(HashMap::new()),
            pending_connections: RwLock::new(HashMap::new()),
            cancel_token: CancellationToken::new(),
            started: RwLock::new(false),
        }
    }

    /// 绑定客户端（自动解绑旧客户端）
    pub async fn bind(&self, client_id: String, ws_tx: mpsc::Sender<TunnelMessage>) {
        let mut bound = self.bound_client.write().await;
        if let Some(old) = bound.take() {
            info!(
                port = self.port,
                old_client = %old.client_id,
                new_client = %client_id,
                "Rebinding port to new client"
            );
        } else {
            info!(
                port = self.port,
                client_id = %client_id,
                "Binding client to port"
            );
        }
        *bound = Some(BoundClient {
            client_id,
            ws_tx,
            bound_at: Utc::now(),
        });
    }

    /// 解绑客户端
    pub async fn unbind(&self, client_id: &str) -> bool {
        let mut bound = self.bound_client.write().await;
        if bound.as_ref().map(|b| b.client_id.as_str()) == Some(client_id) {
            info!(port = self.port, client_id = %client_id, "Unbinding client from port");
            *bound = None;
            true
        } else {
            false
        }
    }

    /// 获取当前绑定的客户端 ID
    pub async fn bound_client_id(&self) -> Option<String> {
        self.bound_client
            .read()
            .await
            .as_ref()
            .map(|b| b.client_id.clone())
    }

    /// 发送消息到绑定的客户端
    async fn send_to_client(&self, msg: TunnelMessage) -> Result<(), &'static str> {
        let bound = self.bound_client.read().await;
        if let Some(client) = bound.as_ref() {
            client
                .ws_tx
                .send(msg)
                .await
                .map_err(|_| "Client channel closed")
        } else {
            Err("No client bound")
        }
    }

    /// 处理来自客户端的消息
    pub async fn handle_client_message(&self, msg: TunnelMessage) {
        match msg {
            TunnelMessage::Connected { conn_id } => {
                debug!(port = self.port, conn_id = %conn_id, "Client connected to local service");
                let mut pending = self.pending_connections.write().await;
                if let Some(tx) = pending.remove(&conn_id) {
                    let _ = tx.send(());
                    debug!(conn_id = %conn_id, "Signaled connection ready");
                }
            }
            TunnelMessage::ConnectFailed { conn_id, error } => {
                warn!(port = self.port, conn_id = %conn_id, error = %error, "Client failed to connect");
                {
                    let mut pending = self.pending_connections.write().await;
                    pending.remove(&conn_id);
                }
                let mut conns = self.active_connections.write().await;
                conns.remove(&conn_id);
            }
            TunnelMessage::Data { conn_id, data } => {
                let conns = self.active_connections.read().await;
                if let Some(conn) = conns.get(&conn_id) {
                    if let Err(e) = conn.data_tx.send(data).await {
                        debug!(conn_id = %conn_id, error = %e, "Failed to forward data");
                    }
                }
            }
            TunnelMessage::Close { conn_id } => {
                debug!(port = self.port, conn_id = %conn_id, "Closing connection");
                let mut conns = self.active_connections.write().await;
                conns.remove(&conn_id);
            }
            _ => {}
        }
    }

    /// 启动监听任务
    pub async fn start(self: Arc<Self>) -> Result<(), std::io::Error> {
        {
            let started = self.started.read().await;
            if *started {
                return Ok(());
            }
        }

        let addr = format!("0.0.0.0:{}", self.port);

        // 尝试绑定端口（带重试）
        let listener = {
            let mut attempts = 0u32;
            loop {
                match TcpListener::bind(&addr).await {
                    Ok(l) => break l,
                    Err(e) if attempts < 5 => {
                        attempts += 1;
                        let delay = 50 * attempts;
                        warn!(
                            port = self.port,
                            attempts,
                            delay_ms = delay,
                            error = %e,
                            "Port binding retry"
                        );
                        tokio::time::sleep(Duration::from_millis(delay as u64)).await;
                    }
                    Err(e) => {
                        error!(port = self.port, error = %e, "Failed to bind port after retries");
                        return Err(e);
                    }
                }
            }
        };

        {
            let mut started = self.started.write().await;
            *started = true;
        }

        info!(port = self.port, name = %self.name, "Port listener started");

        let self_clone = self.clone();
        tokio::spawn(async move {
            self_clone.run_accept_loop(listener).await;
        });

        Ok(())
    }

    /// 接受连接循环
    async fn run_accept_loop(self: Arc<Self>, listener: TcpListener) {
        loop {
            tokio::select! {
                _ = self.cancel_token.cancelled() => {
                    info!(port = self.port, "Port listener shutdown");
                    break;
                }
                result = listener.accept() => {
                    match result {
                        Ok((stream, peer_addr)) => {
                            let conn_id = Uuid::new_v4().to_string();

                            // 检查是否有绑定的客户端
                            let has_client = self.bound_client.read().await.is_some();
                            if !has_client {
                                debug!(
                                    port = self.port,
                                    peer = %peer_addr,
                                    "Connection rejected: no client bound"
                                );
                                // 直接关闭连接
                                drop(stream);
                                continue;
                            }

                            info!(
                                conn_id = %conn_id,
                                port = self.port,
                                peer = %peer_addr,
                                "New connection on port"
                            );

                            let self_clone = self.clone();
                            tokio::spawn(async move {
                                self_clone.handle_connection(conn_id, stream).await;
                            });
                        }
                        Err(e) => {
                            error!(port = self.port, error = %e, "Accept error");
                        }
                    }
                }
            }
        }
    }

    /// 处理单个连接
    async fn handle_connection(self: Arc<Self>, conn_id: String, stream: TcpStream) {
        // 创建数据通道
        let (data_tx, mut data_rx) = mpsc::channel::<Vec<u8>>(64);
        let (ready_tx, ready_rx) = oneshot::channel();

        // 注册连接
        {
            let mut pending = self.pending_connections.write().await;
            pending.insert(conn_id.clone(), ready_tx);
        }

        {
            let mut conns = self.active_connections.write().await;
            conns.insert(conn_id.clone(), ActiveConnection { data_tx });
        }

        // 发送 Connect 请求到客户端
        if let Err(e) = self
            .send_to_client(TunnelMessage::Connect {
                conn_id: conn_id.clone(),
                remote_port: self.port,
            })
            .await
        {
            warn!(conn_id = %conn_id, error = e, "Failed to send connect request");
            self.cleanup_connection(&conn_id).await;
            return;
        }

        // 等待客户端确认连接（带超时）
        match tokio::time::timeout(Duration::from_secs(CONNECTION_TIMEOUT_SECS), ready_rx).await {
            Ok(Ok(())) => {
                debug!(conn_id = %conn_id, "Connection ready, starting data forwarding");
            }
            Ok(Err(_)) => {
                debug!(conn_id = %conn_id, "Ready channel closed");
                self.cleanup_connection(&conn_id).await;
                return;
            }
            Err(_) => {
                warn!(conn_id = %conn_id, "Connection timeout waiting for client");
                self.cleanup_connection(&conn_id).await;
                return;
            }
        }

        // 拆分 TCP 流
        let (mut read_half, mut write_half) = stream.into_split();

        let conn_id_read = conn_id.clone();
        let conn_id_write = conn_id.clone();
        let self_read = self.clone();
        let self_write = self.clone();

        // 读取任务：TCP -> Client
        let read_task = tokio::spawn(async move {
            let mut buf = vec![0u8; READ_BUFFER_SIZE];
            loop {
                match read_half.read(&mut buf).await {
                    Ok(0) => {
                        debug!(conn_id = %conn_id_read, "Remote connection closed");
                        break;
                    }
                    Ok(n) => {
                        let data = buf[..n].to_vec();
                        if let Err(e) = self_read
                            .send_to_client(TunnelMessage::Data {
                                conn_id: conn_id_read.clone(),
                                data,
                            })
                            .await
                        {
                            debug!(conn_id = %conn_id_read, error = e, "Failed to send data to client");
                            break;
                        }
                    }
                    Err(e) => {
                        debug!(conn_id = %conn_id_read, error = %e, "Read error");
                        break;
                    }
                }
            }

            // 通知客户端关闭
            let _ = self_read
                .send_to_client(TunnelMessage::Close {
                    conn_id: conn_id_read.clone(),
                })
                .await;
        });

        // 写入任务：Client -> TCP
        let write_task = tokio::spawn(async move {
            while let Some(data) = data_rx.recv().await {
                if let Err(e) = write_half.write_all(&data).await {
                    debug!(conn_id = %conn_id_write, error = %e, "Write error");
                    break;
                }
            }
        });

        // 等待任一任务结束
        tokio::select! {
            _ = read_task => {}
            _ = write_task => {}
        }

        // 清理连接
        self_write.cleanup_connection(&conn_id).await;
    }

    /// 清理连接资源
    async fn cleanup_connection(&self, conn_id: &str) {
        {
            let mut pending = self.pending_connections.write().await;
            pending.remove(conn_id);
        }
        {
            let mut conns = self.active_connections.write().await;
            conns.remove(conn_id);
        }
    }

    /// 关闭监听器
    pub fn shutdown(&self) {
        self.cancel_token.cancel();
    }

    /// 获取活跃连接数
    pub async fn active_connection_count(&self) -> usize {
        self.active_connections.read().await.len()
    }

    /// 清理所有活跃连接
    ///
    /// 用于 Takeover 时清理旧连接，因为新进程无法处理旧的 conn_id。
    /// 这会导致正在进行的请求中断，客户端需要重试。
    pub async fn clear_all_connections(&self) {
        let mut conns = self.active_connections.write().await;
        let count = conns.len();
        conns.clear();

        let mut pending = self.pending_connections.write().await;
        pending.clear();

        if count > 0 {
            info!(port = self.port, cleared = count, "Cleared all active connections for takeover");
        }
    }

    /// 检查是否已启动
    pub async fn is_started(&self) -> bool {
        *self.started.read().await
    }
}

/// 端口监听器管理器
pub struct PortListenerManager {
    /// 所有监听器 (port -> listener)
    listeners: RwLock<HashMap<u16, Arc<PortListener>>>,
}

impl PortListenerManager {
    /// 创建新的管理器
    pub fn new() -> Self {
        Self {
            listeners: RwLock::new(HashMap::new()),
        }
    }

    /// 确保监听器存在（不存在则创建并启动）
    pub async fn ensure_listener(
        &self,
        port: u16,
        name: &str,
    ) -> Result<Arc<PortListener>, std::io::Error> {
        // 快速路径：已存在
        {
            let listeners = self.listeners.read().await;
            if let Some(listener) = listeners.get(&port) {
                return Ok(listener.clone());
            }
        }

        // 慢速路径：需要创建
        let mut listeners = self.listeners.write().await;

        // 双重检查
        if let Some(listener) = listeners.get(&port) {
            return Ok(listener.clone());
        }

        // 创建并启动新监听器
        let listener = Arc::new(PortListener::new(port, name.to_string()));
        listener.clone().start().await?;
        listeners.insert(port, listener.clone());

        Ok(listener)
    }

    /// 获取指定端口的监听器
    pub async fn get_listener(&self, port: u16) -> Option<Arc<PortListener>> {
        self.listeners.read().await.get(&port).cloned()
    }

    /// 为客户端绑定端口
    pub async fn bind_ports_for_client(
        &self,
        client_id: &str,
        mappings: &[PortMapping],
        ws_tx: mpsc::Sender<TunnelMessage>,
    ) -> Result<(), std::io::Error> {
        for mapping in mappings {
            let listener = self
                .ensure_listener(mapping.remote_port, &mapping.name)
                .await?;
            listener.bind(client_id.to_string(), ws_tx.clone()).await;
        }
        Ok(())
    }

    /// 解绑客户端的所有端口
    pub async fn unbind_client(&self, client_id: &str) {
        let listeners = self.listeners.read().await;
        for listener in listeners.values() {
            listener.unbind(client_id).await;
        }
    }

    /// 获取客户端绑定的端口
    pub async fn get_client_ports(&self, client_id: &str) -> Vec<u16> {
        let listeners = self.listeners.read().await;
        let mut ports = Vec::new();
        for (port, listener) in listeners.iter() {
            if listener.bound_client_id().await.as_deref() == Some(client_id) {
                ports.push(*port);
            }
        }
        ports
    }

    /// 处理来自客户端的消息（路由到对应的监听器）
    pub async fn handle_client_message(&self, client_id: &str, msg: TunnelMessage) {
        // 对于需要路由的消息，先找到对应的监听器
        let port = match &msg {
            TunnelMessage::Connected { .. }
            | TunnelMessage::ConnectFailed { .. }
            | TunnelMessage::Data { .. }
            | TunnelMessage::Close { .. } => {
                // 需要根据 conn_id 找到对应的端口
                // 这里简化处理：广播到所有该客户端绑定的端口
                None
            }
            _ => None,
        };

        if let Some(port) = port {
            if let Some(listener) = self.get_listener(port).await {
                listener.handle_client_message(msg).await;
            }
        } else {
            // 广播到该客户端绑定的所有端口
            let listeners = self.listeners.read().await;
            for listener in listeners.values() {
                if listener.bound_client_id().await.as_deref() == Some(client_id) {
                    listener.handle_client_message(msg.clone()).await;
                }
            }
        }
    }

    /// 获取所有监听器状态
    pub async fn get_all_listeners(&self) -> Vec<(u16, String, Option<String>, usize)> {
        let listeners = self.listeners.read().await;
        let mut result = Vec::new();
        for (port, listener) in listeners.iter() {
            let client_id = listener.bound_client_id().await;
            let conn_count = listener.active_connection_count().await;
            result.push((*port, listener.name.clone(), client_id, conn_count));
        }
        result
    }

    /// 关闭所有监听器
    pub async fn shutdown_all(&self) {
        let listeners = self.listeners.read().await;
        for listener in listeners.values() {
            listener.shutdown();
        }
    }

    /// 热切换：将指定客户端的所有端口绑定切换到新的 WebSocket 连接
    ///
    /// 用于零断线更新：新进程连接后接管旧进程的端口绑定。
    ///
    /// **注意**：切换后，现有活跃连接会被清理（因为新进程没有对应的本地连接）。
    /// 这意味着正在进行的请求会中断，但新请求会无缝使用新进程。
    /// 对于大多数短连接场景（HTTP API），这通常不是问题。
    ///
    /// # Arguments
    /// * `client_id` - 客户端 ID（新旧进程相同）
    /// * `new_ws_tx` - 新进程的 WebSocket 发送通道
    /// * `mappings` - 端口映射配置
    ///
    /// # Returns
    /// * `Ok(switched_count)` - 成功切换的端口数量
    /// * `Err(error)` - 如果有端口未找到
    pub async fn switch_client_binding(
        &self,
        client_id: &str,
        new_ws_tx: mpsc::Sender<TunnelMessage>,
        mappings: &[PortMapping],
    ) -> Result<usize, String> {
        let listeners = self.listeners.read().await;
        let mut switched = 0;
        let mut total_active_conns = 0;

        for mapping in mappings {
            if let Some(listener) = listeners.get(&mapping.remote_port) {
                // 只有当前绑定的是同一个 client_id 才切换
                let current_client = listener.bound_client_id().await;
                let active_conns = listener.active_connection_count().await;
                total_active_conns += active_conns;

                if current_client.as_deref() == Some(client_id) {
                    // 先清理所有活跃连接（因为新进程无法处理旧连接）
                    if active_conns > 0 {
                        info!(
                            port = mapping.remote_port,
                            active_connections = active_conns,
                            "Clearing active connections before takeover"
                        );
                        listener.clear_all_connections().await;
                    }

                    // 原子切换到新的 WebSocket 连接
                    listener
                        .bind(client_id.to_string(), new_ws_tx.clone())
                        .await;
                    info!(
                        port = mapping.remote_port,
                        client_id = %client_id,
                        "Switched port binding to new connection (takeover)"
                    );
                    switched += 1;
                } else {
                    warn!(
                        port = mapping.remote_port,
                        expected_client = %client_id,
                        actual_client = ?current_client,
                        "Port not bound to expected client, binding anyway"
                    );
                    // 即使不是当前客户端，也绑定（可能是首次连接或重连）
                    listener
                        .bind(client_id.to_string(), new_ws_tx.clone())
                        .await;
                    switched += 1;
                }
            } else {
                // 端口监听器不存在，需要创建
                warn!(
                    port = mapping.remote_port,
                    "Port listener not found during takeover, this shouldn't happen"
                );
            }
        }

        if total_active_conns > 0 {
            warn!(
                total_active_connections = total_active_conns,
                "Active connections were cleared during takeover. \
                 These connections will be reset and clients should retry."
            );
        }

        if switched == 0 && !mappings.is_empty() {
            return Err("No ports were switched".to_string());
        }

        Ok(switched)
    }

    /// 检查客户端是否已绑定到指定端口
    pub async fn is_client_bound(&self, client_id: &str, port: u16) -> bool {
        if let Some(listener) = self.get_listener(port).await {
            listener.bound_client_id().await.as_deref() == Some(client_id)
        } else {
            false
        }
    }
}

impl Default for PortListenerManager {
    fn default() -> Self {
        Self::new()
    }
}
