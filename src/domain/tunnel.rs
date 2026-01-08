//! 隧道 (FRP) 相关领域模型
//!
//! 纯数据类型，无 tokio/axum 依赖

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

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

    /// 从配置字符串解析端口映射列表
    /// 格式: "name:remote_port:local_host:local_port,..."
    /// 例如: "minio:19000:localhost:9000,postgres:15432:localhost:5432"
    pub fn parse_mappings(mappings_str: &str) -> Vec<Self> {
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
    /// 客户端发送端口映射配置 (包含客户端标识)
    Config {
        client_id: String,
        mappings: Vec<PortMapping>,
    },
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
    /// HTTP 代理请求 (Server -> Client)
    HttpRequest {
        request_id: String,
        method: String,
        path: String,
        headers: Vec<(String, String)>,
        body: Option<Vec<u8>>,
    },
    /// HTTP 代理响应 (Client -> Server)
    HttpResponse {
        request_id: String,
        status: u16,
        headers: Vec<(String, String)>,
        body: Option<Vec<u8>>,
    },
    /// HTTP 代理错误 (Client -> Server)
    HttpError {
        request_id: String,
        error: String,
    },
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

/// 单个客户端连接信息
#[derive(Clone, Debug, Serialize)]
pub struct ConnectedClientInfo {
    pub client_id: String,
    pub client_addr: String,
    pub connected_at: DateTime<Utc>,
    pub port_mappings: Vec<PortMappingStatus>,
}

/// 服务端状态
#[derive(Clone, Debug, Serialize)]
pub struct TunnelServerStatus {
    pub listening: bool,
    pub listen_port: u16,
    /// 已连接的客户端数量
    pub client_count: usize,
    /// 已连接的客户端列表
    pub clients: Vec<ConnectedClientInfo>,
    /// 向后兼容：是否有客户端连接
    pub client_connected: bool,
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
        let mappings =
            PortMapping::parse_mappings("minio:19000:localhost:9000,postgres:15432:localhost:5432");
        assert_eq!(mappings.len(), 2);
        assert_eq!(mappings[0].name, "minio");
        assert_eq!(mappings[0].remote_port, 19000);
        assert_eq!(mappings[0].local_port, 9000);
        assert_eq!(mappings[1].name, "postgres");
    }

    #[test]
    fn test_port_mapping_parse_empty() {
        let mappings = PortMapping::parse_mappings("");
        assert!(mappings.is_empty());
    }
}
