//! XJP Deploy Agent - 私有云部署代理
//!
//! 模块化重构后的库入口

pub mod error;
pub mod middleware;
pub mod infra;
pub mod domain;
pub mod config;
pub mod state;
pub mod api;
pub mod services;

use std::sync::Arc;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

use crate::{
    config::env::constants::{QUEUE_TIMEOUT_SECS, VERSION},
    domain::{deploy::DeployStatus, tunnel::TunnelMode},
    services::ssh::SshServer,
    state::AppState,
};

/// 运行时配置（由命令行参数提供）
#[derive(Debug, Clone, Default)]
pub struct RuntimeConfig {
    /// 覆盖监听端口
    pub port_override: Option<u16>,
    /// Canary 模式（禁用自动更新）
    pub canary_mode: bool,
    /// Takeover 模式（零断线更新）
    /// 如果设置，客户端会发送 Takeover 请求而不是 Config
    pub takeover_session_id: Option<String>,
}

/// Load environment file from various locations
pub fn load_env_file() {
    let env_paths = [
        std::path::PathBuf::from("config.env"),
        std::path::PathBuf::from(".env"),
        std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.join("config.env")))
            .unwrap_or_default(),
        std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.join(".env")))
            .unwrap_or_default(),
    ];

    for path in &env_paths {
        if path.exists() {
            // Use from_path() instead of from_path_override() to preserve
            // environment variables set via command line (e.g., canary PORT)
            match dotenvy::from_path(path) {
                Ok(_) => {
                    eprintln!("[init] Loaded env from: {}", path.display());
                    break;
                }
                Err(e) => {
                    eprintln!("[init] Failed to load {}: {}", path.display(), e);
                }
            }
        }
    }
}

/// Initialize environment and run the agent (默认配置)
pub async fn init_and_run_agent() {
    init_and_run_agent_with_config(RuntimeConfig::default()).await;
}

/// Initialize environment and run the agent with runtime config
pub async fn init_and_run_agent_with_config(config: RuntimeConfig) {
    // Load .env file if exists
    load_env_file();
    // Run the main agent logic
    run_agent_with_config(config).await;
}

/// Main agent logic - can be called from console mode or service mode (默认配置)
pub async fn run_agent() {
    run_agent_with_config(RuntimeConfig::default()).await;
}

