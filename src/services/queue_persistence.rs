//! 任务队列持久化模块
//!
//! 将部署队列持久化到本地文件，以便在 Agent 重启后恢复

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tokio::fs;
use tracing::{error, info, warn};

/// 持久化队列文件路径
const QUEUE_FILE_NAME: &str = "queue.json";

/// 获取队列文件路径
fn get_queue_file_path() -> PathBuf {
    // 优先使用环境变量指定的路径
    if let Ok(dir) = std::env::var("XJP_DATA_DIR") {
        return PathBuf::from(dir).join(QUEUE_FILE_NAME);
    }

    // 其次使用可执行文件所在目录
    if let Ok(exe_path) = std::env::current_exe() {
        if let Some(parent) = exe_path.parent() {
            return parent.join(QUEUE_FILE_NAME);
        }
    }

    // 默认使用 /opt/xjp-deploy-agent
    PathBuf::from("/opt/xjp-deploy-agent").join(QUEUE_FILE_NAME)
}

/// 持久化的任务
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PersistedTask {
    /// 任务 ID
    pub task_id: String,
    /// 项目名称
    pub project: String,
    /// 部署日志 ID（来自 Deploy Center）
    pub deploy_log_id: Option<String>,
    /// 提交哈希
    pub commit_hash: Option<String>,
    /// 分支
    pub branch: Option<String>,
    /// 任务状态 (queued, running)
    pub status: String,
    /// 入队时间
    pub queued_at: DateTime<Utc>,
    /// 开始执行时间
    pub started_at: Option<DateTime<Utc>>,
}

/// 持久化的队列
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PersistedQueue {
    /// 版本号（用于未来格式升级）
    pub version: u32,
    /// 任务列表
    pub tasks: Vec<PersistedTask>,
    /// 保存时间
    pub saved_at: DateTime<Utc>,
    /// Agent ID（用于验证）
    pub agent_id: Option<String>,
}

impl PersistedQueue {
    /// 创建新的空队列
    pub fn new() -> Self {
        Self {
            version: 1,
            tasks: Vec::new(),
            saved_at: Utc::now(),
            agent_id: std::env::var("DEPLOY_AGENT_NAME").ok(),
        }
    }

    /// 从文件加载队列
    pub async fn load() -> Option<Self> {
        let path = get_queue_file_path();

        if !path.exists() {
            return None;
        }

        match fs::read_to_string(&path).await {
            Ok(content) => {
                match serde_json::from_str::<Self>(&content) {
                    Ok(queue) => {
                        info!(
                            path = %path.display(),
                            tasks = queue.tasks.len(),
                            saved_at = %queue.saved_at,
                            "Loaded persisted queue"
                        );
                        Some(queue)
                    }
                    Err(e) => {
                        warn!(
                            path = %path.display(),
                            error = %e,
                            "Failed to parse persisted queue, ignoring"
                        );
                        None
                    }
                }
            }
            Err(e) => {
                warn!(
                    path = %path.display(),
                    error = %e,
                    "Failed to read persisted queue file"
                );
                None
            }
        }
    }

    /// 保存队列到文件（原子写入）
    pub async fn save(&self) -> anyhow::Result<()> {
        let path = get_queue_file_path();
        let temp_path = path.with_extension("json.tmp");

        // 确保目录存在
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).await?;
        }

        // 序列化
        let content = serde_json::to_string_pretty(self)?;

        // 写入临时文件
        fs::write(&temp_path, &content).await?;

        // 原子重命名
        fs::rename(&temp_path, &path).await?;

        info!(
            path = %path.display(),
            tasks = self.tasks.len(),
            "Saved persisted queue"
        );

        Ok(())
    }

    /// 清除队列文件
    pub async fn clear() -> anyhow::Result<()> {
        let path = get_queue_file_path();

        if path.exists() {
            fs::remove_file(&path).await?;
            info!(path = %path.display(), "Cleared persisted queue");
        }

        Ok(())
    }

    /// 添加任务
    pub fn add_task(&mut self, task: PersistedTask) {
        // 避免重复
        if !self.tasks.iter().any(|t| t.task_id == task.task_id) {
            self.tasks.push(task);
            self.saved_at = Utc::now();
        }
    }

    /// 移除任务
    pub fn remove_task(&mut self, task_id: &str) -> bool {
        let before_len = self.tasks.len();
        self.tasks.retain(|t| t.task_id != task_id);
        let removed = self.tasks.len() < before_len;
        if removed {
            self.saved_at = Utc::now();
        }
        removed
    }

    /// 更新任务状态
    pub fn update_task_status(&mut self, task_id: &str, status: &str, started_at: Option<DateTime<Utc>>) {
        if let Some(task) = self.tasks.iter_mut().find(|t| t.task_id == task_id) {
            task.status = status.to_string();
            if let Some(at) = started_at {
                task.started_at = Some(at);
            }
            self.saved_at = Utc::now();
        }
    }

    /// 获取所有排队中的任务
    pub fn get_queued_tasks(&self) -> Vec<&PersistedTask> {
        self.tasks.iter().filter(|t| t.status == "queued").collect()
    }

    /// 获取所有运行中的任务
    pub fn get_running_tasks(&self) -> Vec<&PersistedTask> {
        self.tasks.iter().filter(|t| t.status == "running").collect()
    }

    /// 检查队列是否为空
    pub fn is_empty(&self) -> bool {
        self.tasks.is_empty()
    }
}

