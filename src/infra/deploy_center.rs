//! 部署中心 HTTP Client
//!
//! 封装与部署中心的所有 HTTP 交互，复用连接池

use reqwest::Client;
use serde::Serialize;
use std::time::Duration;
use tracing::{error, info, warn};

use crate::domain::deploy::{DeployStage, DeployStatus, LogLine};
use crate::config::project::RemoteAgentConfig;

/// 部署中心客户端
///
/// 封装所有与部署中心的 HTTP 交互，包括：
/// - 状态通知 (notify_status)
/// - 日志追加 (append_log)
/// - 心跳发送 (heartbeat)
/// - 配置获取 (fetch_config)
#[derive(Clone)]
pub struct DeployCenterClient {
    client: Client,
    callback_url: Option<String>,
}

impl DeployCenterClient {
    /// 创建新的部署中心客户端
    ///
    /// # Arguments
    /// * `callback_url` - 部署中心回调 URL（可选）
    pub fn new(callback_url: Option<String>) -> Self {
        let client = Client::builder()
            .timeout(Duration::from_secs(30))
            .pool_max_idle_per_host(5)
            .pool_idle_timeout(Duration::from_secs(90))
            .build()
            .expect("Failed to create HTTP client");

        Self {
            client,
            callback_url,
        }
    }

    /// 检查是否配置了回调 URL
    pub fn has_callback(&self) -> bool {
        self.callback_url.is_some()
    }

    /// 获取回调 URL（如果配置了的话）
    pub fn callback_url(&self) -> Option<&str> {
        self.callback_url.as_deref()
    }

    /// 发送心跳
    ///
    /// 用于告知部署中心任务仍在运行
    pub async fn heartbeat(&self, task_id: &str) {
        if let Some(ref url) = self.callback_url {
            let heartbeat_url = format!("{}/api/deploy/logs/{}/heartbeat", url, task_id);
            let _ = self
                .client
                .post(&heartbeat_url)
                .timeout(Duration::from_secs(5))
                .send()
                .await;
        }
    }

    /// 追加日志行
    ///
    /// 实时发送日志到部署中心
    pub async fn append_log(&self, task_id: &str, line: &LogLine) {
        if let Some(ref url) = self.callback_url {
            let append_url = format!("{}/api/deploy/logs/{}/append", url, task_id);
            let _ = self
                .client
                .post(&append_url)
                .json(&serde_json::json!({
                    "line": line.content,
                    "stream": line.stream
                }))
                .send()
                .await;
        }
    }

    /// 追加日志（简化版，直接传入 stream 和 content）
    pub async fn append_log_simple(&self, task_id: &str, stream: &str, content: &str) {
        if let Some(ref url) = self.callback_url {
            let append_url = format!("{}/api/deploy/logs/{}/append", url, task_id);
            let _ = self
                .client
                .post(&append_url)
                .json(&serde_json::json!({
                    "line": content,
                    "stream": stream
                }))
                .send()
                .await;
        }
    }

    /// 通知部署状态（带阶段信息）
    ///
    /// 使用重试机制确保状态更新不会丢失
    pub async fn notify_status_with_stages(
        &self,
        task_id: &str,
        project: &str,
        status: &DeployStatus,
        exit_code: i32,
        stages: &[DeployStage],
    ) -> Result<(), NotifyError> {
        let url = match &self.callback_url {
            Some(url) => url,
            None => return Ok(()), // 未配置回调 URL，静默返回
        };

        let status_str = status.as_str();
        let patch_url = format!("{}/api/deploy/logs/{}", url, task_id);
        let body = NotifyBody {
            status: status_str,
            exit_code,
            stages: Some(stages),
        };

        self.do_notify(&patch_url, task_id, project, status_str, &body)
            .await
    }

    /// 通知部署状态（不带阶段信息）
    pub async fn notify_status(
        &self,
        task_id: &str,
        project: &str,
        status: &DeployStatus,
        exit_code: i32,
    ) -> Result<(), NotifyError> {
        let url = match &self.callback_url {
            Some(url) => url,
            None => return Ok(()),
        };

        let status_str = status.as_str();
        let patch_url = format!("{}/api/deploy/logs/{}", url, task_id);
        let body = NotifyBodySimple {
            status: status_str,
            exit_code,
        };

        self.do_notify(&patch_url, task_id, project, status_str, &body)
            .await
    }

