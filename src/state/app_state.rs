//! 应用状态

use chrono::{DateTime, Utc};
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

/// 全局 shutdown token，用于优雅关闭所有后台任务
static GLOBAL_SHUTDOWN: std::sync::OnceLock<CancellationToken> = std::sync::OnceLock::new();

/// 获取全局 shutdown token
pub fn get_shutdown_token() -> CancellationToken {
    GLOBAL_SHUTDOWN
        .get_or_init(CancellationToken::new)
        .clone()
}

/// 触发全局 shutdown
pub fn trigger_shutdown() {
    if let Some(token) = GLOBAL_SHUTDOWN.get() {
        token.cancel();
    }
}

use crate::api::deploy::TriggerRequest;
use crate::config::{
    autoupdate::{AutoUpdateConfig, AutoUpdateState},
    env::EnvConfig,
    project::{load_projects_from_env, ProjectConfig},
};
use crate::domain::tunnel::{PortMapping, TunnelMode};
use crate::infra::DeployCenterClient;
use crate::services::{
    frp::FrpcManager,
    mesh::MeshClient,
    nfa::NfaSupervisor,
    ssh::SshServer,
    lan_discovery::{LanDiscovery, LanDiscoveryConfig},
    service_router::{ServiceRouter, ServiceRouterConfig},
};

use super::log_hub::LogHub;
use super::port_listener_manager::PortListenerManager;
use super::task_store::TaskStore;
use super::tunnel_state::{TunnelClientState, TunnelServerState};

/// 运行中的部署进程信息
pub struct RunningDeploy {
    pub task_id: String,
    pub cancel_token: CancellationToken,
}

/// 队列中等待的部署请求
pub struct QueuedDeploy {
    pub task_id: String,
    pub project_config: ProjectConfig,
    pub request: TriggerRequest,
    pub queued_at: DateTime<Utc>,
}

/// 应用状态
pub struct AppState {
    // ========== 核心配置 ==========
    /// API 密钥（用于验证请求）
    pub api_key: String,
    /// 环境配置
    pub config: EnvConfig,
    /// 项目配置
    pub projects: HashMap<String, ProjectConfig>,
    /// 服务启动时间
    pub started_at: DateTime<Utc>,

    // ========== 任务管理 ==========
    /// 任务存储
    pub task_store: TaskStore,
    /// 日志中心
    pub log_hub: LogHub,
    /// 每个项目当前运行中的部署 (project -> RunningDeploy)
    pub running_deploys: RwLock<HashMap<String, RunningDeploy>>,
    /// 每个项目的部署队列 (project -> queue)
    pub deploy_queue: RwLock<HashMap<String, VecDeque<QueuedDeploy>>>,

    // ========== 外部服务 ==========
    /// 部署中心客户端
    pub deploy_center: DeployCenterClient,

    // ========== 隧道相关 ==========
    /// 隧道模式
    pub tunnel_mode: TunnelMode,
    /// 隧道客户端 ID (Client 模式，用于标识)
    pub tunnel_client_id: String,
    /// 隧道认证令牌
    pub tunnel_auth_token: String,
    /// 隧道服务端状态 (仅 Server 模式)
    pub tunnel_server_state: Arc<TunnelServerState>,
    /// 隧道客户端状态 (仅 Client 模式)
    pub tunnel_client_state: Arc<TunnelClientState>,
    /// 端口监听器管理器 (仅 Server 模式)
    pub port_listener_manager: Arc<PortListenerManager>,
    /// 端口映射配置 (Client 模式)
    pub tunnel_port_mappings: Vec<PortMapping>,
    /// 隧道服务端 URL (Client 模式)
    pub tunnel_server_url: String,

    // ========== NFA / FRP / SSH ==========
    /// NFA supervisor (Windows 节点，可选)
    pub nfa: Option<Arc<NfaSupervisor>>,
    /// FRP client manager (托管 frpc，可选)
    pub frp: Option<Arc<FrpcManager>>,
    /// SSH 服务器 (可选，需要在 main 中异步初始化后设置)
    pub ssh_server: RwLock<Option<Arc<SshServer>>>,

