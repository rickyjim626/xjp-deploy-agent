//! 隧道 (FRP) 相关领域模型

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tokio::sync::{mpsc, RwLock};
use tokio_util::sync::CancellationToken;

/// 隧道运行模式
#[derive(Clone, Debug, PartialEq)]
pub enum TunnelMode {
    /// 服务端模式 (ECS): 接收隧道连接，暴露远程服务
    Server,
    /// 客户端模式 (私有云): 主动连接服务端，转发本地服务
    Client,
    /// 禁用隧道
    Disabled,
}

impl TunnelMode {
    /// 从字符串解析
    pub fn from_str(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "server" => TunnelMode::Server,
            "client" => TunnelMode::Client,
            _ => TunnelMode::Disabled,
        }
    }
}

impl Default for TunnelMode {
    fn default() -> Self {
        TunnelMode::Disabled
    }
}

/// 端口映射配置
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PortMapping {
    /// 远程端口 (在 ECS 上暴露)
    pub remote_port: u16,
    /// 本地主机
    pub local_host: String,
    /// 本地端口
    pub local_port: u16,
    /// 服务名称 (如 "minio", "postgres")
    pub name: String,
}

impl PortMapping {
    /// 创建新的端口映射
    pub fn new(name: &str, remote_port: u16, local_host: &str, local_port: u16) -> Self {
        Self {
            name: name.to_string(),
            remote_port,
            local_host: local_host.to_string(),
            local_port,
        }
    }

    /// 从环境变量字符串解析端口映射列表
    /// 格式: "name:remote_port:local_host:local_port,..."
    /// 例如: "minio:19000:localhost:9000,postgres:15432:localhost:5432"
    pub fn parse_from_env(mappings_str: &str) -> Vec<Self> {
        mappings_str
            .split(',')
            .filter_map(|s| {
                let parts: Vec<&str> = s.trim().split(':').collect();
                if parts.len() == 4 {
                    Some(PortMapping {
                        name: parts[0].to_string(),
                        remote_port: parts[1].parse().ok()?,
                        local_host: parts[2].to_string(),
                        local_port: parts[3].parse().ok()?,
                    })
                } else {
                    None
                }
            })
            .collect()
    }
}

/// 隧道协议消息
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum TunnelMessage {
    /// 客户端发送端口映射配置
    Config { mappings: Vec<PortMapping> },
    /// 新连接请求 (Server -> Client)
    Connect { conn_id: String, remote_port: u16 },
    /// 连接已建立 (Client -> Server)
    Connected { conn_id: String },
    /// 连接失败 (Client -> Server)
    ConnectFailed { conn_id: String, error: String },
    /// 数据传输
    Data { conn_id: String, data: Vec<u8> },
    /// 连接关闭
    Close { conn_id: String },
    /// 心跳
    Ping,
    Pong,
}

/// 隧道连接信息 (用于 API 响应)
#[derive(Clone, Debug, Serialize)]
pub struct TunnelConnectionInfo {
    pub id: String,
    pub connected_at: DateTime<Utc>,
    pub remote_addr: String,
    pub mappings: Vec<PortMapping>,
    pub bytes_sent: u64,
    pub bytes_received: u64,
}

/// 端口映射状态信息
#[derive(Clone, Debug, Serialize)]
pub struct PortMappingStatus {
    pub mapping: PortMapping,
    pub status: String, // "active", "listening", "error"
    pub active_connections: usize,
}

/// 客户端连接状态
#[derive(Clone, Debug, Serialize)]
pub struct TunnelClientStatus {
    pub connected: bool,
    pub server_url: String,
    pub connected_at: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
    pub reconnect_count: u32,
    pub mappings: Vec<PortMappingStatus>,
}

/// 服务端状态
#[derive(Clone, Debug, Serialize)]
pub struct TunnelServerStatus {
    pub listening: bool,
    pub listen_port: u16,
    pub client_connected: bool,
    pub client_addr: Option<String>,
    pub client_connected_at: Option<DateTime<Utc>>,
    pub port_mappings: Vec<PortMappingStatus>,
}

/// 隧道状态响应
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "mode")]
pub enum TunnelStatusResponse {
    #[serde(rename = "server")]
    Server(TunnelServerStatus),
    #[serde(rename = "client")]
    Client(TunnelClientStatus),
    #[serde(rename = "disabled")]
    Disabled,
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tunnel_mode_from_str() {
        assert_eq!(TunnelMode::from_str("server"), TunnelMode::Server);
        assert_eq!(TunnelMode::from_str("SERVER"), TunnelMode::Server);
        assert_eq!(TunnelMode::from_str("client"), TunnelMode::Client);
        assert_eq!(TunnelMode::from_str("disabled"), TunnelMode::Disabled);
        assert_eq!(TunnelMode::from_str("unknown"), TunnelMode::Disabled);
    }

    #[test]
    fn test_port_mapping_parse() {
        let mappings = PortMapping::parse_from_env("minio:19000:localhost:9000,postgres:15432:localhost:5432");
        assert_eq!(mappings.len(), 2);
        assert_eq!(mappings[0].name, "minio");
        assert_eq!(mappings[0].remote_port, 19000);
        assert_eq!(mappings[0].local_port, 9000);
        assert_eq!(mappings[1].name, "postgres");
    }

    #[test]
    fn test_port_mapping_parse_empty() {
        let mappings = PortMapping::parse_from_env("");
        assert!(mappings.is_empty());
    }
}
