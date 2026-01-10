# 自动发现与自动配置系统设计 V2

## 目标

让同一局域网内的 Agent 能够：
1. **自动发现** - 无需手动配置即可发现彼此
2. **自动配置** - 发现后自动建立内网连接
3. **优先使用内网** - Windows Agent 优先使用内网 RustFS 进行自更新
4. **优先使用内网 NFA** - 私有云优先使用内网 Windows NFA 服务

## 当前问题

| 问题 | 影响 | 解决方案 |
|------|------|---------|
| RustFS 只监听 localhost:9000 | Windows 无法直接访问 | 修改端口绑定为 0.0.0.0:9000 |
| 没有 LAN 发现机制 | Agent 不知道彼此存在 | 实现 UDP 广播 + Mesh 注册 |
| Mesh 未被 autoupdate 使用 | 无法利用内网端点 | 集成 MeshClient 到服务消费者 |
| NFA 调用走公网 | 高延迟 | 支持内网 NFA 端点 |

## 系统架构

```
┌─────────────────────────────────────────────────────────────────────────────┐
│                          192.168.1.0/24 LAN                                  │
│                                                                             │
│  ┌─────────────────────────┐         ┌─────────────────────────────────┐   │
│  │  私有云 192.168.1.4      │ ◄─────► │     Windows 192.168.1.5          │   │
│  │                         │   UDP    │                                 │   │
│  │  ┌───────────────────┐  │ 广播    │  ┌─────────────────────────┐   │   │
│  │  │ Deploy Agent      │  │         │  │ Deploy Agent            │   │   │
│  │  │ :9876             │  │         │  │ :9876                   │   │   │
│  │  │ - MeshClient ✓    │  │         │  │ - MeshClient ✓          │   │   │
│  │  │ - LanDiscovery    │  │         │  │ - LanDiscovery          │   │   │
│  │  │ - AutoUpdate      │  │         │  │ - AutoUpdate            │   │   │
│  │  └───────────────────┘  │         │  └─────────────────────────┘   │   │
│  │                         │         │                                 │   │
│  │  ┌───────────────────┐  │         │  ┌─────────────────────────┐   │   │
│  │  │ RustFS :9000      │◄─┼─────────┼──│ (访问 RustFS)            │   │   │
│  │  │ 0.0.0.0:9000 ✓    │  │  LAN    │  └─────────────────────────┘   │   │
│  │  └───────────────────┘  │  直连   │                                 │   │
│  │                         │         │  ┌─────────────────────────┐   │   │
│  │  ┌───────────────────┐  │         │  │ NFA :9528               │   │   │
│  │  │ (访问 NFA)         │──┼─────────┼──►│ 0.0.0.0:9528 ✓          │   │   │
│  │  └───────────────────┘  │         │  └─────────────────────────┘   │   │
│  └─────────────────────────┘         └─────────────────────────────────┘   │
│              │                                    │                         │
└──────────────┼────────────────────────────────────┼─────────────────────────┘
               │ WebSocket Tunnel                   │
               ▼                                    ▼
        ┌──────────────────────────────────────────────────┐
        │            Deploy Center (GCP VM)                │
        │  ┌────────────────────────────────────────────┐  │
        │  │           Service Mesh Registry            │  │
        │  │  - mesh_network_zones (lan-de-home)       │  │
        │  │  - mesh_services (rustfs, nfa, ...)       │  │
        │  │  - mesh_service_endpoints                 │  │
        │  │  - mesh_agent_locations                   │  │
        │  │  - EndpointResolver (亲和度路由)           │  │
        │  └────────────────────────────────────────────┘  │
        └──────────────────────────────────────────────────┘
```

## 三层架构

### Layer 1: 基础设施层 - 端口暴露

**目标**: 让服务对 LAN 可见

#### 1.1 RustFS 端口配置 (私有云)

