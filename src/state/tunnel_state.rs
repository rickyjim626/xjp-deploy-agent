//! 隧道运行时状态
//!
//! 包含 tokio 同步原语的运行时状态，与 domain/tunnel.rs 的纯数据类型分离。
//! 注意：端口监听器现在由 PortListenerManager 独立管理，不再与客户端生命周期耦合。

use axum::response::Response;
use chrono::{DateTime, Utc};
use std::collections::HashMap;
use tokio::sync::{mpsc, oneshot, RwLock};

use crate::domain::tunnel::{PortMapping, TunnelMessage};

/// 单个客户端的状态 (Server 视角)
///
/// 仅管理客户端的 WebSocket 连接信息，端口监听器由 PortListenerManager 独立管理。
pub struct ClientState {
    /// 客户端 ID
    pub client_id: String,
    /// 客户端地址
    pub client_addr: String,
    /// 连接时间
    pub connected_at: DateTime<Utc>,
    /// 客户端的端口映射配置
    pub mappings: Vec<PortMapping>,
    /// 发送消息到 WebSocket 客户端的通道
    pub ws_tx: mpsc::Sender<TunnelMessage>,
}

impl ClientState {
    pub fn new(
        client_id: String,
        client_addr: String,
        ws_tx: mpsc::Sender<TunnelMessage>,
    ) -> Self {
        Self {
            client_id,
            client_addr,
            connected_at: Utc::now(),
            mappings: Vec::new(),
            ws_tx,
        }
    }
}

/// 隧道状态 (Server 模式) - 支持多客户端
pub struct TunnelServerState {
    /// 已连接的客户端 (client_id -> ClientState)
    pub clients: RwLock<HashMap<String, ClientState>>,
    /// 端口到客户端的映射 (remote_port -> client_id)
    /// 用于快速查找某个端口属于哪个客户端
    pub port_to_client: RwLock<HashMap<u16, String>>,
    /// 等待 HTTP 代理响应的通道 (request_id -> response sender)
    pub pending_http_requests: RwLock<HashMap<String, oneshot::Sender<Response>>>,
}

impl TunnelServerState {
    pub fn new() -> Self {
        Self {
            clients: RwLock::new(HashMap::new()),
            port_to_client: RwLock::new(HashMap::new()),
            pending_http_requests: RwLock::new(HashMap::new()),
        }
    }

    /// 添加客户端
    pub async fn add_client(&self, client_state: ClientState) {
        let client_id = client_state.client_id.clone();
        let mut clients = self.clients.write().await;
        clients.insert(client_id, client_state);
    }

    /// 移除客户端
    ///
    /// 注意：端口监听器由 PortListenerManager 管理，这里只清理客户端状态。
    /// 调用者需要单独调用 PortListenerManager::unbind_client() 来解绑端口。
    pub async fn remove_client(&self, client_id: &str) -> Option<ClientState> {
        let mut clients = self.clients.write().await;
        if let Some(client) = clients.remove(client_id) {
            // 从端口映射中移除
            let mut port_to_client = self.port_to_client.write().await;
            port_to_client.retain(|_, cid| cid != client_id);

            Some(client)
        } else {
            None
        }
    }

    /// 获取客户端数量
    pub async fn client_count(&self) -> usize {
        self.clients.read().await.len()
    }

    /// 检查客户端是否存在
    pub async fn has_client(&self, client_id: &str) -> bool {
        self.clients.read().await.contains_key(client_id)
    }

    /// 注册端口到客户端的映射
    pub async fn register_port(&self, port: u16, client_id: String) {
        let mut port_to_client = self.port_to_client.write().await;
        port_to_client.insert(port, client_id);
    }

    /// 根据端口查找客户端 ID
    pub async fn get_client_by_port(&self, port: u16) -> Option<String> {
        let port_to_client = self.port_to_client.read().await;
        port_to_client.get(&port).cloned()
    }
}

impl Default for TunnelServerState {
    fn default() -> Self {
        Self::new()
    }
}

/// 隧道状态 (Client 模式)
pub struct TunnelClientState {
    /// 是否已连接
    pub connected: RwLock<bool>,
    /// 连接时间
    pub connected_at: RwLock<Option<DateTime<Utc>>>,
    /// 最后错误
    pub last_error: RwLock<Option<String>>,
    /// 重连次数
    pub reconnect_count: RwLock<u32>,
    /// 活跃的本地连接 (conn_id -> 发送通道)
    pub local_connections: RwLock<HashMap<String, mpsc::Sender<Vec<u8>>>>,
}

impl TunnelClientState {
    pub fn new() -> Self {
        Self {
            connected: RwLock::new(false),
            connected_at: RwLock::new(None),
            last_error: RwLock::new(None),
            reconnect_count: RwLock::new(0),
            local_connections: RwLock::new(HashMap::new()),
        }
    }
}

impl Default for TunnelClientState {
    fn default() -> Self {
        Self::new()
    }
}
