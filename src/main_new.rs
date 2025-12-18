//! XJP Deploy Agent - 私有云部署代理
//!
//! 重构后的精简入口点
//!
//! 当所有业务逻辑迁移到 services 层后，替换 main.rs 使用此文件

use std::sync::Arc;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

use xjp_deploy_agent::{
    api,
    config::env::constants::VERSION,
    domain::tunnel::TunnelMode,
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
    // 3.1 隧道客户端 (如果启用)
    if state.tunnel_mode == TunnelMode::Client {
        let state_clone = state.clone();
        tokio::spawn(async move {
            // TODO: services::tunnel::client::start(state_clone).await;
            tracing::info!("Tunnel client task - implementation pending");
        });
    }

    // 3.2 自动更新 (如果启用)
    if state.auto_update_config.is_some() {
        let state_clone = state.clone();
        tokio::spawn(async move {
            services::autoupdate::start(state_clone).await;
        });
    }

    // 3.3 定期清理任务
    {
        let state_clone = state.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(3600));
            loop {
                interval.tick().await;
                state_clone.task_store.cleanup_stale().await;
                state_clone.log_hub.cleanup_expired(24).await;
                tracing::debug!("Completed periodic cleanup");
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