/// Main agent logic with runtime config
pub async fn run_agent_with_config(runtime_config: RuntimeConfig) {
    // 1. 初始化日志
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "xjp_deploy_agent=info".into()),
        )
        .with(tracing_subscriber::fmt::layer().json())
        .init();

    let mode_str = if runtime_config.canary_mode { " (canary mode)" } else { "" };
    tracing::info!(version = VERSION, mode = mode_str, "Starting xjp-deploy-agent");

    // 2. 创建应用状态（应用运行时配置覆盖）
    let mut state = AppState::new();

    // 应用端口覆盖
    if let Some(port) = runtime_config.port_override {
        tracing::info!(port = port, "Port overridden by command line");
        state.config.port = port;
    }

    // Canary 模式禁用自动更新
    if runtime_config.canary_mode {
        tracing::info!("Canary mode: auto-update disabled");
        state.auto_update_config = None;
    }

    let state = Arc::new(state);

    tracing::info!(
        port = state.config.port,
        tunnel_mode = ?state.tunnel_mode,
        auto_update = state.auto_update_config.is_some(),
        projects = state.projects.len(),
        "Configuration loaded"
    );

    // 3. 启动后台任务
    // 3.1 NFA supervisor (Windows 节点)
    if let Some(nfa) = &state.nfa {
        let nfa_clone = nfa.clone();
        tokio::spawn(async move {
            nfa_clone.supervise().await;
        });
    }

    // 3.1.1 SSH 服务器 (如果启用)
    if state.config.ssh.enabled {
        let ssh_config = state.config.ssh.clone();
        let api_key = state.config.api_key.clone();
        let state_clone = state.clone();
        tokio::spawn(async move {
            match SshServer::new(ssh_config.clone(), api_key).await {
                Ok(server) => {
                    tracing::info!(port = ssh_config.port, "SSH server starting");
                    let server = Arc::new(server);
                    // 存储到 AppState 以便 health API 获取状态
                    *state_clone.ssh_server.write().await = Some(server.clone());
                    if let Err(e) = server.run().await {
                        tracing::error!(error = %e, "SSH server error");
                    }
                }
                Err(e) => {
                    tracing::error!(error = %e, "Failed to create SSH server");
                }
            }
        });
    }

    // 3.2 FRP client (托管 frpc)
    if let Some(frp) = &state.frp {
        let frp_clone = frp.clone();
        tokio::spawn(async move {
            frp_clone.supervise().await;
        });
    }

    // 3.2.1 WebSocket 隧道客户端
    if state.tunnel_mode == TunnelMode::Client {
        let state_clone = state.clone();
        let shutdown_token = state::app_state::get_shutdown_token();

        // 检查是否是 Takeover 模式
        if let Some(session_id) = runtime_config.takeover_session_id.clone() {
            tracing::info!(session_id = %session_id, "Starting tunnel client in takeover mode");
            tokio::spawn(async move {
                let takeover_config = services::tunnel::client::TakeoverConfig {
                    session_id,
                    signal_file: None,
                };
                let result = services::tunnel::client::start_takeover(
                    state_clone,
                    takeover_config,
                    shutdown_token,
                ).await;
                match result {
                    services::tunnel::client::ClientResult::TakeoverSuccess => {
                        tracing::info!("Takeover successful, running normally");
                    }
                    services::tunnel::client::ClientResult::TakeoverFailed(e) => {
                        tracing::error!(error = %e, "Takeover failed, exiting");
                        std::process::exit(1);
                    }
                    _ => {
                        tracing::info!(?result, "Tunnel client finished");
                    }
                }
            });
        } else {
            // 正常模式
            tokio::spawn(async move {
                services::tunnel::client::start(state_clone, shutdown_token).await;
            });
        }
    }

    // 3.3 Service Mesh 客户端 (如果启用)
    if let Some(ref mesh_client) = state.mesh_client {
        let mesh_clone = mesh_client.clone();
        tokio::spawn(async move {
            // 启动 Mesh 客户端（注册 Agent）
            if let Err(e) = mesh_clone.start().await {
                tracing::warn!(error = %e, "Failed to start mesh client, continuing without mesh");
            } else {
                // 注册成功后启动心跳
                mesh_clone.start_heartbeat();
            }
        });
    }

    // 3.4 自动更新 (如果启用)
    if state.auto_update_config.is_some() {
        let state_clone = state.clone();
        tokio::spawn(async move {
            services::autoupdate::start(state_clone).await;
        });
    }

    // 3.5 日志上报服务 (上报 journalctl 日志到 Deploy Center，用于离线查看)
    {
        let state_clone = state.clone();
        tokio::spawn(async move {
            services::log_reporter::start(state_clone).await;
        });
    }

    // 3.6 定期清理任务
    {
        let state_clone = state.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
            loop {
                interval.tick().await;

                // 清理过期的队列项
                let expired = state_clone
                    .cleanup_expired_queue_items(QUEUE_TIMEOUT_SECS)
                    .await;
                for (project, task_id) in expired {
                    state_clone
                        .task_store
                        .update_status(&task_id, DeployStatus::Failed, Some(-1))
                        .await;
                    tracing::warn!(
                        task_id = %task_id,
                        project = %project,
                        "Queue item expired after {} seconds",
                        QUEUE_TIMEOUT_SECS
                    );
                }

                // 清理过期的任务和日志（每小时执行一次）
                static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
                let count = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if count % 60 == 0 {
                    state_clone.task_store.cleanup_stale().await;
                    state_clone.log_hub.cleanup_expired(24).await;
                    tracing::debug!("Completed periodic cleanup");
                }
            }
        });
    }

    // 4. 构建路由
    let app = api::router(state.clone());

    // 5. 启动 HTTP server
    let addr = format!("0.0.0.0:{}", state.config.port);
    tracing::info!(address = %addr, "Starting HTTP server");

    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .expect("Failed to bind address");

    axum::serve(listener, app)
        .await
        .expect("Server error");
}
