//! XJP Deploy Agent - 私有云部署代理
//!
//! Supports running as:
//! - Console application (default)
//! - Windows service (with `service` subcommand)
//!
//! Usage:
//! - Normal mode: `xjp-deploy-agent.exe`
//! - Install service: `xjp-deploy-agent.exe service install`
//! - Uninstall service: `xjp-deploy-agent.exe service uninstall`
//! - Run as service: `xjp-deploy-agent.exe service run` (called by SCM)

use std::sync::Arc;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

use xjp_deploy_agent::{
    api,
    config::env::constants::{QUEUE_TIMEOUT_SECS, VERSION},
    domain::{deploy::DeployStatus, tunnel::TunnelMode},
    services::{self, ssh::SshServer},
    state::AppState,
};

fn main() {
    // Parse command line arguments
    let args: Vec<String> = std::env::args().collect();

    // Handle service commands on Windows
    #[cfg(windows)]
    {
        if args.len() >= 2 && args[1] == "service" {
            handle_service_command(&args);
            return;
        }
    }

    // Normal console mode - run with tokio runtime
    let rt = tokio::runtime::Runtime::new().expect("Failed to create runtime");
    rt.block_on(async {
        init_and_run_agent().await;
    });
}

/// Handle Windows service commands
#[cfg(windows)]
fn handle_service_command(args: &[String]) {
    use xjp_deploy_agent::services::windows_service;

    if args.len() < 3 {
        println!("Usage: xjp-deploy-agent.exe service <command>");
        println!();
        println!("Commands:");
        println!("  install    Install as Windows service");
        println!("  uninstall  Uninstall the Windows service");
        println!("  start      Start the service");
        println!("  stop       Stop the service");
        println!("  status     Show service status");
        println!("  run        Run as service (called by SCM)");
        return;
    }

    let command = &args[2];
    let result = match command.as_str() {
        "install" => {
            println!("Installing Windows service...");
            windows_service::install_service()
        }
        "uninstall" => {
            println!("Uninstalling Windows service...");
            windows_service::uninstall_service()
        }
        "start" => {
            println!("Starting Windows service...");
            windows_service::start_service()
        }
        "stop" => {
            println!("Stopping Windows service...");
            windows_service::stop_service()
        }
        "status" => {
            match windows_service::query_service_status() {
                Ok(state) => {
                    println!("Service status: {:?}", state);
                    Ok(())
                }
                Err(e) => Err(e)
            }
        }
        "run" => {
            // This is called by the Service Control Manager
            // First, change to the executable directory
            if let Ok(exe_path) = std::env::current_exe() {
                if let Some(exe_dir) = exe_path.parent() {
                    let _ = std::env::set_current_dir(exe_dir);
                }
            }
            windows_service::start_service_dispatcher()
        }
        _ => {
            println!("Unknown service command: {}", command);
            return;
        }
    };

    if let Err(e) = result {
        eprintln!("Error: {}", e);
        std::process::exit(1);
    }
}

/// Initialize environment and run the agent
async fn init_and_run_agent() {
    // 0. 加载 .env 文件（如果存在）
    load_env_file();

    // Run the main agent logic
    run_agent().await;
}

/// Load environment file from various locations
fn load_env_file() {
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

/// Main agent logic - can be called from console mode or service mode
pub async fn run_agent() {
    // 1. 初始化日志
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "xjp_deploy_agent=info".into()),
        )
        .with(tracing_subscriber::fmt::layer().json())
        .init();

    tracing::info!(version = VERSION, "Starting xjp-deploy-agent");

    // 2. 创建应用状态
    let state = Arc::new(AppState::new());

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
        tokio::spawn(async move {
            services::tunnel::client::start(state_clone).await;
        });
    }

    // 3.3 自动更新 (如果启用)
    if state.auto_update_config.is_some() {
        let state_clone = state.clone();
        tokio::spawn(async move {
            services::autoupdate::start(state_clone).await;
        });
    }

    // 3.4 日志上报服务 (上报 journalctl 日志到 Deploy Center，用于离线查看)
    {
        let state_clone = state.clone();
        tokio::spawn(async move {
            services::log_reporter::start(state_clone).await;
        });
    }

    // 3.5 定期清理任务
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
