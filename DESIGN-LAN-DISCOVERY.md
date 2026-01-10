# xjp-deploy-agent 局域网发现功能设计

## 背景与动机

当前架构中，私有云 (192.168.1.4) 和 Windows (192.168.1.5) Agent 都在同一局域网内，但所有流量都需要绕道 ECS (auth.xiaojinpro.com) 中转。这导致：

1. **RustFS 访问延迟高** - Windows Agent 访问私有云上的 RustFS 需要走公网
2. **带宽浪费** - 大文件传输（如二进制更新）需要经过 ECS 中转
3. **单点故障** - ECS 宕机会影响局域网内 Agent 的协作

## 目标

1. Agent 启动时自动发现局域网内的其他 Agent
2. 发现后自动建立直连，优先使用内网通信
3. RustFS 等服务可被局域网内其他 Agent 直接访问
4. 保持向后兼容，发现失败时回退到现有的 ECS 中转模式

## 网络拓扑

```
┌─────────────────────────────────────────────────────────────────────┐
│                      192.168.1.0/24 局域网                           │
│                                                                     │
│   ┌──────────────────────┐       ┌──────────────────────────────┐  │
│   │  私有云 192.168.1.4   │ ←───→ │    Windows 192.168.1.5       │  │
│   │  Agent :9876         │  直连  │    Agent :9876               │  │
│   │  RustFS :9000        │       │    NFA :9528                 │  │
│   │  PostgreSQL :5432    │       │    SSH :2222                 │  │
│   └──────────────────────┘       └──────────────────────────────┘  │
│              ↑                              ↑                       │
└──────────────┼──────────────────────────────┼───────────────────────┘
               │                              │
               │ WebSocket Tunnel             │ WebSocket Tunnel
               ↓                              ↓
        ┌─────────────────────────────────────────────┐
        │         ECS (auth.xiaojinpro.com)           │
        │         Tunnel Server :9876                 │
        └─────────────────────────────────────────────┘
```

## 设计方案

### 方案一：mDNS/DNS-SD 服务发现 (推荐)

利用 Zeroconf 协议，无需额外配置即可发现局域网服务。

**优点**：
- 标准协议，跨平台支持
- 无需手动配置 IP
- 支持动态发现和服务上下线通知

**实现**：

```rust
// src/services/discovery/mod.rs
pub mod mdns;

// 服务类型定义
pub const SERVICE_TYPE: &str = "_xjp-agent._tcp.local.";

// 发布的 TXT 记录
pub struct AgentServiceInfo {
    pub agent_id: String,      // 唯一标识
    pub version: String,       // Agent 版本
    pub capabilities: Vec<String>, // ["rustfs", "docker", "nfa"]
    pub api_port: u16,         // HTTP API 端口
}
```

```rust
// src/services/discovery/mdns.rs
use mdns_sd::{ServiceDaemon, ServiceInfo};

pub struct MdnsDiscovery {
    daemon: ServiceDaemon,
    discovered_agents: Arc<RwLock<HashMap<String, DiscoveredAgent>>>,
}

#[derive(Clone)]
pub struct DiscoveredAgent {
    pub agent_id: String,
    pub ip_addr: IpAddr,
    pub port: u16,
    pub capabilities: Vec<String>,
    pub version: String,
    pub discovered_at: DateTime<Utc>,
    pub last_seen: DateTime<Utc>,
}

impl MdnsDiscovery {
    pub async fn start(&self, my_info: AgentServiceInfo) -> Result<()> {
        // 1. 注册自己的服务
        self.register_service(&my_info).await?;

        // 2. 浏览局域网内其他 Agent
        self.browse_services().await?;

        Ok(())
    }

    pub fn get_lan_agents(&self) -> Vec<DiscoveredAgent> {
        self.discovered_agents.read().values().cloned().collect()
    }

    pub fn get_rustfs_endpoint(&self) -> Option<String> {
        // 查找具有 rustfs capability 的 Agent
        self.discovered_agents.read()
            .values()
            .find(|a| a.capabilities.contains(&"rustfs".to_string()))
            .map(|a| format!("http://{}:9000", a.ip_addr))
    }
}
```

### 方案二：UDP 广播发现 (备选)

