//! 服务路由器
//!
//! 智能选择服务端点，优先级：Local > LAN > Mesh > Public
//!
//! 这是服务发现的核心组件，负责：
//! - 聚合多个服务发现源（LAN Discovery, Mesh, 配置）
//! - 根据优先级和健康状态选择最佳端点
//! - 提供统一的服务解析 API

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

use super::lan_discovery::LanDiscovery;
use super::mesh::MeshClient;

/// 端点来源
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EndpointSource {
    /// 本地服务（localhost）
    Local,
    /// LAN 发现的 peer
    LanPeer,
    /// Mesh 解析
    Mesh,
    /// 配置的公网端点
    Config,
    /// 硬编码回退
    Fallback,
}

impl EndpointSource {
    /// 获取优先级（数字越小优先级越高）
    pub fn priority(&self) -> i32 {
        match self {
            EndpointSource::Local => 0,
            EndpointSource::LanPeer => 10,
            EndpointSource::Mesh => 20,
            EndpointSource::Config => 30,
            EndpointSource::Fallback => 100,
        }
    }
}

/// 解析后的服务端点
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceEndpoint {
    /// 服务 URL
    pub url: String,
    /// 来源
    pub source: EndpointSource,
    /// 延迟（毫秒）
    pub latency_ms: Option<f64>,
    /// 是否健康
    pub healthy: bool,
    /// 额外信息
    pub metadata: HashMap<String, String>,
}

/// 服务路由配置
#[derive(Debug, Clone)]
pub struct ServiceRouterConfig {
    /// 本地服务配置 (service_id -> url)
    pub local_services: HashMap<String, String>,
    /// 配置的公网端点 (service_id -> url)
    pub configured_endpoints: HashMap<String, String>,
    /// 回退端点 (service_id -> url)
    pub fallback_endpoints: HashMap<String, String>,
}

impl ServiceRouterConfig {
    /// 从环境变量加载
    pub fn from_env() -> Self {
        // 解析本地服务
        // 格式: service_id=url,service_id=url
        let local_services = std::env::var("ROUTER_LOCAL_SERVICES")
            .map(|s| parse_service_map(&s))
            .unwrap_or_default();

        // 解析配置的端点
        let configured_endpoints = std::env::var("ROUTER_ENDPOINTS")
            .map(|s| parse_service_map(&s))
            .unwrap_or_default();

        // 默认回退端点
        // 注意：rustfs 不配置 fallback，让 autoupdate 回退到配置文件中的端点（阿里云 OSS）
        let mut fallback_endpoints = HashMap::new();
        fallback_endpoints.insert("deploy-center".to_string(), "https://auth.xiaojinpro.com".to_string());

        Self {
            local_services,
            configured_endpoints,
            fallback_endpoints,
        }
    }
}

/// 解析服务映射字符串
fn parse_service_map(s: &str) -> HashMap<String, String> {
    s.split(',')
        .filter_map(|item| {
            let parts: Vec<&str> = item.splitn(2, '=').collect();
            if parts.len() == 2 {
                Some((parts[0].to_string(), parts[1].to_string()))
            } else {
                None
            }
        })
        .collect()
}

/// 服务路由器
pub struct ServiceRouter {
    config: ServiceRouterConfig,
    /// LAN 发现服务（可选）
    lan_discovery: Option<Arc<LanDiscovery>>,
    /// Mesh 客户端（可选）
    mesh_client: Option<Arc<MeshClient>>,
    /// 端点缓存
    cache: Arc<RwLock<HashMap<String, CachedEndpoint>>>,
}

/// 缓存的端点
struct CachedEndpoint {
    endpoint: ServiceEndpoint,
    cached_at: std::time::Instant,
}

