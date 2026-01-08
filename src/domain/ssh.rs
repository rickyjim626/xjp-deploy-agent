//! SSH 服务器相关领域模型

use serde::{Deserialize, Serialize};
use std::env;

/// SSH 认证模式
#[derive(Clone, Debug, PartialEq, Default)]
pub enum SshAuthMode {
    /// 使用 API Key 作为密码
    #[default]
    ApiKey,
    /// 使用公钥认证
    PublicKey,
    /// 同时支持两种认证方式
    Both,
}

impl SshAuthMode {
    pub fn from_str(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "api_key" | "apikey" | "password" => SshAuthMode::ApiKey,
            "pubkey" | "publickey" | "public_key" => SshAuthMode::PublicKey,
            "both" => SshAuthMode::Both,
            _ => SshAuthMode::ApiKey,
        }
    }
}

/// SSH 服务器配置
#[derive(Clone, Debug)]
pub struct SshConfig {
    /// 是否启用 SSH 服务器
    pub enabled: bool,
    /// 监听端口
    pub port: u16,
    /// 用户名 (默认 "deploy")
    pub username: String,
    /// 认证模式
    pub auth_mode: SshAuthMode,
    /// 授权公钥文件路径 (用于公钥认证)
    pub authorized_keys_file: Option<String>,
    /// 主机密钥文件路径 (如果不存在则自动生成)
    pub host_key_file: Option<String>,
    /// 会话超时秒数 (默认 3600)
    pub session_timeout_secs: u64,
    /// 空闲超时秒数 (默认 300)
    pub idle_timeout_secs: u64,
    /// 最大并发会话数 (默认 10)
    pub max_sessions: usize,
}

impl Default for SshConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            port: 2222,
            username: "deploy".to_string(),
            auth_mode: SshAuthMode::ApiKey,
            authorized_keys_file: None,
            host_key_file: None,
            session_timeout_secs: 3600,
            idle_timeout_secs: 300,
            max_sessions: 10,
        }
    }
}

impl SshConfig {
    /// 从环境变量加载 SSH 配置
    pub fn from_env() -> Self {
        let enabled = env::var("SSH_ENABLED")
            .ok()
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);

        let port = env::var("SSH_PORT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(2222);

        let username = env::var("SSH_USERNAME").unwrap_or_else(|_| "deploy".to_string());

        let auth_mode = env::var("SSH_AUTH_MODE")
            .map(|v| SshAuthMode::from_str(&v))
            .unwrap_or(SshAuthMode::ApiKey);

        let authorized_keys_file = env::var("SSH_AUTHORIZED_KEYS_FILE").ok().filter(|s| !s.is_empty());
        let host_key_file = env::var("SSH_HOST_KEY_FILE").ok().filter(|s| !s.is_empty());

        let session_timeout_secs = env::var("SSH_SESSION_TIMEOUT_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(3600);

        let idle_timeout_secs = env::var("SSH_IDLE_TIMEOUT_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(300);

        let max_sessions = env::var("SSH_MAX_SESSIONS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(10);

        Self {
            enabled,
            port,
            username,
            auth_mode,
            authorized_keys_file,
            host_key_file,
            session_timeout_secs,
            idle_timeout_secs,
            max_sessions,
        }
    }
}

/// SSH 会话信息
#[derive(Clone, Debug, Serialize)]
pub struct SshSessionInfo {
    pub session_id: String,
    pub username: String,
    pub client_addr: String,
    pub connected_at: chrono::DateTime<chrono::Utc>,
    pub last_activity: chrono::DateTime<chrono::Utc>,
}

/// SSH 服务器状态
#[derive(Clone, Debug, Serialize)]
pub struct SshServerStatus {
    pub enabled: bool,
    pub listening: bool,
    pub port: u16,
    pub active_sessions: usize,
    pub max_sessions: usize,
    pub sessions: Vec<SshSessionInfo>,
}

/// SSH 服务器健康信息 (用于 /health 响应)
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SshHealthInfo {
    pub enabled: bool,
    pub port: u16,
    pub active_sessions: usize,
}
