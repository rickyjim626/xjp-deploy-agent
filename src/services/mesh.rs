//! Service Mesh 客户端
//!
//! 负责与 Deploy Center 的 Mesh API 通信，实现：
//! - Agent 注册与心跳
//! - 服务端点解析
//! - 网络位置检测

use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tracing::{debug, error, info, warn};

/// Mesh 客户端配置
#[derive(Debug, Clone)]
pub struct MeshConfig {
    /// Deploy Center URL
    pub registry_url: String,
    /// API Key
    pub api_key: String,
    /// Agent ID
    pub agent_id: String,
    /// Agent 名称
    pub agent_name: Option<String>,
    /// 本地提供的服务
    pub local_services: Vec<LocalServiceConfig>,
    /// 心跳间隔
    pub heartbeat_interval: Duration,
    /// 是否启用
    pub enabled: bool,
}

impl MeshConfig {
    /// 从环境变量加载配置
    pub fn from_env() -> Option<Self> {
        let enabled = std::env::var("MESH_ENABLED")
            .map(|v| v == "true" || v == "1")
            .unwrap_or(false);

        if !enabled {
            return None;
        }

        let registry_url = std::env::var("MESH_REGISTRY_URL")
            .or_else(|_| std::env::var("DEPLOY_CENTER_CALLBACK_URL"))
            .ok()?;

        let api_key = std::env::var("MESH_API_KEY")
            .or_else(|_| std::env::var("DEPLOY_AGENT_API_KEY"))
            .unwrap_or_default();

        let agent_id = std::env::var("MESH_AGENT_ID")
            .or_else(|_| std::env::var("TUNNEL_CLIENT_ID"))
            .unwrap_or_else(|_| {
                hostname::get()
                    .map(|h| h.to_string_lossy().to_string())
                    .unwrap_or_else(|_| "unknown".to_string())
            });

        let agent_name = std::env::var("MESH_AGENT_NAME").ok();

        // 解析本地服务配置
        // 格式: service_id:local_url,service_id:local_url,...
        let local_services = std::env::var("MESH_LOCAL_SERVICES")
            .map(|s| {
                s.split(',')
                    .filter_map(|item| {
                        let parts: Vec<&str> = item.split(':').collect();
                        if parts.len() >= 2 {
                            Some(LocalServiceConfig {
                                service_id: parts[0].to_string(),
                                local_url: parts[1..].join(":"),
                                health_check_url: None,
                            })
                        } else {
                            None
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();

        let heartbeat_interval = std::env::var("MESH_HEARTBEAT_INTERVAL_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .map(Duration::from_secs)
            .unwrap_or(Duration::from_secs(60));

        Some(Self {
            registry_url,
            api_key,
            agent_id,
            agent_name,
            local_services,
            heartbeat_interval,
            enabled,
        })
    }
}

/// 本地服务配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalServiceConfig {
    pub service_id: String,
    pub local_url: String,
    pub health_check_url: Option<String>,
}

/// 解析后的端点
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolvedEndpoint {
    pub primary: EndpointInfo,
    pub fallbacks: Vec<EndpointInfo>,
    pub resolution_reason: String,
    pub resolution_time_ms: f64,
}

/// 端点信息
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EndpointInfo {
    pub endpoint_id: String,
    pub url: String,
    pub zone_id: String,
    pub zone_name: String,
    pub zone_type: String,
    pub protocol: String,
    pub healthy: bool,
    pub latency_ms: Option<f64>,
    pub affinity_score: i32,
}

/// Agent 注册请求
#[derive(Debug, Serialize)]
struct AgentRegistration {
    agent_id: String,
    agent_name: Option<String>,
    lan_ip: Option<String>,
    wan_ip: Option<String>,
    api_port: Option<i32>,
    capabilities: Vec<String>,
    local_services: Vec<LocalServiceInfo>,
}

/// 本地服务信息
#[derive(Debug, Serialize)]
struct LocalServiceInfo {
    service_id: String,
    local_url: String,
    health_check_url: Option<String>,
}

/// Agent 注册响应
#[derive(Debug, Deserialize)]
struct RegistrationResponse {
    #[allow(dead_code)]
    agent: serde_json::Value,
    detected_zone: Option<String>,
    message: String,
}

/// 心跳请求
#[derive(Debug, Serialize)]
struct HeartbeatRequest {
    agent_id: String,
    healthy: bool,
    detected_zones: Option<Vec<String>>,
}

/// Mesh 客户端
pub struct MeshClient {
    config: MeshConfig,
    http_client: Client,
    /// 端点缓存
    cache: Arc<RwLock<HashMap<String, CachedEndpoint>>>,
    /// 检测到的网络区域
    detected_zone: Arc<RwLock<Option<String>>>,
}

/// 缓存的端点
struct CachedEndpoint {
    endpoint: ResolvedEndpoint,
    cached_at: std::time::Instant,
}

impl MeshClient {
    /// 创建新的 Mesh 客户端
    pub fn new(config: MeshConfig) -> Self {
        let http_client = Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .unwrap_or_default();

        Self {
            config,
            http_client,
            cache: Arc::new(RwLock::new(HashMap::new())),
            detected_zone: Arc::new(RwLock::new(None)),
        }
    }

    /// 启动 Mesh 客户端（注册并启动心跳）
    pub async fn start(&self) -> anyhow::Result<()> {
        if !self.config.enabled {
            debug!("Mesh client is disabled");
            return Ok(());
        }

        info!(
            agent_id = %self.config.agent_id,
            registry_url = %self.config.registry_url,
            "Starting mesh client"
        );

        // 检测网络信息
        let network_info = self.detect_network_info().await;

        // 注册 Agent
        match self.register(&network_info).await {
            Ok(zone) => {
                info!(
                    agent_id = %self.config.agent_id,
                    detected_zone = ?zone,
                    "Mesh registration successful"
                );
                *self.detected_zone.write().await = zone;
            }
            Err(e) => {
                error!(error = %e, "Mesh registration failed");
                return Err(e);
            }
        }

        Ok(())
    }

    /// 启动心跳任务
    pub fn start_heartbeat(self: Arc<Self>) {
        if !self.config.enabled {
            return;
        }

        let interval = self.config.heartbeat_interval;

        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);

            loop {
                ticker.tick().await;

                if let Err(e) = self.send_heartbeat().await {
                    warn!(error = %e, "Mesh heartbeat failed");
                }
            }
        });
    }

    /// 检测网络信息
    async fn detect_network_info(&self) -> NetworkInfo {
        // 获取局域网 IP
        let lan_ip = get_local_ip();

        // 获取公网 IP（可选）
        let wan_ip = self.get_public_ip().await;

        // 获取能力列表
        let capabilities = self.detect_capabilities();

        NetworkInfo {
            lan_ip,
            wan_ip,
            capabilities,
        }
    }

    /// 检测 Agent 能力
    fn detect_capabilities(&self) -> Vec<String> {
        let mut caps = Vec::new();

        // 检查本地服务配置
        for svc in &self.config.local_services {
            if !caps.contains(&svc.service_id) {
                caps.push(svc.service_id.clone());
            }
        }

        // 检查 Docker
        if std::process::Command::new("docker")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
        {
            caps.push("docker".to_string());
        }

        caps
    }

    /// 获取公网 IP
    async fn get_public_ip(&self) -> Option<String> {
        self.http_client
            .get("https://api.ipify.org")
            .timeout(Duration::from_secs(5))
            .send()
            .await
            .ok()?
            .text()
            .await
            .ok()
    }

    /// 注册 Agent
    async fn register(&self, network_info: &NetworkInfo) -> anyhow::Result<Option<String>> {
        let local_services: Vec<LocalServiceInfo> = self
            .config
            .local_services
            .iter()
            .map(|s| LocalServiceInfo {
                service_id: s.service_id.clone(),
                local_url: s.local_url.clone(),
                health_check_url: s.health_check_url.clone(),
            })
            .collect();

        let registration = AgentRegistration {
            agent_id: self.config.agent_id.clone(),
            agent_name: self.config.agent_name.clone(),
            lan_ip: network_info.lan_ip.clone(),
            wan_ip: network_info.wan_ip.clone(),
            api_port: Some(
                std::env::var("PORT")
                    .ok()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(9876),
            ),
            capabilities: network_info.capabilities.clone(),
            local_services,
        };

        let response = self
            .http_client
            .post(format!("{}/mesh/agents/register", self.config.registry_url))
            .header("x-api-key", &self.config.api_key)
            .json(&registration)
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("Registration failed: {} - {}", status, body);
        }

        let result: RegistrationResponse = response.json().await?;
        info!(message = %result.message, "Agent registered");

        Ok(result.detected_zone)
    }

    /// 发送心跳
    async fn send_heartbeat(&self) -> anyhow::Result<()> {
        let request = HeartbeatRequest {
            agent_id: self.config.agent_id.clone(),
            healthy: true,
            detected_zones: None,
        };

        let response = self
            .http_client
            .post(format!("{}/mesh/agents/heartbeat", self.config.registry_url))
            .header("x-api-key", &self.config.api_key)
            .json(&request)
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("Heartbeat failed: {} - {}", status, body);
        }

        debug!(agent_id = %self.config.agent_id, "Heartbeat sent");
        Ok(())
    }