impl Default for PersistedQueue {
    fn default() -> Self {
        Self::new()
    }
}

/// 队列持久化管理器
///
/// 封装了队列的加载、保存和同步逻辑
pub struct QueuePersistence {
    /// 当前队列状态
    queue: tokio::sync::RwLock<PersistedQueue>,
}

impl QueuePersistence {
    /// 创建新的持久化管理器
    pub fn new() -> Self {
        Self {
            queue: tokio::sync::RwLock::new(PersistedQueue::new()),
        }
    }

    /// 从文件加载队列
    pub async fn load(&self) -> Option<PersistedQueue> {
        if let Some(loaded) = PersistedQueue::load().await {
            let mut queue = self.queue.write().await;
            *queue = loaded.clone();
            Some(loaded)
        } else {
            None
        }
    }

    /// 添加任务并持久化
    pub async fn add_task(&self, task: PersistedTask) -> anyhow::Result<()> {
        let mut queue = self.queue.write().await;
        queue.add_task(task);
        queue.save().await
    }

    /// 移除任务并持久化
    pub async fn remove_task(&self, task_id: &str) -> anyhow::Result<bool> {
        let mut queue = self.queue.write().await;
        let removed = queue.remove_task(task_id);
        if removed {
            queue.save().await?;
        }
        Ok(removed)
    }

    /// 更新任务状态并持久化
    pub async fn update_task_status(
        &self,
        task_id: &str,
        status: &str,
        started_at: Option<DateTime<Utc>>,
    ) -> anyhow::Result<()> {
        let mut queue = self.queue.write().await;
        queue.update_task_status(task_id, status, started_at);
        queue.save().await
    }

    /// 获取当前队列快照
    pub async fn get_queue(&self) -> PersistedQueue {
        self.queue.read().await.clone()
    }

    /// 清除所有任务并删除文件
    pub async fn clear(&self) -> anyhow::Result<()> {
        let mut queue = self.queue.write().await;
        queue.tasks.clear();
        PersistedQueue::clear().await
    }
}

impl Default for QueuePersistence {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_persisted_task_serialization() {
        let task = PersistedTask {
            task_id: "test-123".to_string(),
            project: "my-project".to_string(),
            deploy_log_id: Some("log-456".to_string()),
            commit_hash: Some("abc123".to_string()),
            branch: Some("main".to_string()),
            status: "queued".to_string(),
            queued_at: Utc::now(),
            started_at: None,
        };

        let json = serde_json::to_string(&task).unwrap();
        let parsed: PersistedTask = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.task_id, task.task_id);
        assert_eq!(parsed.project, task.project);
        assert_eq!(parsed.status, task.status);
    }

    #[test]
    fn test_persisted_queue_operations() {
        let mut queue = PersistedQueue::new();

        let task = PersistedTask {
            task_id: "test-123".to_string(),
            project: "my-project".to_string(),
            deploy_log_id: None,
            commit_hash: None,
            branch: None,
            status: "queued".to_string(),
            queued_at: Utc::now(),
            started_at: None,
        };

        queue.add_task(task.clone());
        assert_eq!(queue.tasks.len(), 1);

        // 不重复添加
        queue.add_task(task);
        assert_eq!(queue.tasks.len(), 1);

        // 更新状态
        queue.update_task_status("test-123", "running", Some(Utc::now()));
        assert_eq!(queue.tasks[0].status, "running");
        assert!(queue.tasks[0].started_at.is_some());

        // 移除任务
        assert!(queue.remove_task("test-123"));
        assert!(queue.is_empty());

        // 移除不存在的任务
        assert!(!queue.remove_task("nonexistent"));
    }
}
