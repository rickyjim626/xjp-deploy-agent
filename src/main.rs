//! XJP Deploy Agent - 私有云部署代理
//!
//! 重构后的精简入口点
//!
//! 当所有业务逻辑迁移到 services 层后，替换 main.rs 使用此文件

use std::sync::Arc;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

use xjp_deploy_agent::{
    api,
    config::env::constants::{QUEUE_TIMEOUT_SECS, VERSION},
    domain::{deploy::DeployStatus, tunnel::TunnelMode},
    services,
    state::AppState,
};

#[tokio::main]
async fn main() {
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

    // 3.2 FRP client (托管 frpc)
    if let Some(frp) = &state.frp {
        let frp_clone = frp.clone();
        tokio::spawn(async move {
            frp_clone.supervise().await;
        });
    } else if state.tunnel_mode == TunnelMode::Client {
        // WebSocket 隧道客户端
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

    // 3.4 定期清理任务
    {
        let state_clone = state.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(60)); // 每分钟检查一次
            loop {
                interval.tick().await;

                // 清理过期的队列项
                let expired = state_clone
                    .cleanup_expired_queue_items(QUEUE_TIMEOUT_SECS)
                    .await;
                for (project, task_id) in expired {
                    // 标记超时任务为失败
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
                    // 每 60 分钟
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
