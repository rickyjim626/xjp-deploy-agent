//! 日志通道管理
//!
//! 管理任务日志的广播通道，支持 SSE 订阅和自动清理

use chrono::{DateTime, Utc};
use std::collections::HashMap;
use tokio::sync::{broadcast, RwLock};

use crate::domain::deploy::LogLine;

/// 日志通道容量
const LOG_CHANNEL_CAPACITY: usize = 256;

/// 日志通道信息
struct LogChannel {
    /// 广播发送者
    sender: broadcast::Sender<LogLine>,
    /// 创建时间
    created_at: DateTime<Utc>,
    /// 是否已完成
    finished: bool,
}

/// 日志中心
///
/// 管理任务日志的广播通道，解决 log_channels 只创建不删除的问题
pub struct LogHub {
    /// 通道映射 (task_id -> LogChannel)
    channels: RwLock<HashMap<String, LogChannel>>,
}

impl LogHub {
    /// 创建新的日志中心
    pub fn new() -> Self {
        Self {
            channels: RwLock::new(HashMap::new()),
        }
    }

    /// 创建新的日志通道
    ///
    /// 如果通道已存在，返回现有的发送者
    pub async fn create(&self, task_id: &str) -> broadcast::Sender<LogLine> {
        let mut channels = self.channels.write().await;

        if let Some(channel) = channels.get(task_id) {
            return channel.sender.clone();
        }

        let (sender, _) = broadcast::channel(LOG_CHANNEL_CAPACITY);
        channels.insert(
            task_id.to_string(),
            LogChannel {
                sender: sender.clone(),
                created_at: Utc::now(),
                finished: false,
            },
        );

        sender
    }

    /// 订阅日志通道
    ///
    /// 返回接收者，如果通道不存在或已完成返回 None
    pub async fn subscribe(&self, task_id: &str) -> Option<broadcast::Receiver<LogLine>> {
        let channels = self.channels.read().await;
        channels.get(task_id).map(|c| c.sender.subscribe())
    }

    /// 获取发送者
    pub async fn get_sender(&self, task_id: &str) -> Option<broadcast::Sender<LogLine>> {
        let channels = self.channels.read().await;
        channels.get(task_id).map(|c| c.sender.clone())
    }

    /// 标记通道完成
    ///
    /// 通道完成后，SSE 客户端应收到完成事件并关闭连接
    pub async fn finish(&self, task_id: &str) {
        let mut channels = self.channels.write().await;
        if let Some(channel) = channels.get_mut(task_id) {
            channel.finished = true;
        }
    }

    /// 检查通道是否已完成
    pub async fn is_finished(&self, task_id: &str) -> bool {
        let channels = self.channels.read().await;
        channels.get(task_id).map_or(true, |c| c.finished)
    }

    /// 检查通道是否存在
    pub async fn exists(&self, task_id: &str) -> bool {
        let channels = self.channels.read().await;
        channels.contains_key(task_id)
    }

    /// 清理已完成的通道
    ///
    /// 移除已完成且没有活跃订阅者的通道
    pub async fn cleanup(&self) {
        let mut channels = self.channels.write().await;
        channels.retain(|_, channel| {
            // 保留未完成的通道
            if !channel.finished {
                return true;
            }
            // 保留有活跃订阅者的通道
            channel.sender.receiver_count() > 0
        });
    }

    /// 清理所有过期通道
    ///
    /// 移除创建时间超过指定时长的已完成通道
    pub async fn cleanup_expired(&self, max_age_hours: i64) {
        let now = Utc::now();
        let mut channels = self.channels.write().await;

        channels.retain(|_, channel| {
            let age = now - channel.created_at;
            // 保留未过期的通道
            if age.num_hours() < max_age_hours {
                return true;
            }
            // 过期的通道只保留未完成且有订阅者的
            !channel.finished || channel.sender.receiver_count() > 0
        });
    }

    /// 获取通道数量
    pub async fn count(&self) -> usize {
        let channels = self.channels.read().await;
        channels.len()
    }

    /// 获取活跃通道数量（未完成）
    pub async fn active_count(&self) -> usize {
        let channels = self.channels.read().await;
        channels.values().filter(|c| !c.finished).count()
    }
}

impl Default for LogHub {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_create_and_subscribe() {
        let hub = LogHub::new();

        // 创建通道
        let sender = hub.create("task-1").await;
        assert!(hub.exists("task-1").await);

        // 订阅
        let mut receiver = hub.subscribe("task-1").await.unwrap();

        // 发送日志
        let _ = sender.send(LogLine::stdout("Hello"));

        // 接收日志
        let line = receiver.recv().await.unwrap();
        assert_eq!(line.content, "Hello");
    }

    #[tokio::test]
    async fn test_finish_and_cleanup() {
        let hub = LogHub::new();

        hub.create("task-1").await;
        assert!(!hub.is_finished("task-1").await);

        hub.finish("task-1").await;
        assert!(hub.is_finished("task-1").await);

        // 没有订阅者时，cleanup 应该移除通道
        hub.cleanup().await;
        assert!(!hub.exists("task-1").await);
    }

    #[tokio::test]
    async fn test_cleanup_preserves_active_subscribers() {
        let hub = LogHub::new();

        hub.create("task-1").await;
        let _receiver = hub.subscribe("task-1").await; // 保持订阅者存活

        hub.finish("task-1").await;
        hub.cleanup().await;

        // 有订阅者时，通道应该保留
        assert!(hub.exists("task-1").await);
    }
}