```yaml
# docker-compose.rustfs.yml
services:
  rustfs:
    image: quay.io/minio/minio:latest
    ports:
      - "0.0.0.0:9000:9000"   # S3 API - 对 LAN 暴露
      - "0.0.0.0:9001:9001"   # Console
    environment:
      - MINIO_ROOT_USER=minioadmin
      - MINIO_ROOT_PASSWORD=minioadmin
```

#### 1.2 NFA 服务配置 (Windows)

```powershell
# NFA 服务已监听 0.0.0.0:9528，无需修改
# 但需要确保 Windows 防火墙允许入站
netsh advfirewall firewall add rule name="NFA Service" dir=in action=allow protocol=tcp localport=9528
```

---

### Layer 2: 发现层 - 混合发现机制

**目标**: Agent 自动发现彼此

#### 2.1 UDP 广播发现 (主动发现)

```rust
// src/services/lan_discovery/mod.rs

pub const BROADCAST_PORT: u16 = 19999;
pub const BROADCAST_MAGIC: &[u8; 4] = b"XJPA";
pub const BROADCAST_INTERVAL: Duration = Duration::from_secs(30);

#[derive(Serialize, Deserialize)]
pub struct DiscoveryPacket {
    pub magic: [u8; 4],
    pub version: u8,
    pub agent_id: String,
    pub lan_ip: IpAddr,
    pub api_port: u16,
    pub capabilities: Vec<String>,  // ["rustfs", "nfa", "docker"]
    pub services: HashMap<String, ServiceInfo>,
    pub timestamp: i64,
}

#[derive(Serialize, Deserialize)]
pub struct ServiceInfo {
    pub port: u16,
    pub protocol: String,
    pub health_path: Option<String>,
}

pub struct LanDiscovery {
    socket: UdpSocket,
    discovered_peers: Arc<RwLock<HashMap<String, DiscoveredPeer>>>,
    my_info: DiscoveryPacket,
}

impl LanDiscovery {
    /// 启动发现服务
    pub async fn start(&self) -> Result<()> {
        // 并行运行广播发送和接收
        tokio::select! {
            r = self.broadcast_loop() => r,
            r = self.receive_loop() => r,
        }
    }

    /// 定期广播自己的存在
    async fn broadcast_loop(&self) -> Result<()> {
        let broadcast_addr = SocketAddr::new(
            IpAddr::V4(Ipv4Addr::BROADCAST),
            BROADCAST_PORT
        );
        loop {
            let packet = bincode::serialize(&self.my_info)?;
            self.socket.send_to(&packet, broadcast_addr).await?;
            tokio::time::sleep(BROADCAST_INTERVAL).await;
        }
    }

    /// 接收其他 Agent 的广播
    async fn receive_loop(&self) -> Result<()> {
        let mut buf = [0u8; 4096];
        loop {
            let (len, src) = self.socket.recv_from(&mut buf).await?;
            if let Ok(packet) = bincode::deserialize::<DiscoveryPacket>(&buf[..len]) {
                if &packet.magic == BROADCAST_MAGIC && packet.agent_id != self.my_info.agent_id {
                    self.handle_discovery(packet, src).await;
                }
            }
        }
    }

    /// 处理发现的 peer
    async fn handle_discovery(&self, packet: DiscoveryPacket, _src: SocketAddr) {
        let peer = DiscoveredPeer {
            agent_id: packet.agent_id.clone(),
            lan_ip: packet.lan_ip,
            api_port: packet.api_port,
            capabilities: packet.capabilities.clone(),
            services: packet.services.clone(),
            discovered_at: Utc::now(),
            last_seen: Utc::now(),
            healthy: true,
        };

        // 验证连通性
        if self.verify_peer(&peer).await {
            let mut peers = self.discovered_peers.write().await;
            let is_new = !peers.contains_key(&packet.agent_id);
            peers.insert(packet.agent_id.clone(), peer.clone());

            if is_new {
                tracing::info!(
                    agent_id = %packet.agent_id,
                    lan_ip = %packet.lan_ip,
                    capabilities = ?packet.capabilities,
                    "Discovered new LAN peer"
                );
                // 触发 Mesh 端点注册
                self.register_peer_endpoints(&peer).await;
            }
        }
    }

    /// 验证 peer 可达性
    async fn verify_peer(&self, peer: &DiscoveredPeer) -> bool {
        let url = format!("http://{}:{}/health", peer.lan_ip, peer.api_port);
        reqwest::Client::new()
            .get(&url)
            .timeout(Duration::from_secs(5))
            .send()
            .await
            .is_ok()
    }

    /// 将发现的 peer 服务注册到 Mesh
    async fn register_peer_endpoints(&self, peer: &DiscoveredPeer) {
        // 通过 MeshClient 注册发现的端点
        // 这样 Mesh 的 EndpointResolver 就能找到 LAN 端点
    }
}
```

