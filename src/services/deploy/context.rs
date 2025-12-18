//! 部署上下文
//!
//! 统一的部署执行上下文，包含任务信息、日志通道等

use std::sync::Arc;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use crate::domain::deploy::{DeployStage, DeployStatus, LogLine};
use crate::state::AppState;

/// 部署执行上下文
///
/// 封装部署过程中需要的所有状态和工具
#[derive(Clone)]
pub struct DeployContext {
    /// 任务 ID
    pub task_id: String,
    /// 项目名称
    pub project: String,
    /// 应用状态
    pub state: Arc<AppState>,
    /// 日志发送通道
    pub log_tx: Option<broadcast::Sender<LogLine>>,
    /// 取消令牌
    pub cancel_token: CancellationToken,
    /// 提交哈希
    pub commit_hash: Option<String>,
    /// 分支
    pub branch: Option<String>,
}

impl DeployContext {
    /// 发送日志
    pub async fn log(&self, stream: &str, content: &str) {
        let line = LogLine::new(stream, content);

        // 发送到本地广播
        if let Some(ref tx) = self.log_tx {
            let _ = tx.send(line.clone());
        }

        // 发送到部署中心
        self.state
            .deploy_center
            .append_log(&self.task_id, &line)
            .await;
    }

    /// 发送 stdout 日志
    pub async fn log_stdout(&self, content: &str) {
        self.log("stdout", content).await;
    }

    /// 发送 stderr 日志
    pub async fn log_stderr(&self, content: &str) {
        self.log("stderr", content).await;
    }

    /// 更新任务状态
    pub async fn update_status(&self, status: DeployStatus, exit_code: Option<i32>) {
        self.state
            .task_store
            .update_status(&self.task_id, status.clone(), exit_code)
            .await;

        // 通知部署中心
        let _ = self
            .state
            .deploy_center
            .notify_status(&self.task_id, &self.project, &status, exit_code.unwrap_or(-1))
            .await;
    }

    /// 更新任务阶段
    pub async fn update_stages(&self, stages: Vec<DeployStage>) {
        self.state
            .task_store
            .update_stages(&self.task_id, stages.clone())
            .await;
    }

    /// 完成任务
    pub async fn finish(&self, status: DeployStatus, exit_code: Option<i32>, stages: Vec<DeployStage>) {
        // 更新阶段
        self.update_stages(stages.clone()).await;

        // 完成任务
        self.state
            .task_store
            .finish(&self.task_id, status.clone(), exit_code)
            .await;

        // 标记日志通道完成
        self.state.log_hub.finish(&self.task_id).await;

        // 取消注册运行中的部署
        self.state.unregister_running_deploy(&self.project).await;

        // 通知部署中心
        let _ = self
            .state
            .deploy_center
            .notify_status_with_stages(
                &self.task_id,
                &self.project,
                &status,
                exit_code.unwrap_or(-1),
                &stages,
            )
            .await;
    }

    /// 检查是否被取消
    pub fn is_cancelled(&self) -> bool {
        self.cancel_token.is_cancelled()
    }
}
