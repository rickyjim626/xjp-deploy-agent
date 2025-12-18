//! 隧道运行时状态
//!
//! 包含 tokio 同步原语的运行时状态，与 domain/tunnel.rs 的纯数据类型分离

use chrono::{DateTime, Utc};
use std::collections::HashMap;
use tokio::sync::{mpsc, RwLock};
use tokio_util::sync::CancellationToken;

use crate::domain::tunnel::{PortMapping, TunnelMessage};

/// 隧道状态 (Server 模式)
pub struct TunnelServerState {
    /// 客户端 WebSocket 连接是否活跃
    pub client_connected: RwLock<bool>,
    /// 客户端地址
    pub client_addr: RwLock<Option<String>>,
    /// 客户端连接时间
    pub client_connected_at: RwLock<Option<DateTime<Utc>>>,
    /// 客户端的端口映射配置
    pub client_mappings: RwLock<Vec<PortMapping>>,
    /// 活跃的代理连接 (conn_id -> 发送通道)
    pub proxy_connections: RwLock<HashMap<String, mpsc::Sender<Vec<u8>>>>,
    /// 发送消息到 WebSocket 客户端的通道
    pub ws_tx: RwLock<Option<mpsc::Sender<TunnelMessage>>>,
    /// 端口监听器取消令牌 (port -> cancel_token)
    pub port_listeners: RwLock<HashMap<u16, CancellationToken>>,
}

impl TunnelServerState {
    pub fn new() -> Self {
        Self {
            client_connected: RwLock::new(false),
            client_addr: RwLock::new(None),
            client_connected_at: RwLock::new(None),
            client_mappings: RwLock::new(Vec::new()),
            proxy_connections: RwLock::new(HashMap::new()),
            ws_tx: RwLock::new(None),
            port_listeners: RwLock::new(HashMap::new()),
        }
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