impl ServiceRouter {
    /// 创建新的服务路由器
    pub fn new(
        config: ServiceRouterConfig,
        lan_discovery: Option<Arc<LanDiscovery>>,
        mesh_client: Option<Arc<MeshClient>>,
    ) -> Self {
        Self {
            config,
            lan_discovery,
            mesh_client,
            cache: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// 获取服务端点，按优先级选择最佳端点
    ///
    /// 优先级：Local > LAN > Mesh > Config > Fallback
    pub async fn get_endpoint(&self, service_id: &str) -> ServiceEndpoint {
        // 检查缓存（缓存 30 秒）
        {
            let cache = self.cache.read().await;
            if let Some(cached) = cache.get(service_id) {
                if cached.cached_at.elapsed().as_secs() < 30 {
                    return cached.endpoint.clone();
                }
            }
        }

        let endpoint = self.resolve_endpoint(service_id).await;

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

        endpoint
    }

    /// 解析端点（不使用缓存）
    async fn resolve_endpoint(&self, service_id: &str) -> ServiceEndpoint {
        // 1. 检查本地服务
        if let Some(url) = self.config.local_services.get(service_id) {
            debug!(service_id = service_id, url = %url, "Using local service endpoint");
            return ServiceEndpoint {
                url: url.clone(),
                source: EndpointSource::Local,
                latency_ms: Some(0.0),
                healthy: true,
                metadata: HashMap::new(),
            };
        }

        // 2. 检查 LAN 发现的 peer
        if let Some(ref discovery) = self.lan_discovery {
            if discovery.is_enabled() {
                if let Some(url) = discovery.get_service_url(service_id).await {
                    // 获取 peer 信息以获取延迟
                    let peer = discovery.get_service_provider(service_id).await;
                    let latency_ms = peer.as_ref().and_then(|p| p.latency_ms);
                    let healthy = peer.as_ref().map(|p| p.healthy).unwrap_or(false);

                    if healthy {
                        info!(
                            service_id = service_id,
                            url = %url,
                            latency_ms = ?latency_ms,
                            "Using LAN peer endpoint"
                        );
                        return ServiceEndpoint {
                            url,
                            source: EndpointSource::LanPeer,
                            latency_ms,
                            healthy,
                            metadata: HashMap::new(),
                        };
                    }
                }
            }
        }

        // 3. 检查 Mesh
        if let Some(ref mesh) = self.mesh_client {
            if mesh.is_enabled() {
                match mesh.get_service_url(service_id).await {
                    Ok(url) => {
                        info!(
                            service_id = service_id,
                            url = %url,
                            "Using Mesh resolved endpoint"
                        );
                        return ServiceEndpoint {
                            url,
                            source: EndpointSource::Mesh,
                            latency_ms: None,
                            healthy: true,
                            metadata: HashMap::new(),
                        };
                    }
                    Err(e) => {
                        debug!(
                            service_id = service_id,
                            error = %e,
                            "Mesh resolution failed"
                        );
                    }
                }
            }
        }

        // 4. 检查配置的端点
        if let Some(url) = self.config.configured_endpoints.get(service_id) {
            debug!(service_id = service_id, url = %url, "Using configured endpoint");
            return ServiceEndpoint {
                url: url.clone(),
                source: EndpointSource::Config,
                latency_ms: None,
                healthy: true,
                metadata: HashMap::new(),
            };
        }

        // 5. 回退端点
        if let Some(url) = self.config.fallback_endpoints.get(service_id) {
            warn!(
                service_id = service_id,
                url = %url,
                "Using fallback endpoint"
            );
            return ServiceEndpoint {
                url: url.clone(),
                source: EndpointSource::Fallback,
                latency_ms: None,
                healthy: true,
                metadata: HashMap::new(),
            };
        }

        // 最终回退：返回空端点
        warn!(service_id = service_id, "No endpoint found for service");
        ServiceEndpoint {
            url: String::new(),
            source: EndpointSource::Fallback,
            latency_ms: None,
            healthy: false,
            metadata: HashMap::new(),
        }
    }

    // ========== 便捷方法 ==========

    /// 获取 RustFS 端点
    pub async fn get_rustfs_endpoint(&self) -> ServiceEndpoint {
        self.get_endpoint("rustfs").await
    }

    /// 获取 NFA 端点
    pub async fn get_nfa_endpoint(&self) -> ServiceEndpoint {
        self.get_endpoint("nfa").await
    }

    /// 获取服务 URL（便捷方法）
    pub async fn get_url(&self, service_id: &str) -> String {
        self.get_endpoint(service_id).await.url
    }

    /// 获取服务 URL，失败时返回 fallback
    pub async fn get_url_or(&self, service_id: &str, fallback: &str) -> String {
        let endpoint = self.get_endpoint(service_id).await;
        if endpoint.url.is_empty() {
            fallback.to_string()
        } else {
            endpoint.url
        }
    }

    /// 清除缓存
    pub async fn clear_cache(&self) {
        let mut cache = self.cache.write().await;
        cache.clear();
    }

    /// 获取所有已知端点的状态
    pub async fn get_all_endpoints(&self) -> Vec<(String, ServiceEndpoint)> {
        let mut endpoints = Vec::new();

        // 添加本地服务
        for (service_id, url) in &self.config.local_services {
            endpoints.push((
                service_id.clone(),
                ServiceEndpoint {
                    url: url.clone(),
                    source: EndpointSource::Local,
                    latency_ms: Some(0.0),
                    healthy: true,
                    metadata: HashMap::new(),
                },
            ));
        }

        // 添加 LAN 发现的服务
        if let Some(ref discovery) = self.lan_discovery {
            if discovery.is_enabled() {
                for peer in discovery.get_healthy_peers().await {
                    for (service_id, svc) in &peer.services {
                        endpoints.push((
                            service_id.clone(),
                            ServiceEndpoint {
                                url: format!("{}://{}:{}", svc.protocol, peer.lan_ip, svc.port),
                                source: EndpointSource::LanPeer,
                                latency_ms: peer.latency_ms,
                                healthy: peer.healthy,
                                metadata: {
                                    let mut m = HashMap::new();
                                    m.insert("peer_agent_id".to_string(), peer.agent_id.clone());
                                    m
                                },
                            },
                        ));
                    }
                }
            }
        }

        // 添加配置的端点
        for (service_id, url) in &self.config.configured_endpoints {
            endpoints.push((
                service_id.clone(),
                ServiceEndpoint {
                    url: url.clone(),
                    source: EndpointSource::Config,
                    latency_ms: None,
                    healthy: true,
                    metadata: HashMap::new(),
                },
            ));
        }

        // 添加回退端点
        for (service_id, url) in &self.config.fallback_endpoints {
            endpoints.push((
                service_id.clone(),
                ServiceEndpoint {
                    url: url.clone(),
                    source: EndpointSource::Fallback,
                    latency_ms: None,
                    healthy: true,
                    metadata: HashMap::new(),
                },
            ));
        }

        endpoints
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_endpoint_source_priority() {
        assert!(EndpointSource::Local.priority() < EndpointSource::LanPeer.priority());
        assert!(EndpointSource::LanPeer.priority() < EndpointSource::Mesh.priority());
        assert!(EndpointSource::Mesh.priority() < EndpointSource::Config.priority());
        assert!(EndpointSource::Config.priority() < EndpointSource::Fallback.priority());
    }

    #[test]
    fn test_parse_service_map() {
        let s = "rustfs=http://localhost:9000,nfa=http://localhost:9528";
        let map = parse_service_map(s);

        assert_eq!(map.get("rustfs"), Some(&"http://localhost:9000".to_string()));
        assert_eq!(map.get("nfa"), Some(&"http://localhost:9528".to_string()));
    }
}
