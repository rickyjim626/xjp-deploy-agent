//! 自动更新服务 (蓝绿部署策略)
//!
//! 定期检查更新并使用蓝绿部署策略自动应用：
//! 1. 下载新二进制到临时位置
//! 2. 启动 canary 实例进行健康检查
//! 3. 健康检查通过后替换并重启
//! 4. 失败则保持当前版本

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;

use sha2::{Digest, Sha256};
use tokio::fs;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::config::autoupdate::{AutoUpdateConfig, UpdateMetadata};
use crate::config::env::constants::VERSION;
use crate::state::AppState;

/// Canary 健康检查配置
const CANARY_PORT: u16 = 19999;
const CANARY_STARTUP_WAIT_SECS: u64 = 5;
const CANARY_HEALTH_CHECK_TIMEOUT_SECS: u64 = 10;

/// 手动触发更新（供 API 调用）
pub async fn trigger_update(
    state: Arc<AppState>,
    config: AutoUpdateConfig,
    metadata: UpdateMetadata,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    tracing::info!(
        version = %metadata.version,
        "Manual update triggered via API"
    );
    download_and_apply_update(&state, &config, &metadata).await
}

/// 启动自动更新任务
pub async fn start(state: Arc<AppState>) {
    let config = match &state.auto_update_config {
        Some(config) => config.clone(),
        None => {
            tracing::info!("Auto-update disabled");
            return;
        }
    };

    tracing::info!(
        endpoint = %config.endpoint,
        check_interval = config.check_interval_secs,
        "Starting auto-update task (blue-green strategy)"
    );

    let mut interval =
        tokio::time::interval(std::time::Duration::from_secs(config.check_interval_secs));

    loop {
        interval.tick().await;

        // 更新下次检查时间
        {
            let next = chrono::Utc::now()
                + chrono::Duration::seconds(config.check_interval_secs as i64);
            *state.auto_update_state.next_check.write().await = Some(next);
        }

        // 检查更新
        if let Err(e) = check_and_apply_update(&state, &config).await {
            tracing::warn!(error = %e, "Auto-update check failed");
            *state.auto_update_state.last_check_result.write().await =
                Some(format!("Error: {}", e));
        }
    }
}

/// 检查并应用更新 (蓝绿部署)
async fn check_and_apply_update(
    state: &Arc<AppState>,
    config: &AutoUpdateConfig,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // 更新检查时间
    *state.auto_update_state.last_check.write().await = Some(chrono::Utc::now());

    // 获取元数据
    let metadata = fetch_update_metadata(&config.metadata_url()).await?;

    // 检查版本
    *state.auto_update_state.latest_version.write().await = Some(metadata.version.clone());

    if !is_newer_version(&metadata.version, VERSION) {
        *state.auto_update_state.update_available.write().await = false;
        *state.auto_update_state.last_check_result.write().await =
            Some("Already up to date".to_string());
        tracing::info!(
            current = VERSION,
            latest = %metadata.version,
            "No update available"
        );
        return Ok(());
    }

    *state.auto_update_state.update_available.write().await = true;
    tracing::info!(
        current = VERSION,
        latest = %metadata.version,
        "Update available, starting blue-green deployment"
    );

    // 执行蓝绿部署更新
    match download_and_apply_update(state, config, &metadata).await {
        Ok(()) => {
            *state.auto_update_state.last_check_result.write().await =
                Some(format!("Successfully updated to {}", metadata.version));
            tracing::info!(version = %metadata.version, "Update applied successfully");
        }
        Err(e) => {
            *state.auto_update_state.last_check_result.write().await =
                Some(format!("Update failed: {}", e));
            tracing::error!(error = %e, "Failed to apply update");
            return Err(e);
        }
    }

    Ok(())
}