#### 2.2 Mesh 注册表同步 (被动发现)

```rust
// src/services/mesh.rs 扩展

impl MeshClient {
    /// 启动时注册本地服务
    pub async fn register_with_services(&self, services: HashMap<String, ServiceInfo>) -> Result<()> {
        let request = AgentRegistrationRequest {
            agent_id: self.config.agent_id.clone(),
            agent_name: self.config.agent_name.clone(),
            lan_ip: self.detect_lan_ip(),
            wan_ip: self.detect_wan_ip().await,
            api_port: self.config.api_port,
            capabilities: self.detect_capabilities(),
            // 新增：本地服务列表
            local_services: services.into_iter().map(|(id, info)| {
                LocalServiceEndpoint {
                    service_id: id,
                    local_url: format!("http://{}:{}", self.detect_lan_ip(), info.port),
                    protocol: info.protocol,
                    health_check_path: info.health_path,
                }
            }).collect(),
        };

        // POST to /mesh/agents/register
        // Deploy Center 会自动创建 mesh_service_endpoints 记录
    }

    /// 定期从 Mesh 拉取同区域 peer 信息
    pub async fn sync_peers(&self) -> Result<Vec<AgentLocation>> {
        let url = format!("{}/mesh/agents?zone_id={}",
            self.config.registry_url,
            self.detected_zone_id
        );
        // GET 同区域的所有 Agent
    }
}
```

---

### Layer 3: 路由层 - 智能端点选择

**目标**: 自动选择最优端点

#### 3.1 本地服务路由器

```rust
// src/services/service_router.rs

pub struct ServiceRouter {
    mesh_client: Option<Arc<MeshClient>>,
    lan_discovery: Option<Arc<LanDiscovery>>,
    local_services: HashMap<String, String>,  // service_id -> local_url
}

impl ServiceRouter {
    /// 获取服务端点，优先级：本地 > LAN > Mesh > 公网
    pub async fn get_endpoint(&self, service_id: &str) -> ServiceEndpoint {
        // 1. 检查本地是否提供此服务
        if let Some(url) = self.local_services.get(service_id) {
            return ServiceEndpoint {
                url: url.clone(),
                source: EndpointSource::Local,
                latency_ms: 0.0,
            };
        }

        // 2. 检查 LAN 发现的 peer 是否提供此服务
        if let Some(discovery) = &self.lan_discovery {
            if let Some(peer) = discovery.get_service_provider(service_id).await {
                let url = format!("http://{}:{}", peer.lan_ip, peer.services[service_id].port);
                if self.verify_endpoint(&url).await {
                    return ServiceEndpoint {
                        url,
                        source: EndpointSource::LanPeer,
                        latency_ms: self.measure_latency(&url).await,
                    };
                }
            }
        }

        // 3. 使用 Mesh 解析 (会自动选择最优端点)
        if let Some(mesh) = &self.mesh_client {
            if let Ok(endpoint) = mesh.resolve_endpoint(service_id).await {
                return ServiceEndpoint {
                    url: endpoint.url,
                    source: EndpointSource::Mesh,
                    latency_ms: endpoint.latency_ms.unwrap_or(0.0),
                };
            }
        }

        // 4. 回退到硬编码的公网端点
        ServiceEndpoint {
            url: self.get_fallback_url(service_id),
            source: EndpointSource::Fallback,
            latency_ms: 100.0,
        }
    }

    /// 获取 RustFS 端点 (自动更新专用)
    pub async fn get_rustfs_endpoint(&self) -> String {
        self.get_endpoint("rustfs").await.url
    }

    /// 获取 NFA 端点
    pub async fn get_nfa_endpoint(&self, node_id: &str) -> Option<String> {
        let service_id = format!("nfa-{}", node_id);
        let endpoint = self.get_endpoint(&service_id).await;
        Some(endpoint.url)
    }
}
```