简单直接，适合防火墙策略严格的环境。

```rust
// src/services/discovery/udp.rs
pub struct UdpDiscovery {
    broadcast_port: u16, // 19999
    broadcast_interval: Duration, // 30s
}

// 广播消息格式
#[derive(Serialize, Deserialize)]
pub struct DiscoveryBroadcast {
    pub magic: [u8; 4],  // "XJPA"
    pub version: u8,     // 协议版本
    pub agent_id: String,
    pub ip_addr: String,
    pub api_port: u16,
    pub capabilities: Vec<String>,
    pub timestamp: i64,
}
```

### 方案三：通过 Tunnel Server 协调 (混合模式)

利用现有的 Tunnel Server 来交换局域网 Agent 信息。

```rust
// 扩展 TunnelMessage
pub enum TunnelMessage {
    // ... 现有消息类型 ...

    // 新增：局域网信息交换
    LanInfo {
        agent_id: String,
        lan_ip: String,
        lan_port: u16,
        capabilities: Vec<String>,
    },

    // Server 广播已知的局域网 Agent
    LanPeers {
        peers: Vec<LanPeerInfo>,
    },
}
```

## 推荐实现：混合方案

结合方案一和方案三的优点：

1. **主动发现**：使用 mDNS 在局域网内主动发现
2. **被动同步**：通过 Tunnel Server 同步发现信息
3. **健康检查**：定期验证已发现 Agent 的可达性

### 架构图

```
┌────────────────────────────────────────────────────────────────────┐
│                        AppState                                    │
│  ┌──────────────────────────────────────────────────────────────┐ │
│  │                    LanDiscoveryState                         │ │
│  │  ┌─────────────────┐  ┌─────────────────┐  ┌──────────────┐ │ │
│  │  │  MdnsService    │  │  PeerRegistry   │  │ HealthChecker│ │ │
│  │  │  - register()   │  │  - peers: Map   │  │ - check_all()│ │ │
│  │  │  - browse()     │  │  - add_peer()   │  │ - interval   │ │ │
│  │  └────────┬────────┘  └────────┬────────┘  └──────┬───────┘ │ │
│  │           │                    │                   │         │ │
│  │           └────────────────────┼───────────────────┘         │ │
│  │                                ↓                              │ │
│  │                    ┌─────────────────────┐                   │ │
│  │                    │   ServiceRouter     │                   │ │
│  │                    │ - get_rustfs_url()  │                   │ │
│  │                    │ - get_peer_url()    │                   │ │
│  │                    └─────────────────────┘                   │ │
│  └──────────────────────────────────────────────────────────────┘ │
└────────────────────────────────────────────────────────────────────┘
```

## 配置

### 环境变量

```bash
# 启用局域网发现
LAN_DISCOVERY_ENABLED=true

# 发现模式: mdns, udp, hybrid (默认)
LAN_DISCOVERY_MODE=hybrid

# mDNS 配置
MDNS_SERVICE_NAME=xjp-agent-{hostname}

# UDP 广播配置 (备选)
UDP_BROADCAST_PORT=19999
UDP_BROADCAST_INTERVAL_SECS=30

# 本机 capabilities (逗号分隔)
# 可选值: rustfs, docker, nfa, postgres, redis
LAN_CAPABILITIES=rustfs,docker,postgres

# RustFS 内网端口 (如果本机运行 RustFS)
RUSTFS_LAN_PORT=9000

# 健康检查间隔
LAN_HEALTH_CHECK_INTERVAL_SECS=60
```

### 配置示例

**私有云 (.env)**:
```bash
LAN_DISCOVERY_ENABLED=true
LAN_DISCOVERY_MODE=hybrid
LAN_CAPABILITIES=rustfs,docker,postgres
RUSTFS_LAN_PORT=9000
```

**Windows (.env)**:
```bash
LAN_DISCOVERY_ENABLED=true
LAN_DISCOVERY_MODE=hybrid
LAN_CAPABILITIES=nfa,docker
```

## API 扩展

### GET /lan/peers

返回已发现的局域网 Agent 列表：

