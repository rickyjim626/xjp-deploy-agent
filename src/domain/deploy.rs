//! 部署相关领域模型

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// 部署任务状态
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum DeployStatus {
    Running,
    Success,
    Failed,
}

impl DeployStatus {
    /// 转换为字符串
    pub fn as_str(&self) -> &'static str {
        match self {
            DeployStatus::Running => "running",
            DeployStatus::Success => "success",
            DeployStatus::Failed => "failed",
        }
    }

    /// 是否为终态
    pub fn is_terminal(&self) -> bool {
        matches!(self, DeployStatus::Success | DeployStatus::Failed)
    }
}

/// 阶段状态
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum StageStatus {
    Pending,
    Running,
    Success,
    Failed,
    Skipped,
}

/// 部署阶段信息
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DeployStage {
    /// 阶段标识 (e.g., "git_pull", "docker_build", "docker_push")
    pub name: String,
    /// 显示名称 (e.g., "Git Pull", "Docker Build")
    pub display_name: String,
    /// 开始时间
    pub started_at: Option<DateTime<Utc>>,
    /// 结束时间
    pub finished_at: Option<DateTime<Utc>>,
    /// 持续时间（毫秒）
    pub duration_ms: Option<i64>,
    /// 阶段状态
    pub status: StageStatus,
    /// 附加信息
    pub message: Option<String>,
}

impl DeployStage {
    /// 创建新的待执行阶段
    pub fn new(name: &str, display_name: &str) -> Self {
        Self {
            name: name.to_string(),
            display_name: display_name.to_string(),
            started_at: None,
            finished_at: None,
            duration_ms: None,
            status: StageStatus::Pending,
            message: None,
        }
    }

    /// 开始执行阶段
    pub fn start(&mut self) {
        self.started_at = Some(Utc::now());
        self.status = StageStatus::Running;
    }

    /// 完成阶段
    pub fn finish(&mut self, success: bool, message: Option<String>) {
        let now = Utc::now();
        self.finished_at = Some(now);
        self.status = if success {
            StageStatus::Success
        } else {
            StageStatus::Failed
        };
        self.message = message;
        if let Some(started) = self.started_at {
            self.duration_ms = Some((now - started).num_milliseconds());
        }
    }

    /// 跳过阶段
    pub fn skip(&mut self, reason: Option<String>) {
        self.status = StageStatus::Skipped;
        self.message = reason;
    }
}

/// 部署任务信息
#[derive(Clone, Debug, Serialize)]
pub struct DeployTask {
    pub id: String,
    pub project: String,
    pub status: DeployStatus,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    pub exit_code: Option<i32>,
    /// 部署阶段详情
    #[serde(default)]
    pub stages: Vec<DeployStage>,
}

impl DeployTask {
    /// 创建新任务
    pub fn new(id: String, project: String) -> Self {
        Self {
            id,
            project,
            status: DeployStatus::Running,
            started_at: Utc::now(),
            finished_at: None,
            exit_code: None,
            stages: Vec::new(),
        }
    }

    /// 设置任务完成
    pub fn complete(&mut self, status: DeployStatus, exit_code: Option<i32>) {
        self.status = status;
        self.finished_at = Some(Utc::now());
        self.exit_code = exit_code;
    }
}

/// 日志行
#[derive(Clone, Debug, Serialize)]
pub struct LogLine {
    pub timestamp: DateTime<Utc>,
    pub stream: String, // stdout | stderr
    pub content: String,
}

impl LogLine {
    /// 创建新日志行
    pub fn new(stream: &str, content: impl Into<String>) -> Self {
        Self {
            timestamp: Utc::now(),
            stream: stream.to_string(),
            content: content.into(),
        }
    }

    /// 创建 stdout 日志行
    pub fn stdout(content: impl Into<String>) -> Self {
        Self::new("stdout", content)
    }

    /// 创建 stderr 日志行
    pub fn stderr(content: impl Into<String>) -> Self {
        Self::new("stderr", content)
    }
}

/// 部署类型
#[derive(Clone, Debug)]
pub enum DeployType {
    /// 执行脚本（现有方式）
    Script { script: String },
    /// Docker Compose 部署（拉取镜像并重启）
    DockerCompose {
        /// docker-compose.yml 文件路径（相对于 work_dir 或绝对路径）
        compose_file: String,
        /// 要拉取的镜像（如 ghcr.io/rickyjim626/xjp-backend:latest）
        image: String,
        /// Git 仓库目录（用于 git pull 更新配置，可选）
        git_repo_dir: Option<String>,
        /// 要重启的服务名称（如果不指定则重启所有服务）
        service: Option<String>,
    },
    /// Docker Build 部署（拉取代码、构建镜像、推送到 registry）
    DockerBuild {
        /// Dockerfile 路径（相对于 work_dir，默认 "Dockerfile"）
        dockerfile: Option<String>,
        /// 构建上下文路径（相对于 work_dir，默认 "."）
        build_context: Option<String>,
        /// 目标镜像名称（包含 registry，如 ghcr.io/user/repo）
        image: String,
        /// 镜像 tag（默认使用 git commit hash）
        tag: Option<String>,
        /// 是否同时推送 latest tag
        push_latest: bool,
        /// Git 分支（默认 master）
        branch: Option<String>,
    },
    /// Xiaojincut 本地视频处理任务
    Xiaojincut {
        /// 任务类型: "import", "export", "process"
        task_type: String,
        /// 输入文件路径
        input_path: Option<String>,
        /// 输出目录
        output_dir: Option<String>,
        /// AI 提示（用于 AI 处理任务）
        prompt: Option<String>,
        /// xiaojincut 服务端口（默认 19527）
        port: Option<u16>,
    },
}

impl DeployType {
    /// 获取部署类型名称
    pub fn name(&self) -> &'static str {
        match self {
            DeployType::Script { .. } => "script",
            DeployType::DockerCompose { .. } => "docker_compose",
            DeployType::DockerBuild { .. } => "docker_build",
            DeployType::Xiaojincut { .. } => "xiaojincut",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_deploy_status_as_str() {
        assert_eq!(DeployStatus::Running.as_str(), "running");
        assert_eq!(DeployStatus::Success.as_str(), "success");
        assert_eq!(DeployStatus::Failed.as_str(), "failed");
    }

    #[test]
    fn test_deploy_status_is_terminal() {
        assert!(!DeployStatus::Running.is_terminal());
        assert!(DeployStatus::Success.is_terminal());
        assert!(DeployStatus::Failed.is_terminal());
    }

    #[test]
    fn test_deploy_stage_lifecycle() {
        let mut stage = DeployStage::new("test", "Test Stage");
        assert_eq!(stage.status, StageStatus::Pending);

        stage.start();
        assert_eq!(stage.status, StageStatus::Running);
        assert!(stage.started_at.is_some());

        stage.finish(true, Some("Done".to_string()));
        assert_eq!(stage.status, StageStatus::Success);
        assert!(stage.finished_at.is_some());
        assert!(stage.duration_ms.is_some());
    }

    #[test]
    fn test_log_line_creation() {
        let line = LogLine::stdout("Hello");
        assert_eq!(line.stream, "stdout");
        assert_eq!(line.content, "Hello");

        let line = LogLine::stderr("Error");
        assert_eq!(line.stream, "stderr");
        assert_eq!(line.content, "Error");
    }
}