    // ========== 自动更新 ==========
    /// 自动更新配置
    pub auto_update_config: Option<AutoUpdateConfig>,
    /// 自动更新状态
    pub auto_update_state: Arc<AutoUpdateState>,

    // ========== Service Mesh ==========
    /// Mesh 客户端（用于服务发现和端点解析）
    pub mesh_client: Option<Arc<MeshClient>>,

    // ========== LAN Discovery ==========
    /// LAN 发现服务（用于自动发现同网段的 Agent）
    pub lan_discovery: Option<Arc<LanDiscovery>>,

    // ========== Service Router ==========
    /// 服务路由器（智能选择最优端点）
    pub service_router: Option<Arc<ServiceRouter>>,
}

impl AppState {
    /// 创建新的应用状态
    pub fn new() -> Self {
        let config = EnvConfig::from_env();
        let projects = load_projects_from_env();

        tracing::info!(
            api_key_len = config.api_key.len(),
            callback_url = ?config.callback_url,
            port = config.port,
            tunnel_mode = ?config.tunnel.mode,
            auto_update = config.auto_update.is_some(),
            project_count = projects.len(),
            "Loaded configuration"
        );

        for (name, project) in &projects {
            tracing::info!(
                project = %name,
                work_dir = %project.work_dir,
                deploy_type = %project.deploy_type.name(),
                "Registered project"
            );
        }

        let nfa = if config.nfa.enabled {
            Some(Arc::new(NfaSupervisor::new(config.nfa.clone())))
        } else {
            None
        };

        let frp = if config.frp.enabled {
            Some(Arc::new(FrpcManager::new(
                config.frp.clone(),
                config.tunnel.port_mappings.clone(),
            )))
        } else {
            None
        };

        // 初始化 Mesh 客户端（如果配置了）
        let mesh_client = crate::services::mesh::MeshConfig::from_env().map(|mesh_config| {
            tracing::info!(
                agent_id = %mesh_config.agent_id,
                registry_url = %mesh_config.registry_url,
                enabled = mesh_config.enabled,
                "Mesh client configured"
            );
            Arc::new(MeshClient::new(mesh_config))
        });

        // 初始化 LAN 发现服务（如果配置了）
        let lan_discovery = LanDiscoveryConfig::from_env().map(|lan_config| {
            tracing::info!(
                agent_id = %lan_config.agent_id,
                broadcast_port = lan_config.broadcast_port,
                local_services = lan_config.local_services.len(),
                "LAN discovery configured"
            );
            Arc::new(LanDiscovery::new(lan_config))
        });

        // 初始化服务路由器
        let router_config = ServiceRouterConfig::from_env();
        let service_router = Some(Arc::new(ServiceRouter::new(
            router_config,
            lan_discovery.clone(),
            mesh_client.clone(),
        )));

        Self {
            api_key: config.api_key.clone(),
            projects,
            started_at: Utc::now(),

            task_store: TaskStore::new(),
            log_hub: LogHub::new(),
            running_deploys: RwLock::new(HashMap::new()),
            deploy_queue: RwLock::new(HashMap::new()),

            deploy_center: DeployCenterClient::new(config.callback_url.clone()),

            tunnel_mode: config.tunnel.mode.clone(),
            tunnel_client_id: config.tunnel.client_id.clone(),
            tunnel_auth_token: config.tunnel.auth_token.clone(),
            tunnel_server_state: Arc::new(TunnelServerState::new()),
            tunnel_client_state: Arc::new(TunnelClientState::new()),
            port_listener_manager: Arc::new(PortListenerManager::new()),
            tunnel_port_mappings: config.tunnel.port_mappings.clone(),
            tunnel_server_url: config.tunnel.server_url.clone(),

            nfa,
            frp,
            ssh_server: RwLock::new(None),

            auto_update_config: config.auto_update.clone(),
            auto_update_state: Arc::new(AutoUpdateState::new()),

            mesh_client,
            lan_discovery,
            service_router,

            config,
        }
    }