    /// 解析服务端点
    pub async fn resolve(&self, service_id: &str) -> anyhow::Result<ResolvedEndpoint> {
        // 检查缓存
        {
            let cache = self.cache.read().await;
            if let Some(cached) = cache.get(service_id) {
                // 缓存 60 秒有效
                if cached.cached_at.elapsed() < Duration::from_secs(60) {
                    return Ok(cached.endpoint.clone());
                }
            }
        }

        // 从 Registry 获取
        let detected_zone = self.detected_zone.read().await.clone();

        let mut url = format!("{}/mesh/resolve/{}", self.config.registry_url, service_id);

        // 添加查询参数
        let mut params = Vec::new();
        params.push(format!("agent_id={}", self.config.agent_id));
        if let Some(ref zone) = detected_zone {
            params.push(format!("zone_id={}", zone));
        }
        if !params.is_empty() {
            url = format!("{}?{}", url, params.join("&"));
        }

        let response = self
            .http_client
            .get(&url)
            .header("x-api-key", &self.config.api_key)
            .header("X-Agent-Id", &self.config.agent_id)
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("Resolve failed for {}: {} - {}", service_id, status, body);
        }

        let endpoint: ResolvedEndpoint = response.json().await?;

        // 更新缓存
        {
            let mut cache = self.cache.write().await;
            cache.insert(
                service_id.to_string(),
                CachedEndpoint {
                    endpoint: endpoint.clone(),
                    cached_at: std::time::Instant::now(),
                },
            );
        }

