//! API 模块
//!
//! HTTP handlers 和路由组装

pub mod health;
pub mod system;
pub mod containers;
pub mod deploy;
pub mod database;
pub mod tunnel;
pub mod nfa;

use axum::Router;
use std::sync::Arc;
use tower_http::{cors::CorsLayer, trace::TraceLayer};

use crate::state::AppState;

/// 构建完整的 API 路由
///
/// 所有 handlers 已迁移到子模块
pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        // Health & Status
        .merge(health::router())
        // System
        .merge(system::router())
        // Containers
        .merge(containers::router())
        // Deploy
        .merge(deploy::router())
        // Database
        .merge(database::router())
        // Tunnel
        .merge(tunnel::router())
        // NFA
        .merge(nfa::router())
        // Middleware
        .layer(TraceLayer::new_for_http())
        .layer(CorsLayer::permissive())
        .with_state(state)
}