```json
{
  "self": {
    "agent_id": "windows-agent",
    "lan_ip": "192.168.1.5",
    "api_port": 9876,
    "capabilities": ["nfa", "docker"]
  },
  "peers": [
    {
      "agent_id": "privatecloud-agent",
      "lan_ip": "192.168.1.4",
      "api_port": 9876,
      "capabilities": ["rustfs", "docker", "postgres"],
      "version": "0.9.11",
      "discovered_at": "2026-01-10T09:00:00Z",
      "last_seen": "2026-01-10T09:10:00Z",
      "healthy": true,
      "services": {
        "rustfs": "http://192.168.1.4:9000",
        "postgres": "postgresql://192.168.1.4:5432"
      }
    }
  ],
  "discovery_mode": "hybrid",
  "last_broadcast": "2026-01-10T09:09:30Z"
}
```

### GET /lan/service/{service_name}

获取特定服务的最佳端点：

```json
// GET /lan/service/rustfs
{
  "service": "rustfs",
  "preferred_endpoint": "http://192.168.1.4:9000",
  "source": "lan_peer",
  "fallback_endpoint": "https://s3.xiaojinpro.top",
  "latency_ms": 0.3
}
```

## 自动更新流程优化

当前流程：
```
Windows Agent ──(公网)──> ECS ──(tunnel)──> 私有云 RustFS
                        ↑
                    下载二进制
```

优化后流程：
```
Windows Agent ──(局域网直连)──> 私有云 RustFS (192.168.1.4:9000)
                              ↑
                          下载二进制 (< 1ms 延迟)
```

### 修改 auto_update.rs

```rust
impl AutoUpdater {
    async fn get_download_url(&self) -> String {
        // 1. 检查局域网是否有 RustFS
        if let Some(lan_rustfs) = self.state.lan_discovery.get_rustfs_endpoint() {
            // 验证可达性
            if self.check_endpoint(&lan_rustfs).await {
                return format!("{}/xiaojinpro/releases/xjp-deploy-agent/latest", lan_rustfs);
            }
        }

        // 2. 回退到公网端点
        self.config.download_base_url.clone()
    }
}
```

## 实现步骤

### Phase 1: 基础发现 (优先级: 高)

1. 添加 `mdns-sd` 依赖
2. 实现 `LanDiscoveryService`
3. 添加 `LanDiscoveryState` 到 `AppState`
4. 实现 `/lan/peers` API
5. 添加环境变量配置

### Phase 2: 服务路由 (优先级: 高)

1. 实现 `ServiceRouter` 智能选择端点
2. 修改 `auto_update.rs` 使用局域网 RustFS
3. 添加 `/lan/service/{name}` API
4. 添加延迟测量和健康检查

### Phase 3: Tunnel 集成 (优先级: 中)

1. 扩展 `TunnelMessage` 支持 `LanInfo`
2. Server 端维护局域网信息表
3. Client 连接时上报局域网 IP
4. Server 广播局域网 Agent 信息

### Phase 4: 前端展示 (优先级: 低)

1. 部署中心展示局域网拓扑
2. 显示 Agent 间的连接状态
3. 可视化服务路由决策

## 依赖

```toml
# Cargo.toml 新增
[dependencies]
mdns-sd = "0.11"           # mDNS 服务发现
local-ip-address = "0.6"   # 获取本机局域网 IP
```

## 安全考虑

1. **认证** - mDNS 发现的 Agent 仍需 API Key 认证
2. **网络隔离** - 仅在指定子网内进行发现
3. **服务验证** - 验证发现的服务签名/版本
4. **防重放** - 广播消息包含时间戳

## 测试计划

1. **单元测试** - 发现逻辑、服务路由
2. **集成测试** - 两个 Agent 互相发现
3. **网络分区测试** - 发现失败时的回退行为
4. **性能测试** - 发现延迟、广播频率影响

## 总结

本设计通过 mDNS + Tunnel 协调的混合方案，实现局域网内 Agent 的自动发现和直连通信。主要收益：

1. **降低延迟** - RustFS 访问从 ~100ms 降至 <1ms
2. **节省带宽** - 大文件传输不再经过公网中转
3. **提高可靠性** - ECS 宕机不影响局域网内协作
4. **零配置** - 自动发现，无需手动配置 IP

预计开发周期：Phase 1-2 约 2-3 天可完成核心功能。
