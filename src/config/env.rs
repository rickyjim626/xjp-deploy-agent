//! 环境变量配置加载

use std::env;
use tracing::warn;

use crate::config::autoupdate::AutoUpdateConfig;
use crate::domain::ssh::SshConfig;
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
    /// NFA 服务配置（Windows 节点）
    pub nfa: NfaConfig,
    /// FRP 客户端配置（由 deploy-agent 托管 frpc）
    pub frp: FrpConfig,
    /// 自动更新配置
    pub auto_update: Option<AutoUpdateConfig>,
    /// SSH 服务器配置
    pub ssh: SshConfig,
}

/// 隧道配置
#[derive(Clone, Debug)]
pub struct TunnelConfig {
    /// 隧道模式
    pub mode: TunnelMode,
    /// 客户端 ID (Client 模式，用于标识不同客户端)
    pub client_id: String,
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

        // NFA config
        let nfa = NfaConfig::from_env();

        // FRP config
        let frp = FrpConfig::from_env(&tunnel);

        // Auto update config
        let auto_update = AutoUpdateConfig::from_env();

        // SSH config
        let ssh = SshConfig::from_env();

        Self {
            api_key,
            callback_url,
            port,
            tunnel,
            nfa,
            frp,
            auto_update,
            ssh,
        }
    }
}

/// NFA 配置
#[derive(Clone, Debug)]
pub struct NfaConfig {
    pub enabled: bool,
    pub auto_start: bool,
    pub python_path: String,
    pub script_path: String,
    pub port: u16,
    pub max_concurrent: usize,
    pub poll_interval_ms: u64,
    pub job_timeout_secs: u64,
    pub restart_backoff_ms: u64,
}

impl NfaConfig {
    pub fn from_env() -> Self {
        let enabled = env::var("NFA_ENABLED")
            .ok()
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);

        let auto_start = env::var("NFA_AUTO_START")
            .ok()
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(true);

        let python_path = env::var("NFA_PYTHON_PATH").unwrap_or_else(|_| "python".to_string());
        let script_path = env::var("NFA_SCRIPT_PATH")
            .unwrap_or_else(|_| "C:\\alignment-service\\nfa_service.py".to_string());

        let port = env::var("NFA_PORT").ok().and_then(|v| v.parse().ok()).unwrap_or(9528);

        let max_concurrent = env::var("NFA_MAX_CONCURRENT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(1);

        let poll_interval_ms = env::var("NFA_POLL_INTERVAL_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(800);

        let job_timeout_secs = env::var("NFA_JOB_TIMEOUT_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(300);

        let restart_backoff_ms = env::var("NFA_RESTART_BACKOFF_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(1500);

        Self {
            enabled,
            auto_start,
            python_path,
            script_path,
            port,
            max_concurrent,
            poll_interval_ms,
            job_timeout_secs,
            restart_backoff_ms,
        }
    }
}

/// FRP 配置（托管 frpc）
#[derive(Clone, Debug)]
pub struct FrpConfig {
    pub enabled: bool,
    pub frpc_path: String,
    pub server_addr: String,
    pub server_port: u16,
    pub token: Option<String>,
    pub admin_addr: String,
    pub admin_port: u16,
    pub admin_user: Option<String>,
    pub admin_pwd: Option<String>,
}

impl FrpConfig {
    pub fn from_env(_tunnel: &TunnelConfig) -> Self {
        let enabled = env::var("FRP_ENABLED")
            .ok()
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);

        let frpc_path = env::var("FRPC_PATH").unwrap_or_else(|_| "frpc".to_string());
        let server_addr = env::var("FRP_SERVER_ADDR").unwrap_or_else(|_| "127.0.0.1".to_string());
        let server_port = env::var("FRP_SERVER_PORT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(7000);
        let token = env::var("FRP_TOKEN").ok().filter(|s| !s.is_empty());

        let admin_addr = env::var("FRP_ADMIN_ADDR").unwrap_or_else(|_| "127.0.0.1".to_string());
        let admin_port = env::var("FRP_ADMIN_PORT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(7400);
        let admin_user = env::var("FRP_ADMIN_USER").ok().filter(|s| !s.is_empty());
        let admin_pwd = env::var("FRP_ADMIN_PWD").ok().filter(|s| !s.is_empty());

        // FRP is only enabled when explicitly set via FRP_ENABLED=true
        // WebSocket tunnel (TUNNEL_MODE=client) is now the preferred method
        let enabled = enabled;

        Self {
            enabled,
            frpc_path,
            server_addr,
            server_port,
            token,
            admin_addr,
            admin_port,
            admin_user,
            admin_pwd,
        }
    }
}

impl TunnelConfig {
    /// 从环境变量加载隧道配置
    pub fn from_env() -> Self {
        let mode = env::var("TUNNEL_MODE")
            .map(|v| TunnelMode::from_str(&v))
            .unwrap_or(TunnelMode::Disabled);

        // 客户端 ID，默认使用主机名
        let client_id = env::var("TUNNEL_CLIENT_ID").unwrap_or_else(|_| {
            hostname::get()
                .map(|h| h.to_string_lossy().to_string())
                .unwrap_or_else(|_| "unknown".to_string())
        });

        let auth_token = env::var("TUNNEL_AUTH_TOKEN")
            .unwrap_or_else(|_| "change-tunnel-token".to_string());

        let server_url = env::var("TUNNEL_SERVER_URL")
            .unwrap_or_else(|_| "ws://localhost:9878/tunnel/ws".to_string());

        let port_mappings = env::var("TUNNEL_MAPPINGS")
            .map(|v| PortMapping::parse_mappings(&v))
            .unwrap_or_default();

        Self {
            mode,
            client_id,
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