#### 3.2 集成到 AutoUpdate

```rust
// src/services/autoupdate.rs 修改

async fn resolve_binary_url(state: &Arc<AppState>, config: &AutoUpdateConfig, version: &str) -> String {
    let binary_path = config.binary_path_template.replace("{version}", version);

    // 使用 ServiceRouter 获取最优 RustFS 端点
    if let Some(router) = &state.service_router {
        let endpoint = router.get_rustfs_endpoint().await;
        let url = format!("{}/{}", endpoint.trim_end_matches('/'), binary_path);

        tracing::info!(
            endpoint = %endpoint,
            source = ?router.get_endpoint("rustfs").await.source,
            "Resolved RustFS endpoint for auto-update"
        );

        return url;
    }

    // 回退到默认配置
    config.binary_url(version)
}
```

#### 3.3 集成到 NFA 调用

```rust
// 在调用 NFA 服务时

async fn call_nfa_service(state: &Arc<AppState>, node_id: &str, request: NfaRequest) -> Result<NfaResponse> {
    let endpoint = if let Some(router) = &state.service_router {
        router.get_nfa_endpoint(node_id).await
    } else {
        // 回退到隧道端点
        Some(format!("http://localhost:9529"))
    };

    let url = format!("{}/align", endpoint.unwrap_or_default());
    // ... 发送请求
}
```

---

## 实现计划

### Phase 1: 基础设施 (Day 1)

| 任务 | 文件 | 说明 |
|------|------|------|
| 1.1 修改 RustFS 端口 | 私有云 docker-compose | 改为 0.0.0.0:9000 |
| 1.2 验证 NFA 端口 | Windows 防火墙 | 确保 9528 可访问 |
| 1.3 验证 LAN 连通性 | 手动测试 | ping + curl 测试 |

### Phase 2: UDP 发现 (Day 2-3)

| 任务 | 文件 | 说明 |
|------|------|------|
| 2.1 添加 LanDiscovery 模块 | src/services/lan_discovery/mod.rs | UDP 广播发送/接收 |
| 2.2 添加配置项 | src/config/env.rs | LAN_DISCOVERY_ENABLED 等 |
| 2.3 集成到 AppState | src/state/app_state.rs | 添加 lan_discovery 字段 |
| 2.4 启动 LanDiscovery | src/lib.rs | 后台任务启动 |
| 2.5 添加 API 端点 | src/api/lan.rs | GET /lan/peers |

### Phase 3: 服务路由 (Day 4-5)

| 任务 | 文件 | 说明 |
|------|------|------|
| 3.1 实现 ServiceRouter | src/services/service_router.rs | 智能端点选择 |
| 3.2 集成到 AutoUpdate | src/services/autoupdate.rs | 使用 Router 获取 RustFS |
| 3.3 集成到 NFA 调用 | (如适用) | 使用 Router 获取 NFA |
| 3.4 添加 API 端点 | src/api/lan.rs | GET /lan/service/{name} |

### Phase 4: Mesh 增强 (Day 6-7)

| 任务 | 文件 | 说明 |
|------|------|------|
| 4.1 扩展 Mesh 注册 | src/services/mesh.rs | 支持 local_services |
| 4.2 Deploy Center 处理 | deploy-center/api/mesh.rs | 自动创建端点 |
| 4.3 端点健康检查 | deploy-center/mesh/resolver.rs | 定期检查端点 |