        info!(
            service_id = service_id,
            url = %endpoint.primary.url,
            zone = %endpoint.primary.zone_name,
            reason = %endpoint.resolution_reason,
            "Resolved service endpoint"
        );

        Ok(endpoint)
    }

    /// 获取服务 URL（便捷方法）
    pub async fn get_service_url(&self, service_id: &str) -> anyhow::Result<String> {
        let endpoint = self.resolve(service_id).await?;
        Ok(endpoint.primary.url)
    }

    /// 尝试获取服务 URL，失败时返回 fallback
    pub async fn get_service_url_or(&self, service_id: &str, fallback: &str) -> String {
        match self.get_service_url(service_id).await {
            Ok(url) => url,
            Err(e) => {
                warn!(
                    service_id = service_id,
                    error = %e,
                    fallback = fallback,
                    "Failed to resolve service, using fallback"
                );
                fallback.to_string()
            }
        }
    }

    /// 清除缓存
    pub async fn clear_cache(&self) {
        let mut cache = self.cache.write().await;
        cache.clear();
    }

    /// 检查是否启用
    pub fn is_enabled(&self) -> bool {
        self.config.enabled
    }

    /// 获取 Agent ID
    pub fn agent_id(&self) -> &str {
        &self.config.agent_id
    }

    /// 获取检测到的区域
    pub async fn detected_zone(&self) -> Option<String> {
        self.detected_zone.read().await.clone()
    }
}

/// 网络信息
struct NetworkInfo {
    lan_ip: Option<String>,
    wan_ip: Option<String>,
    capabilities: Vec<String>,
}

/// 获取本机局域网 IP
fn get_local_ip() -> Option<String> {
    // 尝试使用 local-ip-address crate 的逻辑
    #[cfg(target_os = "linux")]
    {
        if let Ok(output) = std::process::Command::new("hostname")
            .arg("-I")
            .output()
        {
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
        if let Ok(output) = std::process::Command::new("ipconfig")
            .output()
        {
            if output.status.success() {
                let stdout = String::from_utf8_lossy(&output.stdout);
                for line in stdout.lines() {
                    if line.contains("IPv4") || line.contains("IPv4 Address") {
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
        if let Ok(output) = std::process::Command::new("ifconfig")
            .output()
        {
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
    let parts: Vec<u8> = ip
        .split('.')
        .filter_map(|s| s.parse().ok())
        .collect();

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
        assert!(is_private_ip("192.168.0.1"));
        assert!(is_private_ip("10.0.0.1"));
        assert!(is_private_ip("10.255.255.255"));
        assert!(is_private_ip("172.16.0.1"));
        assert!(is_private_ip("172.31.255.255"));

        assert!(!is_private_ip("8.8.8.8"));
        assert!(!is_private_ip("172.32.0.1"));
        assert!(!is_private_ip("192.169.0.1"));
    }

    #[test]
    fn test_get_local_ip() {
        // 在大多数环境下应该能获取到 IP
        let ip = get_local_ip();
        println!("Detected local IP: {:?}", ip);
        // 不做断言，因为在某些 CI 环境可能没有网络
    }
}
