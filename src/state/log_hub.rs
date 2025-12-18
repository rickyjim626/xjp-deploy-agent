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
            },
        );

        sender
    }

    /// 订阅日志通道
    ///
    /// 返回接收者和一个标识通道是否仍然存在的 flag
    /// 如果通道不存在返回 None
    pub async fn subscribe(&self, task_id: &str) -> Option<broadcast::Receiver<LogLine>> {
        let channels = self.channels.read().await;
        channels.get(task_id).map(|c| c.sender.subscribe())
    }

    /// 获取发送者
    pub async fn get_sender(&self, task_id: &str) -> Option<broadcast::Sender<LogLine>> {
        let channels = self.channels.read().await;
        channels.get(task_id).map(|c| c.sender.clone())
    }

    /// 完成并关闭通道
    ///
    /// 移除通道并 drop sender，这会导致所有 receiver 收到 RecvError::Closed
    /// SSE stream 可以据此发送 complete 事件并结束
    pub async fn finish(&self, task_id: &str) {
        let mut channels = self.channels.write().await;
        // 移除通道，sender 会被 drop，所有 receiver 将收到 Closed 错误
        channels.remove(task_id);
    }

    /// 检查通道是否存在
    pub async fn exists(&self, task_id: &str) -> bool {
        let channels = self.channels.read().await;
        channels.contains_key(task_id)
    }

    /// 清理所有过期通道
    ///
    /// 移除创建时间超过指定时长且没有活跃订阅者的通道
    pub async fn cleanup_expired(&self, max_age_hours: i64) {
        let now = Utc::now();
        let mut channels = self.channels.write().await;

        channels.retain(|task_id, channel| {
            let age = now - channel.created_at;
            // 保留未过期的通道
            if age.num_hours() < max_age_hours {
                return true;
            }
            // 过期的通道：只保留有活跃订阅者的
            let has_subscribers = channel.sender.receiver_count() > 0;
            if !has_subscribers {
                tracing::debug!(task_id = %task_id, "Removing expired log channel");
            }
            has_subscribers
        });
    }

    /// 获取通道数量
    pub async fn count(&self) -> usize {
        let channels = self.channels.read().await;
        channels.len()
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
    async fn test_finish_closes_channel() {
        let hub = LogHub::new();

        hub.create("task-1").await;
        let mut receiver = hub.subscribe("task-1").await.unwrap();

        // 完成通道
        hub.finish("task-1").await;

        // 通道应该被移除
        assert!(!hub.exists("task-1").await);

        // receiver 应该收到 Closed 错误
        let result = receiver.recv().await;
        assert!(matches!(
            result,
            Err(broadcast::error::RecvError::Closed)
        ));
    }

    #[tokio::test]
    async fn test_finish_without_subscribers() {
        let hub = LogHub::new();

        hub.create("task-1").await;

        // 没有订阅者时完成通道
        hub.finish("task-1").await;

        // 通道应该被移除
        assert!(!hub.exists("task-1").await);
    }
}