---

## 配置说明

### 私有云 Agent (.env)

```bash
# 基础配置
PORT=9876
TUNNEL_MODE=client
TUNNEL_SERVER_URL=ws://34.104.147.118:9876/tunnel/ws

# LAN 发现
LAN_DISCOVERY_ENABLED=true
LAN_BROADCAST_PORT=19999
LAN_CAPABILITIES=rustfs,docker,postgres

# 本地服务 (会注册到 Mesh)
LOCAL_SERVICES=rustfs:9000:http,postgres:5432:postgresql

# Mesh 配置
MESH_ENABLED=true
MESH_REGISTRY_URL=https://auth.xiaojinpro.com
MESH_AGENT_ID=privatecloud
```

### Windows Agent (.env)

```bash
# 基础配置
PORT=9876
TUNNEL_MODE=client
TUNNEL_SERVER_URL=ws://auth.xiaojinpro.com:9876/tunnel/ws

# LAN 发现
LAN_DISCOVERY_ENABLED=true
LAN_BROADCAST_PORT=19999
LAN_CAPABILITIES=nfa,docker

# 本地服务
LOCAL_SERVICES=nfa:9528:http

# Mesh 配置
MESH_ENABLED=true
MESH_REGISTRY_URL=https://auth.xiaojinpro.com
MESH_AGENT_ID=windows
```

---

## 预期效果

### Before (当前)

```
Windows 自更新:
Windows → (公网) → ECS Tunnel → (公网) → 私有云 RustFS
延迟: ~100ms, 带宽受限于公网

NFA 调用:
私有云 → (公网) → ECS Tunnel → (公网) → Windows NFA
延迟: ~100ms
```

### After (实现后)

```
Windows 自更新:
Windows → (LAN 直连) → 私有云 RustFS (192.168.1.4:9000)
延迟: <1ms, 千兆带宽

NFA 调用:
私有云 → (LAN 直连) → Windows NFA (192.168.1.5:9528)
延迟: <1ms
```

### 自动发现流程

```
1. 私有云 Agent 启动
   ├── 启动 UDP 广播 (每30秒)
   ├── 注册到 Mesh (携带 local_services)
   └── 监听 UDP 端口

2. Windows Agent 启动
   ├── 启动 UDP 广播
   ├── 收到私有云的广播包
   ├── 验证 192.168.1.4:9876 可达
   ├── 发现私有云提供 rustfs 服务
   └── 更新本地 ServiceRouter

3. Windows 自更新检查
   ├── ServiceRouter.get_rustfs_endpoint()
   ├── 返回 http://192.168.1.4:9000 (LAN)
   └── 直接从内网下载二进制

4. 如果 LAN 不可用
   ├── 自动回退到 Mesh 解析
   └── 使用公网端点
```

---

## 监控与调试

### API 端点

```
GET /lan/peers          # 查看发现的 LAN peers
GET /lan/service/rustfs # 查看 RustFS 最优端点
GET /lan/stats          # 发现统计信息
GET /health             # 包含 LAN 发现状态
```

### 日志关键词

```
"Discovered new LAN peer"     # 发现新 peer
"Resolved RustFS endpoint"    # RustFS 端点解析
"LAN endpoint verified"       # LAN 端点验证成功
"Falling back to Mesh"        # 回退到 Mesh
```

---

## 总结

本设计通过三层架构实现自动发现和自动配置：

1. **基础设施层** - 确保服务对 LAN 可见 (端口绑定)
2. **发现层** - UDP 广播 + Mesh 注册的混合发现
3. **路由层** - ServiceRouter 智能选择最优端点

核心优势：
- **零配置** - 自动发现，无需手动配置 IP
- **高可用** - 多层回退机制
- **低延迟** - LAN 直连 <1ms
- **可观测** - 完整的监控和调试 API

预计开发周期：5-7 天完成全部功能。