    /// 验证 API Key
    pub fn verify_api_key(&self, headers: &axum::http::HeaderMap) -> bool {
        headers
            .get("x-api-key")
            .and_then(|v| v.to_str().ok())
            .map_or(false, |key| key == self.api_key)
    }

    /// 获取项目配置
    pub fn get_project(&self, name: &str) -> Option<&ProjectConfig> {
        self.projects.get(name)
    }

    /// 检查项目是否有正在运行的部署
    pub async fn has_running_deploy(&self, project: &str) -> bool {
        let running = self.running_deploys.read().await;
        running.contains_key(project)
    }

    /// 注册运行中的部署
    pub async fn register_running_deploy(&self, project: &str, task_id: &str) -> CancellationToken {
        let cancel_token = CancellationToken::new();
        let mut running = self.running_deploys.write().await;
        running.insert(
            project.to_string(),
            RunningDeploy {
                task_id: task_id.to_string(),
                cancel_token: cancel_token.clone(),
            },
        );
        cancel_token
    }

    /// 取消注册运行中的部署
    pub async fn unregister_running_deploy(&self, project: &str) {
        let mut running = self.running_deploys.write().await;
        running.remove(project);
    }

    /// 获取运行中的部署任务 ID
    pub async fn get_running_deploy_task_id(&self, project: &str) -> Option<String> {
        let running = self.running_deploys.read().await;
        running.get(project).map(|d| d.task_id.clone())
    }

    /// 取消项目的部署
    pub async fn cancel_deploy(&self, project: &str) -> bool {
        let running = self.running_deploys.read().await;
        if let Some(deploy) = running.get(project) {
            deploy.cancel_token.cancel();
            true
        } else {
            false
        }
    }

    /// 获取回调 URL
    pub fn callback_url(&self) -> Option<&str> {
        self.config.callback_url.as_deref()
    }

    // ========== 队列管理方法 ==========

    /// 将部署请求加入队列，返回队列位置（从 1 开始）
    pub async fn enqueue_deploy(&self, project: &str, deploy: QueuedDeploy) -> usize {
        let mut queue = self.deploy_queue.write().await;
        let project_queue = queue.entry(project.to_string()).or_default();
        project_queue.push_back(deploy);
        project_queue.len()
    }

    /// 从队列中取出下一个部署请求
    pub async fn dequeue_deploy(&self, project: &str) -> Option<QueuedDeploy> {
        let mut queue = self.deploy_queue.write().await;
        queue.get_mut(project).and_then(|q| q.pop_front())
    }

    /// 获取项目队列长度
    pub async fn get_queue_length(&self, project: &str) -> usize {
        let queue = self.deploy_queue.read().await;
        queue.get(project).map_or(0, |q| q.len())
    }

    /// 获取任务在队列中的位置（从 1 开始，0 表示不在队列中）
    pub async fn get_queue_position(&self, project: &str, task_id: &str) -> usize {
        let queue = self.deploy_queue.read().await;
        if let Some(project_queue) = queue.get(project) {
            for (i, item) in project_queue.iter().enumerate() {
                if item.task_id == task_id {
                    return i + 1;
                }
            }
        }
        0
    }

    /// 清理超时的队列项，返回被清理的任务 ID 列表
    pub async fn cleanup_expired_queue_items(&self, timeout_secs: u64) -> Vec<(String, String)> {
        let mut queue = self.deploy_queue.write().await;
        let now = Utc::now();
        let mut expired = Vec::new();

        for (project, items) in queue.iter_mut() {
            let before_len = items.len();
            items.retain(|item| {
                let age = now - item.queued_at;
                if age.num_seconds() > timeout_secs as i64 {
                    expired.push((project.clone(), item.task_id.clone()));
                    false
                } else {
                    true
                }
            });
            if items.len() != before_len {
                tracing::info!(
                    project = %project,
                    removed = before_len - items.len(),
                    "Cleaned up expired queue items"
                );
            }
        }

        expired
    }
}

impl Default for AppState {
    fn default() -> Self {
        Self::new()
    }
}