    /// 执行通知（带重试）
    async fn do_notify<T: Serialize>(
        &self,
        url: &str,
        task_id: &str,
        project: &str,
        status_str: &str,
        body: &T,
    ) -> Result<(), NotifyError> {
        let mut last_error = None;

        for attempt in 1..=3 {
            match self
                .client
                .patch(url)
                .timeout(Duration::from_secs(10))
                .json(body)
                .send()
                .await
            {
                Ok(resp) => {
                    if resp.status().is_success() {
                        info!(
                            task_id = %task_id,
                            project = %project,
                            status = %status_str,
                            attempt = attempt,
                            "Notified deploy center"
                        );
                        return Ok(());
                    } else {
                        warn!(
                            task_id = %task_id,
                            project = %project,
                            status = %resp.status(),
                            attempt = attempt,
                            "Deploy center returned non-success status"
                        );
                    }
                }
                Err(e) => {
                    warn!(
                        task_id = %task_id,
                        project = %project,
                        error = %e,
                        attempt = attempt,
                        "Failed to notify deploy center, will retry"
                    );
                    last_error = Some(e);
                }
            }

            // 重试前等待
            if attempt < 3 {
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }

        // 所有重试都失败了
        error!(
            task_id = %task_id,
            project = %project,
            "Failed to notify deploy center after 3 attempts"
        );

        match last_error {
            Some(e) => Err(NotifyError::Network(e)),
            None => Err(NotifyError::NonSuccessStatus),
        }
    }

    /// 从部署中心获取项目配置
    pub async fn fetch_config(&self, project_slug: &str) -> Option<RemoteAgentConfig> {
        let url = self.callback_url.as_ref()?;
        let config_url = format!("{}/api/deploy/agent-config/{}", url, project_slug);

        match self
            .client
            .get(&config_url)
            .timeout(Duration::from_secs(10))
            .send()
            .await
        {
            Ok(response) => {
                if response.status().is_success() {
                    match response.json::<RemoteAgentConfigResponse>().await {
                        Ok(remote_config) => {
                            info!(
                                project = %project_slug,
                                deploy_type = %remote_config.config.deploy_type,
                                "Fetched project config from deploy center"
                            );
                            Some(remote_config.config)
                        }
                        Err(e) => {
                            warn!(project = %project_slug, error = %e, "Failed to parse remote config");
                            None
                        }
                    }
                } else {
                    info!(
                        project = %project_slug,
                        status = %response.status(),
                        "No config found in deploy center, will use local config"
                    );
                    None
                }
            }
            Err(e) => {
                warn!(project = %project_slug, error = %e, "Failed to fetch config from deploy center");
                None
            }
        }
    }
}

/// 通知错误类型
#[derive(Debug)]
pub enum NotifyError {
    /// 网络错误
    Network(reqwest::Error),
    /// 服务端返回非成功状态码
    NonSuccessStatus,
}

impl std::fmt::Display for NotifyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NotifyError::Network(e) => write!(f, "Network error: {}", e),
            NotifyError::NonSuccessStatus => write!(f, "Server returned non-success status"),
        }
    }
}

impl std::error::Error for NotifyError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            NotifyError::Network(e) => Some(e),
            NotifyError::NonSuccessStatus => None,
        }
    }
}

/// 通知请求体（带阶段）
#[derive(Serialize)]
struct NotifyBody<'a> {
    status: &'a str,
    exit_code: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    stages: Option<&'a [DeployStage]>,
}

/// 通知请求体（简单版）
#[derive(Serialize)]
struct NotifyBodySimple<'a> {
    status: &'a str,
    exit_code: i32,
}

/// 远程配置响应
#[derive(serde::Deserialize)]
struct RemoteAgentConfigResponse {
    config: RemoteAgentConfig,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_client_without_callback() {
        let client = DeployCenterClient::new(None);
        assert!(!client.has_callback());
        assert!(client.callback_url().is_none());
    }

    #[test]
    fn test_client_with_callback() {
        let client = DeployCenterClient::new(Some("https://example.com".to_string()));
        assert!(client.has_callback());
        assert_eq!(client.callback_url(), Some("https://example.com"));
    }
}
