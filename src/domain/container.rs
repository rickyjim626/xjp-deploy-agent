//! 容器相关领域模型

use serde::{Deserialize, Serialize};

/// 容器信息
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContainerInfo {
    pub id: String,
    pub name: String,
    pub image: String,
    pub status: String,
    pub state: String,
    pub created: String,
    pub ports: Vec<String>,
}

/// 容器列表响应
#[derive(Debug, Serialize)]
pub struct ContainersResponse {
    pub containers: Vec<ContainerInfo>,
}

/// 容器日志查询参数
#[derive(Debug, Deserialize)]
pub struct ContainerLogsQuery {
    /// 返回最后 N 行，默认 100
    #[serde(default = "default_log_lines")]
    pub tail: usize,
    /// 是否显示时间戳
    #[serde(default)]
    pub timestamps: bool,
    /// 只显示最近 N 秒的日志
    pub since: Option<String>,
}

fn default_log_lines() -> usize {
    100
}

/// 容器日志响应
#[derive(Debug, Serialize)]
pub struct ContainerLogsResponse {
    pub container: String,
    pub logs: Vec<String>,
    pub total_lines: usize,
}

/// 环境变量
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnvVar {
    pub key: String,
    pub value: String,
    /// 是否为敏感变量（如包含 PASSWORD, SECRET, KEY 等）
    #[serde(default)]
    pub sensitive: bool,
}

impl EnvVar {
    pub fn new(key: String, value: String, sensitive: bool) -> Self {
        Self {
            key,
            value,
            sensitive,
        }
    }

    /// 敏感关键词列表
    const SENSITIVE_KEYWORDS: &'static [&'static str] = &[
        "password", "secret", "key", "token", "credential", "auth",
        "api_key", "apikey", "private", "jwt", "pem", "cert",
    ];

    /// 检查变量名是否敏感
    pub fn is_sensitive_key(key: &str) -> bool {
        let key_lower = key.to_lowercase();
        Self::SENSITIVE_KEYWORDS.iter().any(|kw| key_lower.contains(kw))
    }
}

/// 容器环境变量响应
#[derive(Debug, Serialize)]
pub struct ContainerEnvResponse {
    pub container: String,
    pub env_vars: Vec<EnvVar>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_sensitive_key() {
        assert!(EnvVar::is_sensitive_key("DATABASE_PASSWORD"));
        assert!(EnvVar::is_sensitive_key("api_key"));
        assert!(EnvVar::is_sensitive_key("SECRET_TOKEN"));
        assert!(!EnvVar::is_sensitive_key("DEBUG"));
        assert!(!EnvVar::is_sensitive_key("PORT"));
    }
}
