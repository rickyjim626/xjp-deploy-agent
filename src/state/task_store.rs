//! 任务存储
//!
//! 管理活跃任务和历史记录，自动清理过期任务

use chrono::{Duration, Utc};
use std::collections::{HashMap, VecDeque};
use tokio::sync::RwLock;

use crate::config::env::constants::{MAX_ACTIVE_TASKS, MAX_TASK_HISTORY};
use crate::domain::deploy::{DeployStatus, DeployStage, DeployTask};

/// 任务存储
///
/// 管理活跃任务和历史记录，提供清理策略
pub struct TaskStore {
    /// 活跃任务
    tasks: RwLock<HashMap<String, DeployTask>>,
    /// 历史记录
    history: RwLock<VecDeque<DeployTask>>,
    /// 最大活跃任务数
    max_active: usize,
    /// 最大历史记录数
    max_history: usize,
    /// 任务保留时间
    retention: Duration,
}

impl TaskStore {
    /// 创建新的任务存储
    pub fn new() -> Self {
        Self {
            tasks: RwLock::new(HashMap::new()),
            history: RwLock::new(VecDeque::new()),
            max_active: MAX_ACTIVE_TASKS,
            max_history: MAX_TASK_HISTORY,
            retention: Duration::hours(24),
        }
    }

    /// 使用自定义配置创建
    pub fn with_config(max_active: usize, max_history: usize, retention_hours: i64) -> Self {
        Self {
            tasks: RwLock::new(HashMap::new()),
            history: RwLock::new(VecDeque::new()),
            max_active,
            max_history,
            retention: Duration::hours(retention_hours),
        }
    }

    /// 创建新任务
    pub async fn create(&self, task: DeployTask) -> String {
        let task_id = task.id.clone();
        let mut tasks = self.tasks.write().await;
        tasks.insert(task_id.clone(), task);
        task_id
    }

    /// 获取活跃任务
    pub async fn get(&self, task_id: &str) -> Option<DeployTask> {
        let tasks = self.tasks.read().await;
        tasks.get(task_id).cloned()
    }

    /// 获取任务（优先活跃任务，然后查历史记录）
    pub async fn get_any(&self, task_id: &str) -> Option<DeployTask> {
        // 先查活跃任务
        if let Some(task) = self.get(task_id).await {
            return Some(task);
        }
        // 再查历史记录
        let history = self.history.read().await;
        history.iter().find(|t| t.id == task_id).cloned()
    }

    /// 获取所有活跃任务
    pub async fn get_all(&self) -> Vec<DeployTask> {
        let tasks = self.tasks.read().await;
        tasks.values().cloned().collect()
    }

    /// 检查任务是否存在
    pub async fn exists(&self, task_id: &str) -> bool {
        let tasks = self.tasks.read().await;
        tasks.contains_key(task_id)
    }

    /// 更新任务状态
    pub async fn update_status(
        &self,
        task_id: &str,
        status: DeployStatus,
        exit_code: Option<i32>,
    ) {
        let mut tasks = self.tasks.write().await;
        if let Some(task) = tasks.get_mut(task_id) {
            task.status = status;
            task.exit_code = exit_code;
            if task.status.is_terminal() {
                task.finished_at = Some(Utc::now());
            }
        }
    }

    /// 更新任务阶段
    pub async fn update_stages(&self, task_id: &str, stages: Vec<DeployStage>) {
        let mut tasks = self.tasks.write().await;
        if let Some(task) = tasks.get_mut(task_id) {
            task.stages = stages;
        }
    }

    /// 完成任务并移到历史记录
    pub async fn finish(&self, task_id: &str, status: DeployStatus, exit_code: Option<i32>) {
        // 从活跃任务中移除
        let task = {
            let mut tasks = self.tasks.write().await;
            if let Some(mut task) = tasks.remove(task_id) {
                task.status = status;
                task.exit_code = exit_code;
                task.finished_at = Some(Utc::now());
                Some(task)
            } else {
                None
            }
        };

        // 添加到历史记录
        if let Some(task) = task {
            self.add_to_history(task).await;
        }
    }

    /// 添加任务到历史记录
    pub async fn add_to_history(&self, task: DeployTask) {
        let mut history = self.history.write().await;
        history.push_front(task);

        // 限制历史记录大小
        while history.len() > self.max_history {
            history.pop_back();
        }
    }

    /// 获取历史记录
    pub async fn get_history(
        &self,
        limit: usize,
        project: Option<&str>,
        status: Option<&str>,
    ) -> Vec<DeployTask> {
        let history = self.history.read().await;

        history
            .iter()
            .filter(|task| {
                let project_match = project.map_or(true, |p| task.project == p);
                let status_match = status.map_or(true, |s| task.status.as_str() == s);
                project_match && status_match
            })
            .take(limit)
            .cloned()
            .collect()
    }

    /// 获取历史记录总数
    pub async fn history_count(&self) -> usize {
        let history = self.history.read().await;
        history.len()
    }

    /// 清理过期任务
    ///
    /// 移除超过保留时间的已完成任务
    pub async fn cleanup_stale(&self) {
        let now = Utc::now();
        let cutoff = now - self.retention;

        // 清理活跃任务中的过期任务
        {
            let mut tasks = self.tasks.write().await;
            tasks.retain(|_, task| {
                // 保留运行中的任务和未过期的任务
                !task.status.is_terminal()
                    || task.finished_at.map_or(true, |t| t > cutoff)
            });
        }

        // 清理历史记录中的过期任务
        {
            let mut history = self.history.write().await;
            history.retain(|task| task.finished_at.map_or(true, |t| t > cutoff));
        }
    }

    /// 获取活跃任务数量
    pub async fn active_count(&self) -> usize {
        let tasks = self.tasks.read().await;
        tasks.len()
    }

    /// 检查是否达到活跃任务上限
    pub async fn is_at_capacity(&self) -> bool {
        self.active_count().await >= self.max_active
    }
}

impl Default for TaskStore {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_task_lifecycle() {
        let store = TaskStore::new();

        // 创建任务
        let task = DeployTask::new("task-1".to_string(), "test-project".to_string());
        store.create(task).await;

        // 获取任务
        let task = store.get("task-1").await;
        assert!(task.is_some());
        assert_eq!(task.unwrap().project, "test-project");

        // 完成任务
        store
            .finish("task-1", DeployStatus::Success, Some(0))
            .await;

        // 任务从活跃列表移除
        assert!(store.get("task-1").await.is_none());

        // 任务在历史记录中
        let history = store.get_history(10, None, None).await;
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].id, "task-1");
    }

    #[tokio::test]
    async fn test_history_limit() {
        let store = TaskStore::with_config(10, 5, 24);

        for i in 0..10 {
            let mut task = DeployTask::new(format!("task-{}", i), "test".to_string());
            task.status = DeployStatus::Success;
            task.finished_at = Some(Utc::now());
            store.add_to_history(task).await;
        }

        // 只保留最近 5 个
        assert_eq!(store.history_count().await, 5);
    }
}
