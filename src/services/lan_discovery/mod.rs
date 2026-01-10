//! LAN 自动发现服务
//!
//! 使用 UDP 广播在局域网内发现其他 Agent，实现：
//! - 定期广播自己的存在
//! - 接收并验证其他 Agent 的广播
//! - 维护发现的 peer 列表
//! - 为 ServiceRouter 提供 LAN 端点信息

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::UdpSocket;
use tokio::sync::RwLock;
use tracing::{debug, error, info, warn};

/// 广播端口
pub const BROADCAST_PORT: u16 = 19998;
/// 广播魔数（用于识别我们的包）
pub const BROADCAST_MAGIC: &[u8; 4] = b"XJPA";
/// 广播间隔
pub const BROADCAST_INTERVAL: Duration = Duration::from_secs(30);
/// Peer 超时时间（超过此时间未收到广播则认为离线）
pub const PEER_TIMEOUT: Duration = Duration::from_secs(120);
/// 健康检查超时
pub const HEALTH_CHECK_TIMEOUT: Duration = Duration::from_secs(5);

/// LAN 发现配置
#[derive(Debug, Clone)]
pub struct LanDiscoveryConfig {
    /// 是否启用
    pub enabled: bool,
    /// 广播端口
    pub broadcast_port: u16,
    /// Agent ID
    pub agent_id: String,
    /// API 端口
    pub api_port: u16,
    /// 本地提供的服务
    pub local_services: Vec<LocalService>,
    /// 能力列表
    pub capabilities: Vec<String>,
}