/// 下载并应用更新 (蓝绿部署策略)
async fn download_and_apply_update(
    state: &Arc<AppState>,
    config: &AutoUpdateConfig,
    metadata: &UpdateMetadata,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let binary_url = config.binary_url(&metadata.version);

    // Step 1: 下载新二进制
    *state.auto_update_state.update_progress.write().await = "downloading".to_string();
    tracing::info!(url = %binary_url, "Downloading new binary");

    let temp_dir = std::env::temp_dir();
    let temp_binary_path = temp_dir.join(format!("xjp-deploy-agent-{}", metadata.version));

    download_binary(&binary_url, &temp_binary_path).await?;

    // Step 2: 验证校验和 (如果有)
    if let Some(ref expected_sha256) = metadata.sha256 {
        *state.auto_update_state.update_progress.write().await = "verifying".to_string();
        tracing::info!("Verifying checksum");

        verify_checksum(&temp_binary_path, expected_sha256).await?;
        tracing::info!("Checksum verified");
    }

    // Step 3: 蓝绿健康检查 - 启动 canary 实例
    *state.auto_update_state.update_progress.write().await = "health_check".to_string();
    tracing::info!("Starting canary health check");

    let canary_healthy = run_canary_health_check(&temp_binary_path).await;

    if !canary_healthy {
        // 清理临时文件
        let _ = fs::remove_file(&temp_binary_path).await;
        *state.auto_update_state.update_progress.write().await = "none".to_string();
        return Err("Canary health check failed - keeping current version".into());
    }

    tracing::info!("Canary health check passed");

    // Step 4: 替换当前二进制
    *state.auto_update_state.update_progress.write().await = "applying".to_string();
    tracing::info!("Applying update - replacing binary");

    let current_binary = std::env::current_exe()?;
    let backup_binary = current_binary.with_extension("bak");

    // 备份当前二进制
    if current_binary.exists() {
        fs::rename(&current_binary, &backup_binary).await?;
    }

    // 移动新二进制到当前位置
    if let Err(e) = fs::rename(&temp_binary_path, &current_binary).await {
        // 恢复备份
        if backup_binary.exists() {
            let _ = fs::rename(&backup_binary, &current_binary).await;
        }
        return Err(format!("Failed to replace binary: {}", e).into());
    }

    // 设置执行权限 (Unix only)
    #[cfg(unix)]
    fs::set_permissions(&current_binary, std::fs::Permissions::from_mode(0o755)).await?;

    // 清理备份
    let _ = fs::remove_file(&backup_binary).await;

    *state.auto_update_state.update_progress.write().await = "restarting".to_string();
    tracing::info!("Update applied, restarting...");

    // Step 5: 重启进程
    // 使用 exec 系统调用替换当前进程
    restart_self(&current_binary).await?;

    Ok(())
}

/// 下载二进制文件
async fn download_binary(
    url: &str,
    dest: &PathBuf,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let response = reqwest::get(url).await?;

    if !response.status().is_success() {
        return Err(format!("Download failed with status: {}", response.status()).into());
    }

    let bytes = response.bytes().await?;

    let mut file = fs::File::create(dest).await?;
    file.write_all(&bytes).await?;

    // 设置执行权限 (Unix only)
    #[cfg(unix)]
    fs::set_permissions(dest, std::fs::Permissions::from_mode(0o755)).await?;

    tracing::info!(
        path = %dest.display(),
        size = bytes.len(),
        "Binary downloaded"
    );

    Ok(())
}

/// 验证文件校验和
async fn verify_checksum(
    path: &PathBuf,
    expected: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let contents = fs::read(path).await?;
    let mut hasher = Sha256::new();
    hasher.update(&contents);
    let result = hasher.finalize();
    let actual = format!("{:x}", result);

    if actual != expected.to_lowercase() {
        return Err(format!(
            "Checksum mismatch: expected {}, got {}",
            expected, actual
        )
        .into());
    }

    Ok(())
}

/// 运行 canary 健康检查
///
/// 启动新二进制进行健康检查，验证它能正常启动并响应
async fn run_canary_health_check(binary_path: &PathBuf) -> bool {
    tracing::info!(
        path = %binary_path.display(),
        port = CANARY_PORT,
        "Starting canary instance for health check"
    );

    // 启动 canary 进程
    // 注意：通过环境变量配置而不是 CLI 参数，因为 main.rs 不支持 CLI 参数
    let canary_result = Command::new(binary_path)
        .env("PORT", CANARY_PORT.to_string())
        .env("AUTO_UPDATE_ENABLED", "false") // 禁用 canary 的自动更新
        .env("DEPLOY_AGENT_API_KEY", "canary-health-check") // 提供必要的配置
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();

    let mut child = match canary_result {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %e, "Failed to start canary process");
            return false;
        }
    };

    // 等待启动
    tracing::info!(
        wait_secs = CANARY_STARTUP_WAIT_SECS,
        "Waiting for canary to start"
    );
    tokio::time::sleep(std::time::Duration::from_secs(CANARY_STARTUP_WAIT_SECS)).await;

    // 检查进程是否还在运行
    match child.try_wait() {
        Ok(Some(status)) => {
            tracing::error!(
                exit_code = ?status.code(),
                "Canary process exited prematurely"
            );
            return false;
        }
        Ok(None) => {
            // 进程还在运行，继续检查
        }
        Err(e) => {
            tracing::error!(error = %e, "Failed to check canary process status");
            let _ = child.kill().await;
            return false;
        }
    }

    // 执行健康检查请求
    let health_url = format!("http://127.0.0.1:{}/health", CANARY_PORT);
    tracing::info!(url = %health_url, "Checking canary health endpoint");

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(CANARY_HEALTH_CHECK_TIMEOUT_SECS))
        .build()
        .unwrap();

    let health_check_result = client.get(&health_url).send().await;

    // 无论结果如何，先终止 canary 进程
    let _ = child.kill().await;
    let _ = child.wait().await;

    match health_check_result {
        Ok(response) => {
            if response.status().is_success() {
                tracing::info!("Canary health check passed");
                true
            } else {
                tracing::error!(
                    status = %response.status(),
                    "Canary health check failed with non-success status"
                );
                false
            }
        }
        Err(e) => {
            tracing::error!(error = %e, "Canary health check request failed");
            false
        }
    }
}

