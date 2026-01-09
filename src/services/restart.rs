//! 统一的进程重启管理器
//!
//! 支持多种重启模式：
//! - Unix: 使用 systemd-run 延迟重启
//! - Windows 服务模式: 使用 schtasks 计划任务
//! - Windows 控制台模式: 使用 WMIC 创建独立进程

use std::path::PathBuf;
use thiserror::Error;

/// 重启错误类型
#[derive(Debug, Error)]
pub enum RestartError {
    #[error("Failed to execute command: {0}")]
    CommandFailed(String),

    #[error("Failed to schedule restart task")]
    ScheduleTaskFailed,

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Restart method not supported on this platform")]
    Unsupported,
}

/// 重启模式
#[derive(Debug, Clone)]
pub enum RestartMode {
    /// Windows/Linux 服务模式
    Service,
    /// 控制台模式（需要二进制路径）
    Console(PathBuf),
}

/// 统一的重启管理器
pub struct RestartManager;

impl RestartManager {
    /// 调度重启
    ///
    /// 成功返回后，调用者应该安排进程退出
    pub async fn schedule_restart(mode: RestartMode) -> Result<(), RestartError> {
        tracing::info!(?mode, "Scheduling restart");

        #[cfg(windows)]
        {
            return Self::schedule_restart_windows(mode).await;
        }

        #[cfg(unix)]
        {
            return Self::schedule_restart_unix(mode).await;
        }

        #[cfg(not(any(windows, unix)))]
        {
            Err(RestartError::Unsupported)
        }
    }

    /// Unix 重启实现
    #[cfg(unix)]
    async fn schedule_restart_unix(mode: RestartMode) -> Result<(), RestartError> {
        match mode {
            RestartMode::Service => Self::restart_unix_service().await,
            RestartMode::Console(binary_path) => Self::restart_unix_console(&binary_path).await,
        }
    }

    /// Unix 服务模式: 使用 systemd-run 延迟重启
    #[cfg(unix)]
    async fn restart_unix_service() -> Result<(), RestartError> {
        let result = tokio::process::Command::new("sudo")
            .args([
                "/usr/bin/systemd-run",
                "--on-active=2",
                "--timer-property=AccuracySec=1s",
                "/usr/bin/systemctl",
                "restart",
                "xjp-deploy-agent.service",
            ])
            .output()
            .await?;

        if result.status.success() {
            tracing::info!("Service restart scheduled via systemd-run");
            Ok(())
        } else {
            let stderr = String::from_utf8_lossy(&result.stderr);
            tracing::error!(%stderr, "Failed to schedule restart via systemd-run");
            Err(RestartError::CommandFailed(stderr.to_string()))
        }
    }

    /// Unix 控制台模式: 使用 exec 替换当前进程
    #[cfg(unix)]
    async fn restart_unix_console(binary_path: &PathBuf) -> Result<(), RestartError> {
        use std::os::unix::process::CommandExt;

        let args: Vec<String> = std::env::args().collect();

        tracing::info!(
            binary = %binary_path.display(),
            "Restarting via exec"
        );

        // exec 不会返回，除非出错
        let err = std::process::Command::new(binary_path)
            .args(&args[1..])
            .exec();

        Err(RestartError::CommandFailed(format!("exec failed: {}", err)))
    }

    /// Windows 重启实现
    #[cfg(windows)]
    async fn schedule_restart_windows(mode: RestartMode) -> Result<(), RestartError> {
        match mode {
            RestartMode::Service => Self::restart_windows_service().await,
            RestartMode::Console(binary_path) => Self::restart_windows_console(&binary_path).await,
        }
    }

    /// Windows 服务模式: 依赖服务恢复选项自动重启
    ///
    /// 最可靠的方式是利用 Windows 服务恢复机制：
    /// 1. 服务安装时配置了 failure recovery (restart/5000/restart/10000/restart/30000)
    /// 2. 当服务退出（即使 exit code 0）Windows 会在 5 秒后自动重启
    /// 3. 这种方式完全由 Windows SCM 管理，最可靠
    ///
    /// 备选方案：使用 schtasks 创建计划任务（作为双保险）
    #[cfg(windows)]
    async fn restart_windows_service() -> Result<(), RestartError> {
        // 方案1: 尝试配置服务恢复（如果尚未配置）
        // 这确保即使安装时配置失败，更新时也能配置
        // Note: sc failure expects separate arguments for the values
        let recovery_result = tokio::process::Command::new("sc")
            .args([
                "failure",
                "xjp-deploy-agent",
                "reset=",
                "0",  // 不重置计数器
                "actions=",
                "restart/3000/restart/5000/restart/10000",
            ])
            .output()
            .await;

        if let Ok(output) = recovery_result {
            if output.status.success() {
                tracing::info!("Service recovery options configured - Windows will auto-restart on exit");
                // 服务恢复已配置，直接退出进程，Windows 会自动重启
                return Ok(());
            }
        }

        // 方案2: 回退到 schtasks（双保险）
        tracing::warn!("Service recovery config failed, falling back to schtasks");
        Self::restart_via_schtasks().await
    }

