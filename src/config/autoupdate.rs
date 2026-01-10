//! 自动更新配置

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::env;
use tokio::sync::RwLock;

/// 更新元数据 (从 latest.json 解析)
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct UpdateMetadata {
    pub version: String,
    pub released_at: String,
    pub changelog: Option<String>,
    pub sha256: Option<String>,
}

/// 自动更新配置
#[derive(Clone, Debug)]
pub struct AutoUpdateConfig {
    /// 是否启用
    pub enabled: bool,
    /// 存储端点 (如 https://rickyjim.oss-cn-shanghai-internal.aliyuncs.com)
    pub endpoint: String,
    /// 元数据路径 (如 releases/xjp-deploy-agent/latest.json)
    pub metadata_path: String,
    /// 二进制路径模板 (如 releases/xjp-deploy-agent/xjp-deploy-agent-{version}-linux-musl-amd64)
    pub binary_path_template: String,
    /// 检查间隔（秒）
    pub check_interval_secs: u64,
}

impl AutoUpdateConfig {
    /// 从环境变量加载配置
    pub fn from_env() -> Option<Self> {
        let enabled = env::var("AUTO_UPDATE_ENABLED")
            .map(|v| v == "true" || v == "1")
            .unwrap_or(false);

        if !enabled {
            return None;
        }

        let endpoint = env::var("UPDATE_ENDPOINT").ok()?;
        let metadata_path = env::var("UPDATE_METADATA_PATH").ok()?;
        let binary_path_template = env::var("UPDATE_BINARY_PATH_TEMPLATE").ok()?;
        let check_interval_secs = env::var("UPDATE_CHECK_INTERVAL_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(3600);

        Some(Self {
            enabled,
            endpoint,
            metadata_path,
            binary_path_template,
            check_interval_secs,
        })
    }

    /// 获取元数据 URL
    pub fn metadata_url(&self) -> String {
        format!("{}/{}", self.endpoint, self.metadata_path)
    }

    /// 获取二进制下载 URL
    pub fn binary_url(&self, version: &str) -> String {
        let path = self.binary_path_template.replace("{version}", version);
        format!("{}/{}", self.endpoint, path)
    }
}

/// 自动更新状态
pub struct AutoUpdateState {
    /// 上次检查时间
    pub last_check: RwLock<Option<DateTime<Utc>>>,
    /// 上次检查结果
    pub last_check_result: RwLock<Option<String>>,
    /// 检测到的最新版本
    pub latest_version: RwLock<Option<String>>,
    /// 是否有可用更新
    pub update_available: RwLock<bool>,
    /// 更新进度 (downloading, verifying, applying, deferred, none)
    pub update_progress: RwLock<String>,
    /// 下一次检查时间
    pub next_check: RwLock<Option<DateTime<Utc>>>,
    /// 待执行的更新（当有任务运行时延迟更新）
    pub pending_update: RwLock<Option<UpdateMetadata>>,
    /// 是否正在执行更新（阻止新任务入队）
    pub update_in_progress: RwLock<bool>,
}

impl AutoUpdateState {
    pub fn new() -> Self {
        Self {
            last_check: RwLock::new(None),
            last_check_result: RwLock::new(None),
            latest_version: RwLock::new(None),
            update_available: RwLock::new(false),
            update_progress: RwLock::new("none".to_string()),
            next_check: RwLock::new(None),
            pending_update: RwLock::new(None),
            update_in_progress: RwLock::new(false),
        }
    }

    /// 设置待执行的更新
    pub async fn set_pending_update(&self, metadata: Option<UpdateMetadata>) {
        *self.pending_update.write().await = metadata;
    }

    /// 获取待执行的更新
    pub async fn get_pending_update(&self) -> Option<UpdateMetadata> {
        self.pending_update.read().await.clone()
    }

    /// 检查是否有待执行的更新
    pub async fn has_pending_update(&self) -> bool {
        self.pending_update.read().await.is_some()
    }

    /// 设置更新进行中状态
    pub async fn set_update_in_progress(&self, in_progress: bool) {
        *self.update_in_progress.write().await = in_progress;
    }

    /// 检查是否正在更新
    pub async fn is_update_in_progress(&self) -> bool {
        *self.update_in_progress.read().await
    }
}

impl Default for AutoUpdateState {
    fn default() -> Self {
        Self::new()
    }
}

/// 自动更新状态响应（用于 health 接口）
#[derive(Clone, Debug, Serialize)]
pub struct AutoUpdateStatus {
    pub enabled: bool,
    pub current_version: String,
    pub latest_version: Option<String>,
    pub update_available: bool,
    pub last_check: Option<String>,
    pub last_check_result: Option<String>,
    pub next_check: Option<String>,
    pub update_progress: String,
}

impl AutoUpdateStatus {
    /// 创建禁用状态
    pub fn disabled(current_version: String) -> Self {
        Self {
            enabled: false,
            current_version,
            latest_version: None,
            update_available: false,
            last_check: None,
            last_check_result: None,
            next_check: None,
            update_progress: "none".to_string(),
        }
    }
}