/// 重启当前进程
async fn restart_self(binary_path: &PathBuf) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // 获取当前进程的参数
    let args: Vec<String> = std::env::args().collect();

    tracing::info!(
        binary = %binary_path.display(),
        args = ?args,
        "Restarting process"
    );

    // 在 Unix 系统上使用 exec 替换当前进程
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let err = std::process::Command::new(binary_path)
            .args(&args[1..])
            .exec();
        // 如果 exec 返回，说明出错了
        return Err(format!("exec failed: {}", err).into());
    }

    // 在 Windows 系统上，使用 PowerShell 脚本延迟重启
    #[cfg(windows)]
    {
        restart_self_windows(binary_path, &args).await?;
        return Ok(()); // Windows 函数处理了退出
    }

    #[cfg(not(any(unix, windows)))]
    {
        // Fallback for other platforms
        std::process::Command::new(binary_path)
            .args(&args[1..])
            .spawn()?;
        std::process::exit(0);
    }
}

/// Windows 专用重启函数
///
/// 由于 Windows 无法替换正在运行的 EXE，我们使用 PowerShell 脚本：
/// 1. 当前进程退出
/// 2. PowerShell 等待并复制新二进制
/// 3. PowerShell 启动新版本
#[cfg(windows)]
async fn restart_self_windows(
    binary_path: &PathBuf,
    args: &[String],
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    use std::io::Write;

    let binary_str = binary_path.to_string_lossy().replace('\\', "\\\\");
    let args_str = if args.len() > 1 {
        args[1..].join(" ")
    } else {
        String::new()
    };

    // 创建 PowerShell 重启脚本
    let ps_script = format!(
        r#"
# XJP Deploy Agent 自动更新脚本
$ErrorActionPreference = 'SilentlyContinue'

# 等待旧进程退出
Start-Sleep -Seconds 2

# 尝试停止旧进程 (以防万一)
Get-Process -Name 'xjp-deploy-agent' | Stop-Process -Force

# 再等待一下确保文件释放
Start-Sleep -Seconds 1

# 启动新版本
$binary = "{binary}"
$args = "{args}"

if ($args) {{
    Start-Process -FilePath $binary -ArgumentList $args -WindowStyle Hidden
}} else {{
    Start-Process -FilePath $binary -WindowStyle Hidden
}}

# 清理脚本自身
Start-Sleep -Seconds 1
Remove-Item -Path $MyInvocation.MyCommand.Path -Force
"#,
        binary = binary_str,
        args = args_str
    );

    // 写入临时脚本文件
    let script_path = std::env::temp_dir().join("xjp-deploy-agent-update.ps1");
    let mut file = std::fs::File::create(&script_path)?;
    file.write_all(ps_script.as_bytes())?;
    drop(file);

    tracing::info!(
        script = %script_path.display(),
        "Starting PowerShell update script"
    );

    // 启动 PowerShell 脚本 (隐藏窗口)
    std::process::Command::new("powershell")
        .args([
            "-WindowStyle",
            "Hidden",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
            script_path.to_str().unwrap(),
        ])
        .spawn()?;

    // 给脚本一点时间启动
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    tracing::info!("Exiting for update...");
    std::process::exit(0);
}

/// 获取更新元数据
async fn fetch_update_metadata(
    url: &str,
) -> Result<UpdateMetadata, Box<dyn std::error::Error + Send + Sync>> {
    let response = reqwest::get(url).await?;
    let metadata: UpdateMetadata = response.json().await?;
    Ok(metadata)
}

/// 比较版本号
fn is_newer_version(latest: &str, current: &str) -> bool {
    let parse_version = |v: &str| -> Vec<u32> {
        v.split('.')
            .filter_map(|s| s.parse().ok())
            .collect()
    };

    let latest_parts = parse_version(latest);
    let current_parts = parse_version(current);

    for (l, c) in latest_parts.iter().zip(current_parts.iter()) {
        match l.cmp(c) {
            std::cmp::Ordering::Greater => return true,
            std::cmp::Ordering::Less => return false,
            std::cmp::Ordering::Equal => continue,
        }
    }

    latest_parts.len() > current_parts.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_newer_version() {
        assert!(is_newer_version("1.0.1", "1.0.0"));
        assert!(is_newer_version("1.1.0", "1.0.0"));
        assert!(is_newer_version("2.0.0", "1.9.9"));
        assert!(!is_newer_version("1.0.0", "1.0.0"));
        assert!(!is_newer_version("0.9.9", "1.0.0"));
    }
}