    /// 使用 schtasks 创建计划任务重启服务
    #[cfg(windows)]
    async fn restart_via_schtasks() -> Result<(), RestartError> {
        use chrono::{Local, Duration};

        let task_name = "XJPDeployAgentRestart";

        // 计算 10 秒后的时间
        let run_time = Local::now() + Duration::seconds(10);
        // Windows schtasks 需要 HH:MM 格式（不带秒）
        let time_str = run_time.format("%H:%M").to_string();
        // 日期格式使用 Windows 通用格式 MM/DD/YYYY
        let date_str = run_time.format("%m/%d/%Y").to_string();

        tracing::info!(
            task_name = %task_name,
            scheduled_time = %time_str,
            scheduled_date = %date_str,
            "Creating scheduled task for service restart"
        );

        // 删除旧任务（如果存在）
        let _ = tokio::process::Command::new("schtasks")
            .args(["/Delete", "/TN", task_name, "/F"])
            .output()
            .await;

        // 创建重启命令
        let restart_cmd = "cmd /c \"net stop xjp-deploy-agent & timeout /t 3 /nobreak >nul & net start xjp-deploy-agent\"";

        // 创建新的一次性任务
        let result = tokio::process::Command::new("schtasks")
            .args([
                "/Create",
                "/TN", task_name,
                "/TR", restart_cmd,
                "/SC", "ONCE",
                "/ST", &time_str,
                "/SD", &date_str,
                "/RU", "SYSTEM",
                "/RL", "HIGHEST",
                "/F",
            ])
            .output()
            .await?;

        if result.status.success() {
            tracing::info!(
                scheduled_time = %time_str,
                "Restart scheduled via schtasks"
            );
            Ok(())
        } else {
            let stderr = String::from_utf8_lossy(&result.stderr);
            let stdout = String::from_utf8_lossy(&result.stdout);
            tracing::error!(%stderr, %stdout, "Failed to create scheduled task");

            // 回退到批处理脚本方法
            tracing::warn!("Falling back to batch script restart");
            Self::restart_windows_service_fallback().await
        }
    }

    /// Windows 服务模式回退: 使用批处理脚本
    #[cfg(windows)]
    async fn restart_windows_service_fallback() -> Result<(), RestartError> {
        use std::io::Write;

        let script_content = r#"@echo off
REM XJP Deploy Agent Restart Script
REM Wait for the calling process to exit
timeout /t 3 /nobreak > nul

REM Stop the service
net stop xjp-deploy-agent > nul 2>&1

REM Wait for service to fully stop
timeout /t 2 /nobreak > nul

REM Start the service
net start xjp-deploy-agent

REM Clean up this script
del "%~f0"
"#;

        let script_path = std::env::temp_dir().join("xjp-deploy-agent-restart.bat");

        // 写入脚本文件
        let mut file = std::fs::File::create(&script_path)?;
        file.write_all(script_content.as_bytes())?;
        drop(file);

        tracing::info!(
            script = %script_path.display(),
            "Starting fallback restart script"
        );

        // 使用 WMIC 创建完全独立的进程
        let wmic_result = tokio::process::Command::new("wmic")
            .args([
                "process",
                "call",
                "create",
                &format!("cmd /c \"{}\"", script_path.display()),
            ])
            .output()
            .await;

        match wmic_result {
            Ok(output) if output.status.success() => {
                tracing::info!("Restart script started via WMIC");
                Ok(())
            }
            _ => {
                // 最后的回退: 使用 cmd /c start
                let start_result = std::process::Command::new("cmd")
                    .args(["/C", "start", "/B", "", script_path.to_str().unwrap()])
                    .spawn();

                match start_result {
                    Ok(_) => {
                        tracing::info!("Restart script started via cmd /c start");
                        Ok(())
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "All restart methods failed");
                        Err(RestartError::CommandFailed(e.to_string()))
                    }
                }
            }
        }
    }

    /// Windows 控制台模式: 使用 WMIC 创建独立进程
    ///
    /// WMIC 创建的进程完全独立于父进程，不会随父进程退出而终止
    #[cfg(windows)]
    async fn restart_windows_console(binary_path: &PathBuf) -> Result<(), RestartError> {
        use std::io::Write;

        let binary_str = binary_path.to_string_lossy();
        let work_dir = binary_path
            .parent()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();

        // 创建批处理脚本
        let batch_script = format!(
            r#"@echo off
title XJP Deploy Agent - Auto Update
echo Waiting for old process to exit...
timeout /t 3 /nobreak > nul

echo Stopping any remaining processes...
taskkill /F /IM xjp-deploy-agent.exe > nul 2>&1

echo Waiting for file release...
timeout /t 2 /nobreak > nul

echo Starting new version...
cd /d "{work_dir}"
echo Update completed at %DATE% %TIME% >> update.log
start "" "{binary}"

echo Cleaning up...
timeout /t 2 /nobreak > nul
del "%~f0"
"#,
            binary = binary_str,
            work_dir = work_dir
        );

        let script_path = std::env::temp_dir().join("xjp-deploy-agent-update.bat");

        // 写入脚本文件
        let mut file = std::fs::File::create(&script_path)?;
        file.write_all(batch_script.as_bytes())?;
        drop(file);

        tracing::info!(
            script = %script_path.display(),
            binary = %binary_str,
            "Starting console restart script"
        );

        // 尝试使用 WMIC 创建独立进程
        let wmic_result = tokio::process::Command::new("wmic")
            .args([
                "process",
                "call",
                "create",
                &format!("cmd /c \"{}\"", script_path.display()),
            ])
            .output()
            .await;

        match wmic_result {
            Ok(output) if output.status.success() => {
                tracing::info!("Restart script started via WMIC");
                Ok(())
            }
            _ => {
                // 回退: 使用 cmd /c start 打开新窗口
                tracing::warn!("WMIC failed, falling back to cmd /c start");

                std::process::Command::new("cmd")
                    .args(["/C", "start", "", script_path.to_str().unwrap()])
                    .spawn()?;

                tracing::info!("Restart script started via cmd /c start");
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_restart_mode_debug() {
        let service_mode = RestartMode::Service;
        let console_mode = RestartMode::Console(PathBuf::from("/usr/bin/test"));

        assert!(format!("{:?}", service_mode).contains("Service"));
        assert!(format!("{:?}", console_mode).contains("Console"));
    }
}
