//! 命令执行器
//!
//! 提供统一的命令执行接口，支持：
//! - 实时日志流式输出
//! - 超时控制
//! - 取消支持
//! - stdout/stderr 分离

use std::path::Path;
use std::process::ExitStatus;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use tracing::{error, warn};

use crate::domain::deploy::LogLine;

/// 命令执行器
pub struct CommandRunner;

/// 命令执行错误
#[derive(Debug)]
pub enum CommandError {
    /// 命令启动失败
    SpawnFailed(std::io::Error),
    /// 命令超时
    Timeout,
    /// 命令被取消
    Cancelled,
    /// 等待命令完成失败
    WaitFailed(std::io::Error),
}

impl std::fmt::Display for CommandError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CommandError::SpawnFailed(e) => write!(f, "Failed to spawn command: {}", e),
            CommandError::Timeout => write!(f, "Command timed out"),
            CommandError::Cancelled => write!(f, "Command was cancelled"),
            CommandError::WaitFailed(e) => write!(f, "Failed to wait for command: {}", e),
        }
    }
}

impl std::error::Error for CommandError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            CommandError::SpawnFailed(e) | CommandError::WaitFailed(e) => Some(e),
            _ => None,
        }
    }
}

/// 命令执行结果
pub struct CommandResult {
    /// 退出状态
    pub status: ExitStatus,
    /// 是否因超时而终止
    pub timed_out: bool,
}

impl CommandRunner {
    /// 执行命令并流式输出日志
    ///
    /// # Arguments
    /// * `program` - 要执行的程序
    /// * `args` - 命令行参数
    /// * `work_dir` - 工作目录
    /// * `log_tx` - 日志发送通道
    /// * `cancel` - 取消令牌
    /// * `timeout` - 超时时间
    ///
    /// # Returns
    /// 命令执行结果或错误
    pub async fn run_with_streaming(
        program: &str,
        args: &[&str],
        work_dir: &Path,
        log_tx: broadcast::Sender<LogLine>,
        cancel: CancellationToken,
        timeout: Duration,
    ) -> Result<CommandResult, CommandError> {
        let mut child = Command::new(program)
            .args(args)
            .current_dir(work_dir)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(CommandError::SpawnFailed)?;

        let stdout = child.stdout.take();
        let stderr = child.stderr.take();

        // 启动 stdout 读取任务
        let stdout_tx = log_tx.clone();
        let stdout_task = tokio::spawn(async move {
            if let Some(stdout) = stdout {
                let reader = BufReader::new(stdout);
                let mut lines = reader.lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let log_line = LogLine::new("stdout", line);
                    let _ = stdout_tx.send(log_line);
                }
            }
        });

        // 启动 stderr 读取任务
        let stderr_tx = log_tx.clone();
        let stderr_task = tokio::spawn(async move {
            if let Some(stderr) = stderr {
                let reader = BufReader::new(stderr);
                let mut lines = reader.lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let log_line = LogLine::new("stderr", line);
                    let _ = stderr_tx.send(log_line);
                }
            }
        });

        // 等待命令完成，支持超时和取消
        let result = tokio::select! {
            _ = cancel.cancelled() => {
                warn!("Command cancelled, killing process");
                let _ = child.kill().await;
                Err(CommandError::Cancelled)
            }
            _ = tokio::time::sleep(timeout) => {
                error!("Command timed out after {:?}", timeout);
                let _ = child.kill().await;
                // 等待进程实际终止
                let status = child.wait().await.map_err(CommandError::WaitFailed)?;
                Ok(CommandResult { status, timed_out: true })
            }
            status = child.wait() => {
                let status = status.map_err(CommandError::WaitFailed)?;
                Ok(CommandResult { status, timed_out: false })
            }
        };

        // 等待日志读取完成
        let _ = stdout_task.await;
        let _ = stderr_task.await;

        result
    }

    /// 执行简单命令（无流式输出）
    ///
    /// 用于不需要实时日志的场景（如 git pull）
    pub async fn run_simple(
        program: &str,
        args: &[&str],
        work_dir: &Path,
        timeout: Duration,
    ) -> Result<std::process::Output, CommandError> {
        let child = Command::new(program)
            .args(args)
            .current_dir(work_dir)
            .output();

        tokio::select! {
            result = child => {
                result.map_err(CommandError::SpawnFailed)
            }
            _ = tokio::time::sleep(timeout) => {
                Err(CommandError::Timeout)
            }
        }
    }

    /// 执行 shell 命令
    ///
    /// 使用 sh -c 执行命令字符串
    pub async fn run_shell_with_streaming(
        command: &str,
        work_dir: &Path,
        log_tx: broadcast::Sender<LogLine>,
        cancel: CancellationToken,
        timeout: Duration,
    ) -> Result<CommandResult, CommandError> {
        Self::run_with_streaming("sh", &["-c", command], work_dir, log_tx, cancel, timeout).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[tokio::test]
    async fn test_run_simple_success() {
        let result = CommandRunner::run_simple(
            "echo",
            &["hello"],
            &PathBuf::from("/tmp"),
            Duration::from_secs(5),
        )
        .await;

        assert!(result.is_ok());
        let output = result.unwrap();
        assert!(output.status.success());
        assert!(String::from_utf8_lossy(&output.stdout).contains("hello"));
    }

    #[tokio::test]
    async fn test_run_simple_not_found() {
        let result = CommandRunner::run_simple(
            "nonexistent_command_12345",
            &[],
            &PathBuf::from("/tmp"),
            Duration::from_secs(5),
        )
        .await;

        assert!(matches!(result, Err(CommandError::SpawnFailed(_))));
    }
}
