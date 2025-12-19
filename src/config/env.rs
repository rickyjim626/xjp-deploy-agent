//! 环境变量配置加载

use std::env;
use tracing::warn;

use crate::config::autoupdate::AutoUpdateConfig;
use crate::domain::tunnel::{PortMapping, TunnelMode};

/// 环境配置
#[derive(Clone, Debug)]
pub struct EnvConfig {
    /// API 密钥
    pub api_key: String,
    /// 部署中心回调 URL
    pub callback_url: Option<String>,
    /// 服务监听端口
    pub port: u16,
    /// 隧道配置
    pub tunnel: TunnelConfig,
    /// 自动更新配置
    pub auto_update: Option<AutoUpdateConfig>,
}

/// 隧道配置
#[derive(Clone, Debug)]
pub struct TunnelConfig {
    /// 隧道模式
    pub mode: TunnelMode,
    /// 认证令牌
    pub auth_token: String,
    /// 服务端 URL (Client 模式)
    pub server_url: String,
    /// 端口映射 (Client 模式)
    pub port_mappings: Vec<PortMapping>,
}

impl EnvConfig {
    /// 从环境变量加载配置
    pub fn from_env() -> Self {
        // API Key - 支持旧名称兼容
        let api_key = load_with_fallback("DEPLOY_AGENT_API_KEY", "API_KEY")
            .unwrap_or_else(|| "change-me-in-production".to_string());

        // Callback URL - 支持旧名称兼容
        let callback_url = load_with_fallback("DEPLOY_CENTER_CALLBACK_URL", "CALLBACK_URL");
        if env::var("API_KEY").is_ok() || env::var("CALLBACK_URL").is_ok() {
            warn!("Deprecated environment variables detected. Please use DEPLOY_AGENT_API_KEY and DEPLOY_CENTER_CALLBACK_URL");
        }

        // Port
        let port = env::var("PORT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(9876);

        // Tunnel config
        let tunnel = TunnelConfig::from_env();

        // Auto update config
        let auto_update = AutoUpdateConfig::from_env();

        Self {
            api_key,
            callback_url,
            port,
            tunnel,
            auto_update,
        }
    }
}

impl TunnelConfig {
    /// 从环境变量加载隧道配置
    pub fn from_env() -> Self {
        let mode = env::var("TUNNEL_MODE")
            .map(|v| TunnelMode::from_str(&v))
            .unwrap_or(TunnelMode::Disabled);

        let auth_token = env::var("TUNNEL_AUTH_TOKEN")
            .unwrap_or_else(|_| "change-tunnel-token".to_string());

        let server_url = env::var("TUNNEL_SERVER_URL")
            .unwrap_or_else(|_| "ws://localhost:9878/tunnel/ws".to_string());

        let port_mappings = env::var("TUNNEL_MAPPINGS")
            .map(|v| PortMapping::parse_mappings(&v))
            .unwrap_or_default();

        Self {
            mode,
            auth_token,
            server_url,
            port_mappings,
        }
    }
}

/// 加载环境变量，支持 fallback
fn load_with_fallback(primary: &str, fallback: &str) -> Option<String> {
    env::var(primary).ok().or_else(|| env::var(fallback).ok())
}

/// 常量
pub mod constants {
    /// 部署超时（秒）
    pub const DEPLOY_TIMEOUT_SECS: u64 = 1800; // 30 分钟

    /// 心跳间隔（秒）
    pub const HEARTBEAT_INTERVAL_SECS: u64 = 15;

    /// 任务历史最大保存数量
    pub const MAX_TASK_HISTORY: usize = 100;

    /// 最大活跃任务数
    pub const MAX_ACTIVE_TASKS: usize = 50;

    /// 每个项目最大队列长度
    pub const MAX_QUEUE_SIZE: usize = 10;

    /// 队列任务超时时间（秒）- 等待超过此时间自动取消
    pub const QUEUE_TIMEOUT_SECS: u64 = 600; // 10 分钟

    /// 版本号
    pub const VERSION: &str = env!("CARGO_PKG_VERSION");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_load_with_fallback() {
        // 设置测试环境变量
        env::set_var("TEST_PRIMARY", "primary_value");
        env::set_var("TEST_FALLBACK", "fallback_value");

        assert_eq!(
            load_with_fallback("TEST_PRIMARY", "TEST_FALLBACK"),
            Some("primary_value".to_string())
        );

        env::remove_var("TEST_PRIMARY");
        assert_eq!(
            load_with_fallback("TEST_PRIMARY", "TEST_FALLBACK"),
            Some("fallback_value".to_string())
        );

        env::remove_var("TEST_FALLBACK");
        assert_eq!(load_with_fallback("TEST_PRIMARY", "TEST_FALLBACK"), None);
    }
}