impl LanDiscoveryConfig {
    /// 从环境变量加载配置
    pub fn from_env() -> Option<Self> {
        let enabled = std::env::var("LAN_DISCOVERY_ENABLED")
            .map(|v| v == "true" || v == "1")
            .unwrap_or(false);

        if !enabled {
            return None;
        }

        let broadcast_port = std::env::var("LAN_BROADCAST_PORT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(BROADCAST_PORT);

        let agent_id = std::env::var("TUNNEL_CLIENT_ID")
            .or_else(|_| std::env::var("MESH_AGENT_ID"))
            .unwrap_or_else(|_| {
                hostname::get()
                    .map(|h| h.to_string_lossy().to_string())
                    .unwrap_or_else(|_| "unknown".to_string())
            });

        let api_port = std::env::var("PORT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(9876);

        // 解析本地服务配置
        // 格式: service_id:port:protocol,service_id:port:protocol,...
        let local_services = std::env::var("LAN_LOCAL_SERVICES")
            .or_else(|_| std::env::var("LOCAL_SERVICES"))
            .map(|s| {
                s.split(',')
                    .filter_map(|item| {
                        let parts: Vec<&str> = item.split(':').collect();
                        if parts.len() >= 2 {
                            Some(LocalService {
                                service_id: parts[0].to_string(),
                                port: parts[1].parse().unwrap_or(0),
                                protocol: parts.get(2).map(|s| s.to_string()).unwrap_or_else(|| "http".to_string()),
                                health_path: parts.get(3).map(|s| s.to_string()),
                            })
                        } else {
                            None
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();

        // 能力列表
        let capabilities = std::env::var("LAN_CAPABILITIES")
            .map(|s| s.split(',').map(|s| s.trim().to_string()).collect())
            .unwrap_or_default();

        Some(Self {
            enabled,
            broadcast_port,
            agent_id,
            api_port,
            local_services,
            capabilities,
        })
    }
}

/// 本地服务定义
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalService {
    pub service_id: String,
    pub port: u16,
    pub protocol: String,
    pub health_path: Option<String>,
}

/// 发现包
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoveryPacket {
    /// 魔数（用于快速过滤）
    pub magic: [u8; 4],
    /// 协议版本
    pub version: u8,
    /// Agent ID
    pub agent_id: String,
    /// LAN IP
    pub lan_ip: String,
    /// API 端口
    pub api_port: u16,
    /// 能力列表
    pub capabilities: Vec<String>,
    /// 提供的服务
    pub services: HashMap<String, LocalService>,
    /// 时间戳（Unix 秒）
    pub timestamp: i64,
}

/// 发现的 Peer
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoveredPeer {
    /// Agent ID
    pub agent_id: String,
    /// LAN IP
    pub lan_ip: String,
    /// API 端口
    pub api_port: u16,
    /// 能力列表
    pub capabilities: Vec<String>,
    /// 提供的服务
    pub services: HashMap<String, LocalService>,
    /// 发现时间
    pub discovered_at: chrono::DateTime<chrono::Utc>,
    /// 最后一次看到的时间
    pub last_seen: chrono::DateTime<chrono::Utc>,
    /// 是否健康（通过 HTTP 健康检查验证）
    pub healthy: bool,
    /// 延迟（毫秒）
    pub latency_ms: Option<f64>,
}

impl DiscoveredPeer {
    /// 获取服务 URL
    pub fn get_service_url(&self, service_id: &str) -> Option<String> {
        self.services.get(service_id).map(|svc| {
            format!("{}://{}:{}", svc.protocol, self.lan_ip, svc.port)
        })
    }

    /// 获取 Agent API URL
    pub fn api_url(&self) -> String {
        format!("http://{}:{}", self.lan_ip, self.api_port)
    }
}

/// LAN 发现服务
pub struct LanDiscovery {
    config: LanDiscoveryConfig,
    /// 发现的 peers
    discovered_peers: Arc<RwLock<HashMap<String, DiscoveredPeer>>>,
    /// 本地 IP
    local_ip: Option<String>,
}

impl LanDiscovery {
    /// 创建新的 LAN 发现服务
    pub fn new(config: LanDiscoveryConfig) -> Self {
        let local_ip = detect_local_ip();

        Self {
            config,
            discovered_peers: Arc::new(RwLock::new(HashMap::new())),
            local_ip,
        }
    }

    /// 启动发现服务
    pub async fn start(self: Arc<Self>) -> anyhow::Result<()> {
        if !self.config.enabled {
            info!("LAN discovery is disabled");
            return Ok(());
        }

        info!(
            agent_id = %self.config.agent_id,
            broadcast_port = self.config.broadcast_port,
            local_ip = ?self.local_ip,
            services = ?self.config.local_services.iter().map(|s| &s.service_id).collect::<Vec<_>>(),
            "Starting LAN discovery service"
        );

        // 创建 UDP socket
        let socket = match UdpSocket::bind(format!("0.0.0.0:{}", self.config.broadcast_port)).await {
            Ok(s) => {
                // 启用广播
                s.set_broadcast(true)?;
                Arc::new(s)
            }
            Err(e) => {
                error!(error = %e, port = self.config.broadcast_port, "Failed to bind UDP socket for LAN discovery");
                return Err(e.into());
            }
        };

        // 启动广播任务
        let self_clone = self.clone();
        let socket_clone = socket.clone();
        tokio::spawn(async move {
            self_clone.broadcast_loop(socket_clone).await;
        });

        // 启动接收任务
        let self_clone = self.clone();
        let socket_clone = socket.clone();
        tokio::spawn(async move {
            self_clone.receive_loop(socket_clone).await;
        });

        // 启动清理任务
        let self_clone = self.clone();
        tokio::spawn(async move {
            self_clone.cleanup_loop().await;
        });

        Ok(())
    }

    /// 广播循环
    async fn broadcast_loop(&self, socket: Arc<UdpSocket>) {
        let broadcast_addr = SocketAddr::new(
            IpAddr::V4(Ipv4Addr::BROADCAST),
            self.config.broadcast_port,
        );

        loop {
            // 构建发现包
            let packet = self.build_discovery_packet();

            match bincode::serialize(&packet) {
                Ok(data) => {
                    if let Err(e) = socket.send_to(&data, broadcast_addr).await {
                        warn!(error = %e, "Failed to send broadcast packet");
                    } else {
                        debug!(
                            agent_id = %self.config.agent_id,
                            services = packet.services.len(),
                            "Sent discovery broadcast"
                        );
                    }
                }
                Err(e) => {
                    warn!(error = %e, "Failed to serialize discovery packet");
                }
            }

            tokio::time::sleep(BROADCAST_INTERVAL).await;
        }
    }

    /// 接收循环
    async fn receive_loop(&self, socket: Arc<UdpSocket>) {
        let mut buf = [0u8; 4096];

        loop {
            match socket.recv_from(&mut buf).await {
                Ok((len, src)) => {
                    // 快速检查魔数
                    if len < 4 || &buf[..4] != BROADCAST_MAGIC {
                        continue;
                    }

                    // 反序列化
                    match bincode::deserialize::<DiscoveryPacket>(&buf[..len]) {
                        Ok(packet) => {
                            // 忽略自己的广播
                            if packet.agent_id == self.config.agent_id {
                                continue;
                            }

                            // 处理发现的 peer
                            self.handle_discovery(packet, src).await;
                        }
                        Err(e) => {
                            debug!(error = %e, src = %src, "Failed to deserialize discovery packet");
                        }
                    }
                }
                Err(e) => {
                    warn!(error = %e, "Error receiving broadcast");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        }
    }

    /// 清理循环（移除超时的 peers）
    async fn cleanup_loop(&self) {
        let mut interval = tokio::time::interval(Duration::from_secs(30));

        loop {
            interval.tick().await;

            let now = chrono::Utc::now();
            let mut peers = self.discovered_peers.write().await;

            let before_count = peers.len();
            peers.retain(|agent_id, peer| {
                let age = now - peer.last_seen;
                if age > chrono::Duration::from_std(PEER_TIMEOUT).unwrap_or(chrono::Duration::seconds(120)) {
                    info!(agent_id = %agent_id, "Removing stale peer (timeout)");
                    false
                } else {
                    true
                }
            });

            let removed = before_count - peers.len();
            if removed > 0 {
                info!(removed = removed, remaining = peers.len(), "Cleaned up stale peers");
            }
        }
    }

    /// 处理发现的 peer
    async fn handle_discovery(&self, packet: DiscoveryPacket, _src: SocketAddr) {
        let now = chrono::Utc::now();

        // 验证 peer 可达性
        let (healthy, latency_ms) = self.verify_peer(&packet.lan_ip, packet.api_port).await;

        let peer = DiscoveredPeer {
            agent_id: packet.agent_id.clone(),
            lan_ip: packet.lan_ip.clone(),
            api_port: packet.api_port,
            capabilities: packet.capabilities.clone(),
            services: packet.services.clone(),
            discovered_at: now,
            last_seen: now,
            healthy,
            latency_ms,
        };

        let mut peers = self.discovered_peers.write().await;
        let is_new = !peers.contains_key(&packet.agent_id);

        if is_new {
            info!(
                agent_id = %packet.agent_id,
                lan_ip = %packet.lan_ip,
                capabilities = ?packet.capabilities,
                services = ?packet.services.keys().collect::<Vec<_>>(),
                healthy = healthy,
                "Discovered new LAN peer"
            );
        } else {
            debug!(
                agent_id = %packet.agent_id,
                healthy = healthy,
                latency_ms = ?latency_ms,
                "Updated existing peer"
            );
        }

        // 更新或插入
        if let Some(existing) = peers.get_mut(&packet.agent_id) {
            existing.last_seen = now;
            existing.healthy = healthy;
            existing.latency_ms = latency_ms;
            existing.services = packet.services;
            existing.capabilities = packet.capabilities;
        } else {
            peers.insert(packet.agent_id.clone(), peer);
        }
    }

    /// 验证 peer 可达性
    async fn verify_peer(&self, ip: &str, port: u16) -> (bool, Option<f64>) {
        let url = format!("http://{}:{}/health", ip, port);
        let start = std::time::Instant::now();

        let client = reqwest::Client::builder()
            .timeout(HEALTH_CHECK_TIMEOUT)
            .build()
            .unwrap_or_default();

        match client.get(&url).send().await {
            Ok(response) => {
                let latency = start.elapsed().as_secs_f64() * 1000.0;
                let healthy = response.status().is_success();
                (healthy, Some(latency))
            }
            Err(e) => {
                debug!(error = %e, url = %url, "Peer health check failed");
                (false, None)
            }
        }
    }

    /// 构建发现包
    fn build_discovery_packet(&self) -> DiscoveryPacket {
        let services: HashMap<String, LocalService> = self
            .config
            .local_services
            .iter()
            .map(|s| (s.service_id.clone(), s.clone()))
            .collect();

        DiscoveryPacket {
            magic: *BROADCAST_MAGIC,
            version: 1,
            agent_id: self.config.agent_id.clone(),
            lan_ip: self.local_ip.clone().unwrap_or_else(|| "0.0.0.0".to_string()),
            api_port: self.config.api_port,
            capabilities: self.config.capabilities.clone(),
            services,
            timestamp: chrono::Utc::now().timestamp(),
        }
    }

    // ========== 公共 API ==========

    /// 获取所有发现的 peers
    pub async fn get_peers(&self) -> Vec<DiscoveredPeer> {
        let peers = self.discovered_peers.read().await;
        peers.values().cloned().collect()
    }

    /// 获取健康的 peers
    pub async fn get_healthy_peers(&self) -> Vec<DiscoveredPeer> {
        let peers = self.discovered_peers.read().await;
        peers.values().filter(|p| p.healthy).cloned().collect()
    }

    /// 获取提供特定服务的 peer
    pub async fn get_service_provider(&self, service_id: &str) -> Option<DiscoveredPeer> {
        let peers = self.discovered_peers.read().await;
        peers
            .values()
            .filter(|p| p.healthy && p.services.contains_key(service_id))
            .min_by(|a, b| {
                // 优先选择延迟低的
                a.latency_ms
                    .unwrap_or(f64::MAX)
                    .partial_cmp(&b.latency_ms.unwrap_or(f64::MAX))
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .cloned()
    }

    /// 获取服务的 LAN URL（如果可用）
    pub async fn get_service_url(&self, service_id: &str) -> Option<String> {
        self.get_service_provider(service_id)
            .await
            .and_then(|peer| peer.get_service_url(service_id))
    }

    /// 获取统计信息
    pub async fn get_stats(&self) -> LanDiscoveryStats {
        let peers = self.discovered_peers.read().await;
        let healthy_count = peers.values().filter(|p| p.healthy).count();
        let total_services: usize = peers.values().map(|p| p.services.len()).sum();

        LanDiscoveryStats {
            enabled: self.config.enabled,
            local_ip: self.local_ip.clone(),
            agent_id: self.config.agent_id.clone(),
            broadcast_port: self.config.broadcast_port,
            total_peers: peers.len(),
            healthy_peers: healthy_count,
            total_services,
            local_services: self.config.local_services.len(),
        }
    }

    /// 检查是否启用
    pub fn is_enabled(&self) -> bool {
        self.config.enabled
    }
}

/// LAN 发现统计
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LanDiscoveryStats {
    pub enabled: bool,
    pub local_ip: Option<String>,
    pub agent_id: String,
    pub broadcast_port: u16,
    pub total_peers: usize,
    pub healthy_peers: usize,
    pub total_services: usize,
    pub local_services: usize,
}

/// 检测本机 LAN IP
fn detect_local_ip() -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        if let Ok(output) = std::process::Command::new("hostname").arg("-I").output() {
            if output.status.success() {
                let stdout = String::from_utf8_lossy(&output.stdout);
                if let Some(ip) = stdout.split_whitespace().next() {
                    if is_private_ip(ip) {
                        return Some(ip.to_string());
                    }
                }
            }
        }
    }

    #[cfg(target_os = "windows")]
    {
        if let Ok(output) = std::process::Command::new("ipconfig").output() {
            if output.status.success() {
                let stdout = String::from_utf8_lossy(&output.stdout);
                for line in stdout.lines() {
                    if line.contains("IPv4") {
                        if let Some(ip) = line.split(':').nth(1) {
                            let ip = ip.trim();
                            if is_private_ip(ip) {
                                return Some(ip.to_string());
                            }
                        }
                    }
                }
            }
        }
    }

    #[cfg(target_os = "macos")]
    {
        if let Ok(output) = std::process::Command::new("ifconfig").output() {
            if output.status.success() {
                let stdout = String::from_utf8_lossy(&output.stdout);
                for line in stdout.lines() {
                    if line.contains("inet ") && !line.contains("127.0.0.1") {
                        if let Some(ip) = line.split_whitespace().nth(1) {
                            if is_private_ip(ip) {
                                return Some(ip.to_string());
                            }
                        }
                    }
                }
            }
        }
    }

    None
}

/// 检查是否为私有 IP
fn is_private_ip(ip: &str) -> bool {
    let parts: Vec<u8> = ip.split('.').filter_map(|s| s.parse().ok()).collect();

    if parts.len() != 4 {
        return false;
    }

    // 10.0.0.0/8
    if parts[0] == 10 {
        return true;
    }

    // 172.16.0.0/12
    if parts[0] == 172 && (16..=31).contains(&parts[1]) {
        return true;
    }

    // 192.168.0.0/16
    if parts[0] == 192 && parts[1] == 168 {
        return true;
    }

    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_private_ip() {
        assert!(is_private_ip("192.168.1.1"));
        assert!(is_private_ip("10.0.0.1"));
        assert!(is_private_ip("172.16.0.1"));
        assert!(!is_private_ip("8.8.8.8"));
    }

    #[test]
    fn test_discovery_packet_serialization() {
        let packet = DiscoveryPacket {
            magic: *BROADCAST_MAGIC,
            version: 1,
            agent_id: "test".to_string(),
            lan_ip: "192.168.1.100".to_string(),
            api_port: 9876,
            capabilities: vec!["docker".to_string()],
            services: HashMap::new(),
            timestamp: 1234567890,
        };

        let serialized = bincode::serialize(&packet).unwrap();
        let deserialized: DiscoveryPacket = bincode::deserialize(&serialized).unwrap();

        assert_eq!(packet.agent_id, deserialized.agent_id);
        assert_eq!(packet.lan_ip, deserialized.lan_ip);
    }
}
